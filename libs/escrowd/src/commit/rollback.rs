//! Rollback: undo an unfinished generation from its pre-images.

use std::collections::{BTreeSet, HashSet};
use std::io;
use std::os::fd::BorrowedFd;
use std::path::Path;

use super::{Commits, Lowers, Rooted, lower_of};
use crate::fault;
use crate::journal::{DirTimes, GenState, JournalEntry};
use crate::snapshot::{Generations, Meta};
use crate::sys::{self, Version};

/// What an in-process rollback after a race undoes: the paths apply reached, except
/// the raced path and its descendants, which keep the editor's version.
#[derive(Clone, Copy)]
pub(super) struct Reached<'a> {
    pub(super) applied: &'a HashSet<Rooted>,
    pub(super) raced: (usize, &'a Path),
}

/// Missing, or under a parent that is no longer a directory (an editor replaced it).
fn ignore_missing(r: io::Result<()>) -> io::Result<()> {
    match r {
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => Ok(()),
        r => r,
    }
}

impl Commits {
    /// Undo an unfinished generation: restore each root's base from its pre-images
    /// (once prepared), remove temporary files, then forget the generation.
    /// `only`: what apply reached (an in-process rollback after a race); None after
    /// a crash, when only versions tell what apply reached.
    pub(super) fn rollback(&self, lowers: &Lowers, generation: u64, only: Option<Reached>) -> io::Result<()> {
        let (state, entries, dirs) = {
            let j = self.journal();
            (j.state(generation)?, j.entries(generation)?, j.dirs(generation)?)
        };
        let used: BTreeSet<usize> = entries.iter().map(|e| e.root).collect();
        for &root in &used {
            let lower = lower_of(lowers, root)?;
            let entries: Vec<&JournalEntry> = entries.iter().filter(|e| e.root == root).collect();
            if state == Some(GenState::Prepared) {
                let dirs: Vec<&DirTimes> = dirs.iter().filter(|d| d.root == root).collect();
                self.restore(lower, root, generation, &entries, &dirs, only)?;
            } else {
                for t in entries.iter().filter_map(|e| e.tmp.as_ref()) {
                    ignore_missing(sys::unlink(lower, t, false))?;
                }
            }
            sys::syncfs(lower)?;
        }
        // Forget first: a crash before the files are gone leaves only files, which start sweeps.
        self.journal().forget(generation)?;
        self.unregister(generation)
    }

