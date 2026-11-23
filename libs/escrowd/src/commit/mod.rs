//! Commit: apply a closed scope's change set to the project (and to the other
//! roots it captured: `$HOME`, `/tmp`), all or nothing.
//!
//! Commits are serialized per project. For one scope, over every root at once:
//!
//! 1. **Conflict check**: every path the commit touches must still be as the
//!    scope first saw it (the version it recorded, else its snapshot's), and
//!    the parents of new entries must still be directories. With the policy's
//!    `conflict.reads`, so must every captured file the scope only read. Any
//!    mismatch is a conflict and nothing is written.
//! 2. **Journal**: the generation's intent (each path, the original's metadata,
//!    each new file's temporary name) is on disk before anything changes.
//! 3. **Pre-images**: each base entry about to be overwritten or deleted is
//!    copied into the generation; then the journal says `prepared`.
//! 4. **Apply**: deletes deepest first, then new entries parents first; a new
//!    file is linked (or, across filesystems, copied) from the upper beside its
//!    target and renamed over it; a base file the scope only renamed is renamed,
//!    so it keeps its inode (it waits under a temporary name at the root while
//!    the removes run).
//! 5. **Done**: the journal says `done`; the caller drops the scope.
//!
//! A failure at any step rolls the applied part back from the pre-images; an
//! unfinished generation found at start is rolled back the same way.
//!
//! An editor outside escrowd can write a file between the conflict check and
//! the apply. Apply re-checks each file's inode, size and mtime against the
//! journal just before it removes or replaces it; a mismatch rolls back what was
//! applied (leaving the editor's file alone) and reports a conflict. What
//! remains is the window between that re-check and the rename.
//!
//! Durability: the journal commits each state change with `synchronous = FULL`;
//! `syncfs` makes the pre-images durable before `prepared` and the applied
//! files durable before `done`. Any crash before `done` rolls back, so applied
//! files need no fsync of their own. Cleanup forgets a generation before it
//! removes the generation's files, and start removes files no generation names.

mod apply;
mod conflict;
mod plan;
mod rollback;

use parking_lot::{Mutex, MutexGuard};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::changeset::ChangeSet;
use crate::journal::{DirTimes, GenState, Journal, JournalEntry};
use crate::roots;
use crate::snapshot::{Base, Generations, Meta};
use crate::sys::{self, Version, parent};
use crate::views::ScopeHandle;

use apply::Raced;
use plan::Plan;
use rollback::Reached;

/// A path in one root.
type Rooted = (usize, PathBuf);

/// The base directory of each root, by root index (None: the host has none).
pub type Lowers<'a> = [Option<BorrowedFd<'a>>; roots::COUNT];

/// Shows a root's path to the caller (`~/x` for `$HOME`).
pub type Show<'a> = &'a dyn Fn(usize, &Path) -> PathBuf;

fn lower_of<'a>(lowers: &Lowers<'a>, root: usize) -> io::Result<BorrowedFd<'a>> {
    lowers[root].ok_or_else(|| io::Error::other(format!("root {root} has no base directory")))
}

/// A failed commit whose rollback failed too: the base may be partly applied until
/// the next start rolls it back from the journal.
#[derive(Debug)]
struct RollbackFailed(String);

impl std::fmt::Display for RollbackFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RollbackFailed {}

/// `e`, from `Commits::commit`, is a failure whose rollback failed: unlike any other
/// error there, the scope cannot simply be decided again.
pub fn rollback_failed(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|r| r.is::<RollbackFailed>())
}

pub enum Outcome {
    /// The generation the commit created (None for an empty change set) and the changed paths.
    Committed(Option<u64>, Vec<PathBuf>),
    /// Paths that changed in the project since the scope saw them; nothing was written.
    Conflict(Vec<PathBuf>),
}

/// One root of a commit: its base, the scope's view of it and what to do there.
struct Part<'a> {
    root: usize,
    lower: BorrowedFd<'a>,
    h: &'a ScopeHandle,
    plan: Plan,
}

