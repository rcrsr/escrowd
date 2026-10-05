//! fd-relative file operations on the pre-opened base and the scope uppers.
//!
//! Every path here is relative to a directory fd and never goes through
//! `/proc/self/fd`. The FUSE kernel resolves symlinks itself and looks up one
//! component at a time, so relative paths reaching the daemon never cross a
//! symlink unless the base changes underneath (hardening: phase 2).

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{FileAttr, FileType, INodeNo};
use rustix::fs::{self as rfs, AtFlags, Mode, OFlags, Stat, Timespec, Timestamps};

/// `rel` as an fd-relative path; the empty path (the directory itself) is `.`.
pub fn at(rel: &Path) -> &Path {
    if rel.as_os_str().is_empty() {
        Path::new(".")
    } else {
        rel
    }
}

pub fn parent(rel: &Path) -> &Path {
    rel.parent().unwrap_or(Path::new(""))
}

pub fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    Ok(rfs::open(
        path,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

pub fn lstat(dir: BorrowedFd, rel: &Path) -> io::Result<Stat> {
    Ok(rfs::statat(dir, at(rel), AtFlags::SYMLINK_NOFOLLOW)?)
}

pub fn is_dir(st: &Stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

pub fn open(dir: BorrowedFd, rel: &Path, flags: OFlags, mode: u32) -> io::Result<File> {
    let fd = rfs::openat(
        dir,
        at(rel),
        flags | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_raw_mode(mode),
    )?;
    Ok(File::from(fd))
}

pub fn mkdir(dir: BorrowedFd, rel: &Path, mode: u32) -> io::Result<()> {
    Ok(rfs::mkdirat(dir, rel, Mode::from_raw_mode(mode))?)
}

pub fn unlink(dir: BorrowedFd, rel: &Path, is_dir: bool) -> io::Result<()> {
    Ok(rfs::unlinkat(
        dir,
        rel,
        if is_dir { AtFlags::REMOVEDIR } else { AtFlags::empty() },
    )?)
}

pub fn rename(dir: BorrowedFd, from: &Path, to: &Path, flags: u32) -> io::Result<()> {
    Ok(rfs::renameat_with(
        dir,
        from,
        dir,
        to,
        rfs::RenameFlags::from_bits_retain(flags),
    )?)
}

pub fn symlink(target: &Path, dir: BorrowedFd, rel: &Path) -> io::Result<()> {
    Ok(rfs::symlinkat(target, dir, rel)?)
}

pub fn readlink(dir: BorrowedFd, rel: &Path) -> io::Result<PathBuf> {
    let c = rfs::readlinkat(dir, rel, Vec::new())?;
    Ok(PathBuf::from(OsString::from_vec(c.into_bytes())))
}

pub fn link(dir: BorrowedFd, from: &Path, to: &Path) -> io::Result<()> {
    Ok(rfs::linkat(dir, from, dir, to, AtFlags::empty())?)
}

/// Hard link `src_dir/src` as `dst_dir/dst` (EXDEV across filesystems).
pub fn link_across(src_dir: BorrowedFd, src: &Path, dst_dir: BorrowedFd, dst: &Path) -> io::Result<()> {
    Ok(rfs::linkat(src_dir, src, dst_dir, dst, AtFlags::empty())?)
}

pub fn chmod(dir: BorrowedFd, rel: &Path, mode: u32) -> io::Result<()> {
    Ok(rfs::chmodat(
        dir,
        at(rel),
        Mode::from_raw_mode(mode & 0o7777),
        AtFlags::empty(),
    )?)
}

pub fn chown(dir: BorrowedFd, rel: &Path, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
    let uid = uid.map(rfs::Uid::from_raw);
    let gid = gid.map(rfs::Gid::from_raw);
    Ok(rfs::chownat(dir, at(rel), uid, gid, AtFlags::SYMLINK_NOFOLLOW)?)
}

pub const OMIT: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: rfs::UTIME_OMIT,
};
pub const NOW: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: rfs::UTIME_NOW,
};

pub fn set_times(dir: BorrowedFd, rel: &Path, atime: Timespec, mtime: Timespec) -> io::Result<()> {
    let ts = Timestamps {
        last_access: atime,
        last_modification: mtime,
    };
    Ok(rfs::utimensat(dir, at(rel), &ts, AtFlags::SYMLINK_NOFOLLOW)?)
}

pub fn timespec(t: SystemTime) -> Timespec {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    Timespec {
        tv_sec: d.as_secs() as i64,
        tv_nsec: d.subsec_nanos() as i64,
    }
}

/// Names and types in a directory, without `.` and `..`.
pub fn read_dir(dir: BorrowedFd, rel: &Path) -> io::Result<Vec<(OsString, FileType)>> {
    let fd = rfs::openat(
        dir,
        at(rel),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut out = Vec::new();
    for e in rfs::Dir::new(fd)? {
        let e = e?;
        let name = e.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let kind = match e.file_type() {
            rfs::FileType::Directory => FileType::Directory,
            rfs::FileType::Symlink => FileType::Symlink,
            rfs::FileType::Fifo => FileType::NamedPipe,
            rfs::FileType::Socket => FileType::Socket,
            rfs::FileType::BlockDevice => FileType::BlockDevice,
            rfs::FileType::CharacterDevice => FileType::CharDevice,
            _ => FileType::RegularFile,
        };
        out.push((OsStr::from_bytes(name).to_os_string(), kind));
    }
    Ok(out)
}

/// Whole-file copy of a regular file, symlink or directory from `src_dir/src` to `dst_dir/dst`,
/// keeping the exact mode (no umask) and times; `sync` fsyncs a copied file.
pub fn copy_entry(
    src_dir: BorrowedFd,
    src: &Path,
    dst_dir: BorrowedFd,
    dst: &Path,
    st: &Stat,
    sync: bool,
) -> io::Result<()> {
    let mode = st.st_mode & 0o7777;
    match st.st_mode & libc::S_IFMT {
        libc::S_IFDIR => match mkdir(dst_dir, dst, mode) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
            r => {
                r?;
                chmod(dst_dir, dst, mode)?;
            }
        },
        libc::S_IFLNK => symlink(&readlink(src_dir, src)?, dst_dir, dst)?,
        libc::S_IFREG => {
            let mut from = open(src_dir, src, OFlags::RDONLY, 0)?;
            let mut to = open(dst_dir, dst, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL, mode)?;
            io::copy(&mut from, &mut to)?;
            rfs::fchmod(&to, Mode::from_raw_mode(mode))?;
            if sync {
                to.sync_all()?;
            }
        }
        _ => return Err(io::Error::from_raw_os_error(libc::EPERM)),
    }
    let ts = |s: i64, ns: u64| Timespec {
        tv_sec: s,
        tv_nsec: ns as _,
    };
    set_times(
        dst_dir,
        dst,
        ts(st.st_atime, st.st_atime_nsec),
        ts(st.st_mtime, st.st_mtime_nsec),
    )
}

/// Flush the whole filesystem holding `dir` (the journal's barrier between steps).
/// `dir` may be an O_PATH fd, which syncfs refuses, so it reopens the directory.
pub fn syncfs(dir: BorrowedFd) -> io::Result<()> {
    let fd = rfs::openat(
        dir,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(rfs::syncfs(fd)?)
}

/// Start writing a file's dirty pages to disk without waiting for them.
pub fn start_writeback(f: &File) {
    use std::os::fd::AsRawFd;
    // SAFETY: a valid open fd; the call only starts writeback.
    unsafe { libc::sync_file_range(f.as_raw_fd(), 0, 0, libc::SYNC_FILE_RANGE_WRITE) };
}

/// `mkdir -p` below `dir`.
pub fn mkdirs(dir: BorrowedFd, rel: &Path) -> io::Result<()> {
    if rel.as_os_str().is_empty() || lstat(dir, rel).is_ok_and(|st| is_dir(&st)) {
        return Ok(());
    }
    mkdirs(dir, parent(rel))?;
    match mkdir(dir, rel, 0o755) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    }
}

pub fn depth(rel: &Path) -> usize {
    rel.components().count()
}

fn time(secs: i64, nsecs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs as u64, nsecs as u32)
    } else {
        UNIX_EPOCH
    }
}