    fn restore(
        &self,
        lower: BorrowedFd,
        root: usize,
        generation: u64,
        entries: &[&JournalEntry],
        dirs: &[&DirTimes],
        only: Option<Reached>,
    ) -> io::Result<()> {
        let fmt = |mode: u32| mode & libc::S_IFMT;
        let reached = |e: &JournalEntry| {
            only.is_none_or(|r| {
                r.applied.contains(&(root, e.path.clone())) && !(r.raced.0 == root && e.path.starts_with(r.raced.1))
            })
        };
        let mut order: Vec<&JournalEntry> = entries.iter().copied().filter(|e| reached(e)).collect();
        order.sort_by_key(|e| std::cmp::Reverse(sys::depth(&e.path)));
        for t in entries.iter().filter_map(|e| e.tmp.as_ref()) {
            ignore_missing(sys::unlink(lower, t, false))?;
        }
        // Deepest first: remove what the commit put in place; keep originals it never reached.
        // A directory the commit created that now holds an editor's file stays, with the file.
        for e in &order {
            let Ok(live) = sys::lstat(lower, &e.path) else { continue };
            let keep = e.pre.is_some_and(|m| {
                (m.is_dir() && sys::is_dir(&live))
                    || (fmt(m.mode) == fmt(live.st_mode) && m.version() == Version::of(&live))
            });
            if !keep {
                match sys::unlink(lower, &e.path, sys::is_dir(&live)) {
                    Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => {}
                    r => r?,
                }
            }
        }
        // Parents first: put the originals back.
        let pre_root = self.gens[root].root();
        for (i, e) in order.iter().rev().enumerate() {
            let Some(m) = &e.pre else { continue };
            fault::hit("restore", i)?;
            match sys::lstat(lower, &e.path) {
                Ok(live) if sys::is_dir(&live) => sys::chmod(lower, &e.path, m.mode)?,
                Ok(_) => {}
                Err(_) => {
                    let src = Generations::rel(generation, &e.path);
                    let mut st = sys::lstat(pre_root, &src)?;
                    m.apply(&mut st);
                    sys::copy_entry(pre_root, &src, lower, &e.path, &st, true)?;
                }
            }
        }
        let set = |p: &Path, atime_ns, mtime_ns| {
            if sys::lstat(lower, p).is_ok() {
                sys::set_times(lower, p, Meta::timespec(atime_ns), Meta::timespec(mtime_ns))?;
            }
            Ok::<_, io::Error>(())
        };
        for e in entries.iter().filter(|e| reached(e)) {
            if let Some(m) = e.pre.filter(Meta::is_dir) {
                set(&e.path, m.atime_ns, m.mtime_ns)?;
            }
        }
        for d in dirs {
            set(&d.path, d.atime_ns, d.mtime_ns)?;
        }
        // A copy has a new inode and ctime: scopes that recorded the original must not conflict.
        let journal = self.journal();
        let mut aliases = self.aliases.lock();
        for e in entries.iter().filter(|e| reached(e)) {
            let Some(m) = e.pre else { continue };
            if let Ok(now) = sys::lstat(lower, &e.path).map(|st| Version::of(&st))
                && now != m.version()
            {
                journal.add_restored(root, &e.path, m.version(), now)?;
                aliases.insert((root, e.path.clone(), m.version()), now);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::fs;
    use std::os::fd::AsFd;
    use std::path::PathBuf;

    fn entry(path: &str, pre: Option<Meta>) -> JournalEntry {
        JournalEntry {
            root: 0,
            path: PathBuf::from(path),
            pre,
            tmp: None,
        }
    }

    /// A file `name` in `lower` holding "orig", its pre-image in generation 1, then "new" in its place.
    fn replaced(pre_root: &Path, lower: &Path, name: &str) -> Meta {
        let orig = lower.join(name);
        fs::write(&orig, b"orig").unwrap();
        let fd = sys::open_dir(lower).unwrap();
        let meta = Meta::of(&sys::lstat(fd.as_fd(), Path::new(name)).unwrap());
        let pre = pre_root.join(Generations::rel(1, Path::new(name)));
        fs::create_dir_all(pre.parent().unwrap()).unwrap();
        fs::copy(&orig, &pre).unwrap();
        fs::remove_file(&orig).unwrap();
        fs::write(&orig, b"new!!").unwrap();
        meta
    }

    #[test]
    fn restore_after_a_race_undoes_only_what_apply_reached() {
        let dir = db::temp_dir("rollback-race");
        let state = dir.join("state");
        fs::create_dir_all(&state).unwrap();
        let commits = Commits::open(&state).unwrap();
        let lower = dir.join("lower");
        fs::create_dir_all(lower.join("rd")).unwrap();
        fs::write(lower.join("rd/x"), b"editor").unwrap();
        let reached_meta = replaced(&dir.join("state/generations"), &lower, "re.txt");
        let unreached_meta = replaced(&dir.join("state/generations"), &lower, "un.txt");
        let lfd = sys::open_dir(&lower).unwrap();
        let entries = [
            entry("rd", None),
            entry("rd/x", None),
            entry("re.txt", Some(reached_meta)),
            entry("un.txt", Some(unreached_meta)),
        ];
        let refs: Vec<&JournalEntry> = entries.iter().collect();
        let applied: HashSet<Rooted> = ["rd", "rd/x", "re.txt"].iter().map(|p| (0, PathBuf::from(p))).collect();
        let only = Reached {
            applied: &applied,
            raced: (0, Path::new("rd")),
        };

        commits.restore(lfd.as_fd(), 0, 1, &refs, &[], Some(only)).unwrap();

        assert_eq!(fs::read(lower.join("rd/x")).unwrap(), b"editor");
        assert_eq!(fs::read(lower.join("un.txt")).unwrap(), b"new!!");
        assert_eq!(fs::read(lower.join("re.txt")).unwrap(), b"orig");
        let now = Version::of(&sys::lstat(lfd.as_fd(), Path::new("re.txt")).unwrap());
        let aliases = commits.aliases.lock();
        assert_eq!(
            aliases.get(&(0, PathBuf::from("re.txt"), reached_meta.version())),
            Some(&now)
        );
        assert!(!aliases.contains_key(&(0, PathBuf::from("un.txt"), unreached_meta.version())));
        drop(aliases);
        fs::remove_dir_all(&dir).unwrap();
    }
}
