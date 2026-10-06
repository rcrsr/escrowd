//! Process attribution: who made a change.
//!
//! FUSE gives each request the calling thread's ID in the daemon's PID namespace,
//! so `/proc/<tid>` names sandboxed processes too. The first request of a process
//! reads its program (`/proc/<pid>/exe`, with the binary's device and inode), its
//! arguments and its parent chain, and caches them by PID and start time: a reused
//! PID is a new process. A cached thread's start time is checked again (one read of
//! `/proc/<tid>/stat`) at most every `RECHECK`: within it the thread cannot have
//! exited and had its ID reused, which takes the kernel's PID allocator wrapping
//! around `pid_max`.
//!
//! Exec keeps the PID and start time, so every request also compares the binary's
//! device and inode with the cached ones (one `stat` of `/proc/<tid>/exe`). A fork
//! (its parent's binary and arguments: not exec'd yet, or a subshell) also has its
//! arguments read again, so a fork that execs its parent's binary is seen too. A
//! process that execs its own binary again with new arguments keeps its old record.
//!
//! The chain stops before the daemon (it starts every sandbox), at PID 1, after a
//! session leader (a host app's shell), or after `MAX_PARENTS` parents.
//!
//! What a reviewer can trust: the program and its inode are what the kernel ran;
//! the arguments are what the process says about itself (a process can rewrite its
//! own `argv`).

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_PARENTS: usize = 16;
const RECHECK: Duration = Duration::from_millis(20);
/// Cached tasks and processes, each; past it, entries of dead processes go.
const CACHE: usize = 4096;

/// A process, as recorded in a scope's store and its change set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    /// Random, nonzero, 63 bits: unique across daemon runs, so ledger lines never collide.
    pub id: u64,
    pub pid: u32,
    /// `/proc/<pid>/exe`; ends ` (deleted)` if the binary was removed.
    pub program: PathBuf,
    /// The binary's device and inode.
    pub dev: u64,
    pub ino: u64,
    pub args: Vec<String>,
    /// The parent's id; 0: none recorded.
    pub parent: u64,
}

pub struct Proc {
    pub info: Info,
    pub parent: Option<Arc<Proc>>,
    /// Has its parent's binary and arguments: its arguments are checked on every request.
    forked: bool,
}

impl Proc {
    /// This process and its recorded ancestors, nearest first.
    pub fn chain(self: &Arc<Self>) -> impl Iterator<Item = &Arc<Proc>> {
        std::iter::successors(Some(self), |p| p.parent.as_ref())
    }
}

/// The fields of `/proc/<pid>/stat` the cache uses.
struct Stat {
    ppid: u32,
    session: u32,
    start: u64,
}

fn stat(pid: u32) -> Option<Stat> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // After the command name: state(3) ppid(4) pgrp(5) session(6) … starttime(22).
    let f: Vec<&str> = s.get(s.rfind(')')? + 2..)?.split(' ').collect();
    Some(Stat {
        ppid: f.get(1)?.parse().ok()?,
        session: f.get(3)?.parse().ok()?,
        start: f.get(19)?.parse().ok()?,
    })
}

