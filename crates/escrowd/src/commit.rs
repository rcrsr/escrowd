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

use parking_lot::{Mutex, MutexGuard};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fuser::FileType;
use rustix::fs::{OFlags, Stat};

use crate::changeset::{self, Change, ChangeSet, Kind};
use crate::fault;
use crate::journal::{DirTimes, GenState, Journal, JournalEntry};
use crate::roots;
use crate::snapshot::{Base, Generations, Meta};
use crate::sys::{self, Version, parent};
use crate::views::ScopeHandle;

/// A path in one root.
type Rooted = (usize, PathBuf);

/// The base directory of each root, by root index (None: the host has none).
pub type Lowers<'a> = [Option<BorrowedFd<'a>>; roots::COUNT];

/// Shows a root's path to the caller (`~/x` for `$HOME`).
pub type Show<'a> = &'a dyn Fn(usize, &Path) -> PathBuf;

fn lower_of<'a>(lowers: &Lowers<'a>, root: usize) -> io::Result<BorrowedFd<'a>> {
    lowers[root].ok_or_else(|| io::Error::other(format!("root {root} has no base directory")))
}

/// What an in-process rollback after a race undoes: the paths apply reached, except
/// the raced path and its descendants, which keep the editor's version.
#[derive(Clone, Copy)]
struct Reached<'a> {
    applied: &'a HashSet<Rooted>,
    raced: (usize, &'a Path),
}

/// An editor changed `.1` (in root `.0`) after the conflict check: the commit rolled back.
#[derive(Debug)]
struct Raced(usize, PathBuf);

impl std::fmt::Display for Raced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} changed in the project during the commit", self.1.display())
    }
}

impl std::error::Error for Raced {}

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

/// What a commit does to the base, in order.
struct Plan {
    /// Deepest first: deleted entries, rename sources, entries replaced by another type of entry.
    removes: Vec<PathBuf>,
    /// Parents first, with the upper entry's stat.
    puts: Vec<(PathBuf, Stat)>,
    /// Destination → source of each rename of a base file or symlink the scope left
    /// unchanged: apply renames the original, so it keeps its inode.
    moves: HashMap<PathBuf, PathBuf>,
}

impl Plan {
    fn new<'c>(lower: BorrowedFd, upper: BorrowedFd, changes: impl Iterator<Item = &'c Change>) -> io::Result<Self> {
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

    fn touched(&self) -> BTreeSet<PathBuf> {
        self.removes
            .iter()
            .cloned()
            .chain(self.puts.iter().map(|(p, _)| p.clone()))
            .collect()
    }
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

/// Missing, or under a parent that is no longer a directory (an editor replaced it).
fn ignore_missing(r: io::Result<()>) -> io::Result<()> {
    match r {
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => Ok(()),
        r => r,
    }
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

    fn conflicts(&self, base: &Base, part: &Part, reads: bool) -> io::Result<Vec<PathBuf>> {
        let (lower, h, plan, root) = (part.lower, part.h, &part.plan, part.root);
        let touched = plan.touched();
        let removes: HashSet<&PathBuf> = plan.removes.iter().collect();
        let aliases = self.aliases.lock();
        let same = |p: &Path, want: Option<Version>, have: Option<Version>| match (want, have) {
            (None, None) => true,
            (Some(mut w), Some(have)) => {
                // A rollback replaced the original by a copy; follow the chain of copies.
                for _ in 0..16 {
                    if w == have {
                        return true;
                    }
                    match aliases.get(&(root, p.to_path_buf(), w)) {
                        Some(next) => w = *next,
                        None => return false,
                    }
                }
                false
            }
            _ => false,
        };
        let store = h.store_read();
        let mut out = BTreeSet::new();
        if reads {
            // Files read from the base (never from the upper: those are the scope's own),
            // kept only where they would commit: an ephemeral read changes nothing.
            for (p, _) in store.reads().into_iter().filter(|(_, allowed)| *allowed) {
                if touched.contains(&p) || h.access(&p) != roots::Access::Capture {
                    continue;
                }
                let live = sys::lstat(lower, &p).ok();
                if !same(&p, store.base_version(&p)?, live.as_ref().map(Version::of)) {
                    out.insert(p);
                }
            }
        }
        for p in &touched {
            let want = match store.base_version(p)? {
                Some(v) => Some(v),
                None => base.lstat(p).ok().map(|st| Version::of(&st)),
            };
            let live = sys::lstat(lower, p).ok();
            if !same(p, want, live.as_ref().map(Version::of)) {
                out.insert(p.clone());
                continue;
            }
            // A directory the scope deletes must not have gained entries.
            if removes.contains(p) && live.as_ref().is_some_and(sys::is_dir) {
                for (name, _) in sys::read_dir(lower, p)? {
                    if !touched.contains(&p.join(&name)) {
                        out.insert(p.clone());
                    }
                }
            }
        }
        for (p, _) in &plan.puts {
            for a in p.ancestors().skip(1) {
                if a.as_os_str().is_empty() || touched.contains(a) {
                    break;
                }
                if !sys::lstat(lower, a).is_ok_and(|st| sys::is_dir(&st)) {
                    out.insert(p.clone());
                    break;
                }
            }
        }
        Ok(out.into_iter().collect())
    }

    fn apply(
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

    /// Undo an unfinished generation: restore each root's base from its pre-images
    /// (once prepared), remove temporary files, then forget the generation.
    /// `only`: what apply reached (an in-process rollback after a race); None after
    /// a crash, when only versions tell what apply reached.
    fn rollback(&self, lowers: &Lowers, generation: u64, only: Option<Reached>) -> io::Result<()> {
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
