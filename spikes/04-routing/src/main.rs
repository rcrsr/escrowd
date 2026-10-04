//! Spike 0.4: one FUSE mount, one copy-on-write view per scope, read gating.
//!
//! The mount root lists scopes. `mkdir <mount>/<id>` creates a scope: a view of
//! the shared lower with its own upper directory (<uppers>/<id>), whiteouts and
//! opaque marks, as in spike 0.3. Everything under <mount>/<id>/ belongs to that
//! scope, so the path alone attributes every operation.
//!
//! Inode numbers are per scope: (scope index + 1) << 48 | lower st_ino. Reusing
//! the bare lower st_ino in every scope would let the kernel share page cache
//! (and writeback-cached writes) between scopes.
//!
//! Read gate: opening a file named `.env` fails with EACCES. Every open is
//! appended to the ledger as `scope=… op=… path=… decision=…`.
//!
//! Usage: fuse-routing-spike <lower> <uppers dir> <mount> <ledger file>

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, MountOption, OpenFlags, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, Request, TimeOrNow, WriteFlags,
};

const TTL: Duration = Duration::from_secs(1);
const ROOT: u64 = 1;
const SCOPE_SHIFT: u32 = 48;
const UPPER_BIT: u64 = 1 << 47;

type R<T> = Result<T, Errno>;

fn errno(e: io::Error) -> Errno {
    Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))
}

fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).unwrap()
}

fn kind_of(ft: fs::FileType) -> FileType {
    use std::os::unix::fs::FileTypeExt;
    if ft.is_dir() {
        FileType::Directory
    } else if ft.is_symlink() {
        FileType::Symlink
    } else if ft.is_fifo() {
        FileType::NamedPipe
    } else if ft.is_socket() {
        FileType::Socket
    } else if ft.is_block_device() {
        FileType::BlockDevice
    } else if ft.is_char_device() {
        FileType::CharDevice
    } else {
        FileType::RegularFile
    }
}

fn time(secs: i64, nsecs: i64) -> SystemTime {
    if secs >= 0 { UNIX_EPOCH + Duration::new(secs as u64, nsecs as u32) } else { UNIX_EPOCH }
}

fn attr_of(ino: u64, m: &fs::Metadata) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
        size: m.size(),
        blocks: m.blocks(),
        atime: time(m.atime(), m.atime_nsec()),
        mtime: time(m.mtime(), m.mtime_nsec()),
        ctime: time(m.ctime(), m.ctime_nsec()),
        crtime: time(m.ctime(), m.ctime_nsec()),
        kind: kind_of(m.file_type()),
        perm: (m.mode() & 0o7777) as u16,
        nlink: m.nlink() as u32,
        uid: m.uid(),
        gid: m.gid(),
        rdev: m.rdev() as u32,
        blksize: m.blksize() as u32,
        flags: 0,
    }
}

