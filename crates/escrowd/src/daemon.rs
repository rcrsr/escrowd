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
use crate::reviewer::Runner;
use crate::roots::{self, Protected, Rules};
use crate::sandbox::{self, Mount, Sandbox};
use crate::views::{RootSpec, Unscoped, Views};
use crate::{fuse, review, rpc, sys};

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
    /// Command reviewers running at once.
    reviewers_parallel: usize,
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
    /// `$HOME` (served or not).
    home: Option<PathBuf>,
    bwrap: PathBuf,
    session: BackgroundSession,
    /// `<state>/lock`, held while the daemon runs (see `lock_state`).
    _lock: std::fs::File,
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

/// `path` made absolute with its longest existing prefix canonicalized: what
/// `canonicalize` returns once the directory exists, without creating it.
fn resolve_lexical(path: &Path) -> anyhow::Result<PathBuf> {
    let abs = std::path::absolute(path).with_context(|| format!("resolving {}", path.display()))?;
    let mut tail = Vec::new();
    let mut head = abs.as_path();
    loop {
        match head.canonicalize() {
            Ok(mut base) => {
                base.extend(tail.iter().rev());
                return Ok(base);
            }
            Err(_) => {
                let (Some(parent), Some(name)) = (head.parent(), head.file_name()) else {
                    if head.parent().is_some() {
                        // A `..` after a missing directory cannot be resolved without creating it.
                        bail!("{} has `..` after a directory that does not exist", path.display());
                    }
                    return Ok(abs);
                };
                tail.push(name);
                head = parent;
            }
        }
    }
}

/// Everything the configuration and policy resolve to; built without touching the
/// filesystem beyond reads, so `check` and `start` agree on what is valid.
struct Validated {
    project: PathBuf,
    state: PathBuf,
    mount: PathBuf,
    policy: Policy,
    gate: Gate,
    review: review::Rules,
    read: Vec<PathBuf>,
    home_dir: Option<PathBuf>,
    home: Option<Resolved>,
    tmp: Option<Resolved>,
    /// Writable binds, resolved but possibly not yet created.
    write: Vec<PathBuf>,
    /// The policy file and each reviewer program, as given and canonical (absolute).
    protected: Vec<PathBuf>,
}

/// Each of `paths` made absolute, plus its canonical form when that differs; sorted, no duplicates.
fn protected_forms(paths: impl IntoIterator<Item = PathBuf>) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = std::collections::BTreeSet::new();
    for p in paths {
        let abs = std::path::absolute(&p).with_context(|| format!("resolving {}", p.display()))?;
        if let Ok(canonical) = abs.canonicalize() {
            out.insert(canonical);
        }
        out.insert(abs);
    }
    Ok(out.into_iter().collect())
}

