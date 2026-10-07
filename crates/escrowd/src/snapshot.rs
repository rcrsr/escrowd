//! Base generations: the pre-image layer that keeps each open scope's snapshot of the base.
//!
//! Commit N copies every base entry it is about to overwrite or delete to
//! `<state>/generations/<N>/<path>` and marks every path it creates absent. A
//! scope opened at generation `since` resolves a path through the first
//! generation after `since` that recorded it, else through the live base, so it
//! keeps seeing the base as it was when it opened. A commit records every path
//! it touches (a deleted directory's descendants included), so per-path
//! resolution is complete. Pre-images keep the original's inode number, size,
//! mode and times, so inode numbers and recorded versions stay stable.

use parking_lot::RwLock;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use fuser::FileType;
use rustix::fs::{OFlags, Stat};

use crate::sys::{self, Version, parent};

/// What a pre-image records of the original entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub mode: u32,
    pub ino: u64,
    pub size: i64,
    pub uid: u32,
    pub gid: u32,
    pub atime_ns: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

const NS: i64 = 1_000_000_000;

fn split(ns: i64) -> (i64, i64) {
    (ns.div_euclid(NS), ns.rem_euclid(NS))
}

impl Meta {
    pub fn of(st: &Stat) -> Self {
        let ns = |s: i64, n: i64| s.saturating_mul(NS).saturating_add(n);
        Meta {
            mode: st.st_mode,
            ino: st.st_ino,
            size: st.st_size,
            uid: st.st_uid,
            gid: st.st_gid,
            atime_ns: ns(st.st_atime, st.st_atime_nsec as i64),
            mtime_ns: ns(st.st_mtime, st.st_mtime_nsec as i64),
            ctime_ns: ns(st.st_ctime, st.st_ctime_nsec as i64),
        }
    }

    pub fn version(&self) -> Version {
        Version {
            ino: self.ino,
            size: self.size,
            mtime_ns: self.mtime_ns,
            ctime_ns: self.ctime_ns,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFDIR
    }

    pub fn timespec(ns: i64) -> rustix::fs::Timespec {
        let (s, n) = split(ns);
        rustix::fs::Timespec { tv_sec: s, tv_nsec: n }
    }

    /// The original's identity over the stat of its copy.
    pub fn apply(&self, st: &mut Stat) {
        st.st_mode = self.mode;
        st.st_ino = self.ino;
        st.st_size = self.size;
        st.st_uid = self.uid;
        st.st_gid = self.gid;
        let (s, n) = split(self.atime_ns);
        (st.st_atime, st.st_atime_nsec) = (s, n as _);
        let (s, n) = split(self.mtime_ns);
        (st.st_mtime, st.st_mtime_nsec) = (s, n as _);
        let (s, n) = split(self.ctime_ns);
        (st.st_ctime, st.st_ctime_nsec) = (s, n as _);
    }
}

/// One path a generation recorded: its pre-image, or None where the commit created it.
pub type Entry = Option<Meta>;

#[derive(Default)]
struct Index {
    paths: HashMap<PathBuf, BTreeMap<u64, Entry>>,
    children: HashMap<PathBuf, BTreeSet<OsString>>,
    latest: u64,
}

pub struct Generations {
    dir: PathBuf,
    root: OwnedFd,
    index: RwLock<Index>,
}

impl Generations {
    pub fn open(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        Ok(Generations {
            dir: dir.to_path_buf(),
            root: sys::open_dir(dir)?,
            index: RwLock::new(Index::default()),
        })
    }

    pub fn root(&self) -> BorrowedFd<'_> {
        self.root.as_fd()
    }

    /// Where generation `generation` keeps the pre-image of `rel`, relative to `root()`.
    pub fn rel(generation: u64, rel: &Path) -> PathBuf {
        Path::new(&generation.to_string()).join(rel)
    }

    /// Make a generation's pre-images visible to the scopes opened before it.
    pub fn register(&self, generation: u64, entries: impl IntoIterator<Item = (PathBuf, Entry)>) {
        let mut ix = self.index.write();
        for (p, e) in entries {
            if let Some(name) = p.file_name() {
                ix.children
                    .entry(parent(&p).to_path_buf())
                    .or_default()
                    .insert(name.to_os_string());
            }
            ix.paths.entry(p).or_default().insert(generation, e);
        }
        ix.latest = ix.latest.max(generation);
    }

