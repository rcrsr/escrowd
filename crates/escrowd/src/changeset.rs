//! The change set of a closed scope: its net effect on the base, computed from
//! the upper tree, the whiteouts, the opaque directories and the pinned inodes.
//!
//! - An upper entry the base lacks is a create; one that differs from the base
//!   (type, mode, content or symlink target) is a modify; a copied-up entry left
//!   unchanged is no change.
//! - A whiteout is a delete of the base entry and everything under it; an
//!   opaque directory deletes the base children the scope did not recreate; a
//!   file or symlink in place of a base directory deletes what was under it.
//!
//! "The base" is the scope's snapshot: the project as it was when the scope opened.
//! - A create or modify whose pinned inode is a base inode, matched by a delete
//!   of the file that had that inode, is a rename.
//! - Paths under an `ephemeral` rule are never in it: a rename out of one is a
//!   create, a rename into one a delete.
//!
//! Each change lists its writers: the processes recorded at its path and, for a
//! rename, its source. A change with none recorded (a base entry deleted with its
//! directory) takes those of its nearest ancestor that has some.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};

use fuser::FileType;
use rustix::fs::{OFlags, Stat};

use crate::proc::Info;
use crate::review::Review;
use crate::roots::Access;
use crate::snapshot::Base;
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
    /// The root the path is in (`roots::PROJECT`, `HOME`, `TMP`).
    pub root: usize,
    pub path: PathBuf,
    /// Rename source.
    pub from: Option<PathBuf>,
    /// Ids of the processes that made the change (in `ChangeSet::procs`), sorted.
    pub writers: Vec<u64>,
}

pub struct ChangeSet {
    pub changes: Vec<Change>,
    /// (path, allowed): base reads the gate allowed, and reads it denied.
    pub reads: Vec<(PathBuf, bool)>,
    pub labels: HashMap<String, String>,
    /// The writers of every change and their recorded ancestors, by id.
    pub procs: BTreeMap<u64, Info>,
    /// Changes the unscoped mode saw while the scope was open (set by close).
    pub unscoped: u64,
    /// The close-time review (set by close and by `closed_change_set`; the default before).
    pub review: Review,
}

pub fn build(base: &Base, h: &ScopeHandle) -> io::Result<ChangeSet> {
    let store = h.store_read();
    if !h.captures() {
        return Ok(ChangeSet {
            changes: Vec::new(),
            reads: store.reads(),
            labels: store.labels()?,
            procs: BTreeMap::new(),
            unscoped: 0,
            review: Review::default(),
        });
    }
    drop(store);
    let upper = h.upper.as_fd();
    let mut kinds: BTreeMap<PathBuf, Kind> = BTreeMap::new();
    let mut deletes: BTreeSet<PathBuf> = BTreeSet::new();
    walk_upper(base, upper, Path::new(""), &mut kinds, &mut deletes)?;
    let store = h.store_read();
    for w in store.whiteouts() {
        if !kinds.contains_key(w) && base.lstat(w).is_ok() {
            deletes.insert(w.clone());
            base_descendants(base, w, &mut deletes)?;
        }
    }
    for dir in store.opaque_dirs() {
        let mut below = BTreeSet::new();
        base_descendants(base, dir, &mut below)?;
        deletes.extend(below.into_iter().filter(|p| sys::lstat(upper, p).is_err()));
    }
    for d in deletes {
        kinds.insert(d, Kind::Delete);
    }
    let ephemeral = |p: &Path| h.access(p) == Access::Ephemeral;
    let changes: Vec<Change> = pair_renames(base, &store, kinds)
        .into_iter()
        .filter_map(|mut c| {
            let from_ephemeral = c.from.as_deref().map(ephemeral);
            match (ephemeral(&c.path), from_ephemeral) {
                (true, Some(false)) => {
                    c.path = c.from.take()?;
                    c.kind = Kind::Delete;
                }
                (true, _) => return None,
                (false, Some(true)) => {
                    c.from = None;
                    c.kind = Kind::Create;
                }
                _ => {}
            }
            c.root = h.root;
            c.writers = writers(&store, &c);
            Some(c)
        })
        .collect();
    let mut procs = BTreeMap::new();
    for id in changes.iter().flat_map(|c| &c.writers) {
        let mut next = *id;
        while next != 0 && !procs.contains_key(&next) {
            let Some(p) = store.proc(next) else { break };
            procs.insert(next, p.clone());
            next = p.parent;
        }
    }
    Ok(ChangeSet {
        changes,
        reads: store.reads(),
        labels: store.labels()?,
        procs,
        unscoped: 0,
        review: Review::default(),
    })
}

