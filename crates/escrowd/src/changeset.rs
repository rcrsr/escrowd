//! The change set of a closed scope: its net effect on the base, computed from
//! the upper tree, the whiteouts, the opaque directories and the pinned inodes.
//!
//! - An upper entry the base lacks is a create; one that differs from the base
//!   (type, mode, content or symlink target) is a modify; a copied-up entry left
//!   unchanged is no change.
//! - A whiteout is a delete of the base entry and everything under it; an
//!   opaque directory deletes the base children the scope did not recreate.
//! - A create or modify whose pinned inode is a base inode, matched by a delete
//!   of the file that had that inode, is a rename.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};

use fuser::FileType;
use rustix::fs::{OFlags, Stat};

use crate::store::ScopeStore;
use crate::sys;
use crate::views::{ScopeHandle, UPPER_BIT};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Create,
    Modify,
    Delete,
    Rename,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub kind: Kind,
    pub path: PathBuf,
    /// Rename source.
    pub from: Option<PathBuf>,
}

pub struct ChangeSet {
    pub changes: Vec<Change>,
    /// (path, allowed): base reads the gate allowed, and reads it denied.
    pub reads: Vec<(PathBuf, bool)>,
    pub labels: HashMap<String, String>,
}

pub fn build(lower: BorrowedFd, h: &ScopeHandle) -> io::Result<ChangeSet> {
    let upper = h.upper.as_fd();
    let mut kinds: BTreeMap<PathBuf, Kind> = BTreeMap::new();
    walk_upper(lower, upper, Path::new(""), &mut kinds)?;
    let store = h.store();
    let mut deletes: BTreeSet<PathBuf> = BTreeSet::new();
    for w in store.whiteouts() {
        if !kinds.contains_key(w) && sys::lstat(lower, w).is_ok() {
            deletes.insert(w.clone());
            base_descendants(lower, w, &mut deletes)?;
        }
    }
    for dir in store.opaque_dirs() {
        let mut below = BTreeSet::new();
        base_descendants(lower, dir, &mut below)?;
        deletes.extend(below.into_iter().filter(|p| sys::lstat(upper, p).is_err()));
    }
    for d in deletes {
        kinds.insert(d, Kind::Delete);
    }
    let changes = pair_renames(lower, &store, kinds);
    Ok(ChangeSet {
        changes,
        reads: store.reads(),
        labels: store.labels()?,
    })
}

/// Creates and modifies, pre-order, for every entry in the upper tree.
fn walk_upper(lower: BorrowedFd, upper: BorrowedFd, dir: &Path, out: &mut BTreeMap<PathBuf, Kind>) -> io::Result<()> {
    for (name, kind) in sys::read_dir(upper, dir)? {
        let rel = dir.join(&name);
        let up = sys::lstat(upper, &rel)?;
        match sys::lstat(lower, &rel) {
            Err(_) => {
                out.insert(rel.clone(), Kind::Create);
            }
            Ok(base) => {
                if differs(lower, upper, &rel, &base, &up)? {
                    out.insert(rel.clone(), Kind::Modify);
                }
            }
        }
        if kind == FileType::Directory {
            walk_upper(lower, upper, &rel, out)?;
        }
    }
    Ok(())
}

fn differs(lower: BorrowedFd, upper: BorrowedFd, rel: &Path, base: &Stat, up: &Stat) -> io::Result<bool> {
    let fmt = |st: &Stat| st.st_mode & libc::S_IFMT;
    if fmt(base) != fmt(up) || base.st_mode & 0o7777 != up.st_mode & 0o7777 {
        return Ok(true);
    }
    match fmt(up) {
        libc::S_IFREG => {
            if base.st_size != up.st_size {
                return Ok(true);
            }
            let open = |d| sys::open(d, rel, OFlags::RDONLY, 0);
            Ok(!same_bytes(open(lower)?, open(upper)?)?)
        }
        libc::S_IFLNK => Ok(sys::readlink(lower, rel)? != sys::readlink(upper, rel)?),
        _ => Ok(false),
    }
}

fn same_bytes(mut a: impl Read, mut b: impl Read) -> io::Result<bool> {
    let (mut x, mut y) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let n = a.read(&mut x)?;
        if n == 0 {
            return Ok(b.read(&mut y[..1])? == 0);
        }
        let mut got = 0;
        while got < n {
            let m = b.read(&mut y[got..n])?;
            if m == 0 {
                return Ok(false);
            }
            got += m;
        }
        if x[..n] != y[..n] {
            return Ok(false);
        }
    }
}

/// Every base entry under `dir` (not `dir` itself).
fn base_descendants(lower: BorrowedFd, dir: &Path, out: &mut BTreeSet<PathBuf>) -> io::Result<()> {
    let Ok(entries) = sys::read_dir(lower, dir) else {
        return Ok(()); // not a directory in the base
    };
    for (name, kind) in entries {
        let rel = dir.join(name);
        out.insert(rel.clone());
        if kind == FileType::Directory {
            base_descendants(lower, &rel, out)?;
        }
    }
    Ok(())
}

/// Pair each create or modify that carries a base inode with the delete of the
/// file that had it: together they are one rename.
fn pair_renames(lower: BorrowedFd, store: &ScopeStore, kinds: BTreeMap<PathBuf, Kind>) -> Vec<Change> {
    let base_ino_of_delete: HashMap<u64, PathBuf> = kinds
        .iter()
        .filter(|(_, k)| **k == Kind::Delete)
        .filter_map(|(p, _)| {
            let st = sys::lstat(lower, p).ok()?;
            (!sys::is_dir(&st)).then(|| (st.st_ino, p.clone()))
        })
        .collect();
    let mut sources: BTreeMap<PathBuf, PathBuf> = BTreeMap::new(); // to -> from
    for (p, k) in &kinds {
        if !matches!(k, Kind::Create | Kind::Modify) {
            continue;
        }
        let Some(pin) = store.pin(p) else { continue };
        if pin & UPPER_BIT != 0 {
            continue;
        }
        if let Some(from) = base_ino_of_delete.get(&(pin & (UPPER_BIT - 1)))
            && from != p
            && !sources.values().any(|f| f == from)
        {
            sources.insert(p.clone(), from.clone());
        }
    }
    let renamed_from: BTreeSet<&PathBuf> = sources.values().collect();
    kinds
        .iter()
        .filter(|(p, _)| !renamed_from.contains(p))
        .map(|(p, k)| match sources.get(p) {
            Some(from) => Change {
                kind: Kind::Rename,
                path: p.clone(),
                from: Some(from.clone()),
            },
            None => Change {
                kind: *k,
                path: p.clone(),
                from: None,
            },
        })
        .collect()
}
