//! The copy-on-write overlay of one view: where a path lives (upper or base),
//! copy-up, listings, and the filesystem operations FUSE forwards, in FUSE order.

use super::*;

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
    pub(super) rules: Option<Arc<Rules>>,
    pub idx: u64,
    /// The base generation the scope opened at.
    pub since: u64,
    /// Changes get EROFS (the unscoped root in `deny` mode).
    pub readonly: bool,
    pub upper: OwnedFd,
    /// Frozen between close and the decision: new IO gets EROFS, writes on open handles EBADF.
    pub(super) closed: AtomicBool,
    /// `Views::unscoped_changes` when the scope opened (or reopened); 0 after a restart.
    pub(super) unscoped_at: AtomicU64,
    /// Changes the unscoped mode saw while the scope was open, fixed at close.
    pub(super) unscoped_seen: AtomicU64,
    pub(super) store: RwLock<ScopeStore>,
    pub(super) inodes: Mutex<Inodes>,
}

impl ScopeHandle {
    pub(crate) fn store(&self) -> RwLockWriteGuard<'_, ScopeStore> {
        self.store.write()
    }

    /// Lookups share the store.
    pub(crate) fn store_read(&self) -> RwLockReadGuard<'_, ScopeStore> {
        self.store.read()
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

    pub(super) fn hidden(&self, rel: &Path) -> bool {
        self.rules.as_ref().is_some_and(|r| r.access(rel) == Access::Hidden)
    }

    /// New IO (opens, creates, any change) is refused while the scope is closed.
    pub(super) fn check_open(&self) -> R<()> {
        if self.is_closed() { Err(Errno::EROFS) } else { Ok(()) }
    }
}

impl Views {
    /// The scope's snapshot of the base.
    pub(super) fn base<'a>(&'a self, h: &ScopeHandle) -> Base<'a> {
        Base::new(self.lower(h), &self.commits.gens[h.root], h.since)
    }

    pub(super) fn lower_stat(&self, h: &ScopeHandle, rel: &Path) -> Option<Stat> {
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
    // ---- copy-on-write ----

    pub(super) fn ensure_upper_dir(&self, h: &ScopeHandle, rel: &Path) -> R<()> {
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

    pub(super) fn record(&self, h: &ScopeHandle, rel: &Path, kind: VersionKind, st: &Stat) -> R<()> {
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
            let mut t = h.inodes.lock();
            if let Some(&ino) = t.inos.get(rel)
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
    pub(super) fn unwhiteout(&self, h: &ScopeHandle, rel: &Path, is_dir: bool) -> R<()> {
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
    pub(super) fn log_by(&self, h: &ScopeHandle, op: &str, rel: &Path, by: By, decision: &str) {
        let proc = by.map(|p| p.info.id);
        self.ledger
            .append_by(&h.group, op, &self.shown(h.root, rel), None, proc, decision);
    }

    /// The process behind the FUSE request of thread `tid`; the first time, its
    /// `proc` line and its ancestors' go to the ledger.
    pub(super) fn caller(&self, tid: u32) -> Option<Arc<Proc>> {
        let (p, fresh) = self.procs.of(tid)?;
        for f in &fresh {
            self.ledger.proc(f);
        }
        Some(p)
    }

    /// `by` changed `rel`: it goes into the change set's writers.
    pub(super) fn wrote(&self, h: &ScopeHandle, rel: &Path, by: By) -> R<()> {
        let Some(p) = by else { return Ok(()) };
        if h.store_read().has_writer(rel, p.info.id) {
            return Ok(());
        }
        h.store().add_writer(rel, p).map_err(errno)
    }

    /// `rel` may change: the scope is open, the rules capture it (or keep it
    /// ephemeral), and the scope is writable (ephemeral paths always are). A change
    /// to a captured path of the unscoped scope counts in `unscoped_changes`.
    pub(super) fn may_change(&self, h: &ScopeHandle, op: &str, rel: &Path, by: By) -> R<()> {
        self.check_change(h, op, rel, by, true)
    }

    /// The second path of a rename or a link: checked, not counted again.
    pub(super) fn may_change_too(&self, h: &ScopeHandle, op: &str, rel: &Path, by: By) -> R<()> {
        self.check_change(h, op, rel, by, false)
    }

    pub(super) fn check_change(&self, h: &ScopeHandle, op: &str, rel: &Path, by: By, count: bool) -> R<()> {
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
        h.inodes.lock().forget_path(rel, st.st_nlink > 1);
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
        let moved_entries = h.inodes.lock().rekey(from, to, from_dir);
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
        h.inodes.lock().inos.insert(dst.to_path_buf(), ino);
        self.wrote(h, dst, by)?;
        self.log_by(h, "link", dst, by, "allow");
        Ok(())
    }

    pub(super) fn open_flags(flags: i32) -> OFlags {
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
        let pages = match h.inodes.lock().pages(ino, loc, &st) {
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
}