/// The processes recorded at the change's paths, or at its nearest ancestor that has some.
fn writers(store: &ScopeStore, c: &Change) -> Vec<u64> {
    let mut out = BTreeSet::new();
    for p in std::iter::once(&c.path).chain(&c.from) {
        if let Some(w) = p.ancestors().find_map(|a| store.writers(a)) {
            out.extend(w);
        }
    }
    out.into_iter().collect()
}

/// Creates and modifies, pre-order, for every entry in the upper tree.
fn walk_upper(
    base: &Base,
    upper: BorrowedFd,
    dir: &Path,
    out: &mut BTreeMap<PathBuf, Kind>,
    deletes: &mut BTreeSet<PathBuf>,
) -> io::Result<()> {
    for (name, kind) in sys::read_dir(upper, dir)? {
        let rel = dir.join(&name);
        let up = sys::lstat(upper, &rel)?;
        match base.lstat(&rel) {
            Err(_) => {
                out.insert(rel.clone(), Kind::Create);
            }
            Ok(old) => {
                if differs(base, upper, &rel, &old, &up)? {
                    out.insert(rel.clone(), Kind::Modify);
                }
                if sys::is_dir(&old) && !sys::is_dir(&up) {
                    base_descendants(base, &rel, deletes)?;
                }
            }
        }
        if kind == FileType::Directory {
            walk_upper(base, upper, &rel, out, deletes)?;
        }
    }
    Ok(())
}

fn differs(base: &Base, upper: BorrowedFd, rel: &Path, old: &Stat, up: &Stat) -> io::Result<bool> {
    let fmt = |st: &Stat| st.st_mode & libc::S_IFMT;
    if fmt(old) != fmt(up) || old.st_mode & 0o7777 != up.st_mode & 0o7777 {
        return Ok(true);
    }
    match fmt(up) {
        libc::S_IFREG => {
            if old.st_size != up.st_size {
                return Ok(true);
            }
            let ours = sys::open(upper, rel, OFlags::RDONLY, 0)?;
            Ok(!same_bytes(base.open(rel, OFlags::RDONLY)?, ours)?)
        }
        libc::S_IFLNK => Ok(base.readlink(rel)? != sys::readlink(upper, rel)?),
        _ => Ok(false),
    }
}

pub(crate) fn same_bytes(mut a: impl Read, mut b: impl Read) -> io::Result<bool> {
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
fn base_descendants(base: &Base, dir: &Path, out: &mut BTreeSet<PathBuf>) -> io::Result<()> {
    let Ok(entries) = base.read_dir(dir) else {
        return Ok(()); // not a directory in the base
    };
    for (name, kind) in entries {
        let rel = dir.join(name);
        out.insert(rel.clone());
        if kind == FileType::Directory {
            base_descendants(base, &rel, out)?;
        }
    }
    Ok(())
}

/// Pair each create or modify that carries a base inode with the delete of the
/// file that had it: together they are one rename.
fn pair_renames(base: &Base, store: &ScopeStore, kinds: BTreeMap<PathBuf, Kind>) -> Vec<Change> {
    let base_ino_of_delete: HashMap<u64, PathBuf> = kinds
        .iter()
        .filter(|(_, k)| **k == Kind::Delete)
        .filter_map(|(p, _)| {
            let st = base.lstat(p).ok()?;
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
                root: 0,
                path: p.clone(),
                from: Some(from.clone()),
                writers: Vec::new(),
            },
            None => Change {
                kind: *k,
                root: 0,
                path: p.clone(),
                from: None,
                writers: Vec::new(),
            },
        })
        .collect()
}