fn args(pid: u32) -> Vec<String> {
    let args = fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_default()
        .split(|b| *b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect::<Vec<_>>();
    match args.split_last() {
        Some((last, rest)) if last.is_empty() => rest.to_vec(), // the trailing NUL
        _ => args,
    }
}

/// Whether task `id` still runs what `p` recorded (no exec since); None if it is gone.
fn current(id: u32, p: &Proc) -> Option<bool> {
    let m = fs::metadata(format!("/proc/{id}/exe")).ok()?;
    Some((m.dev(), m.ino()) == (p.info.dev, p.info.ino) && (!p.forked || args(id) == p.info.args))
}

/// The process (thread group) a thread belongs to.
fn tgid(tid: u32) -> Option<u32> {
    let s = fs::read_to_string(format!("/proc/{tid}/status")).ok()?;
    s.lines().find_map(|l| l.strip_prefix("Tgid:"))?.trim().parse().ok()
}

fn new_id() -> u64 {
    let mut b = [0u8; 8];
    loop {
        if crate::sys::random(&mut b).is_err() {
            // getrandom(2) does not fail once seeded; time and a counter are unique enough.
            b = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .to_le_bytes()[..8]
                .try_into()
                .unwrap();
        }
        let id = u64::from_le_bytes(b) >> 1;
        if id != 0 {
            return id;
        }
    }
}

struct Entry {
    start: u64,
    proc: Arc<Proc>,
    /// When `start` was last read.
    checked: Instant,
}

type Cache = HashMap<u32, Entry>;

pub struct Procs {
    daemon: u32,
    /// Thread ID -> its start time and process.
    tasks: Mutex<Cache>,
    /// Process ID -> its start time and process.
    procs: Mutex<Cache>,
    /// Held while a missed thread is looked up, so two threads of a new process
    /// record it once.
    lookup: Mutex<()>,
}

impl Default for Procs {
    fn default() -> Self {
        Procs {
            daemon: std::process::id(),
            tasks: Mutex::default(),
            procs: Mutex::default(),
            lookup: Mutex::default(),
        }
    }
}

/// The cached process of `id` if it started at `start`; marks it checked.
fn cached(c: &Mutex<Cache>, id: u32, start: u64) -> Option<Arc<Proc>> {
    let mut c = c.lock().unwrap();
    let e = c.get_mut(&id).filter(|e| e.start == start)?;
    e.checked = Instant::now();
    Some(e.proc.clone())
}

fn insert(c: &Mutex<Cache>, id: u32, start: u64, p: &Arc<Proc>) {
    let mut c = c.lock().unwrap();
    if c.len() >= CACHE {
        c.retain(|id, e| stat(*id).is_some_and(|st| st.start == e.start));
        if c.len() >= CACHE {
            c.clear();
        }
    }
    let e = Entry {
        start,
        proc: p.clone(),
        checked: Instant::now(),
    };
    c.insert(id, e);
}

impl Procs {
    /// The process thread `tid` belongs to, and the processes first seen in this call
    /// (its ancestors, farthest first, then it); None for the daemon, a kernel thread or
    /// a task already gone.
    pub fn of(&self, tid: u32) -> Option<(Arc<Proc>, Vec<Arc<Proc>>)> {
        if tid == 0 {
            return None;
        }
        let hit = self
            .tasks
            .lock()
            .unwrap()
            .get(&tid)
            .map(|e| (e.proc.clone(), e.checked.elapsed() < RECHECK));
        let st = match hit {
            Some((p, true)) if current(tid, &p)? => return Some((p, Vec::new())),
            _ => stat(tid)?,
        };
        if let Some(p) = cached(&self.tasks, tid, st.start)
            && current(tid, &p)?
        {
            return Some((p, Vec::new()));
        }
        let pid = tgid(tid)?;
        let _one = self.lookup.lock().unwrap();
        let mut fresh = Vec::new();
        let p = self.process(pid, MAX_PARENTS, &mut fresh)?;
        insert(&self.tasks, tid, st.start, &p);
        Some((p, fresh))
    }

    fn process(&self, pid: u32, parents: usize, fresh: &mut Vec<Arc<Proc>>) -> Option<Arc<Proc>> {
        if pid <= 1 || pid == self.daemon {
            return None;
        }
        let st = stat(pid)?;
        if let Some(p) = cached(&self.procs, pid, st.start)
            && current(pid, &p)?
        {
            return Some(p);
        }
        let exe = format!("/proc/{pid}/exe");
        let program = fs::read_link(&exe).ok()?; // none for a kernel thread
        let bin = fs::metadata(&exe).ok()?;
        let args = args(pid);
        let parent = if st.session == pid || parents == 0 {
            None
        } else {
            self.process(st.ppid, parents - 1, fresh)
        };
        let (dev, ino) = (bin.dev(), bin.ino());
        let forked = parent
            .as_ref()
            .is_some_and(|p| (p.info.dev, p.info.ino) == (dev, ino) && p.info.args == args);
        let p = Arc::new(Proc {
            info: Info {
                id: new_id(),
                pid,
                program,
                dev,
                ino,
                args,
                parent: parent.as_ref().map_or(0, |p| p.info.id),
            },
            parent,
            forked,
        });
        insert(&self.procs, pid, st.start, &p);
        fresh.push(p.clone());
        Some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_child_its_program_and_parent_once() {
        let procs = Procs {
            daemon: 0, // the test process is not the daemon
            ..Procs::default()
        };
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        let (p, fresh) = after_exec(&procs, child.id(), "sleep");
        assert_eq!(p.info.pid, child.id());
        assert!(p.info.program.ends_with("sleep"), "{:?}", p.info.program);
        assert_eq!(p.info.args, ["sleep", "5"]);
        let me = p.parent.as_ref().unwrap();
        assert_eq!(me.info.pid, std::process::id());
        assert_eq!(p.info.parent, me.info.id);
        assert!(Arc::ptr_eq(fresh.last().unwrap(), &p));
        // Cached: the same process, nothing new.
        let (again, fresh) = procs.of(child.id()).unwrap();
        assert!(Arc::ptr_eq(&again, &p) && fresh.is_empty());
        child.kill().unwrap();
        child.wait().unwrap();
        std::thread::sleep(RECHECK);
        assert!(procs.of(child.id()).is_none());
    }

    /// The process of `pid` once `program` is its `argv[0]` (a spawned child may not
    /// have exec'd yet).
    fn after_exec(procs: &Procs, pid: u32, program: &str) -> (Arc<Proc>, Vec<Arc<Proc>>) {
        for _ in 0..500 {
            let (p, fresh) = procs.of(pid).unwrap();
            if p.info.args.first().is_some_and(|a| a == program) {
                return (p, fresh);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("{pid} never ran {program}");
    }

    #[test]
    fn an_exec_is_a_new_process() {
        use std::io::Write;
        let procs = Procs {
            daemon: 0,
            ..Procs::default()
        };
        let mut child = std::process::Command::new("sh")
            .args(["-c", "read x; exec sleep 5"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let sh = after_exec(&procs, child.id(), "sh").0;
        assert_eq!(sh.info.args, ["sh", "-c", "read x; exec sleep 5"]);
        child.stdin.as_mut().unwrap().write_all(b"\n").unwrap();
        let sleep = after_exec(&procs, child.id(), "sleep").0;
        assert_eq!(
            (sleep.info.pid, &sleep.info.args[..]),
            (sh.info.pid, &["sleep".to_string(), "5".to_string()][..])
        );
        assert_ne!(sleep.info.id, sh.info.id);
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn stops_before_the_daemon() {
        let procs = Procs::default(); // this test process plays the daemon
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        let (p, fresh) = after_exec(&procs, child.id(), "sleep");
        assert!(p.parent.is_none() && fresh.len() <= 1);
        assert!(procs.of(std::process::id()).is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
