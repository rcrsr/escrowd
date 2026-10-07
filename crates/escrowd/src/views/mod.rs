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
//!
//! Here: the views and their start-up. `scopes` holds the lifecycle, `hold` the
//! held scopes and reviews, `overlay` each view's copy-on-write and FUSE operations,
//! `inodes` the inode table, `files` the open-file table.

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

mod files;
mod hold;
mod inodes;
mod overlay;
mod scopes;

pub use crate::policy::Unscoped;
use files::writeback_thread;
pub use hold::{Identity, Reviewed};
use inodes::Tables;
pub use inodes::{Node, Pages};
pub use overlay::{Loc, ScopeHandle};

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
pub fn errno(e: io::Error) -> Errno {
    Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))
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
        let history = History::open(state_dir, crate::record::settled)?;
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
}
