//! Fault injection for the commit path, debug builds only.
//!
//! `ESCROWD_FAULT=<point>:<n>[:abort]` fails the commit when it reaches step `n`
//! of `point` (`journal`, `preimage`, `apply`, `done`): with an I/O error, which
//! the commit rolls back in place, or with `abort`, which kills the daemon so the
//! next start rolls it back. `committed` fires after the journal says done, before
//! the scope is dropped (only `abort` makes sense there: the next start drops it).
//! A decision the history keeps passes `intended` (written ahead), `applied` (before
//! it is settled) and `settled` (before a committed scope is dropped); with `abort`,
//! the next start settles or cancels it.
//! `restore` fires while a rollback puts originals back (with `abort`: a crash
//! during recovery). `race:<n>` plays an editor: just before apply step `n`
//! re-checks its path, it appends to the live file (creating it if missing; not
//! through a symlink, and not to a directory).
//! Release builds ignore the variable.

use std::io;
use std::os::fd::BorrowedFd;
use std::path::Path;

#[cfg(debug_assertions)]
fn spec() -> &'static Option<(String, usize, bool)> {
    use std::sync::OnceLock;
    static SPEC: OnceLock<Option<(String, usize, bool)>> = OnceLock::new();
    SPEC.get_or_init(|| {
        let v = std::env::var("ESCROWD_FAULT").ok()?;
        let mut parts = v.split(':');
        let point = parts.next()?.to_string();
        let n = parts.next().unwrap_or("0").parse().ok()?;
        Some((point, n, parts.next() == Some("abort")))
    })
}

pub fn hit(point: &str, n: usize) -> io::Result<()> {
    #[cfg(debug_assertions)]
    if let Some((p, at, abort)) = spec()
        && p == point
        && *at == n
    {
        if *abort {
            eprintln!("escrowd: injected abort at {point}:{n}");
            std::process::abort();
        }
        return Err(io::Error::other(format!("injected fault at {point}:{n}")));
    }
    let _ = (point, n);
    Ok(())
}

/// `ESCROWD_FAULT=race:<n>`: write to `rel` under `dir` before apply step `n` re-checks it.
pub fn race(n: usize, dir: BorrowedFd, rel: &Path) {
    #[cfg(debug_assertions)]
    if let Some((p, at, _)) = spec()
        && p == "race"
        && *at == n
    {
        use rustix::fs::{Mode, OFlags};
        use std::io::Write;
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::APPEND | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        if let Ok(fd) = rustix::fs::openat(dir, rel, flags, Mode::from_raw_mode(0o644)) {
            let _ = std::fs::File::from(fd).write_all(b"editor\n");
        }
        eprintln!("escrowd: injected editor write at race:{n} ({})", rel.display());
    }
    let _ = (n, dir, rel);
}
