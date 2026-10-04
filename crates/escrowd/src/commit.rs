//! Commit: apply a closed scope's change set to the project, all or nothing.
//!
//! Commits are serialized per project. For one scope:
//!
//! 1. **Conflict check**: every path the commit touches must still be as the
//!    scope first saw it (the version it recorded, else its snapshot's), and
//!    the parents of new entries must still be directories. Any mismatch is a
//!    conflict and nothing is written.
//! 2. **Journal**: the generation's intent (each path, the original's metadata,
//!    each new file's temporary name) is on disk before anything changes.
//! 3. **Pre-images**: each base entry about to be overwritten or deleted is
//!    copied into the generation; then the journal says `prepared`.
//! 4. **Apply**: deletes deepest first, then new entries parents first; a new
//!    file is written beside its target, fsynced and renamed over it.
//! 5. **Done**: the journal says `done`; the caller drops the scope.
//!
//! A failure at any step rolls the applied part back from the pre-images; an
//! unfinished generation found at start is rolled back the same way. An editor
//! outside escrowd can still write a file between the check and the rename;
//! phase 1 accepts that window.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use rustix::fs::Stat;

use crate::changeset::{ChangeSet, Kind};
use crate::fault;
use crate::journal::{DirTimes, GenState, Journal, JournalEntry};
use crate::snapshot::{Base, Generations, Meta};
use crate::sys::{self, Version, parent};
use crate::views::ScopeHandle;

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
}