/// `protected` relative to `host`, as the root's view sees it; None when none lies in it.
fn protected_in(host: &Path, protected: &[PathBuf]) -> Option<Arc<Protected>> {
    let rel: Vec<PathBuf> = protected
        .iter()
        .filter_map(|p| p.strip_prefix(host).ok())
        .filter(|r| !r.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect();
    (!rel.is_empty()).then(|| Arc::new(Protected::new(rel)))
}

/// Load and compile the policy and check the paths against one another. Creates no
/// directory, takes no lock, opens no store and mounts nothing.
fn validate(config: &Config) -> anyhow::Result<Validated> {
    let project = config
        .project
        .canonicalize()
        .with_context(|| format!("project {}", config.project.display()))?;
    let state = match &config.state {
        Some(s) => s.clone(),
        None => default_state_dir(&project)?,
    };
    let mount = match &config.mount {
        Some(m) => m.clone(),
        None => default_runtime_dir(&project)?.join("view"),
    };
    let (state, mount) = (resolve_lexical(&state)?, resolve_lexical(&mount)?);
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
    for r in &config.read {
        if !r.is_absolute() {
            bail!("read path {} is not absolute", r.display());
        }
        read.push(r.clone());
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
        let w = resolve_lexical(&w)?;
        // A writable bind must never reach the project or escrowd's own state and views.
        for (name, p) in [("project", &project), ("state", &state), ("mount", &mount)] {
            if inside(&w, p) || inside(p, &w) {
                bail!("passthrough {} overlaps the {name} {}", w.display(), p.display());
            }
        }
        write.push(w);
    }
    for resolved in [&home, &tmp].into_iter().flatten() {
        let host = &resolved.host;
        if inside(host, &project) || inside(host, &state) || inside(host, &mount) {
            bail!(
                "root {} must not lie inside the project, the state or the mount",
                host.display()
            );
        }
    }
    let programs = policy
        .reviewers
        .iter()
        .filter(|(_, def)| def.run.is_some())
        .map(|(name, def)| def.program_path(name))
        .collect::<anyhow::Result<Vec<_>>>()?;
    for (name, def) in &policy.reviewers {
        for var in def.env.iter().filter(|v| std::env::var_os(v).is_none()) {
            eprintln!("escrowd: reviewers: {name}: env {var} is unset");
        }
    }
    let protected = protected_forms(config.policy.iter().cloned().chain(programs))?;
    if config.unscoped == Unscoped::Passthrough
        && let Some(p) = protected.iter().find(|p| inside(p, &project))
    {
        bail!(
            "unscoped passthrough: {} is protected and lies in the project",
            p.display()
        );
    }
    for w in &write {
        if let Some(p) = protected.iter().find(|p| inside(p, w)) {
            bail!("passthrough {} contains protected {}", w.display(), p.display());
        }
    }
    let review =
        review::Rules::new(&policy.review, &policy.reviewers, &policy.write).context("policy review: or write:")?;
    Ok(Validated {
        project,
        state,
        mount,
        policy,
        gate,
        review,
        read,
        home_dir,
        home,
        tmp,
        write,
        protected,
    })
}

/// What `check` found valid.
pub struct Checked {
    /// The absolute paths no scope may change (the policy file and the reviewer
    /// programs, as given and canonical), sorted.
    pub protected: Vec<PathBuf>,
}

/// Check that `config` would start: the policy loads and compiles and the paths do not
/// overlap. Creates no directory, takes no lock, opens no store and mounts nothing.
pub fn check(config: &Config) -> anyhow::Result<Checked> {
    validate(config).map(|v| Checked { protected: v.protected })
}

/// Mount the views and load the scopes (rolling back an interrupted commit).
pub fn start(config: Config) -> anyhow::Result<Daemon> {
    let Validated {
        project,
        state,
        mount,
        policy,
        gate,
        review,
        read,
        home_dir,
        home,
        tmp,
        write,
        protected,
    } = validate(&config)?;
    std::fs::create_dir_all(&state).with_context(|| format!("state dir {}", state.display()))?;
    std::fs::create_dir_all(&mount).with_context(|| format!("mount dir {}", mount.display()))?;
    let (state, mount) = (state.canonicalize()?, mount.canonicalize()?);
    let lock = lock_state(&state)?;
    for w in &write {
        std::fs::create_dir_all(w).with_context(|| format!("passthrough {}", w.display()))?;
    }
    let socket_dir = config.socket.parent().map(Path::to_path_buf).unwrap_or_default();
    let socket_dir = socket_dir.canonicalize().unwrap_or(socket_dir);
    let socket_name = config.socket.file_name().map(PathBuf::from).unwrap_or_default();
    let sockets = [
        socket_dir.join(&socket_name),
        exec::exec_socket(&socket_dir.join(&socket_name)),
        rpc::review_socket(&socket_dir.join(&socket_name)),
    ];
    let specs = open_roots(
        &Paths {
            project: &project,
            state: &state,
            mount: &mount,
            sockets: &sockets,
            protected: &protected,
        },
        home,
        tmp,
        home_dir,
        &read,
        &write,
    )?;
    let home = specs[roots::HOME].served.then(|| specs[roots::HOME].host.clone());
    let views = Arc::new(
        Views::new(specs, &state, &mount, gate, config.unscoped, policy.conflict, review).context("loading scopes")?,
    );
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
        reviewers_parallel: policy.reviewers_parallel,
        read,
        write,
        home: home.or_else(std::env::home_dir),
        bwrap: sandbox::find_bwrap(config.bwrap.as_deref()),
        session,
        _lock: lock,
    })
}

