//! The ledger: an append-only record of every operation, attributed to its scope.
//! One line per operation: `<unix ms> scope=<id> op=<op> path=<path> [from=<path>] decision=<allow|deny>`.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Ledger(Mutex<File>);

impl Ledger {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Ledger(Mutex::new(
            OpenOptions::new().create(true).append(true).open(path)?,
        )))
    }

    pub fn append(&self, scope: &str, op: &str, path: &Path, from: Option<&Path>, decision: &str) {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let from = from.map(|f| format!(" from={}", f.display())).unwrap_or_default();
        let line = format!(
            "{ms} scope={scope} op={op} path={}{from} decision={decision}\n",
            path.display()
        );
        // One write per line, so concurrent appends never interleave within a line.
        let _ = self.0.lock().unwrap().write_all(line.as_bytes());
    }
}
