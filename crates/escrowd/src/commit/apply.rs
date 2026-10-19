//! Apply: journal, pre-images, then the removes and puts of each root's plan.

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use fuser::FileType;

use super::{Commits, Lowers, Part, Rooted, lower_of};
use crate::fault;
use crate::journal::{GenState, JournalEntry};
use crate::roots;
use crate::snapshot::Generations;
use crate::sys::{self, Version, parent};

/// An editor changed `.1` (in root `.0`) after the conflict check: the commit rolled back.
#[derive(Debug)]
pub(super) struct Raced(pub(super) usize, pub(super) PathBuf);

impl std::fmt::Display for Raced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} changed in the project during the commit", self.1.display())
    }
}

impl std::error::Error for Raced {}

impl Commits {
    pub(super) fn apply(
        &self,
        lowers: &Lowers,
        parts: &[Part],
        generation: u64,
        entries: &[JournalEntry],
        applied: &mut HashSet<Rooted>,
    ) -> io::Result<()> {
        fault::hit("journal", 0)?;
        let mut copied = [false; roots::COUNT];
        for (i, e) in entries.iter().enumerate() {
            fault::hit("preimage", i)?;
            let Some(m) = &e.pre else { continue };
            copied[e.root] = true;
            let (lower, root) = (lower_of(lowers, e.root)?, self.gens[e.root].root());
            let dst = Generations::rel(generation, &e.path);
            sys::mkdirs(root, parent(&dst))?;
            if m.is_dir() {
                sys::mkdirs(root, &dst)?;
            } else {
                sys::copy_entry(lower, &e.path, root, &dst, &sys::lstat(lower, &e.path)?, false)?;
            }
        }
        // The generations share the state's filesystem: one sync covers them all.
        if let Some(r) = copied.iter().position(|c| *c) {
            sys::syncfs(self.gens[r].root())?;
        }
        self.journal().set_state(generation, GenState::Prepared)?;
        self.register(generation, entries);
        let tmp: HashMap<(usize, &Path), &Path> = entries
            .iter()
            .filter_map(|e| Some(((e.root, e.path.as_path()), e.tmp.as_deref()?)))
            .collect();
        let pre: HashMap<(usize, &Path), Option<Version>> = entries
            .iter()
            .map(|e| ((e.root, e.path.as_path()), e.pre.map(|m| m.version())))
            .collect();
        let mut step = 0;
        for part in parts {
            let (lower, upper, plan, root) = (part.lower, part.h.upper.as_fd(), &part.plan, part.root);
            // Directories change as apply removes and adds their entries; files and links don't.
            let unchanged = |p: &Path, want: Option<Version>| -> io::Result<()> {
                let live = sys::lstat(lower, p).ok();
                if live.as_ref().is_some_and(sys::is_dir) {
                    return Ok(());
                }
                match (want, live.map(|st| Version::of(&st))) {
                    (None, None) => Ok(()),
                    (Some(w), Some(l)) if w.same_content(&l) => Ok(()),
                    _ => Err(io::Error::other(Raced(root, p.to_path_buf()))),
                }
            };
            let mut removed = HashSet::new();
            let sources: HashSet<&PathBuf> = plan.moves.values().collect();
            for p in &plan.removes {
                fault::hit("apply", step)?;
                fault::race(step, lower, p);
                step += 1;
                unchanged(p, pre[&(root, p.as_path())])?;
                let st = sys::lstat(lower, p)?;
                applied.insert((root, p.clone()));
                if sources.contains(p) {
                    sys::rename(lower, p, tmp[&(root, p.as_path())], 0)?;
                } else {
                    sys::unlink(lower, p, sys::is_dir(&st))?;
                }
                removed.insert(p);
            }
            for (p, up) in &plan.puts {
                fault::hit("apply", step)?;
                fault::race(step, lower, p);
                step += 1;
                let want = if removed.contains(p) {
                    None
                } else {
                    pre[&(root, p.as_path())]
                };
                unchanged(p, want)?;
                applied.insert((root, p.clone()));
                if sys::is_dir(up) {
                    match sys::lstat(lower, p) {
                        Ok(_) => sys::chmod(lower, p, up.st_mode)?,
                        Err(_) => sys::copy_entry(upper, p, lower, p, up, false)?,
                    }
                } else if let Some(from) = plan.moves.get(p) {
                    sys::rename(lower, tmp[&(root, from.as_path())], p, 0)?;
                } else {
                    let t = tmp[&(root, p.as_path())];
                    // A new file keeps its inode: link it out of the upper, which a rollback
                    // still needs; copy across filesystems.
                    if !(sys::kind(up.st_mode) == FileType::RegularFile && sys::link_across(upper, p, lower, t).is_ok())
                    {
                        sys::copy_entry(upper, p, lower, t, up, false)?;
                    }
                    sys::rename(lower, t, p, 0)?;
                }
            }
        }
        for part in parts.iter().filter(|p| !p.plan.touched().is_empty()) {
            sys::syncfs(part.lower)?;
        }
        fault::hit("done", 0)?;
        self.journal().set_state(generation, GenState::Done)?;
        self.current.store(generation, Ordering::Release);
        Ok(())
    }
}
