use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tokio::signal::unix::{SignalKind, signal};

#[derive(Parser)]
#[command(name = "escrow", version, about = "Escrowed filesystem IO for scopes")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground.
    Daemon {
        /// Unix socket to serve the protocol on.
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
        /// Policy file (YAML): read rules enforced by the gate.
        #[arg(long)]
        policy: Option<PathBuf>,
        /// FUSE request threads.
        #[arg(long, default_value_t = 4)]
        threads: usize,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Daemon {
            socket,
            project,
            state,
            mount,
            policy,
            threads,
        } => {
            let mut term = signal(SignalKind::terminate())?;
            let shutdown = async move {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            };
            let config = escrowd::daemon::Config {
                socket,
                project,
                state,
                mount,
                policy,
                threads,
            };
            escrowd::daemon::run(config, shutdown).await
        }
        Command::Log { project, state, scope } => log(project, state, scope),
    }
}
