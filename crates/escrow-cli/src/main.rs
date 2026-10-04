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
        /// Deny reads matching this glob (a name without '/' matches at any depth). Repeatable.
        #[arg(long = "deny-read")]
        deny_read: Vec<String>,
        /// FUSE request threads.
        #[arg(long, default_value_t = 4)]
        threads: usize,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Daemon {
            socket,
            project,
            state,
            mount,
            deny_read,
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
                deny_read,
                threads,
            };
            escrowd::daemon::run(config, shutdown).await
        }
    }
}
