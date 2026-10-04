//! Spike 0.3: a copy-on-write FUSE view over a read-only base.
//!
//! Lower = the project (never written). Upper = a staging directory that takes
//! every change: the first write to a lower file copies it up whole; deleting a
//! lower entry records a whiteout; a directory deleted then recreated is opaque
//! (its lower contents stay hidden). Whiteouts and opaque marks live in memory.
//!
//! Inode numbers: an entry backed by the lower keeps the lower st_ino, through
//! copy-up and rename, so it is also stable across remounts while unmodified.
//! Upper-only entries get numbers from a separate range (UPPER_INO_BASE+).
//!
//! Both layers are opened (O_PATH) before mounting and reached through
//! `/proc/self/fd/<fd>/<rel>`, as in spike 0.2, so the view may cover the base.
//!
//! Usage: fuse-cow-spike <lower> <upper> <view>

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
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

/// FOPEN_KEEP_CACHE on every open when ESCROW_KEEP_CACHE=1 (spike 0.6 tuning). Safe here: the
/// lower is immutable while mounted and only this daemon writes the upper.
fn open_flags() -> FopenFlags {
    static K: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *K.get_or_init(|| std::env::var("ESCROW_KEEP_CACHE").map(|v| v == "1").unwrap_or(false)) {
        FopenFlags::FOPEN_KEEP_CACHE
    } else {
        FopenFlags::empty()
    }
}

/// Kernel entry/attr cache time; ESCROW_TTL_SECS overrides (spike 0.6 tuning).
fn ttl() -> &'static Duration {
    static T: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        Duration::from_secs(std::env::var("ESCROW_TTL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(1))
    })
}
const ROOT: u64 = 1;
const UPPER_INO_BASE: u64 = 1 << 56;

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

#[derive(Clone, Copy, PartialEq)]
enum Loc {
    Upper,
    Lower,
}

struct State {
    paths: HashMap<u64, PathBuf>,
    inos: HashMap<PathBuf, u64>,
    next_upper_ino: u64,
    files: HashMap<u64, File>,
    next_fh: u64,
    whiteouts: HashSet<PathBuf>,
    opaque: HashSet<PathBuf>,
}

impl State {
    /// True if a whiteout covers `rel` or an ancestor, or an opaque ancestor hides the lower.
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

    fn rekey(&mut self, from: &Path, to: &Path) {
        if let Some(ino) = self.inos.remove(to) {
            self.paths.remove(&ino);
        }
        let moved: Vec<(PathBuf, u64)> =
            self.inos.iter().filter(|(p, _)| p.starts_with(from)).map(|(p, &i)| (p.clone(), i)).collect();
        for (old, ino) in moved {
            let suffix = old.strip_prefix(from).unwrap();
            let new = if suffix.as_os_str().is_empty() { to.to_path_buf() } else { to.join(suffix) };
            self.inos.remove(&old);
            self.inos.insert(new.clone(), ino);
            self.paths.insert(ino, new);
        }
    }

    fn forget_path(&mut self, rel: &Path) {
        if let Some(ino) = self.inos.remove(rel) {
            if self.paths.get(&ino).map(|p| p == rel).unwrap_or(false) {
                // A hard link may still name this inode (git: link tmp object, unlink tmp).
                match self.inos.iter().find(|(_, i)| **i == ino).map(|(p, _)| p.clone()) {
                    Some(other) => self.paths.insert(ino, other),
                    None => self.paths.remove(&ino),
                };
            }
        }
    }
}

struct Overlay {
    lower: OwnedFd,
    upper: OwnedFd,
    state: Mutex<State>,
}