impl Plan {
    fn new(lower: BorrowedFd, upper: BorrowedFd, cs: &ChangeSet) -> io::Result<Self> {
        let mut removes = BTreeSet::new();
        let mut puts = BTreeSet::new();
        for c in &cs.changes {
            match c.kind {
                Kind::Delete => {
                    removes.insert(c.path.clone());
                }
                Kind::Rename => {
                    removes.extend(c.from.clone());
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
        Ok(Plan { removes, puts })
    }

    fn touched(&self) -> BTreeSet<PathBuf> {
        self.removes
            .iter()
            .cloned()
            .chain(self.puts.iter().map(|(p, _)| p.clone()))
            .collect()
    }
}

pub struct Commits {
    pub gens: Generations,
    journal: Mutex<Journal>,
    /// (path, original version) → the version a rollback left in its place.
    aliases: Mutex<HashMap<(PathBuf, Version), Version>>,
    serial: Mutex<()>,
    current: AtomicU64,
}

fn ignore_missing(r: io::Result<()>) -> io::Result<()> {
    match r {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

impl Commits {
    pub fn open(state_dir: &Path) -> io::Result<Self> {
        let journal = Journal::open(&state_dir.join("journal.sqlite"))?;
        let aliases = journal
            .restored()?
            .into_iter()
            .map(|(p, old, new)| ((p, old), new))
            .collect();
        Ok(Commits {
            gens: Generations::open(&state_dir.join("generations"))?,
            current: AtomicU64::new(journal.current()?),
            journal: Mutex::new(journal),
            aliases: Mutex::new(aliases),
            serial: Mutex::new(()),
        })
    }

    fn journal(&self) -> MutexGuard<'_, Journal> {
        self.journal.lock().unwrap()
    }

    /// The generation new scopes open at.
    pub fn current(&self) -> u64 {
        self.current.load(Ordering::Acquire)
    }

    /// On start: roll back unfinished commits and load the finished generations.
    /// Returns the scopes whose commit finished but which were not dropped yet.
    pub fn recover(&self, lower: BorrowedFd) -> io::Result<Vec<(u64, String)>> {
        let _serial = self.serial.lock().unwrap();
        let mut finished = Vec::new();
        let gens = self.journal().gens()?;
        for (g, scope, state) in gens {
            if state != GenState::Done {
                eprintln!("escrowd: rolling back unfinished commit (generation {g}, scope {scope})");
                self.rollback(lower, g)?;
                continue;
            }
            let entries = self.journal().entries(g)?;
            self.gens.register(g, entries.into_iter().map(|e| (e.path, e.pre)));
            if !scope.is_empty() {
                finished.push((g, scope));
            }
        }
        Ok(finished)
    }

    /// The scope of a finished generation is gone; recovery must not drop a later scope of the same id.
    pub fn scope_dropped(&self, generation: u64) -> io::Result<()> {
        self.journal().clear_scope(generation)
    }

    pub fn commit(&self, lower: BorrowedFd, h: &ScopeHandle, cs: &ChangeSet) -> io::Result<Outcome> {
        let _serial = self.serial.lock().unwrap();
        let plan = Plan::new(lower, h.upper.as_fd(), cs)?;
        let base = Base::new(lower, &self.gens, h.since);
        let conflicts = self.conflicts(lower, &base, h, &plan)?;
        if !conflicts.is_empty() {
            return Ok(Outcome::Conflict(conflicts));
        }
        let paths = cs.changes.iter().map(|c| c.path.clone()).collect();
        let touched = plan.touched();
        if touched.is_empty() {
            return Ok(Outcome::Committed(None, paths));
        }
        let generation = self.journal().alloc()?;
        let files: HashSet<&PathBuf> = plan
            .puts
            .iter()
            .filter(|(_, st)| !sys::is_dir(st))
            .map(|(p, _)| p)
            .collect();
        let entries: Vec<JournalEntry> = touched
            .iter()
            .enumerate()
            .map(|(i, p)| JournalEntry {
                path: p.clone(),
                pre: sys::lstat(lower, p).ok().map(|st| Meta::of(&st)),
                tmp: files
                    .contains(p)
                    .then(|| parent(p).join(format!(".escrow-{generation}-{i}.tmp"))),
            })
            .collect();
        let dirs: Vec<DirTimes> = touched
            .iter()
            .map(|p| parent(p).to_path_buf())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|d| {
                let st = sys::lstat(lower, &d).ok().filter(sys::is_dir)?;
                let m = Meta::of(&st);
                Some(DirTimes {
                    path: d,
                    atime_ns: m.atime_ns,
                    mtime_ns: m.mtime_ns,
                })
            })
            .collect();
        self.journal().begin(generation, &h.id, &entries, &dirs)?;
        if let Err(e) = self.apply(lower, h.upper.as_fd(), generation, &plan, &entries) {
            return match self.rollback(lower, generation) {
                Ok(()) => Err(e),
                Err(r) => Err(io::Error::other(format!("{e}; rollback failed: {r}"))),
            };
        }
        Ok(Outcome::Committed(Some(generation), paths))
    }

    fn conflicts(&self, lower: BorrowedFd, base: &Base, h: &ScopeHandle, plan: &Plan) -> io::Result<Vec<PathBuf>> {
        let touched = plan.touched();
        let removes: HashSet<&PathBuf> = plan.removes.iter().collect();
        let aliases = self.aliases.lock().unwrap();
        let same = |p: &Path, want: Option<Version>, have: Option<Version>| match (want, have) {
            (None, None) => true,
            (Some(mut w), Some(have)) => {
                // A rollback replaced the original by a copy; follow the chain of copies.
                for _ in 0..16 {
                    if w == have {
                        return true;
                    }
                    match aliases.get(&(p.to_path_buf(), w)) {
                        Some(next) => w = *next,
                        None => return false,
                    }
                }
                false
            }
            _ => false,
        };
        let store = h.store();
        let mut out = BTreeSet::new();
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
        lower: BorrowedFd,
        upper: BorrowedFd,
        generation: u64,
        plan: &Plan,
        entries: &[JournalEntry],
    ) -> io::Result<()> {
        fault::hit("journal", 0)?;
        let root = self.gens.root();
        for (i, e) in entries.iter().enumerate() {
            fault::hit("preimage", i)?;
            let Some(m) = &e.pre else { continue };
            let dst = Generations::rel(generation, &e.path);
            sys::mkdirs(root, parent(&dst))?;
            if m.is_dir() {
                sys::mkdirs(root, &dst)?;
            } else {
                sys::copy_entry(lower, &e.path, root, &dst, &sys::lstat(lower, &e.path)?, false)?;
            }
        }
        sys::syncfs(root)?;
        self.journal().set_state(generation, GenState::Prepared)?;
        self.gens
            .register(generation, entries.iter().map(|e| (e.path.clone(), e.pre)));
        let tmp: HashMap<&Path, &Path> = entries
            .iter()
            .filter_map(|e| Some((e.path.as_path(), e.tmp.as_deref()?)))
            .collect();
        let mut step = 0;
        for p in &plan.removes {
            fault::hit("apply", step)?;
            step += 1;
            let st = sys::lstat(lower, p)?;
            sys::unlink(lower, p, sys::is_dir(&st))?;
        }
        for (p, up) in &plan.puts {
            fault::hit("apply", step)?;
            step += 1;
            if sys::is_dir(up) {
                match sys::lstat(lower, p) {
                    Ok(_) => sys::chmod(lower, p, up.st_mode)?,
                    Err(_) => sys::copy_entry(upper, p, lower, p, up, false)?,
                }
            } else {
                let t = tmp[p.as_path()];
                sys::copy_entry(upper, p, lower, t, up, true)?;
                sys::rename(lower, t, p, 0)?;
            }
        }
        sys::syncfs(lower)?;
        fault::hit("done", 0)?;
        self.journal().set_state(generation, GenState::Done)?;
        self.current.store(generation, Ordering::Release);
        Ok(())
    }

    /// Undo an unfinished generation: restore the base from its pre-images (once
    /// prepared), remove temporary files, then forget the generation.
    fn rollback(&self, lower: BorrowedFd, generation: u64) -> io::Result<()> {
        let (state, entries, dirs) = {
            let j = self.journal();
            (j.state(generation)?, j.entries(generation)?, j.dirs(generation)?)
        };
        if state == Some(GenState::Prepared) {
            self.restore(lower, generation, &entries, &dirs)?;
        } else {
            for t in entries.iter().filter_map(|e| e.tmp.as_ref()) {
                ignore_missing(sys::unlink(lower, t, false))?;
            }
        }
        sys::syncfs(lower)?;
        self.gens.unregister(generation);
        self.gens.remove_files(generation)?;
        self.journal().forget(generation)
    }

    fn restore(
        &self,
        lower: BorrowedFd,
        generation: u64,
        entries: &[JournalEntry],
        dirs: &[DirTimes],
    ) -> io::Result<()> {
        let fmt = |mode: u32| mode & libc::S_IFMT;
        let mut order: Vec<&JournalEntry> = entries.iter().collect();
        order.sort_by_key(|e| std::cmp::Reverse(sys::depth(&e.path)));
        // Deepest first: remove what the commit put in place; keep originals it never reached.
        for e in &order {
            let Ok(live) = sys::lstat(lower, &e.path) else { continue };
            let keep = e.pre.is_some_and(|m| {
                (m.is_dir() && sys::is_dir(&live))
                    || (fmt(m.mode) == fmt(live.st_mode) && m.version() == Version::of(&live))
            });
            if !keep {
                sys::unlink(lower, &e.path, sys::is_dir(&live))?;
            }
        }
        for t in entries.iter().filter_map(|e| e.tmp.as_ref()) {
            ignore_missing(sys::unlink(lower, t, false))?;
        }
        // Parents first: put the originals back.
        let root = self.gens.root();
        for e in order.iter().rev() {
            let Some(m) = &e.pre else { continue };
            match sys::lstat(lower, &e.path) {
                Ok(live) if sys::is_dir(&live) => sys::chmod(lower, &e.path, m.mode)?,
                Ok(_) => {}
                Err(_) => {
                    let src = Generations::rel(generation, &e.path);
                    let mut st = sys::lstat(root, &src)?;
                    m.apply(&mut st);
                    sys::copy_entry(root, &src, lower, &e.path, &st, true)?;
                }
            }
        }
        let set = |p: &Path, atime_ns, mtime_ns| {
            if sys::lstat(lower, p).is_ok() {
                sys::set_times(lower, p, Meta::timespec(atime_ns), Meta::timespec(mtime_ns))?;
            }
            Ok::<_, io::Error>(())
        };
        for e in entries {
            if let Some(m) = e.pre.filter(Meta::is_dir) {
                set(&e.path, m.atime_ns, m.mtime_ns)?;
            }
        }
        for d in dirs {
            set(&d.path, d.atime_ns, d.mtime_ns)?;
        }
        // A copy has a new inode and ctime: scopes that recorded the original must not conflict.
        let journal = self.journal();
        let mut aliases = self.aliases.lock().unwrap();
        for e in entries {
            let Some(m) = e.pre else { continue };
            if let Ok(now) = sys::lstat(lower, &e.path).map(|st| Version::of(&st))
                && now != m.version()
            {
                journal.add_restored(&e.path, m.version(), now)?;
                aliases.insert((e.path.clone(), m.version()), now);
            }
        }
        Ok(())
    }

    /// Drop finished generations no open scope reads through: those at or below
    /// the oldest open scope's generation (all of them when no scope is open).
    pub fn gc(&self, oldest_open: Option<u64>) -> io::Result<()> {
        let _serial = self.serial.lock().unwrap();
        let floor = oldest_open.unwrap_or(u64::MAX);
        let done: Vec<u64> = self
            .journal()
            .gens()?
            .into_iter()
            .filter(|(g, _, s)| *s == GenState::Done && *g <= floor)
            .map(|(g, _, _)| g)
            .collect();
        for g in done {
            self.gens.unregister(g);
            self.gens.remove_files(g)?;
            self.journal().forget(g)?;
        }
        if oldest_open.is_none() {
            self.journal().clear_restored()?;
            self.aliases.lock().unwrap().clear();
        }
        Ok(())
    }
}
