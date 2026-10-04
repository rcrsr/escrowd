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
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Daemon { socket } => {
            let mut term = signal(SignalKind::terminate())?;
            let shutdown = async move {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            };
            escrowd::rpc::serve(&socket, shutdown).await
        }
    }
}
