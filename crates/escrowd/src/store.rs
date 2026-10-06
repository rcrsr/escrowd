//! Per-scope store: `<state>/scopes/<id>/upper/` holds the scope's files,
//! `<state>/scopes/<id>/meta.sqlite` its metadata:
//!
//! - whiteouts and opaque directories (which base entries the scope hides),
//! - base versions of every path the scope read or changed (the conflict check, 1.4),
//!   and the reads the gate denied (both go into the change set),
//! - pinned inode numbers: entries whose inode cannot be derived from the base
//!   path they now sit at (upper-only files, renamed files), so inode numbers
//!   survive a daemon restart,
//! - the processes that changed each path (`writers`) and what each process was
//!   (`procs`: program, binary identity, arguments, parent),
//! - scope name, labels, index, lifecycle state, the base generation it opened at
//!   (its snapshot), the upper-only inode counter and the SHA-256 of the scope's
//!   token (project views only; the token itself is never stored).
//!
//! Hot lookups (hidden, pinned inode) hit in-memory copies; every change is
//! written through to SQLite before the FUSE reply, except pins of upper-only
//! numbers, the upper-only counter, first reads and writers: they are written with the
//! next change to a base-derived pin (a rename of a base entry), every
//! `PIN_BATCH` changes, on close and at shutdown, each time as one consistent
//! snapshot. A daemon killed in between gives those entries new numbers after its
//! restart (no process sees both, since a restart leaves the old mount dead) and
//! drops those reads and writers from the change set; the versions of changed paths, which
//! the conflict check uses, are always written.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use crate::policy::Tier;
use crate::proc::{Info, Proc};
use crate::review::{Decision, TierReview};
use crate::sys::{self, Version};
use crate::views::UPPER_BIT;

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

/// A closed scope waiting for reviewers: the tiers still to review, cheapest first,
/// whether its session's next scope waits for the verdict, and the verdicts so far.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hold {
    pub tiers: Vec<Tier>,
    pub wait: bool,
    /// Milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// The strictest verdict so far (commit before any review).
    pub verdict: Decision,
    pub reviews: Vec<TierReview>,
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
    /// SHA-256 of the scope's token, hex; None: no token (the unscoped scope, views
    /// of other roots, scopes opened before protocol 6).
    token_sha256: Option<String>,
    dir: PathBuf,
    /// Shared lookups take the store's read lock; the connection has its own lock.
    db: Mutex<Connection>,
    whiteouts: HashSet<PathBuf>,
    opaque: HashSet<PathBuf>,
    pins: HashMap<PathBuf, u64>,
    versions: HashMap<PathBuf, Seen>,
    denied: HashSet<PathBuf>,
    next_upper_ino: u64,
    /// Pins and the counter not written yet: each path's current pin (or its absence) is.
    unwritten_pins: HashSet<PathBuf>,
    unwritten_counter: bool,
    /// First reads not written yet, with the version read.
    unwritten_reads: HashMap<PathBuf, Version>,
    pub state: ScopeState,
    /// The session that orders the scope among its agent's scopes ("": none).
    pub session: String,
    hold: Option<Hold>,
    /// Path -> the processes that changed it (ids in `procs`).
    writers: HashMap<PathBuf, BTreeSet<u64>>,
    procs: HashMap<u64, Info>,
    /// Writers added and not written yet; paths whose writers moved or went (their
    /// current set, or its absence, is written); processes not written yet.
    added_writers: Vec<(PathBuf, u64)>,
    unwritten_writers: HashSet<PathBuf>,
    unwritten_procs: Vec<u64>,
}

const PIN_BATCH: usize = 4096;

// synchronous first, so the switch of a new database to WAL runs under it.
const PRAGMAS: &str = "
PRAGMA synchronous = NORMAL;
PRAGMA journal_mode = WAL;
";

