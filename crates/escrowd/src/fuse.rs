//! fuser adapter: translates kernel requests into `Views` calls and replies.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fuser::{
    AccessFlags, BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, InitFlags, KernelConfig, LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
    TimeOrNow, WriteFlags,
};

use crate::sys::{self, NOW, OMIT};
use crate::views::{Node, Pages, R, ROOT, ScopeHandle, UNSCOPED, Views, errno};

/// The mount root lists scopes, which RPC calls add and remove.
const TTL: Duration = Duration::from_secs(1);
/// A scope's view changes only through its own requests (it reads a snapshot of the
/// base), so the kernel may cache its entries, attributes and absent names for long.
/// The deny root reads the live base, which commits change.
const SNAPSHOT_TTL: Duration = Duration::from_secs(60);

fn ttl(h: &ScopeHandle) -> &'static Duration {
    if h.readonly { &TTL } else { &SNAPSHOT_TTL }
}

pub struct FuseView(pub Arc<Views>);

/// Mount the views at `mount` and serve them on background threads until the session drops.
pub fn mount(views: Arc<Views>, mount: &Path, threads: usize) -> io::Result<BackgroundSession> {
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("escrowd".into()),
        MountOption::Subtype("escrow".into()),
        MountOption::DefaultPermissions,
    ];
    config.n_threads = Some(threads.max(1));
    config.clone_fd = threads > 1;
    fuser::spawn_mount(FuseView(views), mount, &config)
}

fn reply_entry(v: &Views, h: &ScopeHandle, rel: &Path, reply: ReplyEntry) {
    // A scope's root sits in the mount root, which RPC calls change.
    let ttl = if rel.as_os_str().is_empty() { &TTL } else { ttl(h) };
    match v
        .locate(h, rel)
        .and_then(|(_, st)| Ok(sys::attr(v.ino_for(h, rel)?, &st)))
    {
        Ok(a) => reply.entry(ttl, &a, Generation(0)),
        // A negative entry: the kernel caches the absence (a create replaces it). Not under
        // the unscoped root, which resets in place: kernel 7.0 keeps a negative entry
        // through its invalidation.
        Err(Errno::ENOENT) if !h.readonly && h.group != UNSCOPED => reply.entry(ttl, &sys::absent(), Generation(0)),
        Err(e) => reply.error(e),
    }
}

fn reply_empty(r: R<()>, reply: ReplyEmpty) {
    match r {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(e),
    }
}

/// The largest base file whose pages a first read-only open stores ahead of the reads.
const PREFILL_MAX: u64 = 128 * 1024;

/// Store the whole file in the kernel's page cache, saving the READ round trips that
/// would follow the open (most files opened are read whole); false if that failed.
fn prefill(v: &Views, ino: u64, f: &std::fs::File, size: u64) -> bool {
    let mut buf = vec![0u8; size as usize];
    let mut got = 0;
    while got < buf.len() {
        match f.read_at(&mut buf[got..], got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(_) => return false,
        }
    }
    got == buf.len() && (got == 0 || v.store_pages(ino, &buf))
}

/// A directory: its scope and path (None for the mount root).
type Dir = Option<(Arc<ScopeHandle>, PathBuf)>;

/// Directory `ino`'s entries, `.` and `..` first; a listing from offset 0 is logged.
fn listing(v: &Views, ino: u64, offset: u64) -> R<(Dir, Vec<(FileType, OsString)>)> {
    let mut entries = vec![
        (FileType::Directory, OsString::from(".")),
        (FileType::Directory, OsString::from("..")),
    ];
    match v.node(ino)? {
        Node::Root => {
            entries.extend(v.scope_ids().into_iter().map(|id| (FileType::Directory, id.into())));
            Ok((None, entries))
        }
        Node::In(h, rel) => {
            let listed = v.list_checked(&h, &rel, offset == 0)?;
            entries.extend(listed.into_iter().map(|(name, kind)| (kind, name)));
            Ok((Some((h, rel)), entries))
        }
    }
}

/// The inode number of entry `name` of directory `dir_ino`.
fn entry_ino(v: &Views, dir_ino: u64, at: &Dir, name: &OsStr) -> R<u64> {
    match (name.as_bytes(), at) {
        (b".", _) => Ok(dir_ino),
        (b"..", Some((h, rel))) if !rel.as_os_str().is_empty() => v.ino_for(h, sys::parent(rel)),
        (b"..", _) => Ok(ROOT),
        (_, None) => {
            let h = v.scope(&name.to_string_lossy())?;
            v.ino_for(&h, Path::new(""))
        }
        (_, Some((h, rel))) => v.ino_for(h, &rel.join(name)),
    }
}

