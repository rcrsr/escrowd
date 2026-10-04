//! Spike 0.2: a mirroring FUSE view of a base directory.
//!
//! The base is opened (O_PATH) before the mount, and every access goes through
//! `/proc/self/fd/<base fd>/<relative path>`. That magic link resolves to the
//! opened directory itself, never by name, so mounting the view over the base
//! path cannot loop back into the mount. The daemon never touches the view path
//! after mounting it.
//!
//! Usage: fuse-bwrap-spike <base> <view>

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, MountOption, OpenFlags, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, Request, TimeOrNow, WriteFlags,
};

const TTL: Duration = Duration::from_secs(1);
const ROOT: u64 = 1;

struct State {
    paths: HashMap<u64, PathBuf>,
    inos: HashMap<PathBuf, u64>,
    next_ino: u64,
    files: HashMap<u64, File>,
    next_fh: u64,
}

struct MirrorFs {
    base: OwnedFd,
    state: Mutex<State>,
}

fn errno(e: io::Error) -> Errno {
    Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))
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
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs as u64, nsecs as u32)
    } else {
        UNIX_EPOCH
    }
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

impl MirrorFs {
    /// Path to a base entry through the pre-opened fd; never resolves through the view.
    fn real(&self, rel: &Path) -> PathBuf {
        // The root is "<fd>/." so lstat-style calls see the directory, not the magic link.
        let p = PathBuf::from(format!("/proc/self/fd/{}", self.base.as_raw_fd()));
        if rel.as_os_str().is_empty() { p.join(".") } else { p.join(rel) }
    }

    fn rel(&self, ino: INodeNo) -> Result<PathBuf, Errno> {
        self.state.lock().unwrap().paths.get(&ino.0).cloned().ok_or(Errno::ENOENT)
    }

    fn child(&self, parent: INodeNo, name: &OsStr) -> Result<PathBuf, Errno> {
        Ok(self.rel(parent)?.join(name))
    }

    fn ino_for(&self, rel: &Path) -> u64 {
        let mut st = self.state.lock().unwrap();
        if let Some(&ino) = st.inos.get(rel) {
            return ino;
        }
        let ino = st.next_ino;
        st.next_ino += 1;
        st.paths.insert(ino, rel.to_path_buf());
        st.inos.insert(rel.to_path_buf(), ino);
        ino
    }

    fn entry(&self, rel: &Path, reply: ReplyEntry) {
        match fs::symlink_metadata(self.real(rel)) {
            Ok(m) => reply.entry(&TTL, &attr_of(self.ino_for(rel), &m), Generation(0)),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn forget_path(&self, rel: &Path) {
        let mut st = self.state.lock().unwrap();
        if let Some(ino) = st.inos.remove(rel) {
            st.paths.remove(&ino);
        }
    }

    fn add_file(&self, f: File) -> FileHandle {
        let mut st = self.state.lock().unwrap();
        let fh = st.next_fh;
        st.next_fh += 1;
        st.files.insert(fh, f);
        FileHandle(fh)
    }

    fn with_file<T>(&self, fh: FileHandle, op: impl FnOnce(&File) -> io::Result<T>) -> Result<T, Errno> {
        let st = self.state.lock().unwrap();
        let f = st.files.get(&fh.0).ok_or(Errno::EBADF)?;
        op(f).map_err(errno)
    }

    /// Open options for a kernel open request. With the writeback cache the kernel may
    /// read from write-only files and computes append offsets itself.
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
}

fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).unwrap()
}

