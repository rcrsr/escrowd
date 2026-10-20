mod client;
mod cmd;

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use escrowd::exec::TOKEN_ENV;
use escrowd::rpc::REVIEW_SOCKET_ENV;
use escrowd::views::Unscoped;

use client::{OWN_FAILURE, fail};
use cmd::daemon::DaemonArgs;
use cmd::review::ReviewAction;
use cmd::run::OnExit;

#[derive(Parser)]
#[command(name = "escrow", version, about = "Escrowed filesystem IO for scopes")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a command in a sandbox whose only way to the project is escrowd's view.
    Run {
        #[command(flatten)]
        daemon: DaemonArgs,
        /// IO outside any scope: passthrough, implicit or deny.
        #[arg(long)]
        unscoped: Unscoped,
        /// What happens to the implicit default scope when the command exits.
        #[arg(long, value_enum, default_value = "discard")]
        on_exit: OnExit,
        /// Daemon socket [default: $XDG_RUNTIME_DIR/escrowd/<project-id>/escrow.sock].
        #[arg(long)]
        socket: Option<PathBuf>,
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
        /// The scope's token (from OpenScope); the command does not get it. The
        /// unscoped scope needs none.
        #[arg(long, env = TOKEN_ENV, hide = true, hide_env_values = true, default_value = "")]
        token: String,
        /// The command and its arguments.
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<OsString>,
    },
    /// Run the daemon in the foreground.
    Daemon {
        #[command(flatten)]
        daemon: DaemonArgs,
        /// Unix socket to serve the protocol on (the exec socket is `<socket>.exec`); required
        /// unless --check.
        #[arg(long, env = "ESCROW_SOCKET", required_unless_present = "check")]
        socket: Option<PathBuf>,
        /// IO outside any scope, served at `<mount>/unscoped` for implicit and deny.
        #[arg(long, default_value = "passthrough")]
        unscoped: Unscoped,
        /// Validate the policy and paths, then exit; creates and mounts nothing.
        #[arg(long)]
        check: bool,
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
    /// Review held scopes: list them, show one, give a reviewer's verdict (the human
    /// tier by default). Talks to the daemon's review socket (`<socket>.review`).
    Review {
        /// The review socket itself; wins over --socket and --project.
        #[arg(long, env = REVIEW_SOCKET_ENV)]
        review_socket: Option<PathBuf>,
        /// Daemon socket [default: the socket of `escrow run --project`].
        #[arg(long, env = "ESCROW_SOCKET")]
        socket: Option<PathBuf>,
        /// Project directory (locates the socket of `escrow run`).
        #[arg(long)]
        project: Option<PathBuf>,
        #[command(subcommand)]
        action: ReviewAction,
    },
    /// Print the ledger, optionally for one scope.
    Log {
        /// Project directory (locates the default state directory).
        #[arg(long)]
        project: Option<PathBuf>,
        /// State directory holding ledger.log [default: from --project].
        #[arg(long)]
        state: Option<PathBuf>,
        /// Only lines for this scope id, and the `proc` lines of the processes they name.
        scope: Option<String>,
    },
}

#[tokio::main]
async fn main() {
    match Cli::parse().command {
        Command::Run {
            daemon,
            unscoped,
            on_exit,
            socket,
            cmd,
        } => match cmd::run::run(daemon, socket, unscoped, on_exit, cmd).await {
            Ok(code) => std::process::exit(code),
            Err(e) => fail("run", e, OWN_FAILURE),
        },
        Command::Exec {
            scope,
            socket,
            token,
            cmd,
        } => match cmd::exec::run(scope, socket, token, cmd).await {
            Ok(code) => std::process::exit(code),
            Err(e) => fail("exec", e, OWN_FAILURE),
        },
        Command::Daemon {
            daemon,
            socket,
            unscoped,
            check,
        } => {
            let done = match socket {
                Some(socket) if !check => cmd::daemon::run(daemon, socket, unscoped).await,
                _ => cmd::daemon::check(daemon, unscoped),
            };
            exit_with("daemon", done.map(|()| 0));
        }
        Command::Diff { socket, project, scope } => {
            exit_with("diff", cmd::diff::run(socket, project, scope).await.map(|()| 0));
        }
        Command::Review {
            review_socket,
            socket,
            project,
            action,
        } => exit_with("review", cmd::review::run(review_socket, socket, project, action).await),
        Command::Log { project, state, scope } => exit_with("log", cmd::log::run(project, state, scope).map(|()| 0)),
    }
}

/// Exit with a command's code, or print its failure and exit 1.
fn exit_with(cmd: &str, done: anyhow::Result<i32>) -> ! {
    match done {
        Ok(code) => std::process::exit(code),
        Err(e) => fail(cmd, e, 1),
    }
}
