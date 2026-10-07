//! The scope views: one copy-on-write overlay per scope over the shared base,
//! all served from one FUSE mount whose top-level directories are the scopes.
//! Everything under `<mount>/<scope-id>/` belongs to that scope, so the path
//! alone attributes every operation.
//!
//! Ported from spike 0.4 (itself built on 0.3), with the verified constraints:
//! the base is reached only through a pre-opened fd; inode numbers are
//! `(scope index + 1) << 48 | base st_ino` (upper-only entries: `| 1 << 47 | counter`);
//! an inode stays alive while any hard link names it; lower-backed directories
//! rename with EXDEV, like overlayfs.
//!
//! A scope reads "the base" through its snapshot (`snapshot::Base`): the live
//! project overlaid with the pre-images of commits made after the scope opened.
//!
//! A scope has one view per served root (`roots`): `<id>/` over the project,
//! `<id>.home/` over `$HOME`, `<id>.tmp/` over `/tmp`. Each view is a handle of
//! its own (own upper, store and inode prefix); the scope's lifecycle calls act
//! on all of them together.
//!
//! Opening a scope issues a random token; close, decide and spawn need it
//! (`check_token`). The project view's store keeps the token's SHA-256.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use fuser::{Errno, FileType};
use rustix::fs::{OFlags, Stat};

use crate::changeset::{self, ChangeSet, Kind};
use crate::commit::{Commits, Lowers, Outcome};
use crate::diff;
use crate::error::{self, Error};
use crate::gate::Gate;
use crate::history::{self, History};
use crate::ledger::Ledger;
use crate::policy::{ConflictRules, ConflictVerdict, Tier};
use crate::proc::{Proc, Procs};
use crate::review::{self, Decision, TierReview};
use crate::roots::{self, Access, PROJECT, Rules};
use crate::snapshot::Base;
use crate::store::{Hold, ScopeState, ScopeStore, VersionKind, moved};
use crate::sys::{self, Version, parent};

pub const ROOT: u64 = 1;
/// The default scope behind `<mount>/unscoped/` (unscoped = implicit or deny).
pub const UNSCOPED: &str = "unscoped";
/// Its root keeps one inode number across resets, so a bind mount of it stays valid;
/// so do its other views' roots (`UNSCOPED_ROOT + root index`).
pub const UNSCOPED_ROOT: u64 = 2;
const SCOPE_SHIFT: u32 = 48;
pub const UPPER_BIT: u64 = 1 << 47;

pub type R<T> = Result<T, Errno>;
/// The process that asked for an operation, if known.
type By<'a> = Option<&'a Arc<Proc>>;

/// A scope just opened.
/// A scope as reviewers see it.
pub struct Identity {
    pub id: String,
    pub name: String,
    pub session: String,
    pub token_sha256: Option<String>,
    pub labels: HashMap<String, String>,
    pub hold: Option<Hold>,
}

/// A tier's review of a held scope: still held for the tiers left, or the last
/// pending tier gave its verdict (`Hold::verdict`, with every review; not written).
pub enum Reviewed {
    Held(Hold),
    Final(Hold),
}

pub struct Opened {
    pub id: String,
    /// Its project view in the mount.
    pub root: PathBuf,
    /// Its capability: close, decide and spawn need it. Kept by the caller only.
    pub token: String,
}

/// The hex SHA-256 the store keeps of a token.
pub fn token_sha256(token: &str) -> String {
    use sha2::{Digest, Sha256};
    diff::hex(&Sha256::digest(token.as_bytes()))
}

/// `rel` in `dir` opened for reading if it is a regular file; None for anything else.
fn regular_file(dir: std::os::fd::BorrowedFd, rel: &Path) -> io::Result<Option<File>> {
    let f = match sys::open(dir, rel, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK, 0) {
        Ok(f) => f,
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOENT | libc::ENXIO)) => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(f.metadata()?.is_file().then_some(f))
}

pub fn errno(e: io::Error) -> Errno {
    Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))
}

/// IO outside any scope: what the app's sandbox sees at the project path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unscoped {
    /// The real project, bound directly: real IO, not captured.
    Passthrough,
    /// A default scope, decided at exit or by `settle_unscoped`, then replaced by a fresh one.
    Implicit,
    /// A read-only view of the live project: reads pass (through the gate), changes get EROFS.
    Deny,
}

impl std::str::FromStr for Unscoped {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "passthrough" => Ok(Unscoped::Passthrough),
            "implicit" => Ok(Unscoped::Implicit),
            "deny" => Ok(Unscoped::Deny),
            _ => Err(format!("unscoped mode {s}: expected passthrough, implicit or deny")),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Loc {
    Upper,
    Lower,
}

pub struct ScopeHandle {
    /// The view's name in the mount: the scope id, plus a suffix for roots other than the project.
    pub id: String,
    /// The scope id.
    pub group: String,
    /// The root this view serves (`roots::PROJECT`, `HOME`, `TMP`).
    pub root: usize,
    /// The root's rules; None for the project, where every path is captured.
    rules: Option<Arc<Rules>>,
    pub idx: u64,
    /// The base generation the scope opened at.
    pub since: u64,
    /// Changes get EROFS (the unscoped root in `deny` mode).
    pub readonly: bool,
    pub upper: OwnedFd,
    /// Frozen between close and the decision: new IO gets EROFS, writes on open handles EBADF.
    closed: AtomicBool,
    /// `Views::unscoped_changes` when the scope opened (or reopened); 0 after a restart.
    unscoped_at: AtomicU64,
    /// Changes the unscoped mode saw while the scope was open, fixed at close.
    unscoped_seen: AtomicU64,
    store: RwLock<ScopeStore>,
}

impl ScopeHandle {
    pub(crate) fn store(&self) -> RwLockWriteGuard<'_, ScopeStore> {
        self.store.write().unwrap()
    }

    /// Lookups share the store.
    pub(crate) fn store_read(&self) -> RwLockReadGuard<'_, ScopeStore> {
        self.store.read().unwrap()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// What the view does at `rel`.
    pub fn access(&self, rel: &Path) -> Access {
        self.rules.as_ref().map_or(Access::Capture, |r| r.access(rel))
    }

    /// Some path of this view can end up in the change set.
    pub fn captures(&self) -> bool {
        self.rules.as_ref().is_none_or(|r| r.captures())
    }

    fn hidden(&self, rel: &Path) -> bool {
        self.rules.as_ref().is_some_and(|r| r.access(rel) == Access::Hidden)
    }

    /// New IO (opens, creates, any change) is refused while the scope is closed.
    fn check_open(&self) -> R<()> {
        if self.is_closed() { Err(Errno::EROFS) } else { Ok(()) }
    }
}

type Key = (String, PathBuf);

#[derive(Default)]
struct Tables {
    paths: HashMap<u64, Key>,
    inos: HashMap<Key, u64>,
    files: HashMap<u64, (Arc<ScopeHandle>, Arc<File>)>,
    /// The process that opened each file handle.
    openers: HashMap<u64, u32>,
    next_fh: u64,
    /// The base version whose pages the kernel may cache for each inode opened from the base.
    cached: HashMap<u64, Version>,
    /// Inodes copied up from a base version other than the cached one: the next open drops the pages.
    stale: std::collections::HashSet<u64>,
}

impl Tables {
    /// Entries at or under `from` now live under `to`; returns (new path, ino) of each.
    /// Only a directory has entries under it to look for.
    fn rekey(&mut self, scope: &str, from: &Path, to: &Path, dir: bool) -> Vec<(PathBuf, u64)> {
        if let Some(ino) = self.inos.remove(&(scope.to_string(), to.to_path_buf())) {
            self.paths.remove(&ino);
        }
        let old: Vec<(PathBuf, u64)> = if dir {
            self.inos
                .iter()
                .filter(|((s, p), _)| s == scope && p.starts_with(from))
                .map(|((_, p), &i)| (p.clone(), i))
                .collect()
        } else {
            let key = (scope.to_string(), from.to_path_buf());
            self.inos.get(&key).map(|&i| (key.1, i)).into_iter().collect()
        };
        let mut out = Vec::with_capacity(old.len());
        for (p, ino) in old {
            let new = moved(&p, from, to).expect("filtered on prefix");
            self.inos.remove(&(scope.to_string(), p));
            self.inos.insert((scope.to_string(), new.clone()), ino);
            self.paths.insert(ino, (scope.to_string(), new.clone()));
            out.push((new, ino));
        }
        out
    }