/// The paths of the project, escrowd's state and views, and its sockets.
#[derive(Clone, Copy)]
struct Paths<'a> {
    project: &'a Path,
    state: &'a Path,
    mount: &'a Path,
    sockets: &'a [PathBuf],
    /// The protected paths, absolute.
    protected: &'a [PathBuf],
}

/// Open every base before anything is mounted, so reads of it never loop through a view.
/// A root the policy does not serve is opened too: recovery may need it.
fn open_roots(
    paths: &Paths,
    home: Option<Resolved>,
    tmp: Option<Resolved>,
    home_dir: Option<PathBuf>,
    read: &[PathBuf],
    write: &[PathBuf],
) -> anyhow::Result<Vec<RootSpec>> {
    let Paths {
        project,
        state,
        mount,
        sockets,
        protected,
    } = *paths;
    let open = |p: &Path| sys::open_dir(p).with_context(|| format!("opening {}", p.display()));
    let mut specs = vec![RootSpec {
        host: project.to_path_buf(),
        lower: Some(open(project)?),
        served: true,
        rules: None,
        protected: protected_in(project, protected),
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
                protected: None,
                direct: Vec::new(),
            });
            continue;
        };
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
        let direct: Vec<PathBuf> = write.iter().chain(read).filter(|p| rel(p).is_some()).cloned().collect();
        let stubs = std::iter::once(&project.to_path_buf())
            .chain(&direct)
            .filter_map(rel)
            .collect();
        let hidden = [state, mount]
            .into_iter()
            .map(Path::to_path_buf)
            .chain(sockets.iter().cloned())
            .filter_map(|p| rel(&p))
            .collect();
        let (default, paths) = rules.expect("a resolved root has rules");
        specs.push(RootSpec {
            lower: Some(open(&host)?),
            protected: protected_in(&host, protected),
            host,
            served: true,
            rules: Some(Arc::new(Rules::new(default, paths, stubs, hidden))),
            direct,
        });
    }
    Ok(specs)
}

/// Take `<state>/lock` (`flock`, exclusive) for as long as the file stays open: start-up
/// rolls back every unfinished commit in the state, so a second daemon on it would undo
/// the first one's commit mid-apply. The kernel drops the lock when the holder dies.
fn lock_state(state: &Path) -> anyhow::Result<std::fs::File> {
    use rustix::fs::{FlockOperation, flock};
    use std::io::Write;
    let path = state.join("lock");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(rustix::io::Errno::WOULDBLOCK) => {
            let pid = std::fs::read_to_string(&path).unwrap_or_default();
            bail!("another escrowd (pid {}) serves state {}", pid.trim(), state.display())
        }
        Err(e) => return Err(std::io::Error::from(e)).with_context(|| format!("locking {}", path.display())),
    }
    file.set_len(0)?;
    writeln!(file, "{}", std::process::id())?;
    Ok(file)
}

