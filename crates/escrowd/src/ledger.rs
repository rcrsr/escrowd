//! The ledger: an append-only record of every operation, attributed to its scope.
//!
//! One line per operation:
//! `<unix ms> scope=<id> op=<op> path=<path> [from=<path>] decision=<allow|deny|discard|return|…>`
//! Paths are percent-encoded: bytes outside printable ASCII, and ` `, `=` and `%`,
//! become `%XX`, so every line splits on spaces and every path round-trips.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::sys;

pub struct Ledger(File);

pub fn escape(p: &Path) -> String {
    let mut out = String::new();
    for &b in sys::path_bytes(p) {
        if b.is_ascii_graphic() && b != b'=' && b != b'%' {
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
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let from = from.map(|f| format!(" from={}", escape(f))).unwrap_or_default();
        let line = format!(
            "{ms} scope={scope} op={op} path={}{from} decision={decision}\n",
            escape(path)
        );
        // One O_APPEND write per line: concurrent appends never interleave within a line.
        let _ = (&self.0).write_all(line.as_bytes());
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
