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

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use fuser::{Errno, FileType};
use rustix::fs::{OFlags, Stat};

use crate::changeset::{self, ChangeSet};
use crate::commit::{Commits, Outcome};
use crate::gate::Gate;
use crate::ledger::Ledger;
use crate::snapshot::Base;
use crate::store::{ScopeState, ScopeStore, VersionKind, moved};
use crate::sys::{self, Version, parent};

pub const ROOT: u64 = 1;
/// The default scope behind `<mount>/unscoped/` (unscoped = implicit or deny).
pub const UNSCOPED: &str = "unscoped";
/// Its root keeps one inode number across resets, so a bind mount of it stays valid.
pub const UNSCOPED_ROOT: u64 = 2;
const SCOPE_SHIFT: u32 = 48;
pub const UPPER_BIT: u64 = 1 << 47;

pub type R<T> = Result<T, Errno>;

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
    pub id: String,
    pub idx: u64,
    /// The base generation the scope opened at.
    pub since: u64,
    /// Changes get EROFS (the unscoped root in `deny` mode).
    pub readonly: bool,
    pub upper: OwnedFd,
    /// Frozen between close and the decision: new IO gets EROFS, writes on open handles EBADF.
    closed: AtomicBool,
    store: Mutex<ScopeStore>,
}

impl ScopeHandle {
    pub(crate) fn store(&self) -> MutexGuard<'_, ScopeStore> {
        self.store.lock().unwrap()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// New IO (opens, creates, any change) is refused while the scope is closed.
    fn check_open(&self) -> R<()> {
        if self.is_closed() { Err(Errno::EROFS) } else { Ok(()) }
    }

    /// Changes also need a writable scope.
    fn check_write(&self) -> R<()> {
        if self.readonly {
            Err(Errno::EROFS)
        } else {
            self.check_open()
        }
    }
}

type Key = (String, PathBuf);

#[derive(Default)]
struct Tables {
    paths: HashMap<u64, Key>,
    inos: HashMap<Key, u64>,
    files: HashMap<u64, (Arc<ScopeHandle>, Arc<File>)>,
    next_fh: u64,
}

impl Tables {
    /// Entries at or under `from` now live under `to`; returns (new path, ino) of each.
    fn rekey(&mut self, scope: &str, from: &Path, to: &Path) -> Vec<(PathBuf, u64)> {
        if let Some(ino) = self.inos.remove(&(scope.to_string(), to.to_path_buf())) {
            self.paths.remove(&ino);
        }
        let old: Vec<(PathBuf, u64)> = self
            .inos
            .iter()
            .filter(|((s, p), _)| s == scope && p.starts_with(from))
            .map(|((_, p), &i)| (p.clone(), i))
            .collect();
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

    /// `rel` no longer exists; its inode lives on under another hard link, if any.
    fn forget_path(&mut self, scope: &str, rel: &Path) {
        let key = (scope.to_string(), rel.to_path_buf());
        if let Some(ino) = self.inos.remove(&key)
            && self.paths.get(&ino) == Some(&key)
        {
            match self.inos.iter().find(|(_, i)| **i == ino).map(|(k, _)| k.clone()) {
                Some(other) => self.paths.insert(ino, other),
                None => self.paths.remove(&ino),
            };
        }
    }

    /// Forget a scope's entries; returns the top-level names the kernel may have cached.
    fn forget_scope(&mut self, scope: &str) -> Vec<OsString> {
        let top = self
            .inos
            .keys()
            .filter(|(s, p)| s == scope && p.components().count() == 1)
            .map(|(_, p)| p.as_os_str().to_os_string())
            .collect();
        self.inos.retain(|(s, _), _| s != scope);
        self.paths.retain(|_, (s, _)| s != scope);
        top
    }
}

pub enum Node {
    Root,
    In(Arc<ScopeHandle>, PathBuf),
}

pub struct Views {
    lower: OwnedFd,
    scopes_dir: PathBuf,
    mount: PathBuf,
    gate: Gate,
    ledger: Ledger,
    commits: Commits,
    unscoped: Unscoped,
    notifier: std::sync::OnceLock<fuser::Notifier>,
    scopes: RwLock<HashMap<String, Arc<ScopeHandle>>>,
    next_idx: Mutex<u64>,
    t: Mutex<Tables>,
}

impl Views {
    /// `lower` must be opened before the view is mounted anywhere over the project.
    pub fn new(lower: OwnedFd, state_dir: &Path, mount: &Path, gate: Gate, unscoped: Unscoped) -> io::Result<Self> {
        let scopes_dir = state_dir.join("scopes");
        fs::create_dir_all(&scopes_dir)?;
        let ledger = Ledger::open(&state_dir.join("ledger.log"))?;
        let commits = Commits::open(state_dir)?;
        // Roll back an interrupted commit before any scope sees the base.
        for (generation, id) in commits.recover(lower.as_fd())? {
            let dir = scopes_dir.join(&id);
            if dir.exists() {
                fs::remove_dir_all(&dir)?;
                ledger.append(&id, "decide", Path::new(""), None, "commit");
            }
            commits.scope_dropped(generation)?;
        }
        let views = Views {
            lower,
            scopes_dir,
            mount: mount.to_path_buf(),
            gate,
            ledger,
            commits,
            unscoped,
            notifier: std::sync::OnceLock::new(),
            scopes: RwLock::new(HashMap::new()),
            next_idx: Mutex::new(0),
            t: Mutex::new(Tables {
                next_fh: 1,
                ..Default::default()
            }),
        };
        views.load_scopes()?;
        views.ensure_unscoped()?;
        views.gc()?;
        Ok(views)
    }

