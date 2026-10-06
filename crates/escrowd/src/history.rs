//! Decided scopes, kept after their stores are gone: `<state>/history.sqlite`.
//!
//! A reviewer of a held scope gets its session's earlier decisions, and a client
//! following a held scope (`AwaitDecision`) gets its outcome even after the scope was
//! dropped or the daemon restarted. Each row is one decision of a scope that has a
//! session or was held, as the RPC layer encoded it (a `Decided` message).

use std::io;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

/// Decisions kept per session, and for scopes without one.
const KEEP: i64 = 100;

const SCHEMA: &str = "
PRAGMA synchronous = NORMAL;
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS decided (
    seq INTEGER PRIMARY KEY,
    scope TEXT NOT NULL,
    session TEXT NOT NULL,
    token_sha256 TEXT,
    entry BLOB NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS decided_scope ON decided (scope, seq);
CREATE INDEX IF NOT EXISTS decided_session ON decided (session, seq);
";

pub struct History {
    db: Mutex<Connection>,
}

/// A scope's latest decision.
pub struct Latest {
    pub seq: u64,
    /// The scope's token's SHA-256, hex; None: it had none.
    pub token_sha256: Option<String>,
    pub entry: Vec<u8>,
}

fn sql(e: rusqlite::Error) -> io::Error {
    io::Error::other(e)
}

impl History {
    pub fn open(state: &Path) -> io::Result<Self> {
        let db = Connection::open(state.join("history.sqlite")).map_err(sql)?;
        db.execute_batch(SCHEMA).map_err(sql)?;
        Ok(History { db: Mutex::new(db) })
    }

    /// Record a decision of `scope`; keeps the newest `KEEP` of its session.
    pub fn record(&self, scope: &str, session: &str, token_sha256: Option<&str>, entry: &[u8]) -> io::Result<u64> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction().map_err(sql)?;
        tx.execute(
            "INSERT INTO decided (scope, session, token_sha256, entry) VALUES (?1, ?2, ?3, ?4)",
            params![scope, session, token_sha256, entry],
        )
        .map_err(sql)?;
        let seq = tx.last_insert_rowid() as u64;
        tx.execute(
            "DELETE FROM decided WHERE session = ?1 AND seq <= (
                SELECT seq FROM decided WHERE session = ?1 ORDER BY seq DESC LIMIT 1 OFFSET ?2)",
            params![session, KEEP],
        )
        .map_err(sql)?;
        tx.commit().map_err(sql)?;
        Ok(seq)
    }

    /// The latest decision of `scope`, if one is kept.
    pub fn latest(&self, scope: &str) -> io::Result<Option<Latest>> {
        let db = self.db.lock().unwrap();
        db.query_row(
            "SELECT seq, token_sha256, entry FROM decided WHERE scope = ?1 ORDER BY seq DESC LIMIT 1",
            [scope],
            |r| {
                Ok(Latest {
                    seq: r.get::<_, i64>(0)? as u64,
                    token_sha256: r.get(1)?,
                    entry: r.get(2)?,
                })
            },
        )
        .optional()
        .map_err(sql)
    }

    /// The newest `limit` decisions of `session`, oldest first; none for "".
    pub fn session(&self, session: &str, limit: usize) -> io::Result<Vec<Vec<u8>>> {
        if session.is_empty() {
            return Ok(Vec::new());
        }
        let db = self.db.lock().unwrap();
        let mut st = db
            .prepare("SELECT entry FROM decided WHERE session = ?1 ORDER BY seq DESC LIMIT ?2")
            .map_err(sql)?;
        let rows = st
            .query_map(params![session, limit as i64], |r| r.get::<_, Vec<u8>>(0))
            .map_err(sql)?;
        let mut out = rows.collect::<Result<Vec<_>, _>>().map_err(sql)?;
        out.reverse();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_newest_per_session_and_each_scopes_latest() {
        let dir = std::env::temp_dir().join(format!("escrowd-history-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let h = History::open(&dir).unwrap();
        for i in 0..KEEP + 5 {
            h.record(&format!("s{i}"), "a", Some("t"), &[i as u8]).unwrap();
        }
        h.record("x", "", None, b"first").unwrap();
        let seq = h.record("x", "", None, b"second").unwrap();
        let all = h.session("a", 1000).unwrap();
        assert_eq!(all.len(), KEEP as usize);
        assert_eq!(all[0], vec![5u8]);
        assert_eq!(
            h.session("a", 2).unwrap(),
            vec![vec![KEEP as u8 + 3], vec![KEEP as u8 + 4]]
        );
        assert!(h.session("", 10).unwrap().is_empty());
        let latest = h.latest("x").unwrap().unwrap();
        assert_eq!(
            (latest.seq, latest.entry, latest.token_sha256),
            (seq, b"second".to_vec(), None)
        );
        assert!(h.latest("s0").unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