    /// `rel` no longer exists; its inode lives on under another hard link, if any
    /// (`linked`: it had more than one).
    fn forget_path(&mut self, scope: &str, rel: &Path, linked: bool) {
        let key = (scope.to_string(), rel.to_path_buf());
        if let Some(ino) = self.inos.remove(&key)
            && self.paths.get(&ino) == Some(&key)
        {
            let other = || self.inos.iter().find(|(_, i)| **i == ino).map(|(k, _)| k.clone());
            match linked.then(other).flatten() {
                Some(other) => self.paths.insert(ino, other),
                None => self.paths.remove(&ino),
            };
        }
    }

    /// What the kernel may do with the pages it caches for `ino`, opened at `loc`
    /// (`st`: the base entry when it comes from the base). Files in the upper change
    /// only through the kernel; a base file keeps its pages while its version is
    /// unchanged (an editor outside escrowd changes it).
    fn pages(&mut self, ino: u64, loc: Loc, st: &Stat) -> Pages {
        let fresh = !self.stale.remove(&ino);
        match loc {
            Loc::Upper if fresh => Pages::Keep,
            Loc::Upper => Pages::Drop,
            Loc::Lower => {
                let v = Version::of(st);
                match self.cached.insert(ino, v) {
                    _ if !fresh => Pages::Drop,
                    Some(before) if before == v => Pages::Keep,
                    Some(_) => Pages::Drop,
                    None => Pages::Empty(st.st_size as u64),
                }
            }
        }
    }

    /// Forget a scope's entries; returns the top-level names the kernel may have cached.
    fn forget_scope(&mut self, scope: &str, idx: u64) -> Vec<OsString> {
        let top = self
            .inos
            .keys()
            .filter(|(s, p)| s == scope && p.components().count() == 1)
            .map(|(_, p)| p.as_os_str().to_os_string())
            .collect();
        self.inos.retain(|(s, _), _| s != scope);
        self.paths.retain(|_, (s, _)| s != scope);
        self.cached.retain(|ino, _| ino >> SCOPE_SHIFT != idx);
        self.stale.retain(|ino| ino >> SCOPE_SHIFT != idx);
        top
    }
}

/// What the kernel may do with its cached pages of a file being opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pages {
    /// Drop them: the base file changed since they were cached.
    Drop,
    /// Keep them.
    Keep,
    /// None are cached (a first read-only open of a base file of this size): the
    /// daemon may store them ahead of the reads, then let the kernel keep them.
    Empty(u64),
}

pub enum Node {
    Root,
    In(Arc<ScopeHandle>, PathBuf),
}

/// One root as the daemon serves it.
pub struct RootSpec {
    /// The root's directory on the host (canonical).
    pub host: PathBuf,
    /// Opened before anything is mounted; None when the host has no such directory.
    pub lower: Option<OwnedFd>,
    /// Scopes get a view of it. The project always is.
    pub served: bool,
    /// None for the project.
    pub rules: Option<Arc<Rules>>,
    /// Host paths in the root bound directly into sandboxes (passthrough and read paths).
    pub direct: Vec<PathBuf>,
}

/// A served root of one scope, for the SDK.
pub struct RootView {
    pub host: PathBuf,
    pub view: PathBuf,
    pub direct: Vec<PathBuf>,
}

pub struct Views {
    roots: Vec<RootSpec>,
    scopes_dir: PathBuf,
    /// Dropped scopes' directories, deleted in the background (start empties it).
    trash: PathBuf,
    cleaners: Mutex<Vec<std::thread::JoinHandle<()>>>,
    mount: PathBuf,
    gate: Gate,
    ledger: Ledger,
    commits: Commits,
    /// Decided scopes of sessions, and of holds.
    pub history: History,
    unscoped: Unscoped,
    conflict: ConflictRules,
    review: review::Rules,
    procs: Procs,
    notifier: std::sync::OnceLock<fuser::Notifier>,
    scopes: RwLock<HashMap<String, Arc<ScopeHandle>>>,
    next_idx: Mutex<u64>,
    t: Mutex<Tables>,
    /// Signalled when a file handle is released.
    released: std::sync::Condvar,
    /// The app has exited: deciding the unscoped scope opens no new one.
    last_settle: AtomicBool,
    /// Changes to captured paths the unscoped scope was asked for (allowed or EROFS), since start.
    unscoped_changes: AtomicU64,
    /// Files whose writeback starts off the request threads (a slow disk must not stall them).
    writeback: Mutex<std::sync::mpsc::Sender<Arc<File>>>,
}

impl Views {
    /// `roots` (indexed by root) must be opened before the view is mounted anywhere over them.
    pub fn new(
        roots: Vec<RootSpec>,
        state_dir: &Path,
        mount: &Path,
        gate: Gate,
        unscoped: Unscoped,
        conflict: ConflictRules,
        review: review::Rules,
    ) -> io::Result<Self> {
        let scopes_dir = state_dir.join("scopes");
        fs::create_dir_all(&scopes_dir)?;
        let trash = state_dir.join("trash");
        if trash.exists() {
            fs::remove_dir_all(&trash)?;
        }
        fs::create_dir_all(&trash)?;
        let ledger = Ledger::open(&state_dir.join("ledger.log"))?;
        let commits = Commits::open(state_dir)?;
        let history = History::open(state_dir)?;
        // Roll back an interrupted commit before any scope sees the base.
        let lowers: Lowers = std::array::from_fn(|r| roots.get(r).and_then(|s| s.lower.as_ref()).map(|l| l.as_fd()));
        let mut finished = Vec::new();
        for (generation, id) in commits.recover(&lowers)? {
            history.resolve_scope(&id, history::Fate::Committed)?;
            finished.push(id.clone());
            let mut found = false;
            for root in 0..roots::COUNT {
                let dir = scopes_dir.join(roots::view_name(&id, root));
                if dir.exists() {
                    fs::remove_dir_all(&dir)?;
                    found = true;
                }
            }
            if found {
                ledger.append(&id, "decide", Path::new(""), None, "commit");
            }
            commits.scope_dropped(generation)?;
        }
        crate::fault::hit("recovered", 0)?;
        let views = Views {
            roots,
            scopes_dir,
            trash,
            cleaners: Mutex::new(Vec::new()),
            mount: mount.to_path_buf(),
            gate,
            ledger,
            commits,
            history,
            unscoped,
            conflict,
            review,
            procs: Procs::default(),
            notifier: std::sync::OnceLock::new(),
            scopes: RwLock::new(HashMap::new()),
            next_idx: Mutex::new(0),
            t: Mutex::new(Tables {
                next_fh: 1,
                ..Default::default()
            }),
            released: std::sync::Condvar::new(),
            last_settle: AtomicBool::new(false),
            unscoped_changes: AtomicU64::new(0),
            writeback: Mutex::new(writeback_thread()),
        };
        views.load_scopes()?;
        views.ensure_unscoped()?;
        views.gc(None)?;
        views.history.resolve(|id| views.fate(id, &finished))?;
        // IO outside a scope goes straight to the project, unseen: the ledger says so
        // once (logging each operation would route it through FUSE).
        if unscoped == Unscoped::Passthrough {
            views
                .ledger
                .append(UNSCOPED, "passthrough", Path::new(""), None, "allow");
        }
        Ok(views)
    }

    pub fn unscoped(&self) -> Unscoped {
        self.unscoped
    }

    /// The policy's conflict rules.
    pub fn conflict(&self) -> ConflictRules {
        self.conflict
    }

    /// The app has exited and the daemon stops after this settle: once decided, the
    /// unscoped scope is not replaced.
    pub fn settle_last(&self) {
        self.last_settle.store(true, Ordering::Release);
    }

    /// Lets a reset of the unscoped root drop the kernel's cached entries under it.
    pub fn set_notifier(&self, n: fuser::Notifier) {
        let _ = self.notifier.set(n);
    }

