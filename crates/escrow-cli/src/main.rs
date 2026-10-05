use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use escrowd::commit::Outcome;
use escrowd::daemon::{Config, default_runtime_dir};
use escrowd::exec::{ExecConn, exec_socket};
use escrowd::proto::escrow_client::EscrowClient;
use escrowd::proto::{GetChangeSetRequest, SpawnRequest};
use escrowd::sandbox::Mount;
use escrowd::views::{UNSCOPED, Unscoped};
use tokio::signal::unix::{SignalKind, signal};

#[derive(Parser)]
#[command(name = "escrow", version, about = "Escrowed filesystem IO for scopes")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnExit {
    Commit,
    Discard,
}

#[derive(Subcommand)]
enum Command {
    /// Run a command in a sandbox whose only way to the project is escrowd's view.
    Run {
        /// Project directory.
        #[arg(long)]
        project: PathBuf,
        /// IO outside any scope: passthrough, implicit or deny.
        #[arg(long)]
        unscoped: Unscoped,
        /// What happens to the implicit default scope when the command exits.
        #[arg(long, value_enum, default_value = "discard")]
        on_exit: OnExit,
        /// Policy file (YAML): read rules, path roots.
        #[arg(long)]
        policy: Option<PathBuf>,
        /// Scope stores and ledger [default: $XDG_STATE_HOME/escrowd/<project-id>].
        #[arg(long)]
        state: Option<PathBuf>,
        /// Mount point of the views [default: $XDG_RUNTIME_DIR/escrowd/<project-id>/view].
        #[arg(long)]
        mount: Option<PathBuf>,
        /// Daemon socket [default: $XDG_RUNTIME_DIR/escrowd/<project-id>/escrow.sock].
        #[arg(long)]
        socket: Option<PathBuf>,
        /// bwrap binary [default: $ESCROW_BWRAP, /usr/lib/escrowd/bwrap, bwrap].
        #[arg(long)]
        bwrap: Option<PathBuf>,
        /// FUSE request threads.
        #[arg(long, default_value_t = 4)]
        threads: usize,
        /// A host path the sandboxes may read, besides the policy's roots.other.read (repeatable).
        #[arg(long = "read", value_name = "PATH")]
        reads: Vec<PathBuf>,
        /// The command and its arguments.
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<OsString>,
    },
    /// Run a command in a scope's sandbox (started by the daemon, attached to this terminal).
    Exec {
        /// Scope id.
        #[arg(long)]
        scope: String,
        /// Daemon socket.
        #[arg(long, env = "ESCROW_SOCKET")]
        socket: PathBuf,
        /// The command and its arguments.
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<OsString>,
    },
    /// Run the daemon in the foreground.
    Daemon {
        /// Unix socket to serve the protocol on (the exec socket is `<socket>.exec`).
        #[arg(long, env = "ESCROW_SOCKET")]
        socket: PathBuf,
        /// Project directory the scopes stage changes against.
        #[arg(long)]
        project: PathBuf,
        /// Scope stores and ledger [default: $XDG_STATE_HOME/escrowd/<project-id>].
        #[arg(long)]
        state: Option<PathBuf>,
        /// Mount point of the scope views [default: $XDG_RUNTIME_DIR/escrowd/<project-id>/view].
        #[arg(long)]
        mount: Option<PathBuf>,
        /// Policy file (YAML): read rules enforced by the gate, path roots.
        #[arg(long)]
        policy: Option<PathBuf>,
        /// IO outside any scope, served at `<mount>/unscoped` for implicit and deny.
        #[arg(long, default_value = "passthrough")]
        unscoped: Unscoped,
        /// bwrap binary for scope children [default: $ESCROW_BWRAP, /usr/lib/escrowd/bwrap, bwrap].
        #[arg(long)]
        bwrap: Option<PathBuf>,
        /// FUSE request threads.
        #[arg(long, default_value_t = 4)]
        threads: usize,
    },
    /// Print a closed scope's diff against its snapshot (git format), before it is decided.
    Diff {
        /// Daemon socket [default: the socket of `escrow run --project`].
        #[arg(long, env = "ESCROW_SOCKET")]
        socket: Option<PathBuf>,
        /// Project directory (locates the socket of `escrow run`).
        #[arg(long)]
        project: Option<PathBuf>,
        /// Scope id (`unscoped` for the implicit default scope).
        scope: String,
    },
    /// Print the ledger, optionally for one scope.
    Log {
        /// Project directory (locates the default state directory).
        #[arg(long)]
        project: Option<PathBuf>,
        /// State directory holding ledger.log [default: from --project].
        #[arg(long)]
        state: Option<PathBuf>,
        /// Only lines for this scope id.
        scope: Option<String>,
    },
}