impl Filesystem for MirrorFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        // The unprivileged flag set from docs/phase-0-spikes.md (as AgentFS uses).
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

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        let meta = match fh {
            Some(fh) => self.with_file(fh, |f| f.metadata()),
            None => self.rel(ino).and_then(|rel| fs::symlink_metadata(self.real(&rel)).map_err(errno)),
        };
        match meta {
            Ok(m) => reply.attr(&TTL, &attr_of(ino.0, &m)),
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
        let rel = match self.rel(ino) {
            Ok(r) => r,
            Err(e) => return reply.error(e),
        };
        let real = self.real(&rel);
        let result: io::Result<()> = (|| {
            if let Some(mode) = mode {
                fs::set_permissions(&real, fs::Permissions::from_mode(mode & 0o7777))?;
            }
            if uid.is_some() || gid.is_some() {
                std::os::unix::fs::lchown(&real, uid, gid)?;
            }
            if let Some(size) = size {
                match fh {
                    Some(fh) => self.with_file(fh, |f| f.set_len(size)).map_err(|e| io::Error::from_raw_os_error(i32::from(e)))?,
                    None => OpenOptions::new().write(true).open(&real)?.set_len(size)?,
                }
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
                let times = [ts(atime), ts(mtime)];
                let p = cstr(&real);
                if unsafe { libc::utimensat(libc::AT_FDCWD, p.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        })();
        match result.and_then(|_| fs::symlink_metadata(&real)) {
            Ok(m) => reply.attr(&TTL, &attr_of(ino.0, &m)),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.rel(ino).and_then(|rel| fs::read_link(self.real(&rel)).map_err(errno)) {
            Ok(t) => reply.data(t.as_os_str().as_bytes()),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, mode: u32, umask: u32, reply: ReplyEntry) {
        let rel = match self.child(parent, name) {
            Ok(r) => r,
            Err(e) => return reply.error(e),
        };
        match fs::DirBuilder::new().mode(mode & !umask).create(self.real(&rel)) {
            Ok(()) => self.entry(&rel, reply),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.child(parent, name).and_then(|rel| fs::remove_file(self.real(&rel)).map_err(errno).map(|_| rel)) {
            Ok(rel) => {
                self.forget_path(&rel);
                reply.ok()
            }
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.child(parent, name).and_then(|rel| fs::remove_dir(self.real(&rel)).map_err(errno).map(|_| rel)) {
            Ok(rel) => {
                self.forget_path(&rel);
                reply.ok()
            }
            Err(e) => reply.error(e),
        }
    }

    fn symlink(&self, _req: &Request, parent: INodeNo, link_name: &OsStr, target: &Path, reply: ReplyEntry) {
        let rel = match self.child(parent, link_name) {
            Ok(r) => r,
            Err(e) => return reply.error(e),
        };
        match std::os::unix::fs::symlink(target, self.real(&rel)) {
            Ok(()) => self.entry(&rel, reply),
            Err(e) => reply.error(errno(e)),
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
        let (from, to) = match (self.child(parent, name), self.child(newparent, newname)) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => return reply.error(e),
        };
        let (f, t) = (cstr(&self.real(&from)), cstr(&self.real(&to)));
        let rc = unsafe {
            libc::renameat2(libc::AT_FDCWD, f.as_ptr(), libc::AT_FDCWD, t.as_ptr(), flags.bits())
        };
        if rc != 0 {
            return reply.error(errno(io::Error::last_os_error()));
        }
        // Re-key the moved entry and everything under it.
        let mut st = self.state.lock().unwrap();
        if let Some(ino) = st.inos.remove(&to) {
            st.paths.remove(&ino);
        }
        let moved: Vec<(PathBuf, u64)> = st
            .inos
            .iter()
            .filter(|(p, _)| p.starts_with(&from))
            .map(|(p, &i)| (p.clone(), i))
            .collect();
        for (old, ino) in moved {
            // join("") would add a trailing slash, so the entry itself maps to `to` exactly.
            let suffix = old.strip_prefix(&from).unwrap();
            let new = if suffix.as_os_str().is_empty() { to.clone() } else { to.join(suffix) };
            st.inos.remove(&old);
            st.inos.insert(new.clone(), ino);
            st.paths.insert(ino, new);
        }
        reply.ok()
    }

    fn link(&self, _req: &Request, ino: INodeNo, newparent: INodeNo, newname: &OsStr, reply: ReplyEntry) {
        let (src, dst) = match (self.rel(ino), self.child(newparent, newname)) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => return reply.error(e),
        };
        match fs::hard_link(self.real(&src), self.real(&dst)) {
            Ok(()) => self.entry(&dst, reply),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        match self.rel(ino).and_then(|rel| Self::open_options(flags.0).open(self.real(&rel)).map_err(errno)) {
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
        self.state.lock().unwrap().files.remove(&fh.0);
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
        let mut entries: Vec<(u64, FileType, std::ffi::OsString)> = vec![
            (ino.0, FileType::Directory, ".".into()),
            (self.ino_for(rel.parent().unwrap_or(Path::new(""))), FileType::Directory, "..".into()),
        ];
        let rd = match fs::read_dir(self.real(&rel)) {
            Ok(rd) => rd,
            Err(e) => return reply.error(errno(e)),
        };
        let mut names: Vec<_> = rd.filter_map(Result::ok).collect();
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let kind = e.file_type().map(kind_of).unwrap_or(FileType::RegularFile);
            let child = rel.join(e.file_name());
            entries.push((self.ino_for(&child), kind, e.file_name()));
        }
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok()
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let p = cstr(&self.real(Path::new("")));
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
        // Mounted with default_permissions: the kernel checks modes itself.
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
        let rel = match self.child(parent, name) {
            Ok(r) => r,
            Err(e) => return reply.error(e),
        };
        let mut o = Self::open_options(flags);
        o.write(true).mode(mode & !umask);
        if flags & libc::O_EXCL != 0 {
            o.create_new(true);
        } else {
            o.create(true);
        }
        let real = self.real(&rel);
        match o.open(&real).and_then(|f| f.metadata().map(|m| (f, m))) {
            Ok((f, m)) => {
                let attr = attr_of(self.ino_for(&rel), &m);
                reply.created(&TTL, &attr, Generation(0), self.add_file(f), FopenFlags::empty())
            }
            Err(e) => reply.error(errno(e)),
        }
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: {} <base> <view>", args[0]);
        std::process::exit(2);
    }
    let (base, view) = (Path::new(&args[1]), Path::new(&args[2]));

    // Pre-open the base before mounting, so the view may even cover the base path.
    let p = cstr(base);
    let fd = unsafe { libc::open(p.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let base_fd = unsafe { OwnedFd::from_raw_fd(fd) };

    let mut paths = HashMap::new();
    let mut inos = HashMap::new();
    paths.insert(ROOT, PathBuf::new());
    inos.insert(PathBuf::new(), ROOT);
    let fs = MirrorFs {
        base: base_fd,
        state: Mutex::new(State { paths, inos, next_ino: ROOT + 1, files: HashMap::new(), next_fh: 1 }),
    };

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("escrow-spike".into()),
        MountOption::Subtype("escrow".into()),
        MountOption::DefaultPermissions,
    ];
    eprintln!("serving {} at {}", base.display(), view.display());
    io::stderr().flush()?;
    fuser::mount(fs, view, &config)
}
