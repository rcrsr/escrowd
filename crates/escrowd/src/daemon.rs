//! Daemon startup: pre-open the project, mount the scope views, serve the protocol
//! and the exec socket.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};

use fuser::BackgroundSession;

use crate::exec::{self, Children, ExecServer};
use crate::gate::Gate;
use crate::policy::{Policy, RootRules, Rule};
use crate::roots::{self, Rules};
use crate::sandbox::{self, Mount, Sandbox};
use crate::views::{RootSpec, Unscoped, Views};
use crate::{fuse, rpc, sys};

pub struct Config {
    pub socket: PathBuf,
    pub project: PathBuf,
    /// Where scope stores and the ledger live; default `$XDG_STATE_HOME/escrowd/<project-id>`.
    pub state: Option<PathBuf>,
    /// Where the views are mounted; default `$XDG_RUNTIME_DIR/escrowd/<project-id>/view`
    /// (Ubuntu 26.04 confines fusermount3 to a few roots, `/run/user/<uid>` among them).
    pub mount: Option<PathBuf>,
    /// The policy file; none means no read rules and no extra sandbox paths.
    pub policy: Option<PathBuf>,
    pub threads: usize,
    /// What IO outside a scope does (the `unscoped` root of the mount).
    pub unscoped: Unscoped,
    /// bwrap for scope children [default: `sandbox::find_bwrap`].
    pub bwrap: Option<PathBuf>,
    /// Host paths every sandbox may read besides the policy's (`escrow run --read`); absolute.
    pub read: Vec<PathBuf>,
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

/// `$XDG_STATE_HOME/escrowd/<project-id>` (or `~/.local/state/…`) for a canonical project path.
pub fn default_state_dir(project: &Path) -> anyhow::Result<PathBuf> {
    Ok(env_dir("XDG_STATE_HOME", || {
        std::env::home_dir().map(|h| h.join(".local/state"))
    })?
    .join("escrowd")
    .join(project_id(project)))
}

/// `$XDG_RUNTIME_DIR/escrowd/<project-id>`: the views (`view/`) and sockets of `escrow run`.
pub fn default_runtime_dir(project: &Path) -> anyhow::Result<PathBuf> {
    Ok(env_dir("XDG_RUNTIME_DIR", || None)?
        .join("escrowd")
        .join(project_id(project)))
}

/// A started daemon: views mounted, sockets not yet served.
pub struct Daemon {
    pub views: Arc<Views>,
    pub project: PathBuf,
    pub state: PathBuf,
    pub mount: PathBuf,
    pub socket: PathBuf,
    pub children: Arc<Children>,
    diff: crate::diff::Caps,
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
    /// `$HOME` (served or not).
    home: Option<PathBuf>,
    bwrap: PathBuf,
    session: BackgroundSession,
}

/// A root the policy lists, resolved against the host.
struct Resolved {
    host: PathBuf,
    rules: Option<(Rule, Vec<(PathBuf, Rule)>)>,
}

/// Root `key`'s rules with absolute paths; a `default: passthrough` root with no
/// list is bound whole (its host path joins `passthrough`) and gets no view.
fn resolve_root(
    key: &str,
    rules: Option<&RootRules>,
    host: Option<PathBuf>,
    passthrough: &mut Vec<PathBuf>,
) -> anyhow::Result<Option<Resolved>> {
    let Some(rules) = rules else { return Ok(None) };
    let host = host.with_context(|| format!("{key}: the host has no such directory"))?;
    let paths = rules.paths(key, &host)?;
    let canonical = host
        .canonicalize()
        .with_context(|| format!("{key} {}", host.display()))?;
    if rules.default == Rule::Passthrough {
        if !paths.is_empty() {
            bail!("{key}: default passthrough binds the whole root; it takes no path lists");
        }
        passthrough.push(canonical);
        return Ok(None);
    }
    let mut kept = Vec::new();
    for (p, rule) in paths {
        let rel = p.strip_prefix(&host).expect("checked by paths()").to_path_buf();
        match rule {
            Rule::Passthrough => passthrough.push(p),
            _ => kept.push((rel, rule)),
        }
    }
    Ok(Some(Resolved {
        host: canonical,
        rules: Some((rules.default, kept)),
    }))
}

/// Mount the views and load the scopes (rolling back an interrupted commit).
pub fn start(config: Config) -> anyhow::Result<Daemon> {
    let project = config
        .project
        .canonicalize()
        .with_context(|| format!("project {}", config.project.display()))?;
    let state = match config.state {
        Some(s) => s,
        None => default_state_dir(&project)?,
    };
    let mount = match config.mount {
        Some(m) => m,
        None => default_runtime_dir(&project)?.join("view"),
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
    let policy = match &config.policy {
        Some(p) => Policy::load(p)?,
        None => Policy {
            version: 1,
            ..Default::default()
        },
    };
    let gate = Gate::new(&policy.read.deny).context("policy read.deny pattern")?;
    let mut read = policy.roots.other.read_paths()?;
    for r in config.read {
        if !r.is_absolute() {
            bail!("read path {} is not absolute", r.display());
        }
        read.push(r);
    }
    let home_dir = std::env::home_dir().filter(|h| h.is_dir());
    let mut passthrough = policy.roots.other.passthrough_paths()?;
    let home = resolve_root(
        "roots.home",
        policy.roots.home.as_ref(),
        home_dir.clone(),
        &mut passthrough,
    )?;
    let tmp = resolve_root(
        "roots.tmp",
        policy.roots.tmp.as_ref(),
        Some("/tmp".into()),
        &mut passthrough,
    )?;
    let mut write = Vec::new();
    for w in passthrough {
        std::fs::create_dir_all(&w).with_context(|| format!("passthrough {}", w.display()))?;
        let w = w.canonicalize()?;
        // A writable bind must never reach the project or escrowd's own state and views.
        for (name, p) in [("project", &project), ("state", &state), ("mount", &mount)] {
            if inside(&w, p) || inside(p, &w) {
                bail!("passthrough {} overlaps the {name} {}", w.display(), p.display());
            }
        }
        write.push(w);
    }
    let socket_dir = config.socket.parent().map(Path::to_path_buf).unwrap_or_default();
    let socket_dir = socket_dir.canonicalize().unwrap_or(socket_dir);
    let socket_name = config.socket.file_name().map(PathBuf::from).unwrap_or_default();
    let sockets = [
        socket_dir.join(&socket_name),
        exec::exec_socket(&socket_dir.join(&socket_name)),
    ];
    // Open every base before anything is mounted, so reads of it never loop through a view.
    // A root the policy does not serve is opened too: recovery may need it.
    let open = |p: &Path| sys::open_dir(p).with_context(|| format!("opening {}", p.display()));
    let mut specs = vec![RootSpec {
        host: project.clone(),
        lower: Some(open(&project)?),
        served: true,
        rules: None,
        direct: Vec::new(),
    }];
    let fallback = [
        home_dir.and_then(|h| h.canonicalize().ok()),
        Path::new("/tmp").canonicalize().ok(),
    ];
    for (resolved, fallback) in [home, tmp].into_iter().zip(fallback) {
        let Some(Resolved { host, rules }) = resolved else {
            let lower = fallback.as_deref().and_then(|p| sys::open_dir(p).ok());
            specs.push(RootSpec {
                host: fallback.unwrap_or_default(),
                lower,
                served: false,
                rules: None,
                direct: Vec::new(),
            });
            continue;
        };
        if inside(&host, &project) || inside(&host, &state) || inside(&host, &mount) {
            bail!(
                "root {} must not lie inside the project, the state or the mount",
                host.display()
            );
        }
        // Read paths may be symlinks (bound as such): match them as given, else resolved.
        let rel = |p: &PathBuf| {
            let resolved = p.canonicalize().ok();
            [Some(p), resolved.as_ref()]
                .into_iter()
                .flatten()
                .find_map(|p| p.strip_prefix(&host).ok())
                .filter(|r| !r.as_os_str().is_empty())
                .map(Path::to_path_buf)
        };
        let direct: Vec<PathBuf> = write
            .iter()
            .chain(&read)
            .filter(|p| rel(p).is_some())
            .cloned()
            .collect();
        let stubs = std::iter::once(&project).chain(&direct).filter_map(rel).collect();
        let hidden = [&state, &mount].into_iter().chain(&sockets).filter_map(rel).collect();
        let (default, paths) = rules.expect("a resolved root has rules");
        specs.push(RootSpec {
            lower: Some(open(&host)?),
            host,
            served: true,
            rules: Some(Arc::new(Rules::new(default, paths, stubs, hidden))),
            direct,
        });
    }
    let home = specs[roots::HOME].served.then(|| specs[roots::HOME].host.clone());
    let views = Arc::new(Views::new(specs, &state, &mount, gate, config.unscoped).context("loading scopes")?);
    let session = fuse::mount(views.clone(), &mount, config.threads)
        .with_context(|| format!("mounting views at {}", mount.display()))?;
    views.set_notifier(session.notifier());
    eprintln!(
        "escrowd: project {} views at {} state {}",
        project.display(),
        mount.display(),
        state.display()
    );
    Ok(Daemon {
        views,
        project,
        state,
        mount,
        socket: config.socket,
        children: Arc::new(Children::new(std::time::Duration::from_millis(policy.close.grace_ms))),
        diff: policy.diff.caps(),
        read,
        write,
        home: home.or_else(std::env::home_dir),
        bwrap: sandbox::find_bwrap(config.bwrap.as_deref()),
        session,
    })
}

impl Daemon {
    /// A sandbox that hides escrowd's state, views and sockets.
    pub fn sandbox(&self) -> Sandbox {
        Sandbox {
            bwrap: self.bwrap.clone(),
            read: self.read.clone(),
            write: self.write.clone(),
            hide_dirs: vec![self.state.clone(), self.mount.clone()],
            hide_files: vec![self.socket.clone(), exec::exec_socket(&self.socket)],
            home: self.home.clone(),
        }
    }

    /// Scope `id`'s views of the roots other than the project, as sandbox mounts.
    pub fn root_mounts(&self, id: &str) -> Vec<Mount> {
        self.views
            .root_views(id)
            .into_iter()
            .map(|r| Mount::Bind(r.view, r.host))
            .collect()
    }

    /// Serve the gRPC and exec sockets until `shutdown` resolves, then stop every
    /// child and unmount.
    pub async fn serve(self, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
        let exec_path = exec::exec_socket(&self.socket);
        let exec_listener = rpc::bind(&exec_path)?;
        let server = Arc::new(ExecServer {
            views: self.views.clone(),
            sandbox: self.sandbox(),
            project: self.project.clone(),
            mount: self.mount.clone(),
            children: self.children.clone(),
        });
        let accept = tokio::spawn(async move {
            while let Ok((stream, _)) = exec_listener.accept().await {
                let server = server.clone();
                let Ok(stream) = stream.into_std().and_then(|s| s.set_nonblocking(false).map(|()| s)) else {
                    continue;
                };
                std::thread::spawn(move || {
                    if let Err(e) = server.handle(stream) {
                        eprintln!("escrowd: exec: {e}");
                    }
                });
            }
        });
        let service = rpc::Service::new(self.views.clone(), self.children.clone(), self.diff);
        let served = rpc::serve(&self.socket, service, shutdown).await;
        accept.abort();
        let _ = std::fs::remove_file(&exec_path);
        let children = self.children.clone();
        tokio::task::spawn_blocking(move || children.stop_all()).await?;
        let session = self.session;
        tokio::task::spawn_blocking(move || session.umount_and_join())
            .await?
            .context("unmounting views")?;
        self.views.flush().context("writing scope stores")?;
        served
    }
}

pub async fn run(config: Config, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
    start(config)?.serve(shutdown).await
}