fn set_times(path: &Path, atime: libc::timespec, mtime: libc::timespec) -> io::Result<()> {
    let times = [atime, mtime];
    let p = cstr(path);
    if unsafe { libc::utimensat(libc::AT_FDCWD, p.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn fd_path(fd: i32, rel: &Path) -> PathBuf {
    let p = PathBuf::from(format!("/proc/self/fd/{fd}"));
    if rel.as_os_str().is_empty() { p.join(".") } else { p.join(rel) }
}

fn open_dir(p: &Path) -> io::Result<OwnedFd> {
    let c = cstr(p);
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[derive(Clone, Copy, PartialEq)]
enum Loc {
    Upper,
    Lower,
}

struct Scope {
    idx: u64,
    upper: OwnedFd,
    whiteouts: HashSet<PathBuf>,
    opaque: HashSet<PathBuf>,
    next_upper_ino: u64,
}

impl Scope {
    fn hidden(&self, rel: &Path) -> bool {
        let mut first = true;
        for a in rel.ancestors() {
            if a.as_os_str().is_empty() {
                break;
            }
            if self.whiteouts.contains(a) || (!first && self.opaque.contains(a)) {
                return true;
            }
            first = false;
        }
        false
    }
}

type Key = (String, PathBuf);

struct State {
    scopes: HashMap<String, Scope>,
    next_scope: u64,
    paths: HashMap<u64, Key>,
    inos: HashMap<Key, u64>,
    files: HashMap<u64, File>,
    next_fh: u64,
}

impl State {
    fn rekey(&mut self, scope: &str, from: &Path, to: &Path) {
        if let Some(ino) = self.inos.remove(&(scope.to_string(), to.to_path_buf())) {
            self.paths.remove(&ino);
        }
        let moved: Vec<(PathBuf, u64)> = self
            .inos
            .iter()
            .filter(|((s, p), _)| s == scope && p.starts_with(from))
            .map(|((_, p), &i)| (p.clone(), i))
            .collect();
        for (old, ino) in moved {
            let suffix = old.strip_prefix(from).unwrap();
            let new = if suffix.as_os_str().is_empty() { to.to_path_buf() } else { to.join(suffix) };
            self.inos.remove(&(scope.to_string(), old));
            self.inos.insert((scope.to_string(), new.clone()), ino);
            self.paths.insert(ino, (scope.to_string(), new));
        }
    }

    fn forget_path(&mut self, scope: &str, rel: &Path) {
        let key = (scope.to_string(), rel.to_path_buf());
        if let Some(ino) = self.inos.remove(&key) {
            if self.paths.get(&ino) == Some(&key) {
                match self.inos.iter().find(|(_, i)| **i == ino).map(|(k, _)| k.clone()) {
                    Some(other) => self.paths.insert(ino, other),
                    None => self.paths.remove(&ino),
                };
            }
        }
    }
}

enum Node {
    Root,
    In(String, PathBuf),
}

struct Router {
    lower: OwnedFd,
    uppers: PathBuf,
    ledger: Mutex<File>,
    state: Mutex<State>,
}

impl Router {
    fn st(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn log(&self, scope: &str, op: &str, rel: &Path, decision: &str) {
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
        let line = format!("{ms} scope={scope} op={op} path={} decision={decision}\n", rel.display());
        let _ = self.ledger.lock().unwrap().write_all(line.as_bytes());
    }

    fn node(&self, ino: INodeNo) -> R<Node> {
        if ino.0 == ROOT {
            return Ok(Node::Root);
        }
        let (s, p) = self.st().paths.get(&ino.0).cloned().ok_or(Errno::ENOENT)?;
        Ok(Node::In(s, p))
    }

    /// (scope, rel) of an entry inside a scope; the mount root itself holds no files.
    fn key(&self, ino: INodeNo) -> R<Key> {
        match self.node(ino)? {
            Node::Root => Err(Errno::EPERM),
            Node::In(s, p) => Ok((s, p)),
        }
    }

    fn child(&self, parent: INodeNo, name: &OsStr) -> R<Key> {
        let (s, p) = self.key(parent)?;
        Ok((s, p.join(name)))
    }

    fn lp(&self, rel: &Path) -> PathBuf {
        fd_path(self.lower.as_raw_fd(), rel)
    }

    fn up(&self, scope: &str, rel: &Path) -> R<PathBuf> {
        let st = self.st();
        let sc = st.scopes.get(scope).ok_or(Errno::ENOENT)?;
        Ok(fd_path(sc.upper.as_raw_fd(), rel))
    }

    fn hidden(&self, scope: &str, rel: &Path) -> bool {
        self.st().scopes.get(scope).map(|s| s.hidden(rel)).unwrap_or(true)
    }

    fn lower_meta(&self, scope: &str, rel: &Path) -> Option<fs::Metadata> {
        if self.hidden(scope, rel) {
            return None;
        }
        fs::symlink_metadata(self.lp(rel)).ok()
    }

    fn locate(&self, scope: &str, rel: &Path) -> R<(Loc, fs::Metadata)> {
        if let Ok(m) = fs::symlink_metadata(self.up(scope, rel)?) {
            return Ok((Loc::Upper, m));
        }
        self.lower_meta(scope, rel).map(|m| (Loc::Lower, m)).ok_or(Errno::ENOENT)
    }

    fn real(&self, scope: &str, rel: &Path) -> R<PathBuf> {
        Ok(match self.locate(scope, rel)?.0 {
            Loc::Upper => self.up(scope, rel)?,
            Loc::Lower => self.lp(rel),
        })
    }

    /// Per-scope inode: scope index in the high bits, then the lower st_ino (or an upper counter).
    fn ino_for(&self, scope: &str, rel: &Path) -> u64 {
        let key = (scope.to_string(), rel.to_path_buf());
        if let Some(&ino) = self.st().inos.get(&key) {
            return ino;
        }
        let lower_ino = self.lower_meta(scope, rel).map(|m| m.ino());
        let mut st = self.st();
        if let Some(&ino) = st.inos.get(&key) {
            return ino;
        }
        let sc = st.scopes.get_mut(scope).expect("scope exists");
        let prefix = sc.idx << SCOPE_SHIFT;
        let ino = match lower_ino {
            Some(i) if i < UPPER_BIT => prefix | i,
            _ => {
                sc.next_upper_ino += 1;
                prefix | UPPER_BIT | sc.next_upper_ino
            }
        };
        st.paths.insert(ino, key.clone());
        st.inos.insert(key, ino);
        ino
    }

    fn entry(&self, scope: &str, rel: &Path, reply: ReplyEntry) {
        match self.locate(scope, rel) {
            Ok((_, m)) => reply.entry(&TTL, &attr_of(self.ino_for(scope, rel), &m), Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn create_scope(&self, name: &OsStr) -> R<()> {
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        if name.contains('/') || name.starts_with('.') {
            return Err(Errno::EINVAL);
        }
        if self.st().scopes.contains_key(name) {
            return Err(Errno::EEXIST);
        }
        let dir = self.uppers.join(name);
        fs::create_dir(&dir).map_err(errno)?;
        let upper = open_dir(&dir).map_err(errno)?;
        let mut st = self.st();
        st.next_scope += 1;
        let idx = st.next_scope;
        st.scopes.insert(
            name.to_string(),
            Scope { idx, upper, whiteouts: HashSet::new(), opaque: HashSet::new(), next_upper_ino: 0 },
        );
        Ok(())
    }

    fn ensure_upper_dir(&self, scope: &str, rel: &Path) -> R<()> {
        if rel.as_os_str().is_empty() || self.up(scope, rel)?.is_dir() {
            return Ok(());
        }
        self.ensure_upper_dir(scope, rel.parent().unwrap_or(Path::new("")))?;
        let mode = self.lower_meta(scope, rel).map(|m| m.mode() & 0o7777).unwrap_or(0o755);
        match fs::DirBuilder::new().mode(mode).create(self.up(scope, rel)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(errno(e)),
        }
    }

    fn copy_up(&self, scope: &str, rel: &Path) -> R<()> {
        let (loc, m) = self.locate(scope, rel)?;
        if loc == Loc::Upper {
            return Ok(());
        }
        self.ensure_upper_dir(scope, rel.parent().unwrap_or(Path::new("")))?;
        let (src, dst) = (self.lp(rel), self.up(scope, rel)?);
        let ft = m.file_type();
        if ft.is_dir() {
            return self.ensure_upper_dir(scope, rel);
        } else if ft.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&src).map_err(errno)?, &dst).map_err(errno)?;
        } else if ft.is_file() {
            fs::copy(&src, &dst).map_err(errno)?;
        } else {
            return Err(Errno::EPERM);
        }
        let ts = |s: i64, ns: i64| libc::timespec { tv_sec: s, tv_nsec: ns };
        set_times(&dst, ts(m.atime(), m.atime_nsec()), ts(m.mtime(), m.mtime_nsec())).map_err(errno)
    }

    fn list(&self, scope: &str, rel: &Path) -> R<BTreeMap<OsString, FileType>> {
        let mut out = BTreeMap::new();
        let mut found = false;
        if let Ok(rd) = fs::read_dir(self.up(scope, rel)?) {
            found = true;
            for e in rd.filter_map(Result::ok) {
                out.insert(e.file_name(), e.file_type().map(kind_of).unwrap_or(FileType::RegularFile));
            }
        }
        let opaque = self.st().scopes.get(scope).map(|s| s.opaque.contains(rel)).unwrap_or(true);
        if !opaque && self.lower_meta(scope, rel).map(|m| m.is_dir()).unwrap_or(false) {
            if let Ok(rd) = fs::read_dir(self.lp(rel)) {
                found = true;
                for e in rd.filter_map(Result::ok) {
                    let name = e.file_name();
                    if out.contains_key(&name) || self.hidden(scope, &rel.join(&name)) {
                        continue;
                    }
                    out.insert(name, e.file_type().map(kind_of).unwrap_or(FileType::RegularFile));
                }
            }
        }
        if found { Ok(out) } else { Err(Errno::ENOENT) }
    }

    fn add_file(&self, f: File) -> FileHandle {
        let mut st = self.st();
        let fh = st.next_fh;
        st.next_fh += 1;
        st.files.insert(fh, f);
        FileHandle(fh)
    }

    fn with_file<T>(&self, fh: FileHandle, op: impl FnOnce(&File) -> io::Result<T>) -> R<T> {
        let st = self.st();
        let f = st.files.get(&fh.0).ok_or(Errno::EBADF)?;
        op(f).map_err(errno)
    }

    fn open_options(flags: i32) -> OpenOptions {
        let acc = flags & libc::O_ACCMODE;
        let mut o = OpenOptions::new();
        o.read(true);
        o.write(acc != libc::O_RDONLY);
        o.custom_flags(flags & !(libc::O_ACCMODE | libc::O_APPEND | libc::O_CREAT | libc::O_EXCL | libc::O_TRUNC));
        if flags & libc::O_TRUNC != 0 {
            o.truncate(true);
        }
        o
    }

    /// The read gate: a software rule, decided synchronously while the caller waits.
    fn read_allowed(rel: &Path) -> bool {
        rel.file_name() != Some(OsStr::new(".env"))
    }

    fn unwhiteout(&self, scope: &str, rel: &Path, is_dir: bool) {
        let mut st = self.st();
        if let Some(sc) = st.scopes.get_mut(scope) {
            if sc.whiteouts.remove(rel) && is_dir {
                sc.opaque.insert(rel.to_path_buf());
            }
        }
    }

    fn do_unlink(&self, scope: &str, rel: &Path, dir: bool) -> R<()> {
        let (loc, m) = self.locate(scope, rel)?;
        if dir != m.is_dir() {
            return Err(if dir { Errno::ENOTDIR } else { Errno::EISDIR });
        }
        if dir && !self.list(scope, rel)?.is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
        if loc == Loc::Upper {
            let p = self.up(scope, rel)?;
            if dir { fs::remove_dir(p) } else { fs::remove_file(p) }.map_err(errno)?;
        }
        let lower_visible = self.lower_meta(scope, rel).is_some();
        let mut st = self.st();
        if let Some(sc) = st.scopes.get_mut(scope) {
            if dir {
                sc.whiteouts.retain(|p| !p.starts_with(rel) || p == rel);
                sc.opaque.retain(|p| !p.starts_with(rel));
            }
            if lower_visible {
                sc.whiteouts.insert(rel.to_path_buf());
            }
        }
        st.forget_path(scope, rel);
        Ok(())
    }

    fn do_rename(&self, scope: &str, from: &Path, to: &Path, flags: RenameFlags) -> R<()> {
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            return Err(Errno::EINVAL);
        }
        let (_, fm) = self.locate(scope, from)?;
        let to_exists = self.locate(scope, to).ok();
        if flags.contains(RenameFlags::RENAME_NOREPLACE) && to_exists.is_some() {
            return Err(Errno::EEXIST);
        }
        let from_lower = self.lower_meta(scope, from).is_some();
        if fm.is_dir() && from_lower {
            return Err(Errno::EXDEV);
        }
        if let Some((_, tm)) = &to_exists {
            if tm.is_dir() && self.lower_meta(scope, to).is_some() {
                return Err(Errno::EXDEV);
            }
        }
        self.copy_up(scope, from)?;
        self.ensure_upper_dir(scope, to.parent().unwrap_or(Path::new("")))?;
        let (f, t) = (cstr(&self.up(scope, from)?), cstr(&self.up(scope, to)?));
        let rc = unsafe {
            libc::renameat2(libc::AT_FDCWD, f.as_ptr(), libc::AT_FDCWD, t.as_ptr(), flags.bits())
        };
        if rc != 0 {
            return Err(errno(io::Error::last_os_error()));
        }
        let mut st = self.st();
        if let Some(sc) = st.scopes.get_mut(scope) {
            sc.whiteouts.remove(to);
            if from_lower {
                sc.whiteouts.insert(from.to_path_buf());
            }
        }
        st.rekey(scope, from, to);
        Ok(())
    }

    fn root_attr(&self) -> R<FileAttr> {
        let m = fs::metadata(&self.uppers).map_err(errno)?;
        Ok(attr_of(ROOT, &m))
    }
}

impl Filesystem for Router {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let _ = config.add_capabilities(
            InitFlags::FUSE_ASYNC_READ
                | InitFlags::FUSE_WRITEBACK_CACHE
                | InitFlags::FUSE_PARALLEL_DIROPS
                | InitFlags::FUSE_CACHE_SYMLINKS
                | InitFlags::FUSE_NO_OPENDIR_SUPPORT,
        );
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.node(parent) {
            Ok(Node::Root) => match name.to_str().filter(|n| self.st().scopes.contains_key(*n)) {
                Some(s) => self.entry(s, Path::new(""), reply),
                None => reply.error(Errno::ENOENT),
            },
            Ok(Node::In(s, p)) => self.entry(&s, &p.join(name), reply),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let r = match self.node(ino) {
            Ok(Node::Root) => self.root_attr(),
            Ok(Node::In(s, p)) => self.locate(&s, &p).map(|(_, m)| attr_of(ino.0, &m)),
            Err(e) => Err(e),
        };
        match r {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let r = (|| {
            let (s, rel) = self.key(ino)?;
            self.copy_up(&s, &rel)?;
            let real = self.up(&s, &rel)?;
            if let Some(mode) = mode {
                fs::set_permissions(&real, fs::Permissions::from_mode(mode & 0o7777)).map_err(errno)?;
            }
            if uid.is_some() || gid.is_some() {
                std::os::unix::fs::lchown(&real, uid, gid).map_err(errno)?;
            }
            if let Some(size) = size {
                OpenOptions::new().write(true).open(&real).and_then(|f| f.set_len(size)).map_err(errno)?;
            }
            if atime.is_some() || mtime.is_some() {
                let ts = |t: Option<TimeOrNow>| match t {
                    None => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT },
                    Some(TimeOrNow::Now) => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW },
                    Some(TimeOrNow::SpecificTime(t)) => {
                        let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                        libc::timespec { tv_sec: d.as_secs() as i64, tv_nsec: d.subsec_nanos() as i64 }
                    }
                };
                set_times(&real, ts(atime), ts(mtime)).map_err(errno)?;
            }
            fs::symlink_metadata(&real).map_err(errno)
        })();
        match r {
            Ok(m) => reply.attr(&TTL, &attr_of(ino.0, &m)),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let r = self.key(ino).and_then(|(s, p)| self.real(&s, &p)).and_then(|p| fs::read_link(p).map_err(errno));
        match r {
            Ok(t) => reply.data(t.as_os_str().as_bytes()),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, mode: u32, umask: u32, reply: ReplyEntry) {
        if let Ok(Node::Root) = self.node(parent) {
            return match self.create_scope(name) {
                Ok(()) => self.entry(name.to_str().unwrap(), Path::new(""), reply),
                Err(e) => reply.error(e),
            };
        }
        let r = (|| {
            let (s, rel) = self.child(parent, name)?;
            if self.locate(&s, &rel).is_ok() {
                return Err(Errno::EEXIST);
            }
            self.ensure_upper_dir(&s, rel.parent().unwrap_or(Path::new("")))?;
            fs::DirBuilder::new().mode(mode & !umask).create(self.up(&s, &rel)?).map_err(errno)?;
            self.unwhiteout(&s, &rel, true);
            Ok((s, rel))
        })();
        match r {
            Ok((s, rel)) => self.entry(&s, &rel, reply),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.child(parent, name).and_then(|(s, rel)| self.do_unlink(&s, &rel, false)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.child(parent, name).and_then(|(s, rel)| self.do_unlink(&s, &rel, true)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn symlink(&self, _req: &Request, parent: INodeNo, link_name: &OsStr, target: &Path, reply: ReplyEntry) {
        let r = (|| {
            let (s, rel) = self.child(parent, link_name)?;
            if self.locate(&s, &rel).is_ok() {
                return Err(Errno::EEXIST);
            }
            self.ensure_upper_dir(&s, rel.parent().unwrap_or(Path::new("")))?;
            std::os::unix::fs::symlink(target, self.up(&s, &rel)?).map_err(errno)?;
            self.unwhiteout(&s, &rel, false);
            Ok((s, rel))
        })();
        match r {
            Ok((s, rel)) => self.entry(&s, &rel, reply),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let r = (|| {
            let (s1, from) = self.child(parent, name)?;
            let (s2, to) = self.child(newparent, newname)?;
            if s1 != s2 {
                return Err(Errno::EXDEV); // scopes never share entries
            }
            self.do_rename(&s1, &from, &to, flags)
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn link(&self, _req: &Request, ino: INodeNo, newparent: INodeNo, newname: &OsStr, reply: ReplyEntry) {
        let r = (|| {
            let (s, src) = self.key(ino)?;
            let (s2, dst) = self.child(newparent, newname)?;
            if s != s2 {
                return Err(Errno::EXDEV);
            }
            if self.locate(&s, &dst).is_ok() {
                return Err(Errno::EEXIST);
            }
            self.copy_up(&s, &src)?;
            self.ensure_upper_dir(&s, dst.parent().unwrap_or(Path::new("")))?;
            fs::hard_link(self.up(&s, &src)?, self.up(&s, &dst)?).map_err(errno)?;
            self.unwhiteout(&s, &dst, false);
            self.st().inos.insert((s.clone(), dst.clone()), ino.0);
            Ok((s, dst))
        })();
        match r {
            Ok((s, dst)) => self.entry(&s, &dst, reply),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let r = (|| {
            let (s, rel) = self.key(ino)?;
            let acc = flags.0 & libc::O_ACCMODE;
            let writes = acc != libc::O_RDONLY || flags.0 & libc::O_TRUNC != 0;
            if acc != libc::O_WRONLY && !Self::read_allowed(&rel) {
                self.log(&s, "read", &rel, "deny");
                return Err(Errno::EACCES);
            }
            self.log(&s, if writes { "open-write" } else { "read" }, &rel, "allow");
            let path = if writes {
                self.copy_up(&s, &rel)?;
                self.up(&s, &rel)?
            } else {
                self.real(&s, &rel)?
            };
            Self::open_options(flags.0).open(path).map_err(errno)
        })();
        match r {
            Ok(f) => reply.opened(self.add_file(f), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let r = self.with_file(fh, |f| {
            let mut buf = vec![0u8; size as usize];
            let n = f.read_at(&mut buf, offset)?;
            buf.truncate(n);
            Ok(buf)
        });
        match r {
            Ok(buf) => reply.data(&buf),
            Err(e) => reply.error(e),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.with_file(fh, |f| f.write_all_at(data, offset)) {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(e),
        }
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        reply.ok()
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.st().files.remove(&fh.0);
        reply.ok()
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, datasync: bool, reply: ReplyEmpty) {
        match self.with_file(fh, |f| if datasync { f.sync_data() } else { f.sync_all() }) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let mut entries: Vec<(u64, FileType, OsString)> =
            vec![(ino.0, FileType::Directory, ".".into()), (ROOT, FileType::Directory, "..".into())];
        match self.node(ino) {
            Ok(Node::Root) => {
                let mut names: Vec<String> = self.st().scopes.keys().cloned().collect();
                names.sort();
                for s in names {
                    entries.push((self.ino_for(&s, Path::new("")), FileType::Directory, s.into()));
                }
            }
            Ok(Node::In(s, rel)) => {
                if !rel.as_os_str().is_empty() {
                    entries[1].0 = self.ino_for(&s, rel.parent().unwrap_or(Path::new("")));
                }
                match self.list(&s, &rel) {
                    Ok(names) => {
                        for (name, kind) in names {
                            entries.push((self.ino_for(&s, &rel.join(&name)), kind, name));
                        }
                    }
                    Err(e) => return reply.error(e),
                }
            }
            Err(e) => return reply.error(e),
        }
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok()
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let p = cstr(&self.uppers);
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(p.as_ptr(), &mut s) } != 0 {
            return reply.error(errno(io::Error::last_os_error()));
        }
        reply.statfs(
            s.f_blocks, s.f_bfree, s.f_bavail, s.f_files, s.f_ffree,
            s.f_bsize as u32, s.f_namemax as u32, s.f_frsize as u32,
        );
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        reply.ok()
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let r = (|| {
            let (s, rel) = self.child(parent, name)?;
            let exists = self.locate(&s, &rel).is_ok();
            if exists && flags & libc::O_EXCL != 0 {
                return Err(Errno::EEXIST);
            }
            if exists {
                self.copy_up(&s, &rel)?;
            } else {
                self.ensure_upper_dir(&s, rel.parent().unwrap_or(Path::new("")))?;
            }
            self.log(&s, "create", &rel, "allow");
            let mut o = Self::open_options(flags);
            o.write(true).create(true).mode(mode & !umask);
            let f = o.open(self.up(&s, &rel)?).map_err(errno)?;
            self.unwhiteout(&s, &rel, false);
            let m = f.metadata().map_err(errno)?;
            Ok((s, rel, f, m))
        })();
        match r {
            Ok((s, rel, f, m)) => {
                let attr = attr_of(self.ino_for(&s, &rel), &m);
                reply.created(&TTL, &attr, Generation(0), self.add_file(f), FopenFlags::empty())
            }
            Err(e) => reply.error(e),
        }
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: {} <lower> <uppers dir> <mount> <ledger file>", args[0]);
        std::process::exit(2);
    }
    let (lower, uppers, mount, ledger) =
        (Path::new(&args[1]), PathBuf::from(&args[2]), Path::new(&args[3]), Path::new(&args[4]));
    let fs = Router {
        lower: open_dir(lower)?,
        uppers,
        ledger: Mutex::new(OpenOptions::new().create(true).append(true).open(ledger)?),
        state: Mutex::new(State {
            scopes: HashMap::new(),
            next_scope: 0,
            paths: HashMap::new(),
            inos: HashMap::new(),
            files: HashMap::new(),
            next_fh: 1,
        }),
    };
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("escrow-spike".into()),
        MountOption::Subtype("escrow".into()),
        MountOption::DefaultPermissions,
    ];
    eprintln!("serving {} at {}", lower.display(), mount.display());
    fuser::mount(fs, mount, &config)
}
