//! The commit journal: `<state>/journal.sqlite`, written with `synchronous = FULL`,
//! so each step is on disk before the next one starts.
//!
//! A commit is one generation. Its row moves `journaled` (every path it will
//! touch, the original's metadata and the temporary name of each new file) →
//! `prepared` (pre-images copied and flushed; apply may begin) → `done`. On
//! start, the daemon rolls back every generation that is not `done`. Done
//! generations stay as long as an open scope reads through their pre-images.
//!
//! `restored` maps an original version to the version a rollback left in its
//! place (a copy has a new inode and ctime), so the conflict check does not
//! blame scopes that recorded the original.

use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::snapshot::{Entry, Meta};
use crate::sys::{self, Version};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenState {
    Journaled,
    Prepared,
    Done,
}

impl GenState {
    fn as_str(self) -> &'static str {
        match self {
            GenState::Journaled => "journaled",
            GenState::Prepared => "prepared",
            GenState::Done => "done",
        }
    }

    fn parse(s: &str) -> io::Result<Self> {
        match s {
            "journaled" => Ok(GenState::Journaled),
            "prepared" => Ok(GenState::Prepared),
            "done" => Ok(GenState::Done),
            _ => Err(io::Error::other(format!("journal: bad state {s}"))),
        }
    }
}

/// One path a commit touches.
#[derive(Clone, Debug)]
pub struct JournalEntry {
    pub path: PathBuf,
    /// The base entry before the commit; None if the commit creates the path.
    pub pre: Entry,
    /// Temporary name of the new file or symlink, beside its target.
    pub tmp: Option<PathBuf>,
}

/// A directory whose times a rollback restores (its entries change during apply).
#[derive(Clone, Debug)]
pub struct DirTimes {
    pub path: PathBuf,
    pub atime_ns: i64,
    pub mtime_ns: i64,
}