    pub fn unscoped(&self) -> Unscoped {
        self.unscoped
    }

    /// Lets a reset of the unscoped root drop the kernel's cached entries under it.
    pub fn set_notifier(&self, n: fuser::Notifier) {
        let _ = self.notifier.set(n);
    }

    /// Open the default scope the unscoped mode needs, if it is missing or of the wrong kind.
    fn ensure_unscoped(&self) -> io::Result<()> {
        if self.unscoped == Unscoped::Passthrough {
            return Ok(());
        }
        let readonly = self.unscoped == Unscoped::Deny;
        if let Ok(h) = self.handle(UNSCOPED) {
            if h.readonly == readonly {
                return Ok(());
            }
            self.remove_scope(UNSCOPED)?;
        }
        // A deny root reads the live base; an implicit one keeps a snapshot like any scope.
        let since = if readonly { u64::MAX } else { self.commits.current() };
        let idx = self.alloc_idx()?;
        let store = ScopeStore::create(
            &self.scopes_dir,
            UNSCOPED,
            UNSCOPED,
            idx,
            since,
            readonly,
            &HashMap::new(),
        )?;
        self.attach(store)?;
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

    /// Reattach scopes left by a previous daemon run.
    fn load_scopes(&self) -> io::Result<()> {
        let mut max_idx = 0;
        for e in fs::read_dir(&self.scopes_dir)? {
            let id = e?.file_name().to_string_lossy().into_owned();
            let store = ScopeStore::load(&self.scopes_dir, &id)?;
            max_idx = max_idx.max(store.idx);
            self.attach(store)?;
        }
        *self.next_idx.lock().unwrap() = max_idx;
        Ok(())
    }

    fn attach(&self, store: ScopeStore) -> io::Result<Arc<ScopeHandle>> {
        let upper = sys::open_dir(&store.upper_dir())?;
        let h = Arc::new(ScopeHandle {
            id: store.id.clone(),
            idx: store.idx,
            since: store.since,
            readonly: store.readonly,
            upper,
            closed: AtomicBool::new(store.state == ScopeState::Closed),
            store: Mutex::new(store),
        });
        self.scopes.write().unwrap().insert(h.id.clone(), h.clone());
        Ok(h)
    }

    // ---- scope lifecycle (RPC) ----

    /// Create a scope; returns its id and its root inside the mount.
    pub fn open_scope(&self, name: &str, labels: &HashMap<String, String>) -> io::Result<(String, PathBuf)> {
        let idx = self.alloc_idx()?;
        let id = format!("s{idx}");
        let store = ScopeStore::create(&self.scopes_dir, &id, name, idx, self.commits.current(), false, labels)?;
        let h = self.attach(store)?;
        self.ledger.append(&h.id, "open", Path::new(""), None, "allow");
        Ok((id.clone(), self.mount.join(&id)))
    }

    fn handle(&self, id: &str) -> io::Result<Arc<ScopeHandle>> {
        self.scopes
            .read()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no scope {id}")))
    }

    /// Freeze the scope and return its change set. The caller has fsynced its open files
    /// (syncfs is not a barrier on plain FUSE); sandboxes are stopped here from phase 1.5.
    /// Closing a closed scope returns the same change set.
    pub fn close_scope(&self, id: &str) -> io::Result<ChangeSet> {
        let h = self.handle(id)?;
        if !h.is_closed() {
            h.store().set_state(ScopeState::Closed)?;
            h.closed.store(true, Ordering::Release);
            self.ledger.append(id, "close", Path::new(""), None, "allow");
        }
        changeset::build(&self.base(&h), &h)
    }

    /// Return to agent: the decision's reasons go back and the scope accepts IO again.
    pub fn reopen_scope(&self, id: &str) -> io::Result<()> {
        let h = self.handle(id)?;
        if !h.is_closed() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("scope {id} is not closed"),
            ));
        }
        h.store().set_state(ScopeState::Open)?;
        h.closed.store(false, Ordering::Release);
        self.ledger.append(id, "decide", Path::new(""), None, "return");
        Ok(())
    }

    /// Commit a closed scope's change set to the project, all or nothing, and drop the
    /// scope. On a conflict nothing is written and the scope is dropped (the default
    /// conflict policy); on a failure the commit is rolled back and the scope stays closed.
    pub fn commit_scope(&self, id: &str) -> io::Result<Outcome> {
        let h = self.handle(id)?;
        if !h.is_closed() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("scope {id} is not closed"),
            ));
        }
        let cs = changeset::build(&self.base(&h), &h)?;
        let outcome = self.commits.commit(self.lower.as_fd(), &h, &cs)?;
        match &outcome {
            Outcome::Committed(generation, _) => {
                crate::fault::hit("committed", 0)?;
                self.ledger.append(id, "decide", Path::new(""), None, "commit");
                self.remove_scope(id)?;
                if let Some(g) = generation {
                    self.commits.scope_dropped(*g)?;
                }
                self.ensure_unscoped()?;
            }
            Outcome::Conflict(paths) => {
                for p in paths {
                    self.ledger.append(id, "conflict", p, None, "deny");
                }
                self.ledger.append(id, "decide", Path::new(""), None, "conflict");
                self.remove_scope(id)?;
                self.ensure_unscoped()?;
            }
        }
        self.gc()?;
        Ok(outcome)
    }

    /// Drop a scope and its staged changes.
    pub fn drop_scope(&self, id: &str) -> io::Result<()> {
        self.handle(id)?;
        self.ledger.append(id, "decide", Path::new(""), None, "discard");
        self.remove_scope(id)?;
        self.ensure_unscoped()?;
        self.gc()
    }

    /// Drop generations no open scope reads through any more.
    fn gc(&self) -> io::Result<()> {
        let oldest = self.scopes.read().unwrap().values().map(|h| h.since).min();
        self.commits.gc(oldest)
    }

    fn remove_scope(&self, id: &str) -> io::Result<()> {
        let h = self
            .scopes
            .write()
            .unwrap()
            .remove(id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no scope {id}")))?;
        let names = self.t().forget_scope(id);
        if id == UNSCOPED
            && let Some(n) = self.notifier.get()
        {
            for name in names {
                let _ = n.inval_entry(fuser::INodeNo(UNSCOPED_ROOT), &name);
            }
        }
        // FUSE calls in flight may still hold the handle; they fail once the directory is gone.
        h.store().destroy()
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
        if ino == UNSCOPED_ROOT {
            return Ok(Node::In(self.scope(UNSCOPED)?, PathBuf::new()));
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
        Base::new(self.lower.as_fd(), &self.commits.gens, h.since)
    }

    fn lower_stat(&self, h: &ScopeHandle, rel: &Path) -> Option<Stat> {
        if h.store().hidden(rel) {
            return None;
        }
        self.base(h).lstat(rel).ok()
    }

    pub fn locate(&self, h: &ScopeHandle, rel: &Path) -> R<(Loc, Stat)> {
        if let Ok(st) = sys::lstat(h.upper.as_fd(), rel) {
            return Ok((Loc::Upper, st));
        }
        self.lower_stat(h, rel).map(|st| (Loc::Lower, st)).ok_or(Errno::ENOENT)
    }

    pub fn root_stat(&self) -> R<Stat> {
        sys::lstat(self.lower.as_fd(), Path::new("")).map_err(errno)
    }

    /// The scope's inode for `rel`: a pinned number, else the base st_ino, else a new upper number.
    pub fn ino_for(&self, h: &ScopeHandle, rel: &Path) -> R<u64> {
        if h.id == UNSCOPED && rel.as_os_str().is_empty() {
            return Ok(UNSCOPED_ROOT);
        }
        let key = (h.id.clone(), rel.to_path_buf());
        if let Some(&ino) = self.t().inos.get(&key) {
            return Ok(ino);
        }
        let prefix = h.idx << SCOPE_SHIFT;
        let pinned = h.store().pin(rel);
        let ino = match pinned {
            Some(ino) => ino,
            None => match self.lower_stat(h, rel).map(|st| st.st_ino) {
                Some(i) if i < UPPER_BIT => prefix | i,
                _ => {
                    let mut store = h.store();
                    let ino = prefix | UPPER_BIT | store.alloc_upper_ino().map_err(errno)?;
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
        let (src, src_rel) = self.base(h).src(rel).map_err(errno)?;
        sys::copy_entry(src, &src_rel, h.upper.as_fd(), rel, &st, false).map_err(errno)
    }

    pub fn list(&self, h: &ScopeHandle, rel: &Path) -> R<BTreeMap<OsString, FileType>> {
        let mut out = BTreeMap::new();
        let mut found = false;
        if let Ok(entries) = sys::read_dir(h.upper.as_fd(), rel) {
            found = true;
            out.extend(entries);
        }
        let opaque = h.store().is_opaque(rel);
        if !opaque
            && self.lower_stat(h, rel).is_some_and(|st| sys::is_dir(&st))
            && let Ok(entries) = self.base(h).read_dir(rel)
        {
            found = true;
            let store = h.store();
            for (name, kind) in entries {
                if !out.contains_key(&name) && !store.hidden(&rel.join(&name)) {
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
        self.ledger.append(&h.id, op, rel, None, decision);
    }

    pub fn setattr(
        &self,
        h: &ScopeHandle,
        rel: &Path,
        mode: Option<u32>,
        owner: (Option<u32>, Option<u32>),
        size: Option<u64>,
        times: Option<(rustix::fs::Timespec, rustix::fs::Timespec)>,
    ) -> R<Stat> {
        h.check_write()?;
        self.copy_up(h, rel)?;
        let up = h.upper.as_fd();
        self.log(h, "setattr", rel, "allow");
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
        sys::lstat(up, rel).map_err(errno)
    }

    pub fn readlink(&self, h: &ScopeHandle, rel: &Path) -> R<PathBuf> {
        self.log(h, "readlink", rel, "allow");
        match self.locate(h, rel)?.0 {
            Loc::Upper => sys::readlink(h.upper.as_fd(), rel),
            Loc::Lower => self.base(h).readlink(rel),
        }
        .map_err(errno)
    }

    pub fn mkdir(&self, h: &ScopeHandle, rel: &Path, mode: u32) -> R<()> {
        h.check_write()?;
        if self.locate(h, rel).is_ok() {
            return Err(Errno::EEXIST);
        }
        self.ensure_upper_dir(h, parent(rel))?;
        sys::mkdir(h.upper.as_fd(), rel, mode).map_err(errno)?;
        self.unwhiteout(h, rel, true)?;
        self.log(h, "mkdir", rel, "allow");
        Ok(())
    }

    pub fn unlink(&self, h: &ScopeHandle, rel: &Path, dir: bool) -> R<()> {
        h.check_write()?;
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
            store.unpin_below(rel).map_err(errno)?;
        }
        self.t().forget_path(&h.id, rel);
        self.log(h, if dir { "rmdir" } else { "unlink" }, rel, "allow");
        Ok(())
    }

    pub fn symlink(&self, h: &ScopeHandle, rel: &Path, target: &Path) -> R<()> {
        h.check_write()?;
        if self.locate(h, rel).is_ok() {
            return Err(Errno::EEXIST);
        }
        self.ensure_upper_dir(h, parent(rel))?;
        sys::symlink(target, h.upper.as_fd(), rel).map_err(errno)?;
        self.unwhiteout(h, rel, false)?;
        self.log(h, "symlink", rel, "allow");
        Ok(())
    }

    pub fn rename(&self, h: &ScopeHandle, from: &Path, to: &Path, flags: u32) -> R<()> {
        h.check_write()?;
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
        let moved_entries = self.t().rekey(&h.id, from, to);
        {
            let mut store = h.store();
            if let Some(st) = &to_lower {
                store
                    .record_version(to, VersionKind::Changed, Version::of(st))
                    .map_err(errno)?;
            }
            store.remove_whiteout(to).map_err(errno)?;
            if from_dir && to_in_base.is_some() {
                store.add_opaque(to).map_err(errno)?;
            }
            if from_lower {
                store.add_whiteout(from).map_err(errno)?;
            }
            store.move_pins(from, to).map_err(errno)?;
            // Entries looked up in this run keep their numbers, derived or pinned.
            for (p, ino) in moved_entries {
                store.set_pin(&p, ino).map_err(errno)?;
            }
        }
        self.ledger.append(&h.id, "rename", to, Some(from), "allow");
        Ok(())
    }

    pub fn link(&self, h: &ScopeHandle, ino: u64, src: &Path, dst: &Path) -> R<()> {
        h.check_write()?;
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
        self.log(h, "link", dst, "allow");
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

    pub fn open(&self, h: &ScopeHandle, rel: &Path, flags: i32) -> R<File> {
        let acc = flags & libc::O_ACCMODE;
        let writes = acc != libc::O_RDONLY || flags & libc::O_TRUNC != 0;
        h.check_open()?;
        if writes {
            h.check_write()?;
        }
        if acc != libc::O_WRONLY && !self.gate.read_allowed(rel) {
            h.store().record_denied(rel).map_err(errno)?;
            self.log(h, "read", rel, "deny");
            return Err(Errno::EACCES);
        }
        self.log(h, if writes { "open-write" } else { "read" }, rel, "allow");
        let oflags = Self::open_flags(flags);
        if writes {
            self.copy_up(h, rel)?;
            return sys::open(h.upper.as_fd(), rel, oflags, 0).map_err(errno);
        }
        let (loc, st) = self.locate(h, rel)?;
        match loc {
            Loc::Upper => sys::open(h.upper.as_fd(), rel, oflags, 0),
            Loc::Lower => {
                self.record(h, rel, VersionKind::Read, &st)?;
                self.base(h).open(rel, oflags)
            }
        }
        .map_err(errno)
    }

    pub fn create(&self, h: &ScopeHandle, rel: &Path, mode: u32, flags: i32) -> R<File> {
        h.check_write()?;
        let exists = self.locate(h, rel).is_ok();
        if exists && flags & libc::O_EXCL != 0 {
            return Err(Errno::EEXIST);
        }
        if exists {
            self.copy_up(h, rel)?;
        } else {
            self.ensure_upper_dir(h, parent(rel))?;
        }
        self.log(h, "create", rel, "allow");
        let oflags = Self::open_flags(flags) | OFlags::CREATE;
        let f = sys::open(h.upper.as_fd(), rel, oflags, mode).map_err(errno)?;
        self.unwhiteout(h, rel, false)?;
        Ok(f)
    }

    // ---- open files ----

    pub fn add_file(&self, h: Arc<ScopeHandle>, f: File) -> u64 {
        let mut t = self.t();
        let fh = t.next_fh;
        t.next_fh += 1;
        t.files.insert(fh, (h, Arc::new(f)));
        fh
    }

    pub fn file(&self, fh: u64) -> R<Arc<File>> {
        self.t().files.get(&fh).map(|(_, f)| f.clone()).ok_or(Errno::EBADF)
    }

    /// A handle to write through: the tag is fixed at open, and a closed scope takes no writes.
    pub fn file_for_write(&self, fh: u64) -> R<Arc<File>> {
        let (h, f) = self.t().files.get(&fh).cloned().ok_or(Errno::EBADF)?;
        if h.is_closed() {
            self.ledger.append(&h.id, "write", Path::new(""), None, "deny");
            return Err(Errno::EBADF);
        }
        Ok(f)
    }

    pub fn release(&self, fh: u64) {
        self.t().files.remove(&fh);
    }

    pub fn statfs(&self) -> R<rustix::fs::StatVfs> {
        sys::statvfs(self.lower.as_fd()).map_err(errno)
    }
}