pub struct Commits {
    /// Pre-images, by root index.
    pub gens: Vec<Generations>,
    journal: Mutex<Journal>,
    /// (root, path, original version) → the version a rollback left in its place.
    aliases: Mutex<HashMap<(usize, PathBuf, Version), Version>>,
    serial: Mutex<()>,
    current: AtomicU64,
}

impl Commits {
    pub fn open(state_dir: &Path) -> io::Result<Self> {
        let journal = Journal::open(&state_dir.join("journal.sqlite"))?;
        let aliases = journal
            .restored()?
            .into_iter()
            .map(|(r, p, old, new)| ((r, p, old), new))
            .collect();
        // The project's pre-images keep the directory they had before roots.
        let dirs = ["generations", "generations-home", "generations-tmp"];
        Ok(Commits {
            gens: dirs
                .iter()
                .map(|d| Generations::open(&state_dir.join(d)))
                .collect::<io::Result<_>>()?,
            current: AtomicU64::new(journal.current()?),
            journal: Mutex::new(journal),
            aliases: Mutex::new(aliases),
            serial: Mutex::new(()),
        })
    }

    fn journal(&self) -> MutexGuard<'_, Journal> {
        self.journal.lock()
    }

    /// The generation new scopes open at.
    pub fn current(&self) -> u64 {
        self.current.load(Ordering::Acquire)
    }

    /// On start: roll back unfinished commits and load the finished generations.
    /// Returns the scopes whose commit finished but which were not dropped yet.
    pub fn recover(&self, lowers: &Lowers) -> io::Result<Vec<(u64, String)>> {
        let _serial = self.serial.lock();
        let mut finished = Vec::new();
        let gens = self.journal().gens()?;
        for (g, scope, state) in gens {
            if state != GenState::Done {
                eprintln!("escrowd: rolling back unfinished commit (generation {g}, scope {scope})");
                self.rollback(lowers, g, None)?;
                continue;
            }
            let entries = self.journal().entries(g)?;
            self.register(g, &entries);
            if !scope.is_empty() {
                finished.push((g, scope));
            }
        }
        let known = self.journal().gens()?.into_iter().map(|(g, _, _)| g).collect();
        for gens in &self.gens {
            gens.sweep(&known)?;
        }
        Ok(finished)
    }

    /// Make a generation's pre-images visible, each in its root.
    fn register(&self, generation: u64, entries: &[JournalEntry]) {
        for (root, gens) in self.gens.iter().enumerate() {
            gens.register(
                generation,
                entries
                    .iter()
                    .filter(|e| e.root == root)
                    .map(|e| (e.path.clone(), e.pre)),
            );
        }
    }

    fn unregister(&self, generation: u64) -> io::Result<()> {
        for gens in &self.gens {
            gens.unregister(generation);
            gens.remove_files(generation)?;
        }
        Ok(())
    }

    /// The scope of a finished generation is gone; recovery must not drop a later scope of the same id.
    pub fn scope_dropped(&self, generation: u64) -> io::Result<()> {
        self.journal().clear_scope(generation)
    }

    /// Commit `cs` (built from the views `hs` of one scope, one per root); `reads`: files
    /// the scope only read conflict too; `show` names paths in the outcome.
    pub fn commit(
        &self,
        lowers: &Lowers,
        hs: &[&ScopeHandle],
        cs: &ChangeSet,
        reads: bool,
        show: Show,
    ) -> io::Result<Outcome> {
        let _serial = self.serial.lock();
        let mut parts = Vec::new();
        for h in hs {
            let lower = lower_of(lowers, h.root)?;
            let changes = cs.changes.iter().filter(|c| c.root == h.root);
            let plan = Plan::new(lower, h.upper.as_fd(), changes)?;
            parts.push(Part {
                root: h.root,
                lower,
                h,
                plan,
            });
        }
        let mut conflicts = Vec::new();
        for part in &parts {
            let base = Base::new(part.lower, &self.gens[part.root], part.h.since);
            conflicts.extend(self.conflicts(&base, part, reads)?.iter().map(|p| show(part.root, p)));
        }
        if !conflicts.is_empty() {
            return Ok(Outcome::Conflict(conflicts));
        }
        let paths = cs.changes.iter().map(|c| show(c.root, &c.path)).collect();
        if parts.iter().all(|p| p.plan.touched().is_empty()) {
            return Ok(Outcome::Committed(None, paths));
        }
        let generation = self.journal().next()?;
        let mut entries: Vec<JournalEntry> = Vec::new();
        let mut dirs: Vec<DirTimes> = Vec::new();
        for part in &parts {
            let (lower, plan) = (part.lower, &part.plan);
            let files: HashSet<&PathBuf> = plan
                .puts
                .iter()
                .filter(|(p, st)| !sys::is_dir(st) && !plan.moves.contains_key(p))
                .map(|(p, _)| p)
                .collect();
            let sources: HashSet<&PathBuf> = plan.moves.values().collect();
            let touched = plan.touched();
            for p in &touched {
                let i = entries.len();
                entries.push(JournalEntry {
                    root: part.root,
                    path: p.clone(),
                    pre: sys::lstat(lower, p).ok().map(|st| Meta::of(&st)),
                    // A rename source waits at the root while removes run and its new parent appears.
                    tmp: if sources.contains(p) {
                        Some(PathBuf::from(format!(".escrow-{generation}-{i}.tmp")))
                    } else {
                        files
                            .contains(p)
                            .then(|| parent(p).join(format!(".escrow-{generation}-{i}.tmp")))
                    },
                });
            }
            dirs.extend(
                touched
                    .iter()
                    .map(|p| parent(p).to_path_buf())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .filter_map(|d| {
                        let st = sys::lstat(lower, &d).ok().filter(sys::is_dir)?;
                        let m = Meta::of(&st);
                        Some(DirTimes {
                            root: part.root,
                            path: d,
                            atime_ns: m.atime_ns,
                            mtime_ns: m.mtime_ns,
                        })
                    }),
            );
        }
        self.journal().begin(generation, &hs[0].group, &entries, &dirs)?;
        let mut applied = HashSet::new();
        if let Err(e) = self.apply(lowers, &parts, generation, &entries, &mut applied) {
            let raced = e
                .get_ref()
                .and_then(|r| r.downcast_ref::<Raced>())
                .map(|r| (r.0, r.1.clone()));
            // An editor's file must survive the rollback: undo only what apply reached,
            // and leave the raced path (and anything under it) as the editor left it.
            let only = raced.as_ref().map(|(root, p)| Reached {
                applied: &applied,
                raced: (*root, p),
            });
            return match (self.rollback(lowers, generation, only), raced) {
                (Ok(()), Some((root, p))) => Ok(Outcome::Conflict(vec![show(root, &p)])),
                (Ok(()), None) => Err(e),
                (Err(r), _) => Err(io::Error::other(RollbackFailed(format!("{e}; rollback failed: {r}")))),
            };
        }
        Ok(Outcome::Committed(Some(generation), paths))
    }

    /// Drop finished generations no open scope reads through: those at or below
    /// the oldest open scope's generation (all of them when no scope is open).
    /// `dropped`: the generation whose scope was just dropped (see `scope_dropped`).
    pub fn gc(&self, oldest_open: Option<u64>, dropped: Option<u64>) -> io::Result<()> {
        let _serial = self.serial.lock();
        let floor = oldest_open.unwrap_or(u64::MAX);
        let done: Vec<u64> = self
            .journal()
            .gens()?
            .into_iter()
            .filter(|(g, _, s)| *s == GenState::Done && *g <= floor)
            .map(|(g, _, _)| g)
            .collect();
        // Forget first: a crash before the files are gone leaves only files, which start sweeps.
        self.journal().forget_all(&done, oldest_open.is_none(), dropped)?;
        for g in done {
            self.unregister(g)?;
        }
        if oldest_open.is_none() {
            self.aliases.lock().clear();
        }
        Ok(())
    }
}
