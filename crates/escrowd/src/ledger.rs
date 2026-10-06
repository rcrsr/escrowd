//! The ledger: an append-only record of every operation, attributed to its scope.
//!
//! One line per operation:
//! `<unix ms> scope=<id> op=<op> path=<path> [from=<path>] [proc=<id>] decision=<allow|deny|discard|return|…>`
//! `proc` names the process that asked for a change (create, open for writing,
//! mkdir, symlink, link, rename, unlink, rmdir, setattr). Each process has one line,
//! written before the first line naming it:
//! `<unix ms> proc=<id> pid=<pid> parent=<id|-> exe=<path> ino=<dev>:<ino> args=<arg>,<arg>,…`
//! Ids are 16 hex digits. Paths and arguments are percent-encoded: bytes outside
//! printable ASCII, and ` `, `=` and `%` (and `,` in arguments), become `%XX`, so every
//! line splits on spaces and every path round-trips. Arguments stop at 4 KiB.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::proc::Proc;
use crate::sys;

pub struct Ledger(File);

/// Arguments past this many bytes (encoded) are left out of a `proc` line.
const ARGS_MAX: usize = 4096;

pub fn escape(p: &Path) -> String {
    escape_bytes(sys::path_bytes(p), b"=%")
}

fn escape_bytes(bytes: &[u8], also: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        if b.is_ascii_graphic() && !also.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl Ledger {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Ledger(OpenOptions::new().create(true).append(true).open(path)?))
    }

    pub fn append(&self, scope: &str, op: &str, path: &Path, from: Option<&Path>, decision: &str) {
        self.append_by(scope, op, path, from, None, decision)
    }

    /// A line naming the process `proc` (its `proc` line written before).
    pub fn append_by(
        &self,
        scope: &str,
        op: &str,
        path: &Path,
        from: Option<&Path>,
        proc: Option<u64>,
        decision: &str,
    ) {
        let from = from.map(|f| format!(" from={}", escape(f))).unwrap_or_default();
        let proc = proc.map(|p| format!(" proc={p:016x}")).unwrap_or_default();
        self.write(&format!(
            "scope={scope} op={op} path={}{from}{proc} decision={decision}",
            escape(path)
        ));
    }

    /// The `proc` line of `p`.
    pub fn proc(&self, p: &Proc) {
        let i = &p.info;
        let parent = match i.parent {
            0 => "-".to_string(),
            id => format!("{id:016x}"),
        };
        let mut args = String::new();
        for a in &i.args {
            let a = escape_bytes(a.as_bytes(), b"=%,");
            if args.len() + a.len() + 1 > ARGS_MAX {
                break;
            }
            if !args.is_empty() {
                args.push(',');
            }
            args.push_str(&a);
        }
        self.write(&format!(
            "proc={:016x} pid={} parent={parent} exe={} ino={}:{} args={args}",
            i.id,
            i.pid,
            escape(&i.program),
            i.dev,
            i.ino
        ));
    }

    fn write(&self, fields: &str) {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        // One O_APPEND write per line: concurrent appends never interleave within a line.
        let _ = (&self.0).write_all(format!("{ms} {fields}\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_separators_and_non_ascii() {
        assert_eq!(escape(Path::new("a b=c%d/é")), "a%20b%3Dc%25d/%C3%A9");
        assert_eq!(escape(Path::new("plain/path.txt")), "plain/path.txt");
    }
}