const SCHEMA: &str = "
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
CREATE TABLE IF NOT EXISTS writers (
    path BLOB NOT NULL,
    proc INTEGER NOT NULL,
    PRIMARY KEY (path, proc)
) STRICT, WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS procs (
    id INTEGER PRIMARY KEY,
    pid INTEGER NOT NULL,
    program BLOB NOT NULL,
    dev INTEGER NOT NULL,
    ino INTEGER NOT NULL,
    args BLOB NOT NULL,
    parent INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS reviews (
    seq INTEGER PRIMARY KEY,
    tier TEXT NOT NULL,
    verdict TEXT NOT NULL,
    override INTEGER NOT NULL,
    reasons BLOB NOT NULL
) STRICT;
";

/// Strings joined by NUL (none: empty), as the store keeps arguments and reasons.
fn split_nul(b: &[u8]) -> Vec<String> {
    if b.is_empty() {
        return Vec::new();
    }
    b.split(|b| *b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect()
}

fn sql(e: rusqlite::Error) -> io::Error {
    io::Error::other(e)
}

fn write_version(db: &Connection, rel: &Path, seen: Seen, v: Version) -> io::Result<()> {
    exec(
        db,
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
    .map(|_| ())
}

/// Run one statement, prepared once per connection.
fn exec(db: &Connection, q: &str, p: impl rusqlite::Params) -> io::Result<usize> {
    db.prepare_cached(q).and_then(|mut st| st.execute(p)).map_err(sql)
}

/// `path = p OR path starts with p/` for BLOB paths, as an index range: `?1` is p,
/// `?2` is p + "/" and `?3` is p + "0" (the byte after '/').
const UNDER: &str = "(path = ?1 OR (path >= ?2 AND path < ?3))";

fn under_params(p: &Path) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let exact = sys::path_bytes(p).to_vec();
    let (mut lo, mut hi) = (exact.clone(), exact.clone());
    lo.push(b'/');
    hi.push(b'/' + 1);
    (exact, lo, hi)
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
    /// Create a new scope directory; `token_sha256`: the hash of its token, if it has one.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        scopes_dir: &Path,
        id: &str,
        name: &str,
        idx: u64,
        since: u64,
        readonly: bool,
        labels: &HashMap<String, String>,
        token_sha256: Option<&str>,
        session: &str,
    ) -> io::Result<Self> {
        let dir = scopes_dir.join(id);
        fs::create_dir(&dir)?;
        fs::create_dir(dir.join("upper"))?;
        let mut db = Connection::open(dir.join("meta.sqlite")).map_err(sql)?;
        db.execute_batch(PRAGMAS).map_err(sql)?;
        // One transaction: a scope opens with one write.
        let tx = db.transaction().map_err(sql)?;
        tx.execute_batch(SCHEMA).map_err(sql)?;
        tx.execute(
            "INSERT INTO meta VALUES ('name', ?1), ('idx', ?2), ('since', ?3), ('readonly', ?4), ('next_upper_ino', 0), ('state', 'open')",
            params![name, idx as i64, since as i64, readonly as i64],
        )
        .map_err(sql)?;
        if let Some(t) = token_sha256 {
            tx.execute("INSERT INTO meta VALUES ('token_sha256', ?1)", [t])
                .map_err(sql)?;
        }
        if !session.is_empty() {
            tx.execute("INSERT INTO meta VALUES ('session', ?1)", [session])
                .map_err(sql)?;
        }
        for (k, v) in labels {
            tx.execute("INSERT INTO labels VALUES (?1, ?2)", params![k, v])
                .map_err(sql)?;
        }
        tx.commit().map_err(sql)?;
        Self::from_db(dir, id, db)
    }

    /// Reopen an existing scope directory, e.g. after a daemon restart.
    pub fn load(scopes_dir: &Path, id: &str) -> io::Result<Self> {
        let dir = scopes_dir.join(id);
        let db = Connection::open(dir.join("meta.sqlite")).map_err(sql)?;
        db.execute_batch(PRAGMAS).map_err(sql)?;
        db.execute_batch(SCHEMA).map_err(sql)?;
        Self::from_db(dir, id, db)
    }

    fn from_db(dir: PathBuf, id: &str, db: Connection) -> io::Result<Self> {
        db.set_prepared_statement_cache_capacity(32);
        // A store is closed when its scope is dropped (and deleted) or the daemon
        // stops; a checkpoint then is wasted work, and the next open replays the WAL.
        db.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
            .map_err(sql)?;
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
        let token_sha256 = db
            .query_row("SELECT value FROM meta WHERE key = 'token_sha256'", [], |r| r.get(0))
            .optional()
            .map_err(sql)?;
        let text = |k: &str| -> io::Result<Option<String>> {
            db.query_row("SELECT value FROM meta WHERE key = ?1", [k], |r| r.get(0))
                .optional()
                .map_err(sql)
        };
        let session = text("session")?.unwrap_or_default();
        let hold = match text("hold_tiers")? {
            None => None,
            Some(t) => {
                let tier = |t: &str| Tier::parse(t).ok_or_else(|| io::Error::other(format!("bad held tier {t}")));
                let decision =
                    |d: &str| Decision::parse(d).ok_or_else(|| io::Error::other(format!("bad held verdict {d}")));
                let mut st = db
                    .prepare("SELECT tier, verdict, override, reasons FROM reviews ORDER BY seq")
                    .map_err(sql)?;
                let rows = st
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, Vec<u8>>(3)?,
                        ))
                    })
                    .map_err(sql)?;
                let mut reviews = Vec::new();
                for row in rows {
                    let (t, v, o, reasons) = row.map_err(sql)?;
                    reviews.push(TierReview {
                        tier: tier(&t)?,
                        verdict: decision(&v)?,
                        over: o != 0,
                        reasons: split_nul(&reasons),
                    });
                }
                Some(Hold {
                    tiers: t.split(',').map(tier).collect::<io::Result<_>>()?,
                    wait: text("hold_wait")?.is_some_and(|w| w == "1"),
                    at_ms: text("hold_at")?.and_then(|t| t.parse().ok()).unwrap_or(0),
                    verdict: decision(&text("hold_verdict")?.unwrap_or_else(|| "commit".into()))?,
                    reviews,
                })
            }
        };
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
        let mut writers: HashMap<PathBuf, BTreeSet<u64>> = HashMap::new();
        {
            let mut st = db.prepare("SELECT path, proc FROM writers").map_err(sql)?;
            let rows = st
                .query_map([], |r| Ok((sys::path_from(r.get(0)?), r.get::<_, i64>(1)? as u64)))
                .map_err(sql)?;
            for row in rows {
                let (p, id) = row.map_err(sql)?;
                writers.entry(p).or_default().insert(id);
            }
        }
        let procs = {
            let mut st = db
                .prepare("SELECT id, pid, program, dev, ino, args, parent FROM procs")
                .map_err(sql)?;
            let rows = st
                .query_map([], |r| {
                    let args: Vec<u8> = r.get(5)?;
                    let info = Info {
                        id: r.get::<_, i64>(0)? as u64,
                        pid: r.get(1)?,
                        program: sys::path_from(r.get(2)?),
                        dev: r.get::<_, i64>(3)? as u64,
                        ino: r.get::<_, i64>(4)? as u64,
                        args: split_nul(&args),
                        parent: r.get::<_, i64>(6)? as u64,
                    };
                    Ok((info.id, info))
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
            token_sha256,
            dir,
            db: Mutex::new(db),
            whiteouts,
            opaque,
            pins,
            versions,
            denied,
            next_upper_ino,
            unwritten_pins: HashSet::new(),
            unwritten_counter: false,
            unwritten_reads: HashMap::new(),
            state,
            session,
            hold,
            writers,
            procs,
            added_writers: Vec::new(),
            unwritten_writers: HashSet::new(),
            unwritten_procs: Vec::new(),
        })
    }

    /// Run `f`'s changes in one transaction (the store's own groups nest as savepoints).
    pub fn batch<T>(&mut self, f: impl FnOnce(&mut Self) -> io::Result<T>) -> io::Result<T> {
        self.db.get_mut().unwrap().execute_batch("BEGIN").map_err(sql)?;
        match f(self) {
            Ok(v) => {
                self.db.get_mut().unwrap().execute_batch("COMMIT").map_err(sql)?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.db.get_mut().unwrap().execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Whether `sha256` (hex, of a caller's token) matches the scope's token; a scope
    /// without a token takes any.
    pub fn token_matches(&self, sha256: &str) -> bool {
        self.token_sha256.as_deref().is_none_or(|t| t == sha256)
    }

    pub fn upper_dir(&self) -> PathBuf {
        self.dir.join("upper")
    }

    /// Move the scope's directory (its files and metadata) to `trash`, whose
    /// contents the caller deletes; returns the new path.
    pub fn discard_to(&self, trash: &Path) -> io::Result<PathBuf> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let to = trash.join(format!("{}.{nanos}", self.id));
        fs::rename(&self.dir, &to)?;
        Ok(to)
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
        self.flush()?;
        let v = if state == ScopeState::Closed { "closed" } else { "open" };
        exec(
            self.db.get_mut().unwrap(),
            "UPDATE meta SET value = ?1 WHERE key = 'state'",
            [v],
        )?;
        self.state = state;
        Ok(())
    }

    pub fn hold(&self) -> Option<&Hold> {
        self.hold.as_ref()
    }

    /// Hold the scope for reviewers, or (None) release it; written at once.
    pub fn set_hold(&mut self, hold: Option<Hold>) -> io::Result<()> {
        let db = self.db.get_mut().unwrap();
        let tx = db.transaction().map_err(sql)?;
        tx.execute(
            "DELETE FROM meta WHERE key IN ('hold_tiers', 'hold_wait', 'hold_at', 'hold_verdict')",
            [],
        )
        .map_err(sql)?;
        tx.execute("DELETE FROM reviews", []).map_err(sql)?;
        if let Some(h) = &hold {
            let tiers: Vec<&str> = h.tiers.iter().map(|t| t.as_str()).collect();
            tx.execute(
                "INSERT INTO meta VALUES ('hold_tiers', ?1), ('hold_wait', ?2), ('hold_at', ?3), ('hold_verdict', ?4)",
                params![
                    tiers.join(","),
                    if h.wait { "1" } else { "0" },
                    h.at_ms.to_string(),
                    h.verdict.as_str()
                ],
            )
            .map_err(sql)?;
            for r in &h.reviews {
                tx.execute(
                    "INSERT INTO reviews (tier, verdict, override, reasons) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        r.tier.as_str(),
                        r.verdict.as_str(),
                        r.over as i64,
                        r.reasons.join("\0").into_bytes()
                    ],
                )
                .map_err(sql)?;
            }
        }
        tx.commit().map_err(sql)?;
        self.hold = hold;
        Ok(())
    }

    /// SHA-256 of the scope's token, hex; None: it has none.
    pub fn token_sha256(&self) -> Option<&str> {
        self.token_sha256.as_deref()
    }

    pub fn labels(&self) -> io::Result<HashMap<String, String>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare("SELECT key, value FROM labels").map_err(sql)?;
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
        exec(
            self.db.get_mut().unwrap(),
            "INSERT OR IGNORE INTO denied VALUES (?1)",
            [sys::path_bytes(rel)],
        )?;
        self.denied.insert(rel.to_path_buf());
        Ok(())
    }

    pub fn is_opaque(&self, rel: &Path) -> bool {
        self.opaque.contains(rel)
    }

    pub fn add_whiteout(&mut self, rel: &Path) -> io::Result<()> {
        exec(
            self.db.get_mut().unwrap(),
            "INSERT OR IGNORE INTO whiteouts VALUES (?1)",
            [sys::path_bytes(rel)],
        )?;
        self.whiteouts.insert(rel.to_path_buf());
        Ok(())
    }

    /// Drop a whiteout; returns whether there was one.
    pub fn remove_whiteout(&mut self, rel: &Path) -> io::Result<bool> {
        if !self.whiteouts.contains(rel) {
            return Ok(false);
        }
        exec(
            self.db.get_mut().unwrap(),
            "DELETE FROM whiteouts WHERE path = ?1",
            [sys::path_bytes(rel)],
        )?;
        self.whiteouts.remove(rel);
        Ok(true)
    }

    pub fn add_opaque(&mut self, rel: &Path) -> io::Result<()> {
        exec(
            self.db.get_mut().unwrap(),
            "INSERT OR IGNORE INTO opaque VALUES (?1)",
            [sys::path_bytes(rel)],
        )?;
        self.opaque.insert(rel.to_path_buf());
        Ok(())
    }

    /// A directory at `rel` was removed: forget whiteouts below it and opaque marks at or below it.
    pub fn clear_below(&mut self, rel: &Path) -> io::Result<()> {
        let (exact, lo, hi) = under_params(rel);
        let tx = self.db.get_mut().unwrap().savepoint().map_err(sql)?;
        exec(
            &tx,
            "DELETE FROM whiteouts WHERE path >= ?1 AND path < ?2",
            params![lo, hi],
        )?;
        exec(
            &tx,
            &format!("DELETE FROM opaque WHERE {UNDER}"),
            params![exact, lo, hi],
        )?;
        tx.commit().map_err(sql)?;
        self.whiteouts.retain(|p| !p.starts_with(rel) || p == rel);
        self.opaque.retain(|p| !p.starts_with(rel));
        Ok(())
    }

    pub fn pin(&self, rel: &Path) -> Option<u64> {
        self.pins.get(rel).copied()
    }

    /// Write the pins changed since the last flush, and the counter, in one transaction.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.unwritten_pins.is_empty()
            && !self.unwritten_counter
            && self.unwritten_reads.is_empty()
            && self.added_writers.is_empty()
            && self.unwritten_writers.is_empty()
            && self.unwritten_procs.is_empty()
        {
            return Ok(());
        }
        let tx = self.db.get_mut().unwrap().savepoint().map_err(sql)?;
        for p in &self.unwritten_pins {
            match self.pins.get(p) {
                Some(ino) => exec(
                    &tx,
                    "INSERT OR REPLACE INTO pins VALUES (?1, ?2)",
                    params![sys::path_bytes(p), *ino as i64],
                ),
                None => exec(&tx, "DELETE FROM pins WHERE path = ?1", [sys::path_bytes(p)]),
            }?;
        }
        if self.unwritten_counter {
            exec(
                &tx,
                "UPDATE meta SET value = ?1 WHERE key = 'next_upper_ino'",
                [self.next_upper_ino as i64],
            )?;
        }
        for (p, v) in &self.unwritten_reads {
            write_version(
                &tx,
                p,
                Seen {
                    read: true,
                    changed: false,
                },
                *v,
            )?;
        }
        for id in &self.unwritten_procs {
            let p = &self.procs[id];
            exec(
                &tx,
                "INSERT OR IGNORE INTO procs VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    p.id as i64,
                    p.pid,
                    sys::path_bytes(&p.program),
                    p.dev as i64,
                    p.ino as i64,
                    p.args.join("\0").into_bytes(),
                    p.parent as i64
                ],
            )?;
        }
        for (path, id) in &self.added_writers {
            if !self.unwritten_writers.contains(path) {
                exec(
                    &tx,
                    "INSERT OR IGNORE INTO writers VALUES (?1, ?2)",
                    params![sys::path_bytes(path), *id as i64],
                )?;
            }
        }
        for path in &self.unwritten_writers {
            let bytes = sys::path_bytes(path);
            exec(&tx, "DELETE FROM writers WHERE path = ?1", [bytes])?;
            for id in self.writers.get(path).into_iter().flatten() {
                exec(&tx, "INSERT INTO writers VALUES (?1, ?2)", params![bytes, *id as i64])?;
            }
        }
        tx.commit().map_err(sql)?;
        self.unwritten_pins.clear();
        self.unwritten_counter = false;
        self.unwritten_reads.clear();
        self.added_writers.clear();
        self.unwritten_writers.clear();
        self.unwritten_procs.clear();
        Ok(())
    }

    /// `paths` changed their pins; a change to a base-derived number (`durable`) is
    /// written now, so a rename of a base file survives a crash.
    fn pins_changed(&mut self, paths: impl IntoIterator<Item = PathBuf>, durable: bool) -> io::Result<()> {
        self.unwritten_pins.extend(paths);
        self.maybe_flush(durable)
    }

    fn maybe_flush(&mut self, now: bool) -> io::Result<()> {
        let unwritten = self.unwritten_pins.len()
            + self.unwritten_reads.len()
            + self.added_writers.len()
            + self.unwritten_writers.len();
        if now || unwritten >= PIN_BATCH {
            self.flush()
        } else {
            Ok(())
        }
    }

    pub fn set_pin(&mut self, rel: &Path, ino: u64) -> io::Result<()> {
        self.set_pins(&[(rel.to_path_buf(), ino)])
    }

    pub fn set_pins(&mut self, pins: &[(PathBuf, u64)]) -> io::Result<()> {
        self.pins.extend(pins.iter().cloned());
        let durable = pins.iter().any(|(_, ino)| ino & UPPER_BIT == 0);
        self.pins_changed(pins.iter().map(|(p, _)| p.clone()), durable)
    }

    /// Remove and return the pins at `rel` and, for a directory, under it.
    fn take_pins(&mut self, rel: &Path, dir: bool) -> Vec<(PathBuf, u64)> {
        if !dir {
            return self.pins.remove_entry(rel).into_iter().collect();
        }
        let keys: Vec<PathBuf> = self.pins.keys().filter(|p| p.starts_with(rel)).cloned().collect();
        keys.into_iter().filter_map(|k| self.pins.remove_entry(&k)).collect()
    }

    /// Forget pins at or under `rel` (the entry was removed; `dir`: it was a directory).
    pub fn unpin_below(&mut self, rel: &Path, dir: bool) -> io::Result<()> {
        let gone = self.take_pins(rel, dir);
        let durable = gone.iter().any(|(_, ino)| ino & UPPER_BIT == 0);
        self.pins_changed(gone.into_iter().map(|(p, _)| p), durable)
    }

    /// Pins at or under `from` move under `to`; pins under `to` (the replaced target) go.
    /// `from_dir`, `to_dir`: which of the two are directories.
    pub fn move_pins(&mut self, from: &Path, to: &Path, from_dir: bool, to_dir: bool) -> io::Result<()> {
        let gone = self.take_pins(to, to_dir);
        let moving = self.take_pins(from, from_dir);
        let durable = gone.iter().chain(&moving).any(|(_, ino)| ino & UPPER_BIT == 0);
        let mut changed: Vec<PathBuf> = gone.into_iter().map(|(p, _)| p).collect();
        for (old, ino) in moving {
            let new = moved(&old, from, to).expect("taken on prefix");
            changed.push(old);
            changed.push(new.clone());
            self.pins.insert(new, ino);
        }
        self.pins_changed(changed, durable)
    }

    /// Next upper-only inode counter value; written with its pin.
    pub fn alloc_upper_ino(&mut self) -> u64 {
        self.next_upper_ino += 1;
        self.unwritten_counter = true;
        self.next_upper_ino
    }

    /// The base version of `rel` the scope first saw, if it read or changed it.
    pub fn base_version(&self, rel: &Path) -> io::Result<Option<Version>> {
        if !self.versions.contains_key(rel) {
            return Ok(None);
        }
        if let Some(v) = self.unwritten_reads.get(rel) {
            return Ok(Some(*v));
        }
        self.db
            .lock()
            .unwrap()
            .prepare_cached("SELECT ino, size, mtime_ns, ctime_ns FROM versions WHERE path = ?1")
            .and_then(|mut st| {
                st.query_row([sys::path_bytes(rel)], |r| {
                    Ok(Version {
                        ino: r.get::<_, i64>(0)? as u64,
                        size: r.get(1)?,
                        mtime_ns: r.get(2)?,
                        ctime_ns: r.get(3)?,
                    })
                })
                .optional()
            })
            .map_err(sql)
    }

    /// Whether `rel` already has `kind` recorded (a shared lock suffices to check).
    pub fn has_version(&self, rel: &Path, kind: VersionKind) -> bool {
        self.versions.get(rel).is_some_and(|s| match kind {
            VersionKind::Read => s.read,
            VersionKind::Changed => s.changed,
        })
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
        self.versions.insert(rel.to_path_buf(), seen);
        if old.is_none() && kind == VersionKind::Read {
            self.unwritten_reads.insert(rel.to_path_buf(), v);
            return self.maybe_flush(false);
        }
        // A change keeps the version of a first read not written yet.
        let first = self.unwritten_reads.remove(rel).unwrap_or(v);
        write_version(self.db.get_mut().unwrap(), rel, seen, first)
    }

    /// Whether `proc` is already a writer of `rel` (a shared lock suffices to check).
    pub fn has_writer(&self, rel: &Path, proc: u64) -> bool {
        self.writers.get(rel).is_some_and(|w| w.contains(&proc))
    }

    /// `p` changed `rel`; it and its ancestors are recorded once. Written behind.
    pub fn add_writer(&mut self, rel: &Path, p: &Arc<Proc>) -> io::Result<()> {
        for a in p.chain() {
            if self.procs.contains_key(&a.info.id) {
                break; // and so are its ancestors
            }
            self.procs.insert(a.info.id, a.info.clone());
            self.unwritten_procs.push(a.info.id);
        }
        if self.writers.entry(rel.to_path_buf()).or_default().insert(p.info.id) {
            self.added_writers.push((rel.to_path_buf(), p.info.id));
        }
        self.maybe_flush(false)
    }

    /// The writers at `from` (and under it, a directory) move to `to`, joining any there.
    pub fn move_writers(&mut self, from: &Path, to: &Path, dir: bool) -> io::Result<()> {
        let moving: Vec<PathBuf> = if dir {
            self.writers.keys().filter(|p| p.starts_with(from)).cloned().collect()
        } else {
            self.writers
                .contains_key(from)
                .then(|| from.to_path_buf())
                .into_iter()
                .collect()
        };
        for old in moving {
            let ids = self.writers.remove(&old).expect("listed");
            let new = moved(&old, from, to).expect("filtered on prefix");
            self.writers.entry(new.clone()).or_default().extend(ids);
            self.unwritten_writers.insert(old);
            self.unwritten_writers.insert(new);
        }
        self.maybe_flush(false)
    }

    /// Forget the writers of `rel` (an entry the scope made, now removed).
    pub fn drop_writers(&mut self, rel: &Path) -> io::Result<()> {
        if self.writers.remove(rel).is_some() {
            self.unwritten_writers.insert(rel.to_path_buf());
        }
        self.maybe_flush(false)
    }

    /// The processes that changed `rel`.
    pub fn writers(&self, rel: &Path) -> Option<&BTreeSet<u64>> {
        self.writers.get(rel)
    }

    pub fn proc(&self, id: u64) -> Option<&Info> {
        self.procs.get(&id)
    }
}