async fn diff(socket: Option<PathBuf>, project: Option<PathBuf>, scope: String) -> anyhow::Result<()> {
    let socket = match (socket, project) {
        (Some(s), _) => s,
        (None, Some(p)) => default_runtime_dir(&p.canonicalize()?)?.join("escrow.sock"),
        (None, None) => anyhow::bail!("pass --socket (or ESCROW_SOCKET) or --project"),
    };
    let path = socket.clone();
    let channel = tonic::transport::Endpoint::from_static("http://localhost")
        .connect_with_connector(tower::service_fn(move |_| {
            let path = path.clone();
            async move {
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                    tokio::net::UnixStream::connect(path).await?,
                ))
            }
        }))
        .await
        .with_context(|| format!("connecting to {}", socket.display()))?;
    let cs = EscrowClient::new(channel)
        .max_decoding_message_size(usize::MAX)
        .get_change_set(GetChangeSetRequest { scope_id: scope })
        .await
        .map_err(|s| anyhow::anyhow!("{}", s.message()))?
        .into_inner();
    std::io::stdout().lock().write_all(cs.diff.as_bytes())?;
    Ok(())
}

fn log(project: Option<PathBuf>, state: Option<PathBuf>, scope: Option<String>) -> anyhow::Result<()> {
    let state = match (state, project) {
        (Some(s), _) => s,
        (None, Some(p)) => escrowd::daemon::default_state_dir(&p.canonicalize()?)?,
        (None, None) => anyhow::bail!("pass --project or --state"),
    };
    let file = std::fs::File::open(state.join("ledger.log"))?;
    let needle = scope.map(|s| format!(" scope={s} "));
    let mut out = std::io::stdout().lock();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if needle.as_ref().is_none_or(|n| line.contains(n.as_str())) {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

/// Signals relayed to a child: the ones a terminal or a supervisor sends.
const RELAYED: [i32; 6] = [
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
];

fn lossy(s: impl AsRef<std::ffi::OsStr>) -> String {
    s.as_ref().to_string_lossy().into_owned()
}

async fn exec(scope: String, socket: PathBuf, cmd: Vec<OsString>) -> anyhow::Result<i32> {
    let req = SpawnRequest {
        scope_id: scope,
        argv: cmd.iter().map(lossy).collect(),
        cwd: std::env::current_dir().map(lossy).unwrap_or_default(),
        env: std::env::vars_os().map(|(k, v)| (lossy(k), lossy(v))).collect(),
    };
    let conn = ExecConn::start(&socket, &req).with_context(|| format!("connecting to {}", socket.display()))?;
    let signaller = Arc::new(conn.signaller()?);
    for sig in RELAYED {
        let mut stream = signal(SignalKind::from_raw(sig))?;
        let signaller = signaller.clone();
        tokio::spawn(async move {
            while stream.recv().await.is_some() {
                let _ = signaller.send(sig);
            }
        });
    }
    Ok(tokio::task::spawn_blocking(move || conn.wait()).await??)
}

/// Wait until the daemon's socket takes connections.
async fn wait_for(socket: &Path) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        if Instant::now() > deadline {
            anyhow::bail!("daemon did not open {}", socket.display());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run(
    project: PathBuf,
    unscoped: Unscoped,
    on_exit: OnExit,
    policy: Option<PathBuf>,
    state: Option<PathBuf>,
    mount: Option<PathBuf>,
    socket: Option<PathBuf>,
    bwrap: Option<PathBuf>,
    threads: usize,
    reads: Vec<PathBuf>,
    cmd: Vec<OsString>,
) -> anyhow::Result<i32> {
    let project = project
        .canonicalize()
        .with_context(|| format!("project {}", project.display()))?;
    let socket = match socket {
        Some(s) => s,
        None => {
            let dir = default_runtime_dir(&project)?;
            std::fs::create_dir_all(&dir)?;
            dir.join("escrow.sock")
        }
    };
    let _ = std::fs::remove_file(&socket); // wait_for must not mistake a stale socket for this daemon's
    for r in &reads {
        anyhow::ensure!(r.is_absolute(), "--read {} is not absolute", r.display());
    }
    let daemon = escrowd::daemon::start(Config {
        socket: socket.clone(),
        project: project.clone(),
        state,
        mount,
        policy,
        threads,
        unscoped,
        bwrap,
        read: reads,
    })?;
    let (views, children) = (daemon.views.clone(), daemon.children.clone());
    let sandbox = daemon.sandbox();
    // The unscoped scope's views of $HOME and /tmp. Passthrough mode has none (a scope
    // left by an earlier run in another mode may still exist).
    let root_mounts = match unscoped {
        Unscoped::Passthrough => Vec::new(),
        _ => daemon.root_mounts(UNSCOPED),
    };
    let view = daemon.mount.clone();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(daemon.serve(async {
        let _ = stopped.await;
    }));
    wait_for(&socket).await?;

    // The app's sandbox: the project as the unscoped mode says, every scope under /escrow,
    // the sockets and this binary (for `escrow exec`).
    let inner_socket = PathBuf::from("/run/escrowd/escrow.sock");
    let exe = std::env::current_exe()?;
    let exe_path = exe.clone();
    let project_src = match unscoped {
        Unscoped::Passthrough => project.clone(),
        _ => view.join(UNSCOPED),
    };
    let mut mounts = vec![
        Mount::Bind(project_src, project.clone()),
        Mount::Bind(view.clone(), "/escrow".into()),
        Mount::Bind(socket.clone(), inner_socket.clone()),
        Mount::Bind(exec_socket(&socket), exec_socket(&inner_socket)),
        Mount::RoBind(exe.clone(), exe),
    ];
    mounts.extend(root_mounts);
    let cwd = std::env::current_dir()
        .ok()
        .filter(|d| d.starts_with(&project))
        .unwrap_or_else(|| project.clone());
    let mut app = sandbox.command(&mounts, &cwd, &cmd);
    app.env("ESCROW_SOCKET", &inner_socket)
        .env("ESCROW_PROJECT", &project)
        .env("ESCROW_UNSCOPED", format!("{unscoped:?}").to_lowercase())
        .env("ESCROW_VIEWS", "/escrow")
        .env("ESCROW_EXE", &exe_path);
    let mut child = tokio::process::Command::from(app).spawn().context("starting bwrap")?;
    views.log_sandbox(UNSCOPED, &sandbox.write);
    let pid = child.id().unwrap_or(0) as i32;
    // Ctrl-C reaches the app through the terminal; the launcher outlives it to settle and unmount.
    let mut int = signal(SignalKind::interrupt())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut hup = signal(SignalKind::hangup())?;
    let status = loop {
        tokio::select! {
            s = child.wait() => break s?,
            _ = int.recv() => {}
            _ = term.recv() => unsafe { libc::kill(pid, libc::SIGTERM); },
            _ = hup.recv() => unsafe { libc::kill(pid, libc::SIGHUP); },
        }
    };
    let code = {
        use std::os::unix::process::ExitStatusExt;
        status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
    };

    if unscoped == Unscoped::Implicit {
        let settled = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
            // The app has exited; its last processes may still be writing back.
            views.settle_last();
            let stopped = children.stop(UNSCOPED);
            let cs = views.close_scope_after(UNSCOPED, &stopped)?;
            let n = cs.changes.len();
            Ok(match on_exit {
                OnExit::Discard => {
                    views.drop_scope(UNSCOPED)?;
                    format!("{n} unscoped change(s) discarded")
                }
                OnExit::Commit => match views.commit_closed(UNSCOPED, &cs)? {
                    Outcome::Committed(..) => format!("{n} unscoped change(s) committed"),
                    Outcome::Conflict(paths) => format!(
                        "unscoped changes discarded, conflict on: {}",
                        paths
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                },
            })
        })
        .await?;
        match settled {
            Ok(msg) => eprintln!("escrow: {msg}"),
            Err(e) => eprintln!("escrow: settling unscoped changes failed: {e}"),
        }
    }
    let _ = stop.send(());
    served.await??;
    Ok(code)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Run {
            project,
            unscoped,
            on_exit,
            policy,
            state,
            mount,
            socket,
            bwrap,
            threads,
            reads,
            cmd,
        } => {
            let code = run(
                project, unscoped, on_exit, policy, state, mount, socket, bwrap, threads, reads, cmd,
            )
            .await?;
            std::process::exit(code)
        }
        Command::Exec { scope, socket, cmd } => match exec(scope, socket, cmd).await {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("escrow exec: {e:#}");
                std::process::exit(125)
            }
        },
        Command::Daemon {
            socket,
            project,
            state,
            mount,
            policy,
            unscoped,
            bwrap,
            threads,
        } => {
            let mut term = signal(SignalKind::terminate())?;
            let shutdown = async move {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            };
            let config = Config {
                socket,
                project,
                state,
                mount,
                policy,
                threads,
                unscoped,
                bwrap,
                read: Vec::new(),
            };
            escrowd::daemon::run(config, shutdown).await
        }
        Command::Diff { socket, project, scope } => diff(socket, project, scope).await,
        Command::Log { project, state, scope } => log(project, state, scope),
    }
}
