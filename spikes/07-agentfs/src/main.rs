//! Spike 0.7: AgentFS's OverlayFS (crate agentfs-sdk) behind our own fuser adapter.
//!
//! Same command line as spike 0.3, so 0.3's run.sh can test it unchanged:
//!   fuse-agentfs-spike <lower> <upper dir> <view>
//! The lower is AgentFS's HostFS; the delta is AgentFS's SQLite store at <upper>/delta.db.
//! AgentFS is async (tokio); each FUSE callback blocks on the runtime.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentfs_sdk::error::Error as AfsError;
use agentfs_sdk::filesystem::{
    AgentFS, BoxedFile, FileSystem, HostFS, OverlayFS, Stats, TimeChange, S_IFDIR, S_IFLNK, S_IFMT,
};
use fuser::{
    AccessFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, MountOption, OpenFlags, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use tokio::runtime::Runtime;

const TTL: Duration = Duration::from_secs(1);

type R<T> = Result<T, Errno>;

fn errno(e: AfsError) -> Errno {
    match e {
        AfsError::Fs(f) => Errno::from_i32(f.to_errno()),
        AfsError::Io(io) => Errno::from_i32(io.raw_os_error().unwrap_or(libc::EIO)),
        _ => Errno::EIO,
    }
}

fn time(secs: i64, nsecs: u32) -> SystemTime {
    if secs >= 0 { UNIX_EPOCH + Duration::new(secs as u64, nsecs) } else { UNIX_EPOCH }
}

fn kind(mode: u32) -> FileType {
    match mode & S_IFMT {
        S_IFDIR => FileType::Directory,
        S_IFLNK => FileType::Symlink,
        0o010000 => FileType::NamedPipe,
        0o020000 => FileType::CharDevice,
        0o060000 => FileType::BlockDevice,
        0o140000 => FileType::Socket,
        _ => FileType::RegularFile,
    }
}

fn attr(s: &Stats) -> FileAttr {
    FileAttr {
        ino: INodeNo(s.ino as u64),
        size: s.size.max(0) as u64,
        blocks: (s.size.max(0) as u64).div_ceil(512),
        atime: time(s.atime, s.atime_nsec),
        mtime: time(s.mtime, s.mtime_nsec),
        ctime: time(s.ctime, s.ctime_nsec),
        crtime: time(s.ctime, s.ctime_nsec),
        kind: kind(s.mode),
        perm: (s.mode & 0o7777) as u16,
        nlink: s.nlink,
        uid: s.uid,
        gid: s.gid,
        rdev: s.rdev as u32,
        blksize: 4096,
        flags: 0,
    }
}

fn tc(t: Option<TimeOrNow>) -> TimeChange {
    match t {
        None => TimeChange::Omit,
        Some(TimeOrNow::Now) => TimeChange::Now,
        Some(TimeOrNow::SpecificTime(t)) => {
            let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
            TimeChange::Set(d.as_secs() as i64, d.subsec_nanos())
        }
    }
}

struct Adapter {
    fs: OverlayFS,
    rt: Runtime,
    files: Mutex<HashMap<u64, BoxedFile>>,
    next_fh: AtomicU64,
}

impl Adapter {
    fn run<T>(&self, f: impl std::future::Future<Output = agentfs_sdk::error::Result<T>>) -> R<T> {
        self.rt.block_on(f).map_err(errno)
    }

    fn name(name: &OsStr) -> R<&str> {
        name.to_str().ok_or(Errno::EINVAL)
    }

    fn add(&self, f: BoxedFile) -> FileHandle {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.files.lock().unwrap().insert(fh, f);
        FileHandle(fh)
    }

    fn file(&self, fh: FileHandle) -> R<BoxedFile> {
        self.files.lock().unwrap().get(&fh.0).cloned().ok_or(Errno::EBADF)
    }

    fn exists(&self, parent: INodeNo, name: &str) -> R<bool> {
        Ok(self.run(self.fs.lookup(parent.0 as i64, name))?.is_some())
    }
}

impl Filesystem for Adapter {
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
        let r = Self::name(name).and_then(|n| self.run(self.fs.lookup(parent.0 as i64, n)));
        match r {
            Ok(Some(s)) => reply.entry(&TTL, &attr(&s), Generation(0)),
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(e),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.rt.block_on(self.fs.forget(ino.0 as i64, nlookup));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.run(self.fs.getattr(ino.0 as i64)) {
            Ok(Some(s)) => reply.attr(&TTL, &attr(&s)),
            Ok(None) => reply.error(Errno::ENOENT),
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
        let i = ino.0 as i64;
        let r = (|| {
            if let Some(m) = mode {
                self.run(self.fs.chmod(i, m & 0o7777))?;
            }
            if uid.is_some() || gid.is_some() {
                self.run(self.fs.chown(i, uid, gid))?;
            }
            if let Some(sz) = size {
                let f = match fh.and_then(|h| self.file(h).ok()) {
                    Some(f) => f,
                    None => self.run(self.fs.open(i, libc::O_RDWR))?,
                };
                self.run(f.truncate(sz))?;
            }
            if atime.is_some() || mtime.is_some() {
                self.run(self.fs.utimens(i, tc(atime), tc(mtime)))?;
            }
            self.run(self.fs.getattr(i))?.ok_or(Errno::ENOENT)
        })();
        match r {
            Ok(s) => reply.attr(&TTL, &attr(&s)),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.run(self.fs.readlink(ino.0 as i64)) {
            Ok(Some(t)) => reply.data(t.as_bytes()),
            Ok(None) => reply.error(Errno::EINVAL),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32, umask: u32, reply: ReplyEntry) {
        let r = Self::name(name)
            .and_then(|n| self.run(self.fs.mkdir(parent.0 as i64, n, mode & !umask, req.uid(), req.gid())));
        match r {
            Ok(s) => reply.entry(&TTL, &attr(&s), Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match Self::name(name).and_then(|n| self.run(self.fs.unlink(parent.0 as i64, n))) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match Self::name(name).and_then(|n| self.run(self.fs.rmdir(parent.0 as i64, n))) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn symlink(&self, req: &Request, parent: INodeNo, link_name: &OsStr, target: &Path, reply: ReplyEntry) {
        let r = (|| {
            let n = Self::name(link_name)?;
            let t = target.to_str().ok_or(Errno::EINVAL)?;
            self.run(self.fs.symlink(parent.0 as i64, n, t, req.uid(), req.gid()))
        })();
        match r {
            Ok(s) => reply.entry(&TTL, &attr(&s), Generation(0)),
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
            let (a, b) = (Self::name(name)?, Self::name(newname)?);
            // AgentFS's rename takes no flags: emulate NOREPLACE, refuse EXCHANGE.
            if flags.contains(RenameFlags::RENAME_EXCHANGE) {
                return Err(Errno::EINVAL);
            }
            if flags.contains(RenameFlags::RENAME_NOREPLACE) && self.exists(newparent, b)? {
                return Err(Errno::EEXIST);
            }
            self.run(self.fs.rename(parent.0 as i64, a, newparent.0 as i64, b))
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn link(&self, _req: &Request, ino: INodeNo, newparent: INodeNo, newname: &OsStr, reply: ReplyEntry) {
        let r = Self::name(newname).and_then(|n| self.run(self.fs.link(ino.0 as i64, newparent.0 as i64, n)));
        match r {
            Ok(s) => reply.entry(&TTL, &attr(&s), Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let r = (|| {
            let f = self.run(self.fs.open(ino.0 as i64, flags.0))?;
            if flags.0 & libc::O_TRUNC != 0 {
                self.run(f.truncate(0))?;
            }
            Ok(f)
        })();
        match r {
            Ok(f) => reply.opened(self.add(f), FopenFlags::empty()),
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
        match self.file(fh).and_then(|f| self.run(f.pread(offset, size as u64))) {
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
        match self.file(fh).and_then(|f| self.run(f.pwrite(offset, data))) {
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
        self.files.lock().unwrap().remove(&fh.0);
        reply.ok()
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        match self.file(fh).and_then(|f| self.run(f.fsync())) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let entries = match self.run(self.fs.readdir_plus(ino.0 as i64)) {
            Ok(Some(e)) => e,
            Ok(None) => return reply.error(Errno::ENOENT),
            Err(e) => return reply.error(e),
        };
        let mut all: Vec<(u64, FileType, String)> =
            vec![(ino.0, FileType::Directory, ".".into()), (ino.0, FileType::Directory, "..".into())];
        all.extend(entries.into_iter().map(|e| (e.stats.ino as u64, kind(e.stats.mode), e.name)));
        for (i, (ino, k, name)) in all.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), (i + 1) as u64, k, name) {
                break;
            }
        }
        reply.ok()
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match self.run(self.fs.statfs()) {
            Ok(s) => reply.statfs(1 << 24, 1 << 23, 1 << 23, s.inodes + (1 << 20), 1 << 20, 4096, 255, 4096),
            Err(e) => reply.error(e),
        }
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        reply.ok()
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let r = (|| {
            let n = Self::name(name)?;
            if let Some(s) = self.run(self.fs.lookup(parent.0 as i64, n))? {
                if flags & libc::O_EXCL != 0 {
                    return Err(Errno::EEXIST);
                }
                let f = self.run(self.fs.open(s.ino, flags))?;
                if flags & libc::O_TRUNC != 0 {
                    self.run(f.truncate(0))?;
                }
                let s = self.run(f.fstat())?;
                return Ok((s, f));
            }
            self.run(self.fs.create_file(parent.0 as i64, n, mode & !umask, req.uid(), req.gid()))
        })();
        match r {
            Ok((s, f)) => reply.created(&TTL, &attr(&s), Generation(0), self.add(f), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: {} <lower> <upper dir> <view>", args[0]);
        std::process::exit(2);
    }
    let (lower, upper, view) = (&args[1], Path::new(&args[2]), Path::new(&args[3]));
    let rt = Runtime::new()?;
    let db = upper.join("delta.db");
    let fs = rt
        .block_on(async {
            let base: Arc<dyn FileSystem> = Arc::new(HostFS::new(lower.as_str())?);
            let delta = AgentFS::new(db.to_str().unwrap()).await?;
            let overlay = OverlayFS::new(base, delta);
            overlay.init(lower).await?;
            agentfs_sdk::error::Result::Ok(overlay)
        })
        .map_err(|e| io::Error::other(e.to_string()))?;
    let adapter = Adapter { fs, rt, files: Mutex::new(HashMap::new()), next_fh: AtomicU64::new(1) };
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("escrow-agentfs".into()),
        MountOption::Subtype("escrow".into()),
        MountOption::DefaultPermissions,
    ];
    eprintln!("serving {lower} (delta {}) at {}", db.display(), view.display());
    fuser::mount(adapter, view, &config)
}
