//! Per-scope store: `<state>/scopes/<id>/upper/` holds the scope's files,
//! `<state>/scopes/<id>/meta.sqlite` its metadata:
//!
//! - whiteouts and opaque directories (which base entries the scope hides),
//! - base versions of every path the scope read or changed (the conflict check, 1.4),
//!   and the reads the gate denied (both go into the change set),
//! - pinned inode numbers: entries whose inode cannot be derived from the base
//!   path they now sit at (upper-only files, renamed files), so inode numbers
//!   survive a daemon restart,
//! - scope name, labels, index, lifecycle state, the base generation it opened at
//!   (its snapshot) and the upper-only inode counter.
//!
//! Hot lookups (hidden, pinned inode) hit in-memory copies; every change is
//! written through to SQLite before the FUSE reply.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::sys::{self, Version};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionKind {
    Read,
    Changed,
}

/// `Open` accepts IO; `Closed` is frozen until the decision (commit, discard, or return reopens it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeState {
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Seen {
    read: bool,
    changed: bool,
}

pub struct ScopeStore {
    pub id: String,
    pub name: String,
    pub idx: u64,
    /// The base generation at open: the scope reads the base as it was then.
    pub since: u64,
    /// Reads only (the unscoped root in `deny` mode): every change gets EROFS.
    pub readonly: bool,
    dir: PathBuf,
    db: Connection,
    whiteouts: HashSet<PathBuf>,
    opaque: HashSet<PathBuf>,
    pins: HashMap<PathBuf, u64>,
    versions: HashMap<PathBuf, Seen>,
    denied: HashSet<PathBuf>,
    next_upper_ino: u64,
    pub state: ScopeState,
}

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value ANY) STRICT;
CREATE TABLE IF NOT EXISTS whiteouts (path BLOB PRIMARY KEY) STRICT;
CREATE TABLE IF NOT EXISTS opaque (path BLOB PRIMARY KEY) STRICT;
CREATE TABLE IF NOT EXISTS pins (path BLOB PRIMARY KEY, ino INTEGER NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS labels (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS denied (path BLOB PRIMARY KEY) STRICT;
CREATE TABLE IF NOT EXISTS versions (
    path BLOB PRIMARY KEY,
    read INTEGER NOT NULL,
    changed INTEGER NOT NULL,
    ino INTEGER NOT NULL,
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    ctime_ns INTEGER NOT NULL
) STRICT;
";

fn sql(e: rusqlite::Error) -> io::Error {
    io::Error::other(e)
}

/// `path = p OR path starts with p/` for BLOB paths; `?1` is p, `?2` is p + "/", `?3` its length.
const UNDER: &str = "(path = ?1 OR substr(path, 1, ?3) = ?2)";

fn under_params(p: &Path) -> (Vec<u8>, Vec<u8>, i64) {
    let exact = sys::path_bytes(p).to_vec();
    let mut prefix = exact.clone();
    prefix.push(b'/');
    let n = prefix.len() as i64;
    (exact, prefix, n)
}

/// `p` moved from `from` to `to`, or None if `p` is not at or under `from`.
pub fn moved(p: &Path, from: &Path, to: &Path) -> Option<PathBuf> {
    let suffix = p.strip_prefix(from).ok()?;
    Some(if suffix.as_os_str().is_empty() {
        to.to_path_buf()
    } else {
        to.join(suffix)
    })
}

impl ScopeStore {
    /// Create a new scope directory.
    pub fn create(
        scopes_dir: &Path,
        id: &str,
        name: &str,
        idx: u64,
        since: u64,
        readonly: bool,
        labels: &HashMap<String, String>,
    ) -> io::Result<Self> {
        let dir = scopes_dir.join(id);
        fs::create_dir(&dir)?;
        fs::create_dir(dir.join("upper"))?;
        let db = Connection::open(dir.join("meta.sqlite")).map_err(sql)?;
        db.execute_batch(SCHEMA).map_err(sql)?;
        db.execute(
            "INSERT INTO meta VALUES ('name', ?1), ('idx', ?2), ('since', ?3), ('readonly', ?4), ('next_upper_ino', 0), ('state', 'open')",
            params![name, idx as i64, since as i64, readonly as i64],
        )
        .map_err(sql)?;
        for (k, v) in labels {
            db.execute("INSERT INTO labels VALUES (?1, ?2)", params![k, v])
                .map_err(sql)?;
        }
        Self::load(scopes_dir, id)
    }

    /// Reopen an existing scope directory, e.g. after a daemon restart.
    pub fn load(scopes_dir: &Path, id: &str) -> io::Result<Self> {
        let dir = scopes_dir.join(id);
        let db = Connection::open(dir.join("meta.sqlite")).map_err(sql)?;
        db.execute_batch(SCHEMA).map_err(sql)?;
        let meta = |k: &str| -> io::Result<rusqlite::types::Value> {
            db.query_row("SELECT value FROM meta WHERE key = ?1", [k], |r| r.get(0))
                .optional()
                .map_err(sql)?
                .ok_or_else(|| io::Error::other(format!("scope {id}: meta.{k} missing")))
        };
        let int = |v: rusqlite::types::Value| match v {
            rusqlite::types::Value::Integer(i) => Ok(i as u64),
            _ => Err(io::Error::other("bad meta integer")),
        };
        let name = match meta("name")? {
            rusqlite::types::Value::Text(t) => t,
            _ => return Err(io::Error::other("bad meta name")),
        };
        let idx = int(meta("idx")?)?;
        let since = int(meta("since")?)?;
        let readonly = int(meta("readonly")?)? != 0;
        let next_upper_ino = int(meta("next_upper_ino")?)?;
        let state = match meta("state")? {
            rusqlite::types::Value::Text(t) if t == "closed" => ScopeState::Closed,
            _ => ScopeState::Open,
        };
        let paths = |table: &str| -> io::Result<HashSet<PathBuf>> {
            let mut st = db.prepare(&format!("SELECT path FROM {table}")).map_err(sql)?;
            let rows = st.query_map([], |r| r.get::<_, Vec<u8>>(0)).map_err(sql)?;
            rows.map(|r| r.map(sys::path_from).map_err(sql)).collect()
        };
        let whiteouts = paths("whiteouts")?;
        let denied = paths("denied")?;
        let opaque = paths("opaque")?;
        let pins = {
            let mut st = db.prepare("SELECT path, ino FROM pins").map_err(sql)?;
            let rows = st
                .query_map([], |r| Ok((sys::path_from(r.get(0)?), r.get::<_, i64>(1)? as u64)))
                .map_err(sql)?;
            rows.collect::<Result<HashMap<_, _>, _>>().map_err(sql)?
        };
        let versions = {
            let mut st = db.prepare("SELECT path, read, changed FROM versions").map_err(sql)?;
            let rows = st
                .query_map([], |r| {
                    let seen = Seen {
                        read: r.get(1)?,
                        changed: r.get(2)?,
                    };
                    Ok((sys::path_from(r.get(0)?), seen))
                })
                .map_err(sql)?;
            rows.collect::<Result<HashMap<_, _>, _>>().map_err(sql)?
        };
        Ok(ScopeStore {
            id: id.to_string(),
            name,
            idx,
            since,
            readonly,
            dir,
            db,
            whiteouts,
            opaque,
            pins,
            versions,
            denied,
            next_upper_ino,
            state,
        })
    }

    pub fn upper_dir(&self) -> PathBuf {
        self.dir.join("upper")
    }

    /// Delete the scope's directory: its files and metadata.
    pub fn destroy(&self) -> io::Result<()> {
        fs::remove_dir_all(&self.dir)
    }

    /// Hidden by a whiteout on itself or an ancestor, or by an opaque ancestor.
    pub fn hidden(&self, rel: &Path) -> bool {
        let mut first = true;
        for a in rel.ancestors() {
            if a.as_os_str().is_empty() {
                break;
            }
            if self.whiteouts.contains(a) || (!first && self.opaque.contains(a)) {
                return true;
            }
            first = false;
        }
        false
    }

    pub fn set_state(&mut self, state: ScopeState) -> io::Result<()> {
        let v = if state == ScopeState::Closed { "closed" } else { "open" };
        self.db
            .execute("UPDATE meta SET value = ?1 WHERE key = 'state'", [v])
            .map_err(sql)?;
        self.state = state;
        Ok(())
    }

    pub fn labels(&self) -> io::Result<HashMap<String, String>> {
        let mut st = self.db.prepare("SELECT key, value FROM labels").map_err(sql)?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(sql)?;
        rows.collect::<Result<_, _>>().map_err(sql)
    }

    pub fn whiteouts(&self) -> impl Iterator<Item = &PathBuf> {
        self.whiteouts.iter()
    }

    pub fn opaque_dirs(&self) -> impl Iterator<Item = &PathBuf> {
        self.opaque.iter()
    }

    /// Base reads (allowed) and gate denials, sorted by path.
    pub fn reads(&self) -> Vec<(PathBuf, bool)> {
        let mut out: Vec<(PathBuf, bool)> = self
            .versions
            .iter()
            .filter(|(_, s)| s.read)
            .map(|(p, _)| (p.clone(), true))
            .chain(self.denied.iter().map(|p| (p.clone(), false)))
            .collect();
        out.sort();
        out
    }

    pub fn record_denied(&mut self, rel: &Path) -> io::Result<()> {
        if self.denied.contains(rel) {
            return Ok(());
        }
        self.db
            .execute("INSERT OR IGNORE INTO denied VALUES (?1)", [sys::path_bytes(rel)])
            .map_err(sql)?;
        self.denied.insert(rel.to_path_buf());
        Ok(())
    }

    pub fn is_opaque(&self, rel: &Path) -> bool {
        self.opaque.contains(rel)
    }

    pub fn add_whiteout(&mut self, rel: &Path) -> io::Result<()> {
        self.db
            .execute("INSERT OR IGNORE INTO whiteouts VALUES (?1)", [sys::path_bytes(rel)])
            .map_err(sql)?;
        self.whiteouts.insert(rel.to_path_buf());
        Ok(())
    }

    /// Drop a whiteout; returns whether there was one.
    pub fn remove_whiteout(&mut self, rel: &Path) -> io::Result<bool> {
        if !self.whiteouts.contains(rel) {
            return Ok(false);
        }
        self.db
            .execute("DELETE FROM whiteouts WHERE path = ?1", [sys::path_bytes(rel)])
            .map_err(sql)?;
        self.whiteouts.remove(rel);
        Ok(true)
    }

    pub fn add_opaque(&mut self, rel: &Path) -> io::Result<()> {
        self.db
            .execute("INSERT OR IGNORE INTO opaque VALUES (?1)", [sys::path_bytes(rel)])
            .map_err(sql)?;
        self.opaque.insert(rel.to_path_buf());
        Ok(())
    }

    /// A directory at `rel` was removed: forget whiteouts below it and opaque marks at or below it.
    pub fn clear_below(&mut self, rel: &Path) -> io::Result<()> {
        let (exact, prefix, n) = under_params(rel);
        let tx = self.db.transaction().map_err(sql)?;
        tx.execute(
            "DELETE FROM whiteouts WHERE substr(path, 1, ?2) = ?1",
            params![prefix, n],
        )
        .map_err(sql)?;
        tx.execute(&format!("DELETE FROM opaque WHERE {UNDER}"), params![exact, prefix, n])
            .map_err(sql)?;
        tx.commit().map_err(sql)?;
        self.whiteouts.retain(|p| !p.starts_with(rel) || p == rel);
        self.opaque.retain(|p| !p.starts_with(rel));
        Ok(())
    }

    pub fn pin(&self, rel: &Path) -> Option<u64> {
        self.pins.get(rel).copied()
    }

    pub fn set_pin(&mut self, rel: &Path, ino: u64) -> io::Result<()> {
        self.db
            .execute(
                "INSERT OR REPLACE INTO pins VALUES (?1, ?2)",
                params![sys::path_bytes(rel), ino as i64],
            )
            .map_err(sql)?;
        self.pins.insert(rel.to_path_buf(), ino);
        Ok(())
    }

    /// Forget pins at or under `rel` (the entry was removed).
    pub fn unpin_below(&mut self, rel: &Path) -> io::Result<()> {
        let (exact, prefix, n) = under_params(rel);
        self.db
            .execute(&format!("DELETE FROM pins WHERE {UNDER}"), params![exact, prefix, n])
            .map_err(sql)?;
        self.pins.retain(|p, _| !p.starts_with(rel));
        Ok(())
    }

    /// Pins at or under `from` move under `to`; pins under `to` (the replaced target) go.
    pub fn move_pins(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        self.unpin_below(to)?;
        let (exact, prefix, n) = under_params(from);
        let tx = self.db.transaction().map_err(sql)?;
        let rows: Vec<(Vec<u8>, i64)> = {
            let mut st = tx
                .prepare(&format!("SELECT path, ino FROM pins WHERE {UNDER}"))
                .map_err(sql)?;
            let rows = st
                .query_map(params![exact, prefix, n], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)?
        };
        tx.execute(&format!("DELETE FROM pins WHERE {UNDER}"), params![exact, prefix, n])
            .map_err(sql)?;
        let mut renamed = Vec::with_capacity(rows.len());
        for (old, ino) in rows {
            let new = moved(&sys::path_from(old), from, to).expect("selected on prefix");
            tx.execute(
                "INSERT OR REPLACE INTO pins VALUES (?1, ?2)",
                params![sys::path_bytes(&new), ino],
            )
            .map_err(sql)?;
            renamed.push((new, ino as u64));
        }
        tx.commit().map_err(sql)?;
        self.pins.retain(|p, _| !p.starts_with(from));
        self.pins.extend(renamed);
        Ok(())
    }

    /// Next upper-only inode counter value, persisted before it is handed out.
    pub fn alloc_upper_ino(&mut self) -> io::Result<u64> {
        self.next_upper_ino += 1;
        self.db
            .execute(
                "UPDATE meta SET value = ?1 WHERE key = 'next_upper_ino'",
                [self.next_upper_ino as i64],
            )
            .map_err(sql)?;
        Ok(self.next_upper_ino)
    }

    /// The base version of `rel` the scope first saw, if it read or changed it.
    pub fn base_version(&self, rel: &Path) -> io::Result<Option<Version>> {
        if !self.versions.contains_key(rel) {
            return Ok(None);
        }
        self.db
            .query_row(
                "SELECT ino, size, mtime_ns, ctime_ns FROM versions WHERE path = ?1",
                [sys::path_bytes(rel)],
                |r| {
                    Ok(Version {
                        ino: r.get::<_, i64>(0)? as u64,
                        size: r.get(1)?,
                        mtime_ns: r.get(2)?,
                        ctime_ns: r.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(sql)
    }

    /// Record the base version of `rel` the first time the scope reads or changes it.
    /// Later reads and changes set their flag but keep the version first seen.
    pub fn record_version(&mut self, rel: &Path, kind: VersionKind, v: Version) -> io::Result<()> {
        let old = self.versions.get(rel).copied();
        let mut seen = old.unwrap_or_default();
        match kind {
            VersionKind::Read => seen.read = true,
            VersionKind::Changed => seen.changed = true,
        }
        if old == Some(seen) {
            return Ok(());
        }
        self.db
            .execute(
                "INSERT INTO versions VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(path) DO UPDATE SET read = excluded.read, changed = excluded.changed",
                params![
                    sys::path_bytes(rel),
                    seen.read,
                    seen.changed,
                    v.ino as i64,
                    v.size,
                    v.mtime_ns,
                    v.ctime_ns
                ],
            )
            .map_err(sql)?;
        self.versions.insert(rel.to_path_buf(), seen);
        Ok(())
    }
}