    pub fn unregister(&self, generation: u64) {
        let mut ix = self.index.write();
        let mut emptied = Vec::new();
        for (p, gens) in ix.paths.iter_mut() {
            if gens.remove(&generation).is_some() && gens.is_empty() {
                emptied.push(p.clone());
            }
        }
        for p in emptied {
            ix.paths.remove(&p);
            let dir = parent(&p).to_path_buf();
            if let (Some(name), Some(names)) = (p.file_name(), ix.children.get_mut(&dir)) {
                names.remove(name);
                if names.is_empty() {
                    ix.children.remove(&dir);
                }
            }
        }
        ix.latest = ix
            .paths
            .values()
            .filter_map(|g| g.keys().next_back())
            .copied()
            .max()
            .unwrap_or(0);
    }

    /// Delete a generation's pre-image files.
    /// Remove generation directories the journal no longer names: a crash between
    /// forgetting a generation and removing its files leaves them behind.
    pub fn sweep(&self, keep: &std::collections::HashSet<u64>) -> io::Result<()> {
        for e in fs::read_dir(&self.dir)? {
            let e = e?;
            if let Some(g) = e.file_name().to_str().and_then(|n| n.parse::<u64>().ok())
                && !keep.contains(&g)
            {
                self.remove_files(g)?;
            }
        }
        Ok(())
    }

    pub fn remove_files(&self, generation: u64) -> io::Result<()> {
        match fs::remove_dir_all(self.dir.join(generation.to_string())) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn resolve(&self, since: u64, rel: &Path) -> Option<(u64, Entry)> {
        let ix = self.index.read();
        if since >= ix.latest {
            return None;
        }
        let (g, e) = ix.paths.get(rel)?.range(since + 1..).next()?;
        Some((*g, *e))
    }

    /// Names recorded below `rel` by generations after `since`.
    fn children(&self, since: u64, rel: &Path) -> Vec<OsString> {
        let ix = self.index.read();
        if since >= ix.latest {
            return Vec::new();
        }
        ix.children
            .get(rel)
            .map(|n| n.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// A scope's view of the base: the live project overlaid with the pre-images
/// of every generation committed after the scope opened.
pub struct Base<'a> {
    pub lower: BorrowedFd<'a>,
    gens: &'a Generations,
    since: u64,
}

fn enoent() -> io::Error {
    io::Error::from_raw_os_error(libc::ENOENT)
}

impl<'a> Base<'a> {
    pub fn new(lower: BorrowedFd<'a>, gens: &'a Generations, since: u64) -> Self {
        Base { lower, gens, since }
    }

    /// Where the bytes of `rel` live: the live base or a generation's copy.
    pub fn src(&self, rel: &Path) -> io::Result<(BorrowedFd<'a>, PathBuf)> {
        match self.gens.resolve(self.since, rel) {
            None => Ok((self.lower, rel.to_path_buf())),
            Some((_, None)) => Err(enoent()),
            Some((g, Some(_))) => Ok((self.gens.root(), Generations::rel(g, rel))),
        }
    }

    pub fn lstat(&self, rel: &Path) -> io::Result<Stat> {
        match self.gens.resolve(self.since, rel) {
            None => sys::lstat(self.lower, rel),
            Some((_, None)) => Err(enoent()),
            Some((g, Some(m))) => {
                let mut st = sys::lstat(self.gens.root(), &Generations::rel(g, rel))?;
                m.apply(&mut st);
                Ok(st)
            }
        }
    }

    pub fn open(&self, rel: &Path, flags: OFlags) -> io::Result<File> {
        let (fd, p) = self.src(rel)?;
        sys::open(fd, &p, flags, 0)
    }

    pub fn readlink(&self, rel: &Path) -> io::Result<PathBuf> {
        let (fd, p) = self.src(rel)?;
        sys::readlink(fd, &p)
    }

    pub fn read_dir(&self, rel: &Path) -> io::Result<Vec<(OsString, FileType)>> {
        let resolved = self.gens.resolve(self.since, rel);
        let extra = self.gens.children(self.since, rel);
        match resolved {
            None if extra.is_empty() => return sys::read_dir(self.lower, rel),
            Some((_, None)) => return Err(enoent()),
            Some((_, Some(m))) if !m.is_dir() => return Err(io::Error::from_raw_os_error(libc::ENOTDIR)),
            _ => {}
        }
        let live = match (sys::read_dir(self.lower, rel), resolved) {
            (Err(e), None) => return Err(e),
            (live, _) => live.unwrap_or_default(),
        };
        let mut names: BTreeSet<OsString> = live.into_iter().map(|(n, _)| n).collect();
        names.extend(extra);
        Ok(names
            .into_iter()
            .filter_map(|n| {
                let st = self.lstat(&rel.join(&n)).ok()?;
                Some((n, sys::kind(st.st_mode)))
            })
            .collect())
    }
}
