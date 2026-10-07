//! Open files: the file handle table, who opened each, handles that settle before
//! a close, and writeback off the request threads.

use super::*;

/// The open files of every view, by file handle.
pub(super) struct Files {
    files: HashMap<u64, (Arc<ScopeHandle>, Arc<File>)>,
    /// The process that opened each file handle.
    openers: HashMap<u64, u32>,
    next_fh: u64,
}

impl Default for Files {
    fn default() -> Self {
        Files {
            files: HashMap::new(),
            openers: HashMap::new(),
            next_fh: 1,
        }
    }
}

impl Views {
    // ---- open files ----

    /// `pid`: the process that opened it (FUSE reports the calling thread).
    pub fn add_file(&self, h: Arc<ScopeHandle>, f: File, pid: u32) -> u64 {
        let mut t = self.files.lock();
        let fh = t.next_fh;
        t.next_fh += 1;
        t.files.insert(fh, (h, Arc::new(f)));
        t.openers.insert(fh, pid);
        fh
    }

    /// Wait (up to `within`) until scope `id` holds no handle opened by a process in
    /// one of the `stopped` process groups (its sandboxes) or by a process that is
    /// exiting or gone. A process killed by a signal sends no FUSE flush: its dirty
    /// pages reach the daemon only with the release, which the kernel sends as the
    /// process exits, possibly after its sandbox's bwrap is reaped. Close waits for
    /// those writes before it freezes the scope. Handles of live processes outside
    /// the sandboxes (the SDK's app) are the SDK's to flush.
    pub(super) fn settle_dead_handles(&self, id: &str, stopped: &[i32], within: std::time::Duration) {
        let deadline = std::time::Instant::now() + within;
        loop {
            // The openers under the lock, their state in /proc outside it: FUSE
            // requests of every scope take this lock.
            let openers: Vec<u32> = {
                let t = self.files.lock();
                t.files
                    .iter()
                    .filter(|(_, (h, _))| h.group == id)
                    .filter_map(|(fh, _)| t.openers.get(fh).copied())
                    .collect()
            };
            let pending = openers.into_iter().any(|p| finishing(p, stopped));
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if !pending || left.is_zero() {
                return;
            }
            let mut t = self.files.lock();
            self.released
                .wait_for(&mut t, left.min(std::time::Duration::from_millis(20)));
        }
    }

    pub fn file(&self, fh: u64) -> R<Arc<File>> {
        self.files
            .lock()
            .files
            .get(&fh)
            .map(|(_, f)| f.clone())
            .ok_or(Errno::EBADF)
    }

    /// A handle to write through: the tag is fixed at open, and a closed scope takes no writes.
    pub fn file_for_write(&self, fh: u64) -> R<Arc<File>> {
        let (h, f) = self.files.lock().files.get(&fh).cloned().ok_or(Errno::EBADF)?;
        if h.is_closed() {
            self.ledger.append(&h.group, "write", Path::new(""), None, "deny");
            return Err(Errno::EBADF);
        }
        Ok(f)
    }

    pub fn release(&self, fh: u64) {
        let f = {
            let mut t = self.files.lock();
            t.openers.remove(&fh);
            t.files.remove(&fh)
        };
        self.released.notify_all();
        // A file opened for writing is in the upper: start writing its data to disk
        // now, so the flush before a commit finds little left.
        if let Some((_, f)) = f
            && rustix::fs::fcntl_getfl(&*f).is_ok_and(|fl| fl.contains(OFlags::RDWR))
        {
            let _ = self.writeback.lock().send(f);
        }
    }

    pub fn statfs(&self) -> R<rustix::fs::StatVfs> {
        sys::statvfs(self.roots[PROJECT].lower.as_ref().ok_or(Errno::EIO)?.as_fd()).map_err(errno)
    }
}

/// A thread that starts the writeback of each file it is sent.
pub(super) fn writeback_thread() -> std::sync::mpsc::Sender<Arc<File>> {
    let (tx, rx) = std::sync::mpsc::channel::<Arc<File>>();
    std::thread::spawn(move || {
        for f in rx {
            sys::start_writeback(&f);
        }
    });
    tx
}

/// Process (or thread) `pid` is gone, exiting, or in one of the `stopped` process groups.
pub(super) fn finishing(pid: u32, stopped: &[i32]) -> bool {
    const PF_EXITING: u64 = 0x4;
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return true;
    };
    // Fields after the command name: state ppid pgrp session tty_nr tpgid flags …
    let Some(rest) = stat.rfind(')').and_then(|i| stat.get(i + 2..)) else {
        return true;
    };
    let f: Vec<&str> = rest.split_whitespace().collect();
    let state = f.first().and_then(|s| s.chars().next()).unwrap_or('X');
    let pgrp = f.get(2).and_then(|s| s.parse::<i32>().ok());
    let flags = f.get(6).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    matches!(state, 'Z' | 'X' | 'x') || flags & PF_EXITING != 0 || pgrp.is_some_and(|g| stopped.contains(&g))
}
