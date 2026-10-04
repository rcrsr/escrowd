//! Fault injection for the commit path, debug builds only.
//!
//! `ESCROWD_FAULT=<point>:<n>[:abort]` fails the commit when it reaches step `n`
//! of `point` (`journal`, `preimage`, `apply`, `done`): with an I/O error, which
//! the commit rolls back in place, or with `abort`, which kills the daemon so the
//! next start rolls it back. `committed` fires after the journal says done, before
//! the scope is dropped (only `abort` makes sense there: the next start drops it).
//! Release builds ignore the variable.

use std::io;

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