pub struct Journal {
    db: Connection,
}

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS gens (gen INTEGER PRIMARY KEY, scope TEXT NOT NULL, state TEXT NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS entries (
    gen INTEGER NOT NULL,
    path BLOB NOT NULL,
    present INTEGER NOT NULL,
    mode INTEGER NOT NULL,
    ino INTEGER NOT NULL,
    size INTEGER NOT NULL,
    uid INTEGER NOT NULL,
    gid INTEGER NOT NULL,
    atime_ns INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    ctime_ns INTEGER NOT NULL,
    tmp BLOB,
    PRIMARY KEY (gen, path)
) STRICT;
CREATE TABLE IF NOT EXISTS dirs (
    gen INTEGER NOT NULL,
    path BLOB NOT NULL,
    atime_ns INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    PRIMARY KEY (gen, path)
) STRICT;
CREATE TABLE IF NOT EXISTS restored (
    path BLOB NOT NULL,
    old_ino INTEGER NOT NULL,
    old_size INTEGER NOT NULL,
    old_mtime_ns INTEGER NOT NULL,
    old_ctime_ns INTEGER NOT NULL,
    new_ino INTEGER NOT NULL,
    new_size INTEGER NOT NULL,
    new_mtime_ns INTEGER NOT NULL,
    new_ctime_ns INTEGER NOT NULL,
    PRIMARY KEY (path, old_ino, old_size, old_mtime_ns, old_ctime_ns)
) STRICT;
";

fn sql(e: rusqlite::Error) -> io::Error {
    io::Error::other(e)
}

fn version(r: &rusqlite::Row, at: usize) -> rusqlite::Result<Version> {
    Ok(Version {
        ino: r.get::<_, i64>(at)? as u64,
        size: r.get(at + 1)?,
        mtime_ns: r.get(at + 2)?,
        ctime_ns: r.get(at + 3)?,
    })
}

impl Journal {
    pub fn open(path: &Path) -> io::Result<Self> {
        let db = Connection::open(path).map_err(sql)?;
        db.execute_batch(SCHEMA).map_err(sql)?;
        Ok(Journal { db })
    }

    fn meta(&self, key: &str) -> io::Result<u64> {
        let v: Option<i64> = self
            .db
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .map_err(sql)?;
        Ok(v.unwrap_or(0) as u64)
    }

    /// The generation the base is at: the last commit that finished.
    pub fn current(&self) -> io::Result<u64> {
        self.meta("current")
    }

    /// A new generation number; numbers never repeat, even after old generations are dropped.
    pub fn alloc(&self) -> io::Result<u64> {
        let generation = self.meta("last")?.max(self.current()?) + 1;
        self.db
            .execute(
                "INSERT INTO meta VALUES ('last', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [generation as i64],
            )
            .map_err(sql)?;
        Ok(generation)
    }

    /// Record a commit's intent in one transaction.
    pub fn begin(
        &mut self,
        generation: u64,
        scope: &str,
        entries: &[JournalEntry],
        dirs: &[DirTimes],
    ) -> io::Result<()> {
        let tx = self.db.transaction().map_err(sql)?;
        tx.execute(
            "INSERT INTO gens VALUES (?1, ?2, ?3)",
            params![generation as i64, scope, GenState::Journaled.as_str()],
        )
        .map_err(sql)?;
        {
            let mut st = tx
                .prepare("INSERT INTO entries VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)")
                .map_err(sql)?;
            for e in entries {
                let m = e.pre.unwrap_or(Meta {
                    mode: 0,
                    ino: 0,
                    size: 0,
                    uid: 0,
                    gid: 0,
                    atime_ns: 0,
                    mtime_ns: 0,
                    ctime_ns: 0,
                });
                st.execute(params![
                    generation as i64,
                    sys::path_bytes(&e.path),
                    e.pre.is_some(),
                    m.mode,
                    m.ino as i64,
                    m.size,
                    m.uid,
                    m.gid,
                    m.atime_ns,
                    m.mtime_ns,
                    m.ctime_ns,
                    e.tmp.as_deref().map(sys::path_bytes),
                ])
                .map_err(sql)?;
            }
            let mut st = tx.prepare("INSERT INTO dirs VALUES (?1, ?2, ?3, ?4)").map_err(sql)?;
            for d in dirs {
                st.execute(params![
                    generation as i64,
                    sys::path_bytes(&d.path),
                    d.atime_ns,
                    d.mtime_ns
                ])
                .map_err(sql)?;
            }
        }
        tx.commit().map_err(sql)
    }

    pub fn set_state(&mut self, generation: u64, state: GenState) -> io::Result<()> {
        let tx = self.db.transaction().map_err(sql)?;
        tx.execute(
            "UPDATE gens SET state = ?2 WHERE gen = ?1",
            params![generation as i64, state.as_str()],
        )
        .map_err(sql)?;
        if state == GenState::Done {
            tx.execute(
                "INSERT INTO meta VALUES ('current', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [generation as i64],
            )
            .map_err(sql)?;
        }
        tx.commit().map_err(sql)
    }

    /// Every generation on record, oldest first: (generation, scope id, state).
    pub fn gens(&self) -> io::Result<Vec<(u64, String, GenState)>> {
        let mut st = self
            .db
            .prepare("SELECT gen, scope, state FROM gens ORDER BY gen")
            .map_err(sql)?;
        let rows = st
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get(1)?, r.get::<_, String>(2)?))
            })
            .map_err(sql)?;
        rows.map(|r| {
            let (g, s, state) = r.map_err(sql)?;
            Ok((g, s, GenState::parse(&state)?))
        })
        .collect()
    }

    pub fn state(&self, generation: u64) -> io::Result<Option<GenState>> {
        let s: Option<String> = self
            .db
            .query_row("SELECT state FROM gens WHERE gen = ?1", [generation as i64], |r| {
                r.get(0)
            })
            .optional()
            .map_err(sql)?;
        s.map(|s| GenState::parse(&s)).transpose()
    }

    pub fn entries(&self, generation: u64) -> io::Result<Vec<JournalEntry>> {
        let mut st = self
            .db
            .prepare(
                "SELECT path, present, mode, ino, size, uid, gid, atime_ns, mtime_ns, ctime_ns, tmp
                 FROM entries WHERE gen = ?1",
            )
            .map_err(sql)?;
        let rows = st
            .query_map([generation as i64], |r| {
                let present: bool = r.get(1)?;
                let pre = present
                    .then(|| -> rusqlite::Result<Meta> {
                        Ok(Meta {
                            mode: r.get(2)?,
                            ino: r.get::<_, i64>(3)? as u64,
                            size: r.get(4)?,
                            uid: r.get(5)?,
                            gid: r.get(6)?,
                            atime_ns: r.get(7)?,
                            mtime_ns: r.get(8)?,
                            ctime_ns: r.get(9)?,
                        })
                    })
                    .transpose()?;
                Ok(JournalEntry {
                    path: sys::path_from(r.get(0)?),
                    pre,
                    tmp: r.get::<_, Option<Vec<u8>>>(10)?.map(sys::path_from),
                })
            })
            .map_err(sql)?;
        rows.collect::<Result<_, _>>().map_err(sql)
    }

    pub fn dirs(&self, generation: u64) -> io::Result<Vec<DirTimes>> {
        let mut st = self
            .db
            .prepare("SELECT path, atime_ns, mtime_ns FROM dirs WHERE gen = ?1")
            .map_err(sql)?;
        let rows = st
            .query_map([generation as i64], |r| {
                Ok(DirTimes {
                    path: sys::path_from(r.get(0)?),
                    atime_ns: r.get(1)?,
                    mtime_ns: r.get(2)?,
                })
            })
            .map_err(sql)?;
        rows.collect::<Result<_, _>>().map_err(sql)
    }

    /// Drop a generation's rows (rolled back, or no longer read by any scope).
    pub fn forget(&mut self, generation: u64) -> io::Result<()> {
        let tx = self.db.transaction().map_err(sql)?;
        for table in ["gens", "entries", "dirs"] {
            tx.execute(&format!("DELETE FROM {table} WHERE gen = ?1"), [generation as i64])
                .map_err(sql)?;
        }
        tx.commit().map_err(sql)
    }

    pub fn clear_scope(&self, generation: u64) -> io::Result<()> {
        self.db
            .execute("UPDATE gens SET scope = '' WHERE gen = ?1", [generation as i64])
            .map_err(sql)?;
        Ok(())
    }

    pub fn add_restored(&self, path: &Path, old: Version, new: Version) -> io::Result<()> {
        self.db
            .execute(
                "INSERT OR REPLACE INTO restored VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    sys::path_bytes(path),
                    old.ino as i64,
                    old.size,
                    old.mtime_ns,
                    old.ctime_ns,
                    new.ino as i64,
                    new.size,
                    new.mtime_ns,
                    new.ctime_ns
                ],
            )
            .map_err(sql)?;
        Ok(())
    }

    pub fn restored(&self) -> io::Result<Vec<(PathBuf, Version, Version)>> {
        let mut st = self.db.prepare("SELECT * FROM restored").map_err(sql)?;
        let rows = st
            .query_map([], |r| Ok((sys::path_from(r.get(0)?), version(r, 1)?, version(r, 5)?)))
            .map_err(sql)?;
        rows.collect::<Result<_, _>>().map_err(sql)
    }

    pub fn clear_restored(&self) -> io::Result<()> {
        self.db.execute("DELETE FROM restored", []).map_err(sql)?;
        Ok(())
    }
}
