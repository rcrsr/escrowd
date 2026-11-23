//! The plan of one root's commit: what to remove, put and move, in apply order.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};

use fuser::FileType;
use rustix::fs::{OFlags, Stat};

use crate::changeset::{self, Change, Kind};
use crate::sys;

/// What a commit does to the base, in order.
pub(super) struct Plan {
    /// Deepest first: deleted entries, rename sources, entries replaced by another type of entry.
    pub(super) removes: Vec<PathBuf>,
    /// Parents first, with the upper entry's stat.
    pub(super) puts: Vec<(PathBuf, Stat)>,
    /// Destination → source of each rename of a base file or symlink the scope left
    /// unchanged: apply renames the original, so it keeps its inode.
    pub(super) moves: HashMap<PathBuf, PathBuf>,
}

impl Plan {
    pub(super) fn new<'c>(
        lower: BorrowedFd,
        upper: BorrowedFd,
        changes: impl Iterator<Item = &'c Change>,
    ) -> io::Result<Self> {
        let mut removes = BTreeSet::new();
        let mut puts = BTreeSet::new();
        let mut moves = HashMap::new();
        for c in changes {
            match c.kind {
                Kind::Delete => {
                    removes.insert(c.path.clone());
                }
                Kind::Rename => {
                    let from = c.from.clone().unwrap_or_default();
                    if unchanged_copy(lower, &from, upper, &c.path)? {
                        moves.insert(c.path.clone(), from.clone());
                    }
                    removes.insert(from);
                    puts.insert(c.path.clone());
                }
                Kind::Create | Kind::Modify => {
                    puts.insert(c.path.clone());
                }
            }
        }
        let puts = puts
            .into_iter()
            .map(|p| Ok((sys::lstat(upper, &p)?, p)))
            .map(|r: io::Result<_>| r.map(|(st, p)| (p, st)))
            .collect::<io::Result<Vec<_>>>()?;
        for (p, up) in &puts {
            if let Ok(live) = sys::lstat(lower, p)
                && sys::is_dir(&live) != sys::is_dir(up)
            {
                removes.insert(p.clone());
            }
        }
        let mut removes: Vec<PathBuf> = removes.into_iter().collect();
        removes.sort_by_key(|p| std::cmp::Reverse(sys::depth(p)));
        Ok(Plan { removes, puts, moves })
    }

    pub(super) fn touched(&self) -> BTreeSet<PathBuf> {
        self.removes
            .iter()
            .cloned()
            .chain(self.puts.iter().map(|(p, _)| p.clone()))
            .collect()
    }
}

/// The upper's `to` is the base's `from` as it was: same type, mode, mtime and bytes.
fn unchanged_copy(lower: BorrowedFd, from: &Path, upper: BorrowedFd, to: &Path) -> io::Result<bool> {
    let (Ok(old), Ok(new)) = (sys::lstat(lower, from), sys::lstat(upper, to)) else {
        return Ok(false);
    };
    if (old.st_mode, old.st_size, old.st_mtime, old.st_mtime_nsec)
        != (new.st_mode, new.st_size, new.st_mtime, new.st_mtime_nsec)
    {
        return Ok(false);
    }
    match sys::kind(new.st_mode) {
        FileType::RegularFile => changeset::same_bytes(
            sys::open(lower, from, OFlags::RDONLY, 0)?,
            sys::open(upper, to, OFlags::RDONLY, 0)?,
        ),
        FileType::Symlink => Ok(sys::readlink(lower, from)? == sys::readlink(upper, to)?),
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::fs;
    use std::os::fd::AsFd;
    use std::time::{Duration, SystemTime};

    fn change(kind: Kind, path: &str, from: Option<&str>) -> Change {
        Change {
            kind,
            root: 0,
            path: PathBuf::from(path),
            from: from.map(PathBuf::from),
            writers: Vec::new(),
        }
    }

    fn plan(name: &str, setup: impl FnOnce(&Path, &Path), changes: &[Change]) -> Plan {
        let dir = db::temp_dir(name);
        let (lower, upper) = (dir.join("lower"), dir.join("upper"));
        fs::create_dir_all(&lower).unwrap();
        fs::create_dir_all(&upper).unwrap();
        setup(&lower, &upper);
        let (lfd, ufd) = (sys::open_dir(&lower).unwrap(), sys::open_dir(&upper).unwrap());
        let plan = Plan::new(lfd.as_fd(), ufd.as_fd(), changes.iter()).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        plan
    }

    fn write_at(path: &Path, bytes: &[u8], mtime: SystemTime) {
        fs::write(path, bytes).unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    #[test]
    fn removes_deepest_first() {
        let changes = [
            change(Kind::Delete, "a", None),
            change(Kind::Delete, "a/b/c", None),
            change(Kind::Delete, "a/b", None),
        ];
        let p = plan("plan-removes", |_, _| {}, &changes);
        let want: Vec<PathBuf> = ["a/b/c", "a/b", "a"].iter().map(PathBuf::from).collect();
        assert_eq!(p.removes, want);
    }

    #[test]
    fn puts_parents_first() {
        let changes = [change(Kind::Create, "d/f", None), change(Kind::Create, "d", None)];
        let p = plan(
            "plan-puts",
            |_, upper| {
                fs::create_dir(upper.join("d")).unwrap();
                fs::write(upper.join("d/f"), b"x").unwrap();
            },
            &changes,
        );
        let got: Vec<&Path> = p.puts.iter().map(|(p, _)| p.as_path()).collect();
        assert_eq!(got, [Path::new("d"), Path::new("d/f")]);
    }

    #[test]
    fn a_type_change_adds_a_remove() {
        let changes = [change(Kind::Create, "x", None)];
        let p = plan(
            "plan-type-change",
            |lower, upper| {
                fs::create_dir(lower.join("x")).unwrap();
                fs::write(upper.join("x"), b"file").unwrap();
            },
            &changes,
        );
        assert_eq!(p.removes, [PathBuf::from("x")]);
        assert_eq!(p.puts.len(), 1);
    }

    #[test]
    fn an_unchanged_rename_becomes_a_move() {
        let changes = [change(Kind::Rename, "new", Some("old"))];
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let p = plan(
            "plan-rename-unchanged",
            |lower, upper| {
                write_at(&lower.join("old"), b"same", mtime);
                write_at(&upper.join("new"), b"same", mtime);
            },
            &changes,
        );
        assert_eq!(p.moves.get(Path::new("new")), Some(&PathBuf::from("old")));
        assert_eq!(p.removes, [PathBuf::from("old")]);
    }

    #[test]
    fn a_modified_rename_is_not_a_move() {
        let changes = [change(Kind::Rename, "new", Some("old"))];
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let p = plan(
            "plan-rename-modified",
            |lower, upper| {
                write_at(&lower.join("old"), b"same", mtime);
                write_at(&upper.join("new"), b"diff", mtime);
            },
            &changes,
        );
        assert!(p.moves.is_empty());
        assert_eq!(p.removes, [PathBuf::from("old")]);
    }
}