impl Daemon {
    /// A sandbox that hides escrowd's state, views and sockets.
    pub fn sandbox(&self) -> Sandbox {
        Sandbox {
            bwrap: self.bwrap.clone(),
            read: self.read.clone(),
            write: self.write.clone(),
            hide_dirs: vec![self.state.clone(), self.mount.clone()],
            hide_files: vec![
                self.socket.clone(),
                exec::exec_socket(&self.socket),
                rpc::review_socket(&self.socket),
            ],
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
        let runner = Runner::new(self.reviewers_parallel, rpc::review_socket(&self.socket));
        let service = rpc::Service::new(self.views.clone(), self.children.clone(), self.diff, runner);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parent_component_after_a_missing_directory_is_refused() {
        let err = resolve_lexical(Path::new("/escrowd-no-such-dir/missing/../sub")).unwrap_err();

        assert!(err.to_string().contains("`..`"), "{err}");
    }

    #[test]
    fn a_missing_tail_keeps_its_canonical_existing_prefix() {
        let resolved = resolve_lexical(Path::new("/tmp/escrowd-no-such-dir/sub")).unwrap();

        assert_eq!(
            resolved,
            Path::new("/tmp")
                .canonicalize()
                .unwrap()
                .join("escrowd-no-such-dir/sub")
        );
    }

    /// A config over a fresh project, state and mount outside it, with `policy` written
    /// to `<dir>/policy.yaml` when given.
    fn config(name: &str, policy: Option<&str>, unscoped: Unscoped) -> (Config, PathBuf) {
        let dir = crate::db::temp_dir(name).canonicalize().unwrap();
        let project = dir.join("project");
        std::fs::create_dir(&project).unwrap();
        let policy = policy.map(|text| {
            let path = dir.join("policy.yaml");
            std::fs::write(&path, text).unwrap();
            path
        });
        let config = Config {
            socket: PathBuf::new(),
            project,
            state: Some(dir.join("state")),
            mount: Some(dir.join("mount")),
            policy,
            threads: 1,
            unscoped,
            bwrap: None,
            read: Vec::new(),
        };
        (config, dir)
    }

    #[test]
    fn a_protected_policy_in_the_project_refuses_unscoped_passthrough() {
        let (mut config, dir) = config("protected-pass", None, Unscoped::Passthrough);
        let policy = config.project.join("policy.yaml");
        std::fs::write(&policy, "version: 1\n").unwrap();
        config.policy = Some(policy.clone());

        let err = check(&config).err().unwrap().to_string();
        config.unscoped = Unscoped::Implicit;
        let checked = check(&config).unwrap();

        assert_eq!(
            err,
            format!(
                "unscoped passthrough: {} is protected and lies in the project",
                policy.display()
            )
        );
        assert_eq!(checked.protected, [policy]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An executable `<dir>/tools/prog` and a symlink `<dir>/link` to it.
    fn program(dir: &Path) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let tools = dir.join("tools");
        std::fs::create_dir(&tools).unwrap();
        let prog = tools.join("prog");
        std::fs::write(&prog, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.join("link");
        symlink(&prog, &link).unwrap();
        (prog, link)
    }

    #[test]
    fn a_passthrough_path_over_a_reviewer_program_is_refused() {
        let (mut config, dir) = config("protected-write", None, Unscoped::Implicit);
        let (prog, _) = program(&dir);
        let policy = dir.join("policy.yaml");
        let text = format!(
            "version: 1\nreviewers:\n  lint: {{run: [{}]}}\nroots:\n  other:\n    passthrough: [{}]\n",
            prog.display(),
            prog.parent().unwrap().display()
        );
        std::fs::write(&policy, text).unwrap();
        config.policy = Some(policy);

        let err = check(&config).err().unwrap().to_string();

        assert_eq!(
            err,
            format!(
                "passthrough {} contains protected {}",
                prog.parent().unwrap().display(),
                prog.display()
            )
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_protected_set_has_each_path_as_given_and_canonical_sorted() {
        let (mut config, dir) = config("protected-set", None, Unscoped::Implicit);
        let (prog, link) = program(&dir);
        let policy = dir.join("policy.yaml");
        std::fs::write(
            &policy,
            format!("version: 1\nreviewers:\n  lint: {{run: [{}]}}\n", link.display()),
        )
        .unwrap();
        config.policy = Some(policy.clone());

        let checked = check(&config).unwrap();

        let mut want = vec![policy, link, prog];
        want.sort();
        assert_eq!(checked.protected, want);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