/// The cache lifetime and attributes of entry `name` (the kernel ignores those of `.` and `..`).
fn entry_attr(v: &Views, dir_ino: u64, at: &Dir, name: &OsStr) -> R<(&'static Duration, FileAttr)> {
    let ino = entry_ino(v, dir_ino, at, name)?;
    match (name.as_bytes(), at) {
        (b"." | b"..", _) => Ok((
            &TTL,
            FileAttr {
                ino: INodeNo(ino),
                ..sys::absent()
            },
        )),
        (_, None) => {
            let h = v.scope(&name.to_string_lossy())?;
            Ok((&TTL, sys::attr(ino, &v.locate(&h, Path::new(""))?.1)))
        }
        (_, Some((h, rel))) => Ok((ttl(h), sys::attr(ino, &v.locate(h, &rel.join(name))?.1))),
    }
}

impl Filesystem for FuseView {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        // Unprivileged cache flags; passthrough needs CAP_SYS_ADMIN and is not used.
        let _ = config.add_capabilities(
            InitFlags::FUSE_ASYNC_READ
                | InitFlags::FUSE_WRITEBACK_CACHE
                | InitFlags::FUSE_PARALLEL_DIROPS
                | InitFlags::FUSE_CACHE_SYMLINKS
                | InitFlags::FUSE_NO_OPENDIR_SUPPORT
                | InitFlags::FUSE_DO_READDIRPLUS
                | InitFlags::FUSE_READDIRPLUS_AUTO,
        );
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let v = &self.0;
        match v.node(parent.0) {
            Ok(Node::Root) => match name.to_str().map(|n| v.scope(n)) {
                Some(Ok(h)) => reply_entry(v, &h, Path::new(""), reply),
                _ => reply.error(Errno::ENOENT),
            },
            Ok(Node::In(h, p)) => reply_entry(v, &h, &p.join(name), reply),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let v = &self.0;
        let r = match v.node(ino.0) {
            Ok(Node::Root) => v.root_stat().map(|st| (&TTL, sys::attr(ROOT, &st))),
            Ok(Node::In(h, p)) => v.locate(&h, &p).map(|(_, st)| (ttl(&h), sys::attr(ino.0, &st))),
            Err(e) => Err(e),
        };
        match r {
            Ok((ttl, a)) => reply.attr(ttl, &a),
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
        let ts = |t: Option<TimeOrNow>| match t {
            None => OMIT,
            Some(TimeOrNow::Now) => NOW,
            Some(TimeOrNow::SpecificTime(t)) => sys::timespec(t),
        };
        let times = (atime.is_some() || mtime.is_some()).then(|| (ts(atime), ts(mtime)));
        let r = self.0.key(ino.0).and_then(|(h, rel)| {
            let st = self.0.setattr(&h, &rel, mode, (uid, gid), size, times)?;
            Ok((ttl(&h), st))
        });
        match r {
            Ok((ttl, st)) => reply.attr(ttl, &sys::attr(ino.0, &st)),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.0.key(ino.0).and_then(|(h, rel)| self.0.readlink(&h, &rel)) {
            Ok(t) => reply.data(t.as_os_str().as_bytes()),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, mode: u32, umask: u32, reply: ReplyEntry) {
        // Scopes are created over RPC, never by mkdir in the mount root.
        match self
            .0
            .child(parent.0, name)
            .and_then(|(h, rel)| self.0.mkdir(&h, &rel, mode & !umask).map(|()| (h, rel)))
        {
            Ok((h, rel)) => reply_entry(&self.0, &h, &rel, reply),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        reply_empty(
            self.0
                .child(parent.0, name)
                .and_then(|(h, rel)| self.0.unlink(&h, &rel, false)),
            reply,
        )
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        reply_empty(
            self.0
                .child(parent.0, name)
                .and_then(|(h, rel)| self.0.unlink(&h, &rel, true)),
            reply,
        )
    }

    fn symlink(&self, _req: &Request, parent: INodeNo, link_name: &OsStr, target: &Path, reply: ReplyEntry) {
        match self
            .0
            .child(parent.0, link_name)
            .and_then(|(h, rel)| self.0.symlink(&h, &rel, target).map(|()| (h, rel)))
        {
            Ok((h, rel)) => reply_entry(&self.0, &h, &rel, reply),
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
            let (h, from) = self.0.child(parent.0, name)?;
            let (h2, to) = self.0.child(newparent.0, newname)?;
            if h.id != h2.id {
                return Err(Errno::EXDEV); // scopes never share entries
            }
            self.0.rename(&h, &from, &to, flags.bits())
        })();
        reply_empty(r, reply)
    }

    fn link(&self, _req: &Request, ino: INodeNo, newparent: INodeNo, newname: &OsStr, reply: ReplyEntry) {
        let r = (|| {
            let (h, src) = self.0.key(ino.0)?;
            let (h2, dst) = self.0.child(newparent.0, newname)?;
            if h.id != h2.id {
                return Err(Errno::EXDEV);
            }
            self.0.link(&h, ino.0, &src, &dst)?;
            Ok((h, dst))
        })();
        match r {
            Ok((h, dst)) => reply_entry(&self.0, &h, &dst, reply),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        match self
            .0
            .key(ino.0)
            .and_then(|(h, rel)| Ok((self.0.open(&h, ino.0, &rel, flags.0)?, h)))
        {
            Ok(((f, pages), h)) => {
                let keep = match pages {
                    Pages::Keep => true,
                    Pages::Drop => false,
                    Pages::Empty(size) => size <= PREFILL_MAX && prefill(&self.0, ino.0, &f, size),
                };
                let flags = if keep {
                    FopenFlags::FOPEN_KEEP_CACHE
                } else {
                    FopenFlags::empty()
                };
                reply.opened(FileHandle(self.0.add_file(h, f, req.pid())), flags)
            }
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
        let r = self.0.file(fh.0).and_then(|f| {
            let mut buf = vec![0u8; size as usize];
            let n = f.read_at(&mut buf, offset).map_err(errno)?;
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
        match self
            .0
            .file_for_write(fh.0)
            .and_then(|f| f.write_all_at(data, offset).map_err(errno))
        {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(e),
        }
    }

    /// Nothing to do at close: the kernel writes dirty pages back before FLUSH, and
    /// ENOSYS tells it to skip the round trip on every later close.
    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        reply.error(Errno::ENOSYS)
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
        self.0.release(fh.0);
        reply.ok()
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, datasync: bool, reply: ReplyEmpty) {
        let r = self
            .0
            .file(fh.0)
            .and_then(|f| if datasync { f.sync_data() } else { f.sync_all() }.map_err(errno));
        reply_empty(r, reply)
    }

    /// A scope's listings change only through its own requests: the kernel may cache them
    /// across opens. The mount root (scopes come and go) and the deny root (the live base) do not.
    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let flags = match self.0.node(ino.0) {
            Ok(Node::In(h, _)) if !h.readonly => FopenFlags::FOPEN_CACHE_DIR | FopenFlags::FOPEN_KEEP_CACHE,
            Ok(_) => FopenFlags::empty(),
            Err(e) => return reply.error(e),
        };
        reply.opened(FileHandle(0), flags)
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let v = &self.0;
        let (at, entries) = match listing(v, ino.0, offset) {
            Ok(l) => l,
            Err(e) => return reply.error(e),
        };
        // Inode numbers only for the entries that fit: the kernel asks again from the next offset.
        for (i, (kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            let Ok(child) = entry_ino(v, ino.0, &at, name) else {
                continue;
            };
            if reply.add(INodeNo(child), (i + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok()
    }

    /// A listing with each entry's attributes: saves the kernel a lookup per entry.
    fn readdirplus(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectoryPlus) {
        let v = &self.0;
        let (at, entries) = match listing(v, ino.0, offset) {
            Ok(l) => l,
            Err(e) => return reply.error(e),
        };
        for (i, (_, name)) in entries.iter().enumerate().skip(offset as usize) {
            let Ok((ttl, attr)) = entry_attr(v, ino.0, &at, name) else {
                continue;
            };
            if reply.add(attr.ino, (i + 1) as u64, name, ttl, &attr, Generation(0)) {
                break;
            }
        }
        reply.ok()
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match self.0.statfs() {
            Ok(s) => reply.statfs(
                s.f_blocks,
                s.f_bfree,
                s.f_bavail,
                s.f_files,
                s.f_ffree,
                s.f_bsize as u32,
                s.f_namemax as u32,
                s.f_frsize as u32,
            ),
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
        let v = &self.0;
        let r = (|| {
            let (h, rel) = v.child(parent.0, name)?;
            let f = v.create(&h, &rel, mode & !umask, flags)?;
            let st = rustix::fs::fstat(&f).map_err(|e| errno(e.into()))?;
            let attr = sys::attr(v.ino_for(&h, &rel)?, &st);
            Ok((attr, f, h))
        })();
        match r {
            Ok((attr, f, h)) => reply.created(
                ttl(&h),
                &attr,
                Generation(0),
                FileHandle(v.add_file(h, f, req.pid())),
                FopenFlags::empty(),
            ),
            Err(e) => reply.error(e),
        }
    }
}