    /// The roots scopes get views of.
    fn served(&self) -> impl Iterator<Item = usize> + '_ {
        self.roots.iter().enumerate().filter(|(_, r)| r.served).map(|(i, _)| i)
    }

    fn lowers(&self) -> Lowers<'_> {
        std::array::from_fn(|r| self.roots.get(r).and_then(|s| s.lower.as_ref()).map(|l| l.as_fd()))
    }

    /// The base directory of `h`'s root (served roots have one).
    fn lower(&self, h: &ScopeHandle) -> std::os::fd::BorrowedFd<'_> {
        self.roots[h.root]
            .lower
            .as_ref()
            .expect("a served root has a base")
            .as_fd()
    }

    /// `rel` in root `root` as the ledger and the change set show it: project-relative,
    /// `~/…` in `$HOME`, absolute elsewhere.
    pub fn shown(&self, root: usize, rel: &Path) -> PathBuf {
        match root {
            PROJECT => rel.to_path_buf(),
            roots::HOME => Path::new("~").join(rel),
            _ => self.roots[root].host.join(rel),
        }
    }

    /// Open the default scope the unscoped mode needs, if it is missing or of the wrong kind.
    fn ensure_unscoped(&self) -> io::Result<()> {
        if self.unscoped == Unscoped::Passthrough || self.last_settle.load(Ordering::Acquire) {
            return Ok(());
        }
        let readonly = self.unscoped == Unscoped::Deny;
        if let Some(h) = self.scopes.read().unwrap().get(UNSCOPED).cloned() {
            if h.readonly == readonly {
                return Ok(());
            }
            self.remove_scope(UNSCOPED)?;
        }
        // A deny root reads the live base; an implicit one keeps a snapshot like any scope.
        let since = if readonly { u64::MAX } else { self.commits.current() };
        for root in self.served().collect::<Vec<_>>() {
            let idx = self.alloc_idx()?;
            let store = ScopeStore::create(
                &self.scopes_dir,
                &roots::view_name(UNSCOPED, root),
                UNSCOPED,
                idx,
                since,
                readonly,
                &HashMap::new(),
                None,
                "",
            )?;
            self.attach(store)?;
        }
        self.ledger.append(UNSCOPED, "open", Path::new(""), None, "allow");
        Ok(())
    }

    fn alloc_idx(&self) -> io::Result<u64> {
        let mut n = self.next_idx.lock().unwrap();
        *n += 1;
        if *n >= 1 << (64 - SCOPE_SHIFT) {
            return Err(io::Error::other("scope index space exhausted"));
        }
        Ok(*n)
    }

    /// Reattach scopes left by a previous daemon run. A view of a root the policy
    /// no longer serves is dropped with its changes.
    fn load_scopes(&self) -> io::Result<()> {
        let mut max_idx = 0;
        for e in fs::read_dir(&self.scopes_dir)? {
            let id = e?.file_name().to_string_lossy().into_owned();
            let store = ScopeStore::load(&self.scopes_dir, &id)?;
            max_idx = max_idx.max(store.idx);
            let (group, root) = roots::split_view(&id);
            if !self.roots.get(root).is_some_and(|r| r.served) {
                eprintln!("escrowd: dropping view {id}: the policy no longer serves its root");
                self.ledger
                    .append(group, "decide", &self.shown(root, Path::new("")), None, "discard");
                let gone = store.discard_to(&self.trash)?;
                fs::remove_dir_all(gone)?;
                continue;
            }
            self.attach(store)?;
        }
        *self.next_idx.lock().unwrap() = max_idx;
        Ok(())
    }

    fn attach(&self, store: ScopeStore) -> io::Result<Arc<ScopeHandle>> {
        let upper = sys::open_dir(&store.upper_dir())?;
        let (group, root) = roots::split_view(&store.id);
        let h = Arc::new(ScopeHandle {
            id: store.id.clone(),
            group: group.to_string(),
            root,
            rules: self.roots[root].rules.clone(),
            idx: store.idx,
            since: store.since,
            readonly: store.readonly,
            upper,
            closed: AtomicBool::new(store.state == ScopeState::Closed),
            unscoped_at: AtomicU64::new(self.unscoped_changes.load(Ordering::Relaxed)),
            unscoped_seen: AtomicU64::new(0),
            store: RwLock::new(store),
        });
        self.scopes.write().unwrap().insert(h.id.clone(), h.clone());
        Ok(h)
    }

    // ---- scope lifecycle (RPC) ----

    /// Create a scope with a view per served root; returns its id, its project view in
    /// the mount and its token.
    pub fn open_scope(&self, name: &str, labels: &HashMap<String, String>, session: &str) -> error::Result<Opened> {
        let since = self.commits.current();
        let mut bytes = [0u8; 32];
        sys::random(&mut bytes)?;
        let token = diff::hex(&bytes);
        let hash = token_sha256(&token);
        let mut id = String::new();
        for root in self.served().collect::<Vec<_>>() {
            let idx = self.alloc_idx()?;
            if root == PROJECT {
                id = format!("s{idx}");
            }
            let view = roots::view_name(&id, root);
            let (labels, token, session) = if root == PROJECT {
                (labels, Some(hash.as_str()), session)
            } else {
                (&HashMap::new(), None, "")
            };
            let store = ScopeStore::create(&self.scopes_dir, &view, name, idx, since, false, labels, token, session)?;
            self.attach(store)?;
        }
        self.ledger.append(&id, "open", Path::new(""), None, "allow");
        Ok(Opened {
            root: self.mount.join(&id),
            id,
            token,
        })
    }

    /// `token` is scope `id`'s (`Denied` if not; `NoScope` if no such scope).
    /// The unscoped scope has none and takes any.
    pub fn check_token(&self, id: &str, token: &str) -> error::Result<()> {
        let hs = self.handles(id)?;
        if hs[0].store_read().token_matches(&token_sha256(token)) {
            return Ok(());
        }
        self.ledger.append(id, "token", Path::new(""), None, "deny");
        Err(Error::Denied(format!("scope {id}: missing or wrong token")))
    }

    /// The served roots other than the project, with scope `id`'s view of each in the
    /// mount; none if the scope does not exist (the unscoped scope in passthrough mode).
    pub fn root_views(&self, id: &str) -> Vec<RootView> {
        if !self.scopes.read().unwrap().contains_key(id) {
            return Vec::new();
        }
        self.served()
            .filter(|r| *r != PROJECT)
            .map(|r| RootView {
                host: self.roots[r].host.clone(),
                view: self.mount.join(roots::view_name(id, r)),
                direct: self.roots[r].direct.clone(),
            })
            .collect()
    }

    /// Every view of scope `id`, the project's first.
    fn handles(&self, id: &str) -> error::Result<Vec<Arc<ScopeHandle>>> {
        let scopes = self.scopes.read().unwrap();
        let hs: Vec<Arc<ScopeHandle>> = self
            .served()
            .filter_map(|r| scopes.get(&roots::view_name(id, r)).cloned())
            .collect();
        if hs.first().is_none_or(|h| h.root != PROJECT) {
            return Err(Error::no_scope(id));
        }
        Ok(hs)
    }

    /// The change set of every view of a scope.
    fn change_set(&self, hs: &[Arc<ScopeHandle>]) -> io::Result<ChangeSet> {
        let mut out = ChangeSet {
            changes: Vec::new(),
            reads: Vec::new(),
            labels: HashMap::new(),
            procs: BTreeMap::new(),
            unscoped: 0,
            review: review::Review::default(),
        };
        for h in hs {
            let cs = changeset::build(&self.base(h), h)?;
            if h.root == PROJECT {
                out.labels = cs.labels;
            }
            out.changes.extend(cs.changes);
            out.procs.extend(cs.procs);
            out.reads.extend(
                cs.reads
                    .into_iter()
                    .map(|(p, allowed)| (self.shown(h.root, &p), allowed)),
            );
        }
        Ok(out)
    }

    /// The change set of a closed scope, with its review; an open scope is an error
    /// (`State`).
    pub fn closed_change_set(&self, id: &str) -> error::Result<ChangeSet> {
        let hs = self.closed_handles(id)?;
        let mut cs = self.change_set(&hs)?;
        cs.review = self.review(id, &hs, &cs, false)?;
        Ok(cs)
    }

    /// Whether the policy has `review:` or `write:` rules.
    pub fn reviews(&self) -> bool {
        !self.review.is_empty()
    }

    /// Run the software tier's write rules over `cs` and find the tiers above it that
    /// `cs` needs; `log`: write each broken rule to the ledger.
    fn review(&self, id: &str, hs: &[Arc<ScopeHandle>], cs: &ChangeSet, log: bool) -> io::Result<review::Review> {
        let mut hits = Vec::new();
        let mut shown = Vec::new();
        for c in &cs.changes {
            let path = self.shown(c.root, &c.path);
            let from = c.from.as_deref().map(|f| self.shown(c.root, f));
            let writers = c.writers.iter().filter_map(|id| cs.procs.get(id));
            if let Some(p) = std::iter::once(&path).chain(&from).find(|p| self.review.denied(p)) {
                hits.push(review::Hit {
                    path: p.clone(),
                    rule: review::Broken::Deny,
                });
            } else if let Some((p, rule)) = std::iter::once(&path)
                .chain(&from)
                .find_map(|p| Some((p, self.review.only_by(p, writers.clone())?)))
            {
                hits.push(review::Hit { path: p.clone(), rule });
            } else if c.kind != Kind::Delete
                && self.review.checks_content()
                && let Some(h) = hs.iter().find(|h| h.root == c.root)
                && let Some(f) = regular_file(h.upper.as_fd(), &c.path)?
                && let Some(found) = self.review.forbidden_content(f)?
            {
                hits.push(review::Hit {
                    path: path.clone(),
                    rule: review::Broken::Content(found.to_string()),
                });
            }
            shown.push(path);
            shown.extend(from);
        }
        if log {
            for hit in &hits {
                self.ledger.append(id, hit.op(), &hit.path, None, "deny");
            }
        }
        Ok(self.review.review(shown.iter().map(PathBuf::as_path), hits))
    }

    /// The content diff of a closed scope's change set `cs`, against its snapshot.
    pub fn diff(&self, id: &str, cs: &ChangeSet, caps: diff::Caps) -> error::Result<String> {
        let hs = self.closed_handles(id)?;
        let mut out = diff::Diff::new(caps);
        for h in &hs {
            let changes: Vec<_> = cs.changes.iter().filter(|c| c.root == h.root).cloned().collect();
            if changes.is_empty() {
                continue;
            }
            let base = self.base(h);
            let root = diff::Root {
                base: &base,
                upper: h.upper.as_fd(),
                shown: &|p| self.shown(h.root, p).to_string_lossy().into_owned(),
                withheld: &|p| h.root == PROJECT && !self.gate.read_allowed(p),
            };
            out.add(&root, &changes)?;
        }
        Ok(out.finish())
    }

    /// Freeze the scope and return its change set, after its sandboxes (process groups
    /// `stopped`) were stopped. The caller has fsynced its open files (syncfs is not a
    /// barrier on plain FUSE). Closing a closed scope returns the same change set.
    pub fn close_scope_after(&self, id: &str, stopped: &[i32]) -> error::Result<ChangeSet> {
        let hs = self.handles(id)?;
        let first = !hs[0].is_closed();
        if first {
            self.settle_dead_handles(id, stopped, std::time::Duration::from_secs(5));
            if id != UNSCOPED {
                let now = self.unscoped_changes.load(Ordering::Relaxed);
                let at = hs[0].unscoped_at.load(Ordering::Relaxed);
                hs[0].unscoped_seen.store(now.saturating_sub(at), Ordering::Relaxed);
            }
            for h in &hs {
                h.store().set_state(ScopeState::Closed)?;
                h.closed.store(true, Ordering::Release);
            }
            self.ledger.append(id, "close", Path::new(""), None, "allow");
        }
        let mut cs = self.change_set(&hs)?;
        cs.unscoped = hs[0].unscoped_seen.load(Ordering::Relaxed);
        cs.review = self.review(id, &hs, &cs, first)?;
        Ok(cs)
    }

    /// The hold of scope `id`, if reviewers have it (`NoScope` if no such scope).
    pub fn held(&self, id: &str) -> error::Result<Option<Hold>> {
        Ok(self.handles(id)?[0].store_read().hold().cloned())
    }

    /// Scope `id`'s name, session, token hash and hold (`NoScope` if no such scope).
    pub fn identity(&self, id: &str) -> error::Result<Identity> {
        let hs = self.handles(id)?;
        let store = hs[0].store_read();
        Ok(Identity {
            id: id.to_string(),
            name: store.name.clone(),
            session: store.session.clone(),
            token_sha256: store.token_sha256().map(str::to_string),
            labels: store.labels()?,
            hold: store.hold().cloned(),
        })
    }

    /// Every held scope, oldest hold first.
    pub fn held_scopes(&self) -> error::Result<Vec<Identity>> {
        let ids: Vec<String> = {
            let scopes = self.scopes.read().unwrap();
            scopes
                .values()
                .filter(|h| h.root == PROJECT && h.store_read().hold().is_some())
                .map(|h| h.group.clone())
                .collect()
        };
        let mut out = Vec::new();
        for id in ids {
            match self.identity(&id) {
                Ok(i) if i.hold.is_some() => out.push(i),
                Ok(_) => {}
                Err(e) if e.is_no_scope() => {}
                Err(e) => return Err(e),
            }
        }
        out.sort_by(|a, b| (a.hold.as_ref().map(|h| h.at_ms), &a.id).cmp(&(b.hold.as_ref().map(|h| h.at_ms), &b.id)));
        Ok(out)
    }

    /// Hold closed scope `id` for the reviewers of `tiers` (cheapest first); `wait`: its
    /// session's next scope does not open until the verdict.
    pub fn hold_scope(&self, id: &str, tiers: Vec<Tier>, wait: bool) -> error::Result<Hold> {
        let hs = self.closed_handles(id)?;
        let names: Vec<&str> = tiers.iter().map(|t| t.as_str()).collect();
        let decision = format!("{}{}", names.join(","), if wait { ",wait" } else { "" });
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let hold = Hold {
            tiers,
            wait,
            at_ms,
            verdict: Decision::Commit,
            reviews: Vec::new(),
        };
        hs[0].store().set_hold(Some(hold.clone()))?;
        self.ledger.append(id, "hold", Path::new(""), None, &decision);
        Ok(hold)
    }

    /// `tier`'s verdict on held scope `id`. The tier must be pending; a human's verdict
    /// also stands for the cheaper tiers pending before it. A verdict looser than the
    /// one so far needs a human and `over` (`Denied` otherwise); the ledger
    /// records each verdict and every override. Before the last pending tier the hold
    /// keeps the verdict (written at once); after it nothing is written: the caller
    /// applies the final verdict, which drops or reopens the scope.
    pub fn review_scope(
        &self,
        id: &str,
        tier: Tier,
        verdict: Decision,
        reasons: Vec<String>,
        over: bool,
    ) -> error::Result<Reviewed> {
        let hs = self.closed_handles(id)?;
        let mut store = hs[0].store();
        let Some(mut hold) = store.hold().cloned() else {
            return Err(Error::State(format!("scope {id} is not held")));
        };
        let Some(at) = hold.tiers.iter().position(|t| *t == tier) else {
            let waiting: Vec<&str> = hold.tiers.iter().map(|t| t.as_str()).collect();
            return Err(Error::State(format!(
                "scope {id} is not waiting for {}: it waits for {}",
                tier.as_str(),
                waiting.join(", ")
            )));
        };
        if tier != Tier::Human && at > 0 {
            return Err(Error::State(format!(
                "scope {id} waits for {} first",
                hold.tiers[0].as_str()
            )));
        }
        let looser = verdict < hold.verdict;
        if looser && !(over && tier == Tier::Human) {
            return Err(Error::Denied(format!(
                "{} cannot loosen {} to {}: only a human can, with an override",
                tier.as_str(),
                hold.verdict.as_str(),
                verdict.as_str()
            )));
        }
        if looser {
            let change = format!("{}-to-{}", hold.verdict.as_str(), verdict.as_str());
            self.ledger.append(id, "override", Path::new(""), None, &change);
            hold.verdict = verdict;
        } else {
            hold.verdict = hold.verdict.max(verdict);
        }
        self.ledger.append(
            id,
            &format!("review-{}", tier.as_str()),
            Path::new(""),
            None,
            verdict.as_str(),
        );
        hold.reviews.push(TierReview {
            tier,
            verdict,
            reasons,
            over: looser,
        });
        hold.tiers.drain(..=at);
        if hold.tiers.is_empty() {
            return Ok(Reviewed::Final(hold));
        }
        store.set_hold(Some(hold.clone()))?;
        Ok(Reviewed::Held(hold))
    }

    /// The held scope of `session` its next scope waits for, if any.
    pub fn blocking_hold(&self, session: &str) -> Option<String> {
        if session.is_empty() {
            return None;
        }
        let scopes = self.scopes.read().unwrap();
        let mut ids: Vec<&String> = scopes
            .values()
            .filter(|h| h.root == PROJECT)
            .filter(|h| {
                let store = h.store_read();
                store.session == session && store.hold().is_some_and(|hold| hold.wait)
            })
            .map(|h| &h.group)
            .collect();
        ids.sort();
        ids.first().map(|id| id.to_string())
    }

    /// Return to agent: the decision's reasons go back and the scope accepts IO again.
    pub fn reopen_scope(&self, id: &str) -> error::Result<()> {
        let hs = self.closed_handles(id)?;
        let now = self.unscoped_changes.load(Ordering::Relaxed);
        if hs[0].store_read().hold().is_some() {
            hs[0].store().set_hold(None)?;
        }
        for h in &hs {
            h.store().set_state(ScopeState::Open)?;
            h.unscoped_at.store(now, Ordering::Relaxed);
            h.closed.store(false, Ordering::Release);
        }
        self.ledger.append(id, "decide", Path::new(""), None, "return");
        Ok(())
    }

    /// What a scope with a pending decision shows at start (`history::Fate`); `finished`:
    /// the scopes whose commit the journal finished.
    fn fate(&self, id: &str, finished: &[String]) -> history::Fate {
        if finished.iter().any(|f| f == id) {
            return history::Fate::Committed;
        }
        let Ok(hs) = self.handles(id) else {
            return history::Fate::Gone;
        };
        if hs[0].is_closed() {
            return history::Fate::Closed;
        }
        // A decided unscoped scope is replaced by a fresh one, with no changes.
        if id == UNSCOPED && self.change_set(&hs).is_ok_and(|cs| cs.changes.is_empty()) {
            return history::Fate::Gone;
        }
        history::Fate::Open
    }

    /// Commit a closed scope's change set to the project, all or nothing, and drop the
    /// scope. On a conflict nothing is written and the policy's `conflict.verdict`
    /// drops the scope (discard) or reopens it (return); on a failure the commit is
    /// rolled back and the scope stays closed (`Aborted`).
    pub fn commit_scope(&self, id: &str) -> error::Result<Outcome> {
        let hs = self.closed_handles(id)?;
        let cs = self.change_set(&hs)?;
        self.commit_change_set(id, &hs, &cs, false)
    }

    /// `commit_scope` with the change set `close_scope` returned (the scope is frozen since).
    pub fn commit_closed(&self, id: &str, cs: &ChangeSet) -> error::Result<Outcome> {
        let hs = self.closed_handles(id)?;
        self.commit_change_set(id, &hs, cs, false)
    }

    /// `commit_scope` (with `cs`, if given) that keeps a committed scope until
    /// `finish_commit`: the caller settles the decision in the history in between, so
    /// a scope gone at the next start was never committed (`history::Fate`).
    pub fn commit_kept(&self, id: &str, cs: Option<&ChangeSet>) -> error::Result<Outcome> {
        let hs = self.closed_handles(id)?;
        match cs {
            Some(cs) => self.commit_change_set(id, &hs, cs, true),
            None => self.commit_change_set(id, &hs, &self.change_set(&hs)?, true),
        }
    }

    /// Drop a scope `commit_kept` committed.
    pub fn finish_commit(&self, id: &str, generation: Option<u64>) -> error::Result<()> {
        self.remove_scope(id)?;
        // One journal write records the scope gone (before a new scope of the same id
        // opens) and forgets what no scope reads any more.
        self.gc(generation)?;
        Ok(self.ensure_unscoped()?)
    }

    fn closed_handles(&self, id: &str) -> error::Result<Vec<Arc<ScopeHandle>>> {
        let hs = self.handles(id)?;
        if !hs[0].is_closed() {
            return Err(Error::State(format!("scope {id} is not closed")));
        }
        Ok(hs)
    }

    fn commit_change_set(
        &self,
        id: &str,
        hs: &[Arc<ScopeHandle>],
        cs: &ChangeSet,
        keep: bool,
    ) -> error::Result<Outcome> {
        let hs: Vec<&ScopeHandle> = hs.iter().map(|h| h.as_ref()).collect();
        let show = |root: usize, p: &Path| self.shown(root, p);
        // A failed commit rolled back in place: the scope is still closed, and the
        // caller may decide again. A failed rollback is the host's problem.
        let outcome = match self.commits.commit(&self.lowers(), &hs, cs, self.conflict.reads, &show) {
            Ok(o) => o,
            Err(e) if crate::commit::rollback_failed(&e) => return Err(e.into()),
            Err(e) => return Err(Error::Aborted(format!("commit rolled back: {e}"))),
        };
        match &outcome {
            Outcome::Committed(generation, _) => {
                crate::fault::hit("committed", 0)?;
                self.ledger.append(id, "decide", Path::new(""), None, "commit");
                if !keep {
                    self.finish_commit(id, *generation)?;
                }
                return Ok(outcome);
            }
            Outcome::Conflict(paths) => {
                for p in paths {
                    self.ledger.append(id, "conflict", p, None, "deny");
                }
                self.ledger.append(id, "decide", Path::new(""), None, "conflict");
                if self.conflict.verdict == ConflictVerdict::Return {
                    // Back to the agent with its changes; reopen logs `decide=return`.
                    self.reopen_scope(id)?;
                    return Ok(outcome);
                }
                self.remove_scope(id)?;
                self.gc(None)?;
            }
        }
        self.ensure_unscoped()?;
        Ok(outcome)
    }

    /// Drop a scope and its staged changes.
    pub fn drop_scope(&self, id: &str) -> error::Result<()> {
        self.handles(id)?;
        self.ledger.append(id, "decide", Path::new(""), None, "discard");
        self.remove_scope(id)?;
        self.gc(None)?;
        Ok(self.ensure_unscoped()?)
    }

    /// Drop generations no open scope reads through any more; `dropped`: the
    /// generation whose scope was just dropped.
    fn gc(&self, dropped: Option<u64>) -> io::Result<()> {
        let oldest = self.scopes.read().unwrap().values().map(|h| h.since).min();
        self.commits.gc(oldest, dropped)
    }

    /// Remove every view of scope `id`.
    fn remove_scope(&self, id: &str) -> error::Result<()> {
        let hs: Vec<Arc<ScopeHandle>> = {
            let mut scopes = self.scopes.write().unwrap();
            (0..roots::COUNT)
                .filter_map(|r| scopes.remove(&roots::view_name(id, r)))
                .collect()
        };
        if hs.is_empty() {
            return Err(Error::no_scope(id));
        }
        for h in hs {
            let names = self.t().forget_scope(&h.id, h.idx);
            // After the last settle the mount goes away: invalidating is wasted kernel work.
            if id == UNSCOPED
                && !self.last_settle.load(Ordering::Acquire)
                && let Some(n) = self.notifier.get()
            {
                let root_ino = fuser::INodeNo(UNSCOPED_ROOT + h.root as u64);
                for name in names {
                    let _ = n.inval_entry(root_ino, &name);
                }
                let _ = n.inval_inode(root_ino, 0, 0);
            }
            // FUSE calls in flight may still hold the handle; they fail once the directory is gone.
            let gone = h.store().discard_to(&self.trash)?;
            let mut cleaners = self.cleaners.lock().unwrap();
            cleaners.retain(|c| !c.is_finished());
            cleaners.push(std::thread::spawn(move || {
                let _ = fs::remove_dir_all(gone);
            }));
        }
        Ok(())
    }

    /// At shutdown: write every scope's deferred metadata and finish deleting dropped scopes.
    pub fn flush(&self) -> io::Result<()> {
        for h in self.scopes.read().unwrap().values() {
            h.store().flush()?;
        }
        for c in self.cleaners.lock().unwrap().drain(..) {
            let _ = c.join();
        }
        Ok(())
    }

    pub fn scope_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.scopes.read().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }

    // ---- lookups ----

    fn t(&self) -> MutexGuard<'_, Tables> {
        self.t.lock().unwrap()
    }

    pub fn scope(&self, id: &str) -> R<Arc<ScopeHandle>> {
        self.scopes.read().unwrap().get(id).cloned().ok_or(Errno::ENOENT)
    }

    pub fn node(&self, ino: u64) -> R<Node> {
        if ino == ROOT {
            return Ok(Node::Root);
        }
        if (UNSCOPED_ROOT..UNSCOPED_ROOT + roots::COUNT as u64).contains(&ino) {
            let view = roots::view_name(UNSCOPED, (ino - UNSCOPED_ROOT) as usize);
            return Ok(Node::In(self.scope(&view)?, PathBuf::new()));
        }
        let (s, p) = self.t().paths.get(&ino).cloned().ok_or(Errno::ENOENT)?;
        Ok(Node::In(self.scope(&s)?, p))
    }

    /// The scope and path of an entry inside a scope; the mount root itself holds no files.
    pub fn key(&self, ino: u64) -> R<(Arc<ScopeHandle>, PathBuf)> {
        match self.node(ino)? {
            Node::Root => Err(Errno::EPERM),
            Node::In(h, p) => Ok((h, p)),
        }
    }

    pub fn child(&self, parent: u64, name: &OsStr) -> R<(Arc<ScopeHandle>, PathBuf)> {
        let (h, p) = self.key(parent)?;
        Ok((h, p.join(name)))
    }

    /// The scope's snapshot of the base.
    fn base<'a>(&'a self, h: &ScopeHandle) -> Base<'a> {
        Base::new(self.lower(h), &self.commits.gens[h.root], h.since)
    }

    fn lower_stat(&self, h: &ScopeHandle, rel: &Path) -> Option<Stat> {
        if h.store_read().hidden(rel) || h.hidden(rel) {
            return None;
        }
        self.base(h).lstat(rel).ok()
    }

    pub fn locate(&self, h: &ScopeHandle, rel: &Path) -> R<(Loc, Stat)> {
        if h.hidden(rel) {
            return Err(Errno::ENOENT);
        }
        if let Ok(st) = sys::lstat(h.upper.as_fd(), rel) {
            return Ok((Loc::Upper, st));
        }
        self.lower_stat(h, rel).map(|st| (Loc::Lower, st)).ok_or(Errno::ENOENT)
    }

    pub fn root_stat(&self) -> R<Stat> {
        sys::lstat(
            self.roots[PROJECT].lower.as_ref().ok_or(Errno::EIO)?.as_fd(),
            Path::new(""),
        )
        .map_err(errno)
    }

    /// The scope's inode for `rel`: a pinned number, else the base st_ino, else a new upper number.
    pub fn ino_for(&self, h: &ScopeHandle, rel: &Path) -> R<u64> {
        if h.group == UNSCOPED && rel.as_os_str().is_empty() {
            return Ok(UNSCOPED_ROOT + h.root as u64);
        }
        let key = (h.id.clone(), rel.to_path_buf());
        if let Some(&ino) = self.t().inos.get(&key) {
            return Ok(ino);
        }
        let prefix = h.idx << SCOPE_SHIFT;
        let pinned = h.store_read().pin(rel);
        let ino = match pinned {
            Some(ino) => ino,
            None => match self.lower_stat(h, rel).map(|st| st.st_ino) {
                Some(i) if i < UPPER_BIT => prefix | i,
                _ => {
                    let mut store = h.store();
                    let ino = prefix | UPPER_BIT | store.alloc_upper_ino();
                    store.set_pin(rel, ino).map_err(errno)?;
                    ino
                }
            },
        };
        let mut t = self.t();
        if let Some(&existing) = t.inos.get(&key) {
            return Ok(existing);
        }
        t.paths.insert(ino, key.clone());
        t.inos.insert(key, ino);
        Ok(ino)
    }

    // ---- copy-on-write ----

    fn ensure_upper_dir(&self, h: &ScopeHandle, rel: &Path) -> R<()> {
        if rel.as_os_str().is_empty() || sys::lstat(h.upper.as_fd(), rel).is_ok_and(|st| sys::is_dir(&st)) {
            return Ok(());
        }
        self.ensure_upper_dir(h, parent(rel))?;
        let mode = self.lower_stat(h, rel).map(|st| st.st_mode & 0o7777).unwrap_or(0o755);
        match sys::mkdir(h.upper.as_fd(), rel, mode) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(errno(e)),
            Err(_) => Ok(()),
            // The process umask must not change the mode the base has.
            Ok(()) => sys::chmod(h.upper.as_fd(), rel, mode).map_err(errno),
        }
    }

    fn record(&self, h: &ScopeHandle, rel: &Path, kind: VersionKind, st: &Stat) -> R<()> {
        if h.store_read().has_version(rel, kind) {
            return Ok(());
        }
        h.store().record_version(rel, kind, Version::of(st)).map_err(errno)
    }

    pub fn copy_up(&self, h: &ScopeHandle, rel: &Path) -> R<()> {
        let (loc, st) = self.locate(h, rel)?;
        if loc == Loc::Upper {
            return Ok(());
        }
        self.record(h, rel, VersionKind::Changed, &st)?;
        self.ensure_upper_dir(h, parent(rel))?;
        if sys::is_dir(&st) {
            return self.ensure_upper_dir(h, rel);
        }
        {
            let mut t = self.t();
            if let Some(&ino) = t.inos.get(&(h.id.clone(), rel.to_path_buf()))
                && t.cached.get(&ino).is_some_and(|v| *v != Version::of(&st))
            {
                t.stale.insert(ino);
            }
        }
        let (src, src_rel) = self.base(h).src(rel).map_err(errno)?;
        sys::copy_entry(src, &src_rel, h.upper.as_fd(), rel, &st, false).map_err(errno)
    }

    /// A listing the rules allow (a stub lists as empty, a denied directory gets
    /// EACCES), logged when `log` (a listing from its start).
    pub fn list_checked(&self, h: &ScopeHandle, rel: &Path, log: bool) -> R<BTreeMap<OsString, FileType>> {
        match h.access(rel) {
            Access::Deny => {
                self.log(h, "list", rel, "deny");
                Err(Errno::EACCES)
            }
            Access::Stub => Ok(BTreeMap::new()),
            Access::Hidden => Err(Errno::ENOENT),
            Access::Capture | Access::Ephemeral => {
                if log {
                    self.log(h, "list", rel, "allow");
                }
                self.list(h, rel)
            }
        }
    }

    pub fn list(&self, h: &ScopeHandle, rel: &Path) -> R<BTreeMap<OsString, FileType>> {
        let mut out = BTreeMap::new();
        let mut found = false;
        if let Ok(entries) = sys::read_dir(h.upper.as_fd(), rel) {
            found = true;
            out.extend(entries);
        }
        let opaque = h.store_read().is_opaque(rel);
        if !opaque
            && self.lower_stat(h, rel).is_some_and(|st| sys::is_dir(&st))
            && let Ok(entries) = self.base(h).read_dir(rel)
        {
            found = true;
            let store = h.store_read();
            for (name, kind) in entries {
                if !out.contains_key(&name) && !store.hidden(&rel.join(&name)) && !h.hidden(&rel.join(&name)) {
                    out.insert(name, kind);
                }
            }
        }
        if found { Ok(out) } else { Err(Errno::ENOENT) }
    }

    /// A new entry exists at `rel` in the upper: it no longer hides a base entry.
    /// A directory created where the base had one hides the base directory's contents.
    fn unwhiteout(&self, h: &ScopeHandle, rel: &Path, is_dir: bool) -> R<()> {
        let mut store = h.store();
        if store.remove_whiteout(rel).map_err(errno)? && is_dir {
            store.add_opaque(rel).map_err(errno)?;
        }
        Ok(())
    }

    // ---- operations, in FUSE order ----

    pub fn log(&self, h: &ScopeHandle, op: &str, rel: &Path, decision: &str) {
        self.log_by(h, op, rel, None, decision)
    }

    /// A ledger line naming the process `by` that asked for it.
    fn log_by(&self, h: &ScopeHandle, op: &str, rel: &Path, by: By, decision: &str) {
        let proc = by.map(|p| p.info.id);
        self.ledger
            .append_by(&h.group, op, &self.shown(h.root, rel), None, proc, decision);
    }

    /// The process behind the FUSE request of thread `tid`; the first time, its
    /// `proc` line and its ancestors' go to the ledger.
    fn caller(&self, tid: u32) -> Option<Arc<Proc>> {
        let (p, fresh) = self.procs.of(tid)?;
        for f in &fresh {
            self.ledger.proc(f);
        }
        Some(p)
    }

    /// `by` changed `rel`: it goes into the change set's writers.
    fn wrote(&self, h: &ScopeHandle, rel: &Path, by: By) -> R<()> {
        let Some(p) = by else { return Ok(()) };
        if h.store_read().has_writer(rel, p.info.id) {
            return Ok(());
        }
        h.store().add_writer(rel, p).map_err(errno)
    }

    /// `rel` may change: the scope is open, the rules capture it (or keep it
    /// ephemeral), and the scope is writable (ephemeral paths always are). A change
    /// to a captured path of the unscoped scope counts in `unscoped_changes`.
    fn may_change(&self, h: &ScopeHandle, op: &str, rel: &Path, by: By) -> R<()> {
        self.check_change(h, op, rel, by, true)
    }

    /// The second path of a rename or a link: checked, not counted again.
    fn may_change_too(&self, h: &ScopeHandle, op: &str, rel: &Path, by: By) -> R<()> {
        self.check_change(h, op, rel, by, false)
    }

    fn check_change(&self, h: &ScopeHandle, op: &str, rel: &Path, by: By, count: bool) -> R<()> {
        h.check_open()?;
        let access = h.access(rel);
        if count && access == Access::Capture && h.group == UNSCOPED {
            self.unscoped_changes.fetch_add(1, Ordering::Relaxed);
        }
        match access {
            Access::Capture if h.readonly => Err(Errno::EROFS),
            Access::Capture | Access::Ephemeral => Ok(()),
            Access::Deny | Access::Stub => {
                self.log_by(h, op, rel, by, "deny");
                Err(Errno::EACCES)
            }
            Access::Hidden => Err(Errno::ENOENT),
        }
    }

    /// A sandbox for `scope` starts with `writable` host paths outside escrow: one
    /// `op=sandbox-write` line per path (absolute), so the ledger shows unescrowed IO.
    pub fn log_sandbox(&self, scope: &str, writable: &[PathBuf]) {
        for w in writable {
            self.ledger.append(scope, "sandbox-write", w, None, "allow");
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn setattr(
        &self,
        h: &ScopeHandle,
        tid: u32,
        rel: &Path,
        mode: Option<u32>,
        owner: (Option<u32>, Option<u32>),
        size: Option<u64>,
        times: Option<(rustix::fs::Timespec, rustix::fs::Timespec)>,
    ) -> R<Stat> {
        let by = self.caller(tid);
        let by = by.as_ref();
        self.may_change(h, "setattr", rel, by)?;
        self.copy_up(h, rel)?;
        let up = h.upper.as_fd();
        self.log_by(h, "setattr", rel, by, "allow");
        if let Some(mode) = mode {
            sys::chmod(up, rel, mode).map_err(errno)?;
        }
        if owner.0.is_some() || owner.1.is_some() {
            sys::chown(up, rel, owner.0, owner.1).map_err(errno)?;
        }
        if let Some(size) = size {
            sys::open(up, rel, OFlags::WRONLY, 0)
                .and_then(|f| f.set_len(size))
                .map_err(errno)?;
        }
        if let Some((atime, mtime)) = times {
            sys::set_times(up, rel, atime, mtime).map_err(errno)?;
        }
        self.wrote(h, rel, by)?;
        sys::lstat(up, rel).map_err(errno)
    }

    pub fn readlink(&self, h: &ScopeHandle, rel: &Path) -> R<PathBuf> {
        if h.access(rel) == Access::Deny {
            self.log(h, "readlink", rel, "deny");
            return Err(Errno::EACCES);
        }
        self.log(h, "readlink", rel, "allow");
        match self.locate(h, rel)?.0 {
            Loc::Upper => sys::readlink(h.upper.as_fd(), rel),
            Loc::Lower => self.base(h).readlink(rel),
        }
        .map_err(errno)
    }

    pub fn mkdir(&self, h: &ScopeHandle, tid: u32, rel: &Path, mode: u32) -> R<()> {
        let by = self.caller(tid);
        let by = by.as_ref();
        self.may_change(h, "mkdir", rel, by)?;
        if self.locate(h, rel).is_ok() {
            return Err(Errno::EEXIST);
        }
        self.ensure_upper_dir(h, parent(rel))?;
        sys::mkdir(h.upper.as_fd(), rel, mode).map_err(errno)?;
        self.unwhiteout(h, rel, true)?;
        self.wrote(h, rel, by)?;
        self.log_by(h, "mkdir", rel, by, "allow");
        Ok(())
    }

    pub fn unlink(&self, h: &ScopeHandle, tid: u32, rel: &Path, dir: bool) -> R<()> {
        let by = self.caller(tid);
        let by = by.as_ref();
        let op = if dir { "rmdir" } else { "unlink" };
        self.may_change(h, op, rel, by)?;
        let (loc, st) = self.locate(h, rel)?;
        if dir != sys::is_dir(&st) {
            return Err(if dir { Errno::ENOTDIR } else { Errno::EISDIR });
        }
        if dir && !self.list(h, rel)?.is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
        if loc == Loc::Upper {
            sys::unlink(h.upper.as_fd(), rel, dir).map_err(errno)?;
        }
        let lower = self.lower_stat(h, rel);
        {
            let mut store = h.store();
            if dir {
                store.clear_below(rel).map_err(errno)?;
            }
            if let Some(st) = &lower {
                store
                    .record_version(rel, VersionKind::Changed, Version::of(st))
                    .map_err(errno)?;
                store.add_whiteout(rel).map_err(errno)?;
            }
            // Only this name: other hard links keep their own pins.
            store.unpin_below(rel, dir).map_err(errno)?;
            match (&lower, by) {
                // A base entry's delete is a change: its writers include the deleter.
                (Some(_), Some(p)) => store.add_writer(rel, p).map_err(errno)?,
                (Some(_), None) => {}
                // An entry the scope made is gone without a trace in the change set.
                (None, _) => store.drop_writers(rel).map_err(errno)?,
            }
        }
        self.t().forget_path(&h.id, rel, st.st_nlink > 1);
        self.log_by(h, op, rel, by, "allow");
        Ok(())
    }

    pub fn symlink(&self, h: &ScopeHandle, tid: u32, rel: &Path, target: &Path) -> R<()> {
        let by = self.caller(tid);
        let by = by.as_ref();
        self.may_change(h, "symlink", rel, by)?;
        if self.locate(h, rel).is_ok() {
            return Err(Errno::EEXIST);
        }
        self.ensure_upper_dir(h, parent(rel))?;
        sys::symlink(target, h.upper.as_fd(), rel).map_err(errno)?;
        self.unwhiteout(h, rel, false)?;
        self.wrote(h, rel, by)?;
        self.log_by(h, "symlink", rel, by, "allow");
        Ok(())
    }

    pub fn rename(&self, h: &ScopeHandle, tid: u32, from: &Path, to: &Path, flags: u32) -> R<()> {
        let by = self.caller(tid);
        let by = by.as_ref();
        self.may_change(h, "rename", from, by)?;
        self.may_change_too(h, "rename", to, by)?;
        if flags & libc::RENAME_EXCHANGE != 0 {
            return Err(Errno::EINVAL);
        }
        let (_, fst) = self.locate(h, from)?;
        let to_exists = self.locate(h, to).ok();
        if flags & libc::RENAME_NOREPLACE != 0 && to_exists.is_some() {
            return Err(Errno::EEXIST);
        }
        let from_lower = self.lower_stat(h, from).is_some();
        let from_dir = sys::is_dir(&fst);
        if from_dir && from_lower {
            return Err(Errno::EXDEV);
        }
        let to_lower = self.lower_stat(h, to);
        if let (Some((_, tst)), Some(_)) = (&to_exists, &to_lower)
            && sys::is_dir(tst)
        {
            return Err(Errno::EXDEV);
        }
        // A base entry at `to`, even one this scope deleted: a directory landing there must hide it.
        let to_in_base = self.base(h).lstat(to).ok();
        self.copy_up(h, from)?;
        self.ensure_upper_dir(h, parent(to))?;
        sys::rename(h.upper.as_fd(), from, to, flags).map_err(errno)?;
        let moved_entries = self.t().rekey(&h.id, from, to, from_dir);
        let to_dir = to_exists.as_ref().is_some_and(|(_, st)| sys::is_dir(st));
        h.store()
            .batch(|store| {
                if let Some(st) = &to_lower {
                    store.record_version(to, VersionKind::Changed, Version::of(st))?;
                }
                store.remove_whiteout(to)?;
                if from_dir && to_in_base.is_some() {
                    store.add_opaque(to)?;
                }
                if from_lower {
                    store.add_whiteout(from)?;
                }
                store.move_pins(from, to, from_dir, to_dir)?;
                // Entries looked up in this run keep their numbers, derived or pinned.
                store.set_pins(&moved_entries)?;
                // The writers move with the entry; the renamer changed the destination
                // and, for a base entry, the source (a rename or a delete there).
                store.move_writers(from, to, from_dir)?;
                if let Some(p) = by {
                    store.add_writer(to, p)?;
                    if from_lower {
                        store.add_writer(from, p)?;
                    }
                }
                Ok(())
            })
            .map_err(errno)?;
        self.ledger.append_by(
            &h.group,
            "rename",
            &self.shown(h.root, to),
            Some(&self.shown(h.root, from)),
            by.map(|p| p.info.id),
            "allow",
        );
        Ok(())
    }

    pub fn link(&self, h: &ScopeHandle, tid: u32, ino: u64, src: &Path, dst: &Path) -> R<()> {
        let by = self.caller(tid);
        let by = by.as_ref();
        self.may_change(h, "link", src, by)?;
        self.may_change_too(h, "link", dst, by)?;
        if self.locate(h, dst).is_ok() {
            return Err(Errno::EEXIST);
        }
        self.copy_up(h, src)?;
        self.ensure_upper_dir(h, parent(dst))?;
        sys::link(h.upper.as_fd(), src, dst).map_err(errno)?;
        self.unwhiteout(h, dst, false)?;
        {
            let mut store = h.store();
            store.set_pin(src, ino).map_err(errno)?;
            store.set_pin(dst, ino).map_err(errno)?;
        }
        let mut t = self.t();
        t.inos.insert((h.id.clone(), dst.to_path_buf()), ino);
        drop(t);
        self.wrote(h, dst, by)?;
        self.log_by(h, "link", dst, by, "allow");
        Ok(())
    }

    fn open_flags(flags: i32) -> OFlags {
        let acc = flags & libc::O_ACCMODE;
        let keep = flags & !(libc::O_ACCMODE | libc::O_APPEND | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW);
        // Always readable: with the writeback cache the kernel reads pages of write-only files.
        let base = if acc == libc::O_RDONLY {
            OFlags::RDONLY
        } else {
            OFlags::RDWR
        };
        base | OFlags::from_bits_retain(keep as u32)
    }

    /// Open `rel` (inode `ino`); also returns what the kernel may do with its cached pages.
    /// Reads name no process: only opens for writing resolve the caller (thread `tid`).
    pub fn open(&self, h: &ScopeHandle, tid: u32, ino: u64, rel: &Path, flags: i32) -> R<(File, Pages)> {
        let acc = flags & libc::O_ACCMODE;
        let writes = acc != libc::O_RDONLY || flags & libc::O_TRUNC != 0;
        h.check_open()?;
        let by = if writes { self.caller(tid) } else { None };
        let by = by.as_ref();
        // The root's rules first, then the gate's read rules (project paths).
        let denied = match h.access(rel) {
            Access::Deny | Access::Stub => Some(if writes { "open-write" } else { "read" }),
            Access::Hidden => return Err(Errno::ENOENT),
            _ if h.root == PROJECT && acc != libc::O_WRONLY && !self.gate.read_allowed(rel) => Some("read"),
            _ => None,
        };
        if let Some(op) = denied {
            h.store().record_denied(rel).map_err(errno)?;
            self.log_by(h, op, rel, by, "deny");
            return Err(Errno::EACCES);
        }
        if writes {
            self.may_change(h, "open-write", rel, by)?;
        }
        self.log_by(h, if writes { "open-write" } else { "read" }, rel, by, "allow");
        let oflags = Self::open_flags(flags);
        let (loc, st) = self.locate(h, rel)?;
        let f = if writes {
            self.copy_up(h, rel)?;
            self.wrote(h, rel, by)?;
            sys::open(h.upper.as_fd(), rel, oflags, 0)
        } else {
            match loc {
                Loc::Upper => sys::open(h.upper.as_fd(), rel, oflags, 0),
                Loc::Lower => {
                    self.record(h, rel, VersionKind::Read, &st)?;
                    self.base(h).open(rel, oflags)
                }
            }
        }
        .map_err(errno)?;
        let pages = match self.t().pages(ino, loc, &st) {
            Pages::Empty(_) if writes => Pages::Drop,
            p => p,
        };
        Ok((f, pages))
    }

    /// Put `data` in the kernel's page cache of `ino` from offset 0; false if it refused.
    pub fn store_pages(&self, ino: u64, data: &[u8]) -> bool {
        self.notifier
            .get()
            .is_some_and(|n| n.store(fuser::INodeNo(ino), 0, data).is_ok())
    }

    pub fn create(&self, h: &ScopeHandle, tid: u32, rel: &Path, mode: u32, flags: i32) -> R<File> {
        let by = self.caller(tid);
        let by = by.as_ref();
        self.may_change(h, "create", rel, by)?;
        let exists = self.locate(h, rel).is_ok();
        if exists && flags & libc::O_EXCL != 0 {
            return Err(Errno::EEXIST);
        }
        if exists {
            self.copy_up(h, rel)?;
        } else {
            self.ensure_upper_dir(h, parent(rel))?;
        }
        self.log_by(h, "create", rel, by, "allow");
        let oflags = Self::open_flags(flags) | OFlags::CREATE;
        let f = sys::open(h.upper.as_fd(), rel, oflags, mode).map_err(errno)?;
        self.unwhiteout(h, rel, false)?;
        self.wrote(h, rel, by)?;
        Ok(f)
    }

    // ---- open files ----

    /// `pid`: the process that opened it (FUSE reports the calling thread).
    pub fn add_file(&self, h: Arc<ScopeHandle>, f: File, pid: u32) -> u64 {
        let mut t = self.t();
        let fh = t.next_fh;
        t.next_fh += 1;
        t.files.insert(fh, (h, Arc::new(f)));
        t.openers.insert(fh, pid);
        fh
    }

    /// Wait (up to `within`) until scope `id` holds no handle opened by a process in
    /// one of the `stopped` process groups (its sandboxes) or by a process that is
    /// exiting or gone. A process killed by a signal sends no FUSE flush: its dirty
    /// pages reach the daemon only with the release, which the kernel sends as the
    /// process exits, possibly after its sandbox's bwrap is reaped. Close waits for
    /// those writes before it freezes the scope. Handles of live processes outside
    /// the sandboxes (the SDK's app) are the SDK's to flush.
    fn settle_dead_handles(&self, id: &str, stopped: &[i32], within: std::time::Duration) {
        let deadline = std::time::Instant::now() + within;
        let mut t = self.t();
        loop {
            let pending = t
                .files
                .iter()
                .any(|(fh, (h, _))| h.group == id && t.openers.get(fh).is_some_and(|p| finishing(*p, stopped)));
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if !pending || left.is_zero() {
                return;
            }
            t = self
                .released
                .wait_timeout(t, left.min(std::time::Duration::from_millis(20)))
                .unwrap()
                .0;
        }
    }

    pub fn file(&self, fh: u64) -> R<Arc<File>> {
        self.t().files.get(&fh).map(|(_, f)| f.clone()).ok_or(Errno::EBADF)
    }

    /// A handle to write through: the tag is fixed at open, and a closed scope takes no writes.
    pub fn file_for_write(&self, fh: u64) -> R<Arc<File>> {
        let (h, f) = self.t().files.get(&fh).cloned().ok_or(Errno::EBADF)?;
        if h.is_closed() {
            self.ledger.append(&h.group, "write", Path::new(""), None, "deny");
            return Err(Errno::EBADF);
        }
        Ok(f)
    }

    pub fn release(&self, fh: u64) {
        let f = {
            let mut t = self.t();
            t.openers.remove(&fh);
            t.files.remove(&fh)
        };
        self.released.notify_all();
        // A file opened for writing is in the upper: start writing its data to disk
        // now, so the flush before a commit finds little left.
        if let Some((_, f)) = f
            && rustix::fs::fcntl_getfl(&*f).is_ok_and(|fl| fl.contains(OFlags::RDWR))
        {
            let _ = self.writeback.lock().unwrap().send(f);
        }
    }

    pub fn statfs(&self) -> R<rustix::fs::StatVfs> {
        sys::statvfs(self.roots[PROJECT].lower.as_ref().ok_or(Errno::EIO)?.as_fd()).map_err(errno)
    }
}

/// A thread that starts the writeback of each file it is sent.
fn writeback_thread() -> std::sync::mpsc::Sender<Arc<File>> {
    let (tx, rx) = std::sync::mpsc::channel::<Arc<File>>();
    std::thread::spawn(move || {
        for f in rx {
            sys::start_writeback(&f);
        }
    });
    tx
}

/// Process (or thread) `pid` is gone, exiting, or in one of the `stopped` process groups.
fn finishing(pid: u32, stopped: &[i32]) -> bool {
    const PF_EXITING: u64 = 0x4;
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return true;
    };
    // Fields after the command name: state ppid pgrp session tty_nr tpgid flags …
    let Some(rest) = stat.rfind(')').and_then(|i| stat.get(i + 2..)) else {
        return true;
    };
    let f: Vec<&str> = rest.split_whitespace().collect();
    let state = f.first().and_then(|s| s.chars().next()).unwrap_or('X');
    let pgrp = f.get(2).and_then(|s| s.parse::<i32>().ok());
    let flags = f.get(6).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    matches!(state, 'Z' | 'X' | 'x') || flags & PF_EXITING != 0 || pgrp.is_some_and(|g| stopped.contains(&g))
}
