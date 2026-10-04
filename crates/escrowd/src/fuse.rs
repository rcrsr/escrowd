//! fuser adapter: translates kernel requests into `Views` calls and replies.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fuser::{
    AccessFlags, BackgroundSession, Config, Errno, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    InitFlags, KernelConfig, LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow, WriteFlags,
};

use crate::sys::{self, NOW, OMIT};
use crate::views::{Node, R, ROOT, ScopeHandle, Views, errno};

const TTL: Duration = Duration::from_secs(1);

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
    match v
        .locate(h, rel)
        .and_then(|(_, st)| Ok(sys::attr(v.ino_for(h, rel)?, &st)))
    {
        Ok(a) => reply.entry(&TTL, &a, Generation(0)),
        Err(e) => reply.error(e),
    }
}

fn reply_empty(r: R<()>, reply: ReplyEmpty) {
    match r {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(e),
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
                | InitFlags::FUSE_NO_OPENDIR_SUPPORT,
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
            Ok(Node::Root) => v.root_stat().map(|st| sys::attr(ROOT, &st)),
            Ok(Node::In(h, p)) => v.locate(&h, &p).map(|(_, st)| sys::attr(ino.0, &st)),
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
        let ts = |t: Option<TimeOrNow>| match t {
            None => OMIT,
            Some(TimeOrNow::Now) => NOW,
            Some(TimeOrNow::SpecificTime(t)) => sys::timespec(t),
        };
        let times = (atime.is_some() || mtime.is_some()).then(|| (ts(atime), ts(mtime)));
        let r = self
            .0
            .key(ino.0)
            .and_then(|(h, rel)| self.0.setattr(&h, &rel, mode, (uid, gid), size, times));
        match r {
            Ok(st) => reply.attr(&TTL, &sys::attr(ino.0, &st)),
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

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        match self
            .0
            .key(ino.0)
            .and_then(|(h, rel)| Ok((self.0.open(&h, &rel, flags.0)?, h)))
        {
            Ok((f, h)) => reply.opened(FileHandle(self.0.add_file(h, f)), FopenFlags::empty()),
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

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let v = &self.0;
        let r = (|| {
            let mut entries: Vec<(u64, FileType, std::ffi::OsString)> = vec![
                (ino.0, FileType::Directory, ".".into()),
                (ROOT, FileType::Directory, "..".into()),
            ];
            match v.node(ino.0)? {
                Node::Root => {
                    for id in v.scope_ids() {
                        let h = v.scope(&id)?;
                        entries.push((v.ino_for(&h, Path::new(""))?, FileType::Directory, id.into()));
                    }
                }
                Node::In(h, rel) => {
                    if offset == 0 {
                        v.log(&h, "list", &rel, "allow");
                    }
                    if !rel.as_os_str().is_empty() {
                        entries[1].0 = v.ino_for(&h, sys::parent(&rel))?;
                    }
                    for (name, kind) in v.list(&h, &rel)? {
                        entries.push((v.ino_for(&h, &rel.join(&name))?, kind, name));
                    }
                }
            }
            Ok(entries)
        })();
        let entries = match r {
            Ok(e) => e,
            Err(e) => return reply.error(e),
        };
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), (i + 1) as u64, kind, name) {
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
        _req: &Request,
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
                &TTL,
                &attr,
                Generation(0),
                FileHandle(v.add_file(h, f)),
                FopenFlags::empty(),
            ),
            Err(e) => reply.error(e),
        }
    }
}