pub fn kind(mode: u32) -> FileType {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => FileType::Directory,
        libc::S_IFLNK => FileType::Symlink,
        libc::S_IFIFO => FileType::NamedPipe,
        libc::S_IFSOCK => FileType::Socket,
        libc::S_IFBLK => FileType::BlockDevice,
        libc::S_IFCHR => FileType::CharDevice,
        _ => FileType::RegularFile,
    }
}

pub fn attr(ino: u64, st: &Stat) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
        size: st.st_size as u64,
        blocks: st.st_blocks as u64,
        atime: time(st.st_atime, st.st_atime_nsec as i64),
        mtime: time(st.st_mtime, st.st_mtime_nsec as i64),
        ctime: time(st.st_ctime, st.st_ctime_nsec as i64),
        crtime: time(st.st_ctime, st.st_ctime_nsec as i64),
        kind: kind(st.st_mode),
        perm: (st.st_mode & 0o7777) as u16,
        nlink: st.st_nlink as u32,
        uid: st.st_uid,
        gid: st.st_gid,
        rdev: st.st_rdev as u32,
        blksize: st.st_blksize as u32,
        flags: 0,
    }
}

/// The attributes of a negative entry: inode 0 tells the kernel the name does not exist.
pub fn absent() -> FileAttr {
    FileAttr {
        ino: INodeNo(0),
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::RegularFile,
        perm: 0,
        nlink: 0,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 0,
        flags: 0,
    }
}

/// The base version of a file: what the conflict check at commit compares (1.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Version {
    pub ino: u64,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl Version {
    pub fn of(st: &Stat) -> Self {
        let ns = |s: i64, n: i64| s.saturating_mul(1_000_000_000).saturating_add(n);
        Version {
            ino: st.st_ino,
            size: st.st_size,
            mtime_ns: ns(st.st_mtime, st.st_mtime_nsec as i64),
            ctime_ns: ns(st.st_ctime, st.st_ctime_nsec as i64),
        }
    }

    /// Same inode, size and mtime: what an editor's write or rename-over changes.
    /// Ignores ctime, which escrowd's own unlink of a hard link changes too.
    pub fn same_content(&self, other: &Version) -> bool {
        (self.ino, self.size, self.mtime_ns) == (other.ino, other.size, other.mtime_ns)
    }
}

pub fn statvfs(fd: impl AsFd) -> io::Result<rfs::StatVfs> {
    Ok(rfs::fstatvfs(fd)?)
}

pub fn path_bytes(p: &Path) -> &[u8] {
    p.as_os_str().as_bytes()
}

pub fn path_from(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes))
}
