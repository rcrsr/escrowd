//! Daemon startup: pre-open the project, mount the scope views, serve the protocol.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};

use crate::gate::Gate;
use crate::views::Views;
use crate::{fuse, rpc, sys};

pub struct Config {
    pub socket: PathBuf,
    pub project: PathBuf,
    /// Where scope stores and the ledger live; default `$XDG_STATE_HOME/escrowd/<project-id>`.
    pub state: Option<PathBuf>,
    /// Where the views are mounted; default `$XDG_RUNTIME_DIR/escrowd/<project-id>/view`
    /// (Ubuntu 26.04 confines fusermount3 to a few roots, `/run/user/<uid>` among them).
    pub mount: Option<PathBuf>,
    pub deny_read: Vec<String>,
    pub threads: usize,
}

/// `<project basename>-<FNV-1a 64 of the canonical path>`: stable and readable.
pub fn project_id(project: &Path) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in project.as_os_str().as_bytes() {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100000001b3);
    }
    let base = project
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{base}-{h:016x}")
}

fn env_dir(var: &str, fallback: impl FnOnce() -> Option<PathBuf>) -> anyhow::Result<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(fallback)
        .with_context(|| format!("{var} is unset and has no fallback"))
}

fn inside(a: &Path, b: &Path) -> bool {
    a.starts_with(b)
}

pub async fn run(config: Config, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
    let project = config
        .project
        .canonicalize()
        .with_context(|| format!("project {}", config.project.display()))?;
    let id = project_id(&project);
    let state = match config.state {
        Some(s) => s,
        None => env_dir("XDG_STATE_HOME", || {
            std::env::home_dir().map(|h| h.join(".local/state"))
        })?
        .join("escrowd")
        .join(&id),
    };
    let mount = match config.mount {
        Some(m) => m,
        None => env_dir("XDG_RUNTIME_DIR", || None)?
            .join("escrowd")
            .join(&id)
            .join("view"),
    };
    std::fs::create_dir_all(&state).with_context(|| format!("state dir {}", state.display()))?;
    std::fs::create_dir_all(&mount).with_context(|| format!("mount dir {}", mount.display()))?;
    let (state, mount) = (state.canonicalize()?, mount.canonicalize()?);
    // The daemon must never touch its own view (deadlock), and staged files must not land in the project.
    if inside(&state, &project) || inside(&mount, &project) || inside(&project, &mount) || inside(&state, &mount) {
        bail!(
            "project {}, state {} and mount {} must not contain one another",
            project.display(),
            state.display(),
            mount.display()
        );
    }
    let gate = Gate::new(&config.deny_read).context("--deny-read pattern")?;
    // Open the base before anything is mounted, so reads of it never loop through a view.
    let lower = sys::open_dir(&project).with_context(|| format!("opening {}", project.display()))?;
    let views = Arc::new(Views::new(lower, &state, &mount, gate).context("loading scopes")?);
    let session = fuse::mount(views.clone(), &mount, config.threads)
        .with_context(|| format!("mounting views at {}", mount.display()))?;
    eprintln!(
        "escrowd: project {} views at {} state {}",
        project.display(),
        mount.display(),
        state.display()
    );
    let served = rpc::serve(&config.socket, views, shutdown).await;
    tokio::task::spawn_blocking(move || session.umount_and_join())
        .await?
        .context("unmounting views")?;
    served
}
