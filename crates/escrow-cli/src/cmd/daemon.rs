//! `escrow daemon`: the daemon in the foreground, and the options it shares with `escrow run`.

use std::path::PathBuf;

use escrowd::daemon::Config;
use escrowd::views::Unscoped;
use tokio::signal::unix::{SignalKind, signal};

/// The options that configure a daemon, whether `escrow daemon` or `escrow run` starts it.
#[derive(clap::Args)]
pub(crate) struct DaemonArgs {
    /// Project directory the scopes stage changes against.
    #[arg(long)]
    pub(crate) project: PathBuf,
    /// Scope stores and ledger [default: $XDG_STATE_HOME/escrowd/<project-id>].
    #[arg(long)]
    pub(crate) state: Option<PathBuf>,
    /// Mount point of the scope views [default: $XDG_RUNTIME_DIR/escrowd/<project-id>/view].
    #[arg(long)]
    pub(crate) mount: Option<PathBuf>,
    /// Policy file (YAML): read rules, path roots.
    #[arg(long)]
    pub(crate) policy: Option<PathBuf>,
    /// bwrap binary for scope children [default: $ESCROW_BWRAP, /usr/lib/escrowd/bwrap, bwrap].
    #[arg(long)]
    pub(crate) bwrap: Option<PathBuf>,
    /// FUSE request threads.
    #[arg(long, default_value_t = 4)]
    pub(crate) threads: usize,
    /// A host path the sandboxes may read, besides the policy's roots.other.read (repeatable, absolute).
    #[arg(long = "read", value_name = "PATH")]
    pub(crate) reads: Vec<PathBuf>,
}

impl DaemonArgs {
    /// The daemon's configuration; the socket and the unscoped mode default per command.
    pub(crate) fn into_config(self, socket: PathBuf, unscoped: Unscoped) -> Config {
        Config {
            socket,
            project: self.project,
            state: self.state,
            mount: self.mount,
            policy: self.policy,
            threads: self.threads,
            unscoped,
            bwrap: self.bwrap,
            read: self.reads,
        }
    }
}

/// `escrow daemon --check`: validate the policy and paths; no socket, nothing created.
/// Prints one `protected: <path>` line per path no scope may change.
pub(crate) fn check(args: DaemonArgs, unscoped: Unscoped) -> anyhow::Result<()> {
    let checked = escrowd::daemon::check(&args.into_config(PathBuf::new(), unscoped))?;
    for p in &checked.protected {
        println!("protected: {}", p.display());
    }
    Ok(())
}

pub(crate) async fn run(args: DaemonArgs, socket: PathBuf, unscoped: Unscoped) -> anyhow::Result<()> {
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let shutdown = async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        // Shutdown waits for running calls (bounded) and unmounts; a second
        // signal exits at once and leaves the mount stale.
        tokio::spawn(async move {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            eprintln!("escrowd: second signal, exiting without unmounting");
            std::process::exit(1);
        });
    };
    escrowd::daemon::run(args.into_config(socket, unscoped), shutdown).await
}
