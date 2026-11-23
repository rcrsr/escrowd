//! `escrow run`: a command in a sandbox whose only way to the project is escrowd's view,
//! under a daemon this process hosts.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::ValueEnum;
use escrowd::daemon::default_runtime_dir;
use escrowd::exec::exec_socket;
use escrowd::proto::escrow_service_client::EscrowServiceClient;
use escrowd::proto::{DecideRequest, OutcomeStatus, SettleUnscopedRequest, Verdict};
use escrowd::sandbox::{Mount, Sandbox};
use escrowd::views::{UNSCOPED, Unscoped, Views};
use tokio::signal::unix::{SignalKind, signal};

use super::daemon::DaemonArgs;
use crate::client::{NOT_COMMITTED, channel, reviewer_names};

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum OnExit {
    Commit,
    Discard,
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

/// Close the unscoped scope and decide it over the protocol socket, so the policy's
/// review rules (`write:`, `review:`) and conflict policy apply as to any scope. Returns
/// the line to print and whether the changes were committed.
async fn settle_unscoped(socket: &Path, on_exit: OnExit) -> anyhow::Result<(String, bool)> {
    let mut client = EscrowServiceClient::new(channel(socket).await?).max_decoding_message_size(usize::MAX);
    let err = anyhow::Error::from;
    let cs = client
        .settle_unscoped(SettleUnscopedRequest {})
        .await
        .map_err(err)?
        .into_inner()
        .change_set
        .unwrap_or_default();
    let n = cs.changes.len();
    let verdict = match on_exit {
        OnExit::Commit => Verdict::Commit,
        OnExit::Discard => Verdict::Discard,
    };
    let out = client
        .decide(DecideRequest {
            scope_id: UNSCOPED.into(),
            verdict: verdict.into(),
            ..Default::default()
        })
        .await
        .map_err(err)?
        .into_inner()
        .outcome
        .unwrap_or_default();
    let committed = matches!(OutcomeStatus::try_from(out.status), Ok(OutcomeStatus::Committed));
    let msg = match OutcomeStatus::try_from(out.status) {
        Ok(OutcomeStatus::Committed) => format!("{n} unscoped change(s) committed"),
        Ok(OutcomeStatus::Discarded) if out.reasons.is_empty() => format!("{n} unscoped change(s) discarded"),
        Ok(OutcomeStatus::Discarded) => format!("{n} unscoped change(s) discarded: {}", out.reasons.join("; ")),
        Ok(OutcomeStatus::Conflict) => format!(
            "unscoped changes {}, conflict on: {}",
            if out.reopened {
                "kept for the next run"
            } else {
                "discarded"
            },
            out.paths.join(", ")
        ),
        Ok(OutcomeStatus::Returned) => format!("{n} unscoped change(s) kept for the next run"),
        // Unknown statuses read as held (protocol.md, versioning).
        _ => format!(
            "{n} unscoped change(s) held for review ({}): run `escrow daemon` on the project and decide with `escrow review`",
            reviewer_names(&out.reviewers, &out.tiers)
        ),
    };
    Ok((msg, committed))
}

/// Run the command; returns its exit code, or 124 when `--on-exit commit` ended without a
/// commit. Starting the daemon is the last step that can return early: once it serves,
/// `drive` carries every other step and the daemon is always stopped and awaited after it.
pub(crate) async fn run(
    mut args: DaemonArgs,
    socket: Option<PathBuf>,
    unscoped: Unscoped,
    on_exit: OnExit,
    cmd: Vec<OsString>,
) -> anyhow::Result<i32> {
    let project = args
        .project
        .canonicalize()
        .with_context(|| format!("project {}", args.project.display()))?;
    let socket = match socket {
        Some(s) => s,
        None => {
            let dir = default_runtime_dir(&project)?;
            std::fs::create_dir_all(&dir)?;
            dir.join("escrow.sock")
        }
    };
    // wait_for must not mistake a stale socket for this daemon's, nor a live one's.
    escrowd::rpc::clear_stale(&socket)?;
    args.project = project.clone();
    let daemon = escrowd::daemon::start(args.into_config(socket.clone(), unscoped))?;
    let views = daemon.views.clone();
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
    let driven = drive(Drive {
        views,
        sandbox,
        root_mounts,
        view,
        project,
        socket,
        unscoped,
        on_exit,
        cmd,
    })
    .await;
    let _ = stop.send(());
    let served = match served.await {
        Ok(r) => r.context("serving"),
        Err(e) => Err(anyhow::Error::from(e).context("serving")),
    };
    match (driven, served) {
        (Ok(code), Ok(())) => Ok(code),
        (Ok(_), Err(e)) => Err(e),
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(serve)) => {
            eprintln!("escrow run: {serve:#}");
            Err(e)
        }
    }
}

/// What `drive` needs from the started daemon.
struct Drive {
    views: Arc<Views>,
    sandbox: Sandbox,
    root_mounts: Vec<Mount>,
    view: PathBuf,
    project: PathBuf,
    socket: PathBuf,
    unscoped: Unscoped,
    on_exit: OnExit,
    cmd: Vec<OsString>,
}

/// Everything between the daemon serving and its stop: the held check, the sandboxed
/// app and the unscoped settle. Its error is escrow's own failure; its `Ok` is the exit code.
async fn drive(d: Drive) -> anyhow::Result<i32> {
    let Drive {
        views,
        sandbox,
        root_mounts,
        view,
        project,
        socket,
        unscoped,
        on_exit,
        cmd,
    } = d;
    // A held unscoped scope is frozen until its reviewers decide: the app would get a
    // read-only project (implicit), or the mode change would drop it (deny).
    if unscoped != Unscoped::Passthrough
        && let Ok(Some(hold)) = views.held(UNSCOPED)
    {
        anyhow::bail!(
            "the last run's unscoped changes are held for {} review: decide them first \
             (`escrow daemon --project {}` with the same --state, then `escrow review`)",
            hold.reviewers[0],
            project.display()
        );
    }
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
    let pid = child.id().context("the sandboxed command has no pid")? as i32;
    // Ctrl-C reaches the app through the terminal; the launcher outlives it to settle and unmount.
    let mut int = signal(SignalKind::interrupt())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut hup = signal(SignalKind::hangup())?;
    let status = loop {
        tokio::select! {
            s = child.wait() => break s?,
            _ = int.recv() => {}
            _ = term.recv() => {
                // SAFETY: kill takes plain integers and reads no memory; a stale pid only fails with ESRCH.
                unsafe { libc::kill(pid, libc::SIGTERM); }
            }
            _ = hup.recv() => {
                // SAFETY: kill takes plain integers and reads no memory; a stale pid only fails with ESRCH.
                unsafe { libc::kill(pid, libc::SIGHUP); }
            }
        }
    };
    let code = {
        use std::os::unix::process::ExitStatusExt;
        status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
    };

    if unscoped == Unscoped::Implicit {
        // The app has exited; its last processes may still be writing back.
        views.settle_last();
        let (msg, committed) = settle_unscoped(&socket, on_exit)
            .await
            .context("settling unscoped changes")?;
        eprintln!("escrow run: {msg}");
        // The child's own failure is never masked; a requested commit that did not
        // happen is the launcher's 124.
        if code == 0 && matches!(on_exit, OnExit::Commit) && !committed {
            return Ok(NOT_COMMITTED);
        }
    }
    Ok(code)
}