impl Overlay {
    fn st(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn at(fd: &OwnedFd, rel: &Path) -> PathBuf {
        let p = PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()));
        if rel.as_os_str().is_empty() { p.join(".") } else { p.join(rel) }
    }

    fn lp(&self, rel: &Path) -> PathBuf {
        Self::at(&self.lower, rel)
    }

    fn up(&self, rel: &Path) -> PathBuf {
        Self::at(&self.upper, rel)
    }

    /// Lower metadata for `rel` if the lower entry is visible.
    fn lower_meta(&self, rel: &Path) -> Option<fs::Metadata> {
        if self.st().hidden(rel) {
            return None;
        }
        fs::symlink_metadata(self.lp(rel)).ok()
    }

    fn locate(&self, rel: &Path) -> R<(Loc, fs::Metadata)> {
        if let Ok(m) = fs::symlink_metadata(self.up(rel)) {
            return Ok((Loc::Upper, m));
        }
        match self.lower_meta(rel) {
            Some(m) => Ok((Loc::Lower, m)),
            None => Err(Errno::ENOENT),
        }
    }

    fn real(&self, rel: &Path) -> R<PathBuf> {
        Ok(match self.locate(rel)?.0 {
            Loc::Upper => self.up(rel),
            Loc::Lower => self.lp(rel),
        })
    }

    fn rel(&self, ino: INodeNo) -> R<PathBuf> {
        self.st().paths.get(&ino.0).cloned().ok_or(Errno::ENOENT)
    }

    fn child(&self, parent: INodeNo, name: &OsStr) -> R<PathBuf> {
        Ok(self.rel(parent)?.join(name))
    }

    /// The inode number for `rel`: the visible lower st_ino if there is one, else a new upper number.
    fn ino_for(&self, rel: &Path) -> u64 {
        if let Some(&ino) = self.st().inos.get(rel) {
            return ino;
        }
        let lower_ino = self.lower_meta(rel).map(|m| m.ino());
        let mut st = self.st();
        if let Some(&ino) = st.inos.get(rel) {
            return ino;
        }
        let ino = match lower_ino {
            Some(i) if i != ROOT && i < UPPER_INO_BASE => i,
            _ => {
                st.next_upper_ino += 1;
                st.next_upper_ino
            }
        };
        st.paths.insert(ino, rel.to_path_buf());
        st.inos.insert(rel.to_path_buf(), ino);
        ino
    }

    fn entry(&self, rel: &Path, reply: ReplyEntry) {
        match self.locate(rel) {
            Ok((_, m)) => reply.entry(ttl(), &attr_of(self.ino_for(rel), &m), Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    /// Make sure `rel` exists as a directory in the upper, mirroring lower modes.
    fn ensure_upper_dir(&self, rel: &Path) -> R<()> {
        if rel.as_os_str().is_empty() || self.up(rel).is_dir() {
            return Ok(());
        }
        self.ensure_upper_dir(rel.parent().unwrap_or(Path::new("")))?;
        let mode = self.lower_meta(rel).map(|m| m.mode() & 0o7777).unwrap_or(0o755);
        match fs::DirBuilder::new().mode(mode).create(self.up(rel)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(errno(e)),
        }
    }

    /// Copy a lower entry to the upper (whole file), keeping mode and times.
    fn copy_up(&self, rel: &Path) -> R<()> {
        let (loc, m) = self.locate(rel)?;
        if loc == Loc::Upper {
            return Ok(());
        }
        self.ensure_upper_dir(rel.parent().unwrap_or(Path::new("")))?;
        let (src, dst) = (self.lp(rel), self.up(rel));
        let ft = m.file_type();
        if ft.is_dir() {
            return self.ensure_upper_dir(rel);
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

    /// Merged directory listing: upper entries, then visible lower entries not shadowed.
    fn list(&self, rel: &Path) -> R<BTreeMap<OsString, FileType>> {
        let mut out = BTreeMap::new();
        let mut found = false;
        if let Ok(rd) = fs::read_dir(self.up(rel)) {
            found = true;
            for e in rd.filter_map(Result::ok) {
                out.insert(e.file_name(), e.file_type().map(kind_of).unwrap_or(FileType::RegularFile));
            }
        }
        let opaque = self.st().opaque.contains(rel);
        if !opaque && self.lower_meta(rel).map(|m| m.is_dir()).unwrap_or(false) {
            if let Ok(rd) = fs::read_dir(self.lp(rel)) {
                found = true;
                for e in rd.filter_map(Result::ok) {
                    let name = e.file_name();
                    if out.contains_key(&name) || self.st().hidden(&rel.join(&name)) {
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

    /// With the writeback cache the kernel may read write-only files and handles append itself.
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

    fn do_setattr(
        &self,
        rel: &Path,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        fh: Option<FileHandle>,
    ) -> R<fs::Metadata> {
        self.copy_up(rel)?;
        let real = self.up(rel);
        if let Some(mode) = mode {
            fs::set_permissions(&real, fs::Permissions::from_mode(mode & 0o7777)).map_err(errno)?;
        }
        if uid.is_some() || gid.is_some() {
            std::os::unix::fs::lchown(&real, uid, gid).map_err(errno)?;
        }
        if let Some(size) = size {
            // An open fh may still point at the lower copy; truncate the upper by path.
            let _ = fh;
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
    }

    fn do_unlink(&self, rel: &Path, dir: bool) -> R<()> {
        let (loc, m) = self.locate(rel)?;
        if dir != m.is_dir() {
            return Err(if dir { Errno::ENOTDIR } else { Errno::EISDIR });
        }
        if dir && !self.list(rel)?.is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
        if loc == Loc::Upper {
            let r = if dir { fs::remove_dir(self.up(rel)) } else { fs::remove_file(self.up(rel)) };
            r.map_err(errno)?;
        }
        let lower_visible = self.lower_meta(rel).is_some();
        let mut st = self.st();
        if dir {
            st.whiteouts.retain(|p| !p.starts_with(rel) || p == rel);
            st.opaque.retain(|p| !p.starts_with(rel));
        }
        if lower_visible {
            st.whiteouts.insert(rel.to_path_buf());
        }
        st.forget_path(rel);
        Ok(())
    }

    /// Clear a whiteout at `rel` after creating an upper entry; a recreated lower dir turns opaque.
    fn unwhiteout(&self, rel: &Path, is_dir: bool) {
        let mut st = self.st();
        if st.whiteouts.remove(rel) && is_dir {
            st.opaque.insert(rel.to_path_buf());
        }
    }

    fn do_rename(&self, from: &Path, to: &Path, flags: RenameFlags) -> R<()> {
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            return Err(Errno::EINVAL);
        }
        let (_, fm) = self.locate(from)?;
        let to_exists = self.locate(to).ok();
        if flags.contains(RenameFlags::RENAME_NOREPLACE) && to_exists.is_some() {
            return Err(Errno::EEXIST);
        }
        let from_lower = self.lower_meta(from).is_some();
        // Directories with lower content would need redirects; report EXDEV like overlayfs,
        // so tools fall back to copy and delete.
        if fm.is_dir() && from_lower {
            return Err(Errno::EXDEV);
        }
        if let Some((_, tm)) = &to_exists {
            if tm.is_dir() && self.lower_meta(to).is_some() {
                return Err(Errno::EXDEV);
            }
        }
        self.copy_up(from)?;
        self.ensure_upper_dir(to.parent().unwrap_or(Path::new("")))?;
        let (f, t) = (cstr(&self.up(from)), cstr(&self.up(to)));
        let rc = unsafe {
            libc::renameat2(libc::AT_FDCWD, f.as_ptr(), libc::AT_FDCWD, t.as_ptr(), flags.bits())
        };
        if rc != 0 {
            return Err(errno(io::Error::last_os_error()));
        }
        let mut st = self.st();
        st.whiteouts.remove(to);
        if from_lower {
            st.whiteouts.insert(from.to_path_buf());
        }
        st.rekey(from, to);
        Ok(())
    }
}

impl Filesystem for Overlay {
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
        match self.child(parent, name) {
            Ok(rel) => self.entry(&rel, reply),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.rel(ino).and_then(|rel| self.locate(&rel)) {
            Ok((_, m)) => reply.attr(ttl(), &attr_of(ino.0, &m)),
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
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        match self.rel(ino).and_then(|rel| self.do_setattr(&rel, mode, uid, gid, size, atime, mtime, fh)) {
            Ok(m) => reply.attr(ttl(), &attr_of(ino.0, &m)),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.rel(ino).and_then(|rel| self.real(&rel)).and_then(|p| fs::read_link(p).map_err(errno)) {
            Ok(t) => reply.data(t.as_os_str().as_bytes()),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, mode: u32, umask: u32, reply: ReplyEntry) {
        let r = (|| {
            let rel = self.child(parent, name)?;
            if self.locate(&rel).is_ok() {
                return Err(Errno::EEXIST);
            }
            self.ensure_upper_dir(rel.parent().unwrap_or(Path::new("")))?;
            fs::DirBuilder::new().mode(mode & !umask).create(self.up(&rel)).map_err(errno)?;
            self.unwhiteout(&rel, true);
            Ok(rel)
        })();
        match r {
            Ok(rel) => self.entry(&rel, reply),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.child(parent, name).and_then(|rel| self.do_unlink(&rel, false)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.child(parent, name).and_then(|rel| self.do_unlink(&rel, true)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn symlink(&self, _req: &Request, parent: INodeNo, link_name: &OsStr, target: &Path, reply: ReplyEntry) {
        let r = (|| {
            let rel = self.child(parent, link_name)?;
            if self.locate(&rel).is_ok() {
                return Err(Errno::EEXIST);
            }
            self.ensure_upper_dir(rel.parent().unwrap_or(Path::new("")))?;
            std::os::unix::fs::symlink(target, self.up(&rel)).map_err(errno)?;
            self.unwhiteout(&rel, false);
            Ok(rel)
        })();
        match r {
            Ok(rel) => self.entry(&rel, reply),
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
        let r = self
            .child(parent, name)
            .and_then(|from| self.child(newparent, newname).map(|to| (from, to)))
            .and_then(|(from, to)| self.do_rename(&from, &to, flags));
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn link(&self, _req: &Request, ino: INodeNo, newparent: INodeNo, newname: &OsStr, reply: ReplyEntry) {
        let r = (|| {
            let src = self.rel(ino)?;
            let dst = self.child(newparent, newname)?;
            if self.locate(&dst).is_ok() {
                return Err(Errno::EEXIST);
            }
            self.copy_up(&src)?;
            self.ensure_upper_dir(dst.parent().unwrap_or(Path::new("")))?;
            fs::hard_link(self.up(&src), self.up(&dst)).map_err(errno)?;
            self.unwhiteout(&dst, false);
            let mut st = self.st();
            st.inos.insert(dst.clone(), ino.0);
            Ok(dst)
        })();
        match r {
            Ok(dst) => self.entry(&dst, reply),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let r = (|| {
            let rel = self.rel(ino)?;
            let writes = flags.0 & libc::O_ACCMODE != libc::O_RDONLY || flags.0 & libc::O_TRUNC != 0;
            let path = if writes {
                self.copy_up(&rel)?;
                self.up(&rel)
            } else {
                self.real(&rel)?
            };
            Self::open_options(flags.0).open(path).map_err(errno)
        })();
        match r {
            Ok(f) => reply.opened(self.add_file(f), open_flags()),
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
        let rel = match self.rel(ino) {
            Ok(r) => r,
            Err(e) => return reply.error(e),
        };
        let names = match self.list(&rel) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        let parent = rel.parent().unwrap_or(Path::new("")).to_path_buf();
        let mut entries: Vec<(u64, FileType, OsString)> = vec![
            (ino.0, FileType::Directory, ".".into()),
            (self.ino_for(&parent), FileType::Directory, "..".into()),
        ];
        for (name, kind) in names {
            entries.push((self.ino_for(&rel.join(&name)), kind, name));
        }
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok()
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let p = cstr(&self.up(Path::new("")));
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
            let rel = self.child(parent, name)?;
            let exists = self.locate(&rel).is_ok();
            if exists && flags & libc::O_EXCL != 0 {
                return Err(Errno::EEXIST);
            }
            if exists {
                self.copy_up(&rel)?;
            } else {
                self.ensure_upper_dir(rel.parent().unwrap_or(Path::new("")))?;
            }
            let mut o = Self::open_options(flags);
            o.write(true).create(true).mode(mode & !umask);
            let f = o.open(self.up(&rel)).map_err(errno)?;
            self.unwhiteout(&rel, false);
            let m = f.metadata().map_err(errno)?;
            Ok((rel, f, m))
        })();
        match r {
            Ok((rel, f, m)) => {
                let attr = attr_of(self.ino_for(&rel), &m);
                reply.created(ttl(), &attr, Generation(0), self.add_file(f), open_flags())
            }
            Err(e) => reply.error(e),
        }
    }
}

fn open_dir(p: &Path) -> io::Result<OwnedFd> {
    let c = cstr(p);
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: {} <lower> <upper> <view>", args[0]);
        std::process::exit(2);
    }
    let (lower, upper, view) = (Path::new(&args[1]), Path::new(&args[2]), Path::new(&args[3]));
    let lower_fd = open_dir(lower)?;
    let upper_fd = open_dir(upper)?;

    let mut paths = HashMap::new();
    let mut inos = HashMap::new();
    paths.insert(ROOT, PathBuf::new());
    inos.insert(PathBuf::new(), ROOT);
    let fs = Overlay {
        lower: lower_fd,
        upper: upper_fd,
        state: Mutex::new(State {
            paths,
            inos,
            next_upper_ino: UPPER_INO_BASE,
            files: HashMap::new(),
            next_fh: 1,
            whiteouts: HashSet::new(),
            opaque: HashSet::new(),
        }),
    };

    let mut config = Config::default();
    // Request threads; ESCROW_THREADS overrides (spike 0.6 tuning).
    if let Some(n) = std::env::var("ESCROW_THREADS").ok().and_then(|v| v.parse().ok()) {
        config.n_threads = Some(n);
        config.clone_fd = true;
    }
    config.mount_options = vec![
        MountOption::FSName("escrow-spike".into()),
        MountOption::Subtype("escrow".into()),
        MountOption::DefaultPermissions,
    ];
    eprintln!("serving {} (upper {}) at {}", lower.display(), upper.display(), view.display());
    fuser::mount(fs, view, &config)
}
