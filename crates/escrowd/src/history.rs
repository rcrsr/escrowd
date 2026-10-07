//! Decided scopes, kept after their stores are gone: `<state>/history.sqlite`.
//!
//! A reviewer of a held scope gets its session's earlier decisions, and a client
//! following a held scope (`AwaitDecision`) gets its outcome even after the scope was
//! dropped or the daemon restarted. Each row is one decision of a scope that has a
//! session or was held, in the record format of `record.rs`; this module stores the
//! bytes and does not read them.
//!
//! A decision is written ahead, pending, with the outcome it intends, then settled
//! with the outcome it got. A daemon killed in between leaves a pending row, which the
//! next start settles from what survived (`Fate`): no decision is applied without one
//! kept, and none is kept that was not applied.

use std::io;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::db::{self, Durability, sql};

/// Decisions kept per session, and for scopes without one.
const KEEP: i64 = 100;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS decided (
    seq INTEGER PRIMARY KEY,
    scope TEXT NOT NULL,
    session TEXT NOT NULL,
    token_sha256 TEXT,
    entry BLOB NOT NULL,
    pending INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX IF NOT EXISTS decided_scope ON decided (scope, seq);
CREATE INDEX IF NOT EXISTS decided_session ON decided (session, seq);
";

pub struct History {
    db: Mutex<Connection>,
    settled_as: Settle,
}

/// A pending record as its scope's fate shows it applied; None: not applied.
pub type Settle = fn(&[u8], Fate) -> Option<Vec<u8>>;

/// What a pending decision's scope shows at the next start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// Its commit finished: the journal has it.
    Committed,
    /// Open again: a return reopened it.
    Open,
    /// Still closed (held or not): nothing was applied.
    Closed,
    /// Dropped: a discard, or a commit's conflict under `conflict.verdict: discard`.
    Gone,
}

/// A scope's latest decision.
pub struct Latest {
    pub seq: u64,
    /// The scope's token's SHA-256, hex; None: it had none.
    pub token_sha256: Option<String>,
    pub entry: Vec<u8>,
}

/// Version 1: the table with pending rows. A history from 3.4 to 3.6 has it
/// without the column.
fn v1_pending(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch(SCHEMA)?;
    if !db::has_column(tx, "decided", "pending")? {
        tx.execute_batch("ALTER TABLE decided ADD COLUMN pending INTEGER NOT NULL DEFAULT 0")?;
    }
    Ok(())
}

impl History {
    /// `settle` reads a pending record at start (`resolve`).
    pub fn open(state: &Path, settle: Settle) -> io::Result<Self> {
        // Full: a decision is applied only after its pending row is on disk, so the
        // row must survive whatever the commit survives.
        let mut db = db::open(&state.join("history.sqlite"), Durability::Full)?;
        db::migrate(&mut db, "history.sqlite", &[v1_pending])?;
        Ok(History {
            db: Mutex::new(db),
            settled_as: settle,
        })
    }

    /// Record a decision of `scope`; keeps the newest `KEEP` of its session.
    pub fn record(&self, scope: &str, session: &str, token_sha256: Option<&str>, entry: &[u8]) -> io::Result<u64> {
        let seq = self.intend(scope, session, token_sha256, entry)?;
        self.settle(seq, entry)?;
        Ok(seq)
    }

    /// Write a decision of `scope` ahead, pending, with the outcome it intends. Readers
    /// do not see it until `settle`.
    pub fn intend(&self, scope: &str, session: &str, token_sha256: Option<&str>, entry: &[u8]) -> io::Result<u64> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO decided (scope, session, token_sha256, entry, pending) VALUES (?1, ?2, ?3, ?4, 1)",
            params![scope, session, token_sha256, entry],
        )
        .map_err(sql)?;
        Ok(db.last_insert_rowid() as u64)
    }

    /// The pending decision `seq` got `entry`; keeps the newest `KEEP` of its session.
    pub fn settle(&self, seq: u64, entry: &[u8]) -> io::Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction().map_err(sql)?;
        let session: String = tx
            .query_row("SELECT session FROM decided WHERE seq = ?1", [seq as i64], |r| r.get(0))
            .map_err(sql)?;
        tx.execute(
            "UPDATE decided SET entry = ?2, pending = 0 WHERE seq = ?1",
            params![seq as i64, entry],
        )
        .map_err(sql)?;
        tx.execute(
            "DELETE FROM decided WHERE session = ?1 AND pending = 0 AND seq <= (
                SELECT seq FROM decided WHERE session = ?1 AND pending = 0
                ORDER BY seq DESC LIMIT 1 OFFSET ?2)",
            params![session, KEEP],
        )
        .map_err(sql)?;
        tx.commit().map_err(sql)
    }

    /// The pending decision `seq` was not applied.
    pub fn cancel(&self, seq: u64) -> io::Result<()> {
        let db = self.db.lock().unwrap();
        db.execute("DELETE FROM decided WHERE seq = ?1", [seq as i64])
            .map_err(sql)
            .map(drop)
    }

    /// At start: settle each pending decision from what its scope shows (`fate`), or
    /// cancel it. Returns how many were settled.
    pub fn resolve(&self, fate: impl Fn(&str) -> Fate) -> io::Result<usize> {
        self.resolve_where(None, fate)
    }

    /// `resolve` for scope `id` alone. Recovery settles a finished commit's decision
    /// before it drops the scope: a start killed in between must not see it gone.
    pub fn resolve_scope(&self, id: &str, fate: Fate) -> io::Result<usize> {
        self.resolve_where(Some(id), |_| fate)
    }

    fn resolve_where(&self, scope: Option<&str>, fate: impl Fn(&str) -> Fate) -> io::Result<usize> {
        let pending: Vec<(u64, String, Vec<u8>)> = {
            let db = self.db.lock().unwrap();
            let mut st = db
                .prepare(
                    "SELECT seq, scope, entry FROM decided WHERE pending = 1 AND (?1 IS NULL OR scope = ?1)
                     ORDER BY seq",
                )
                .map_err(sql)?;
            st.query_map([scope], |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?, r.get(2)?)))
                .map_err(sql)?
                .collect::<Result<_, _>>()
                .map_err(sql)?
        };
        let mut settled = 0;
        for (seq, scope, entry) in pending {
            match (self.settled_as)(&entry, fate(&scope)) {
                Some(entry) => {
                    self.settle(seq, &entry)?;
                    settled += 1;
                }
                None => self.cancel(seq)?,
            }
        }
        Ok(settled)
    }

    /// The latest decision of `scope`, if one is kept.
    pub fn latest(&self, scope: &str) -> io::Result<Option<Latest>> {
        let db = self.db.lock().unwrap();
        db.query_row(
            "SELECT seq, token_sha256, entry FROM decided WHERE scope = ?1 AND pending = 0
             ORDER BY seq DESC LIMIT 1",
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
            .prepare("SELECT entry FROM decided WHERE session = ?1 AND pending = 0 ORDER BY seq DESC LIMIT ?2")
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
    use crate::proto::{Decided, OutcomeStatus};
    use crate::record;
    use prost::Message;

    #[test]
    fn keeps_the_newest_per_session_and_each_scopes_latest() {
        let dir = std::env::temp_dir().join(format!("escrowd-history-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let h = History::open(&dir, record::settled).unwrap();
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

    #[test]
    fn a_history_from_before_pending_rows_gets_the_column() {
        let dir = crate::db::temp_dir("history-v0");
        {
            let old = Connection::open(dir.join("history.sqlite")).unwrap();
            old.execute_batch(
                "CREATE TABLE decided (seq INTEGER PRIMARY KEY, scope TEXT NOT NULL, session TEXT NOT NULL,
                     token_sha256 TEXT, entry BLOB NOT NULL) STRICT;
                 INSERT INTO decided VALUES (1, 's1', 'a', NULL, X'00');",
            )
            .unwrap();
        }
        let h = History::open(&dir, record::settled).unwrap();
        assert_eq!(h.latest("s1").unwrap().unwrap().seq, 1, "an old row reads as settled");
        assert_eq!(h.session("a", 10).unwrap(), vec![vec![0u8]]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_start_settles_or_cancels_each_pending_decision() {
        use crate::proto::Outcome;
        let dir = std::env::temp_dir().join(format!("escrowd-pending-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let h = History::open(&dir, record::settled).unwrap();
        let entry = |status: OutcomeStatus| {
            Decided {
                outcome: Some(Outcome {
                    status: status.into(),
                    paths: vec!["a".into()],
                    ..Default::default()
                }),
                ..Default::default()
            }
            .encode_to_vec()
        };
        let cases = [
            (
                "done",
                OutcomeStatus::Committed,
                Fate::Committed,
                Some((OutcomeStatus::Committed, 1)),
            ),
            ("held", OutcomeStatus::Committed, Fate::Closed, None),
            (
                "reopened",
                OutcomeStatus::Returned,
                Fate::Open,
                Some((OutcomeStatus::Returned, 1)),
            ),
            (
                "dropped",
                OutcomeStatus::Discarded,
                Fate::Gone,
                Some((OutcomeStatus::Discarded, 1)),
            ),
            (
                "conflict",
                OutcomeStatus::Committed,
                Fate::Gone,
                Some((OutcomeStatus::Conflict, 0)),
            ),
            ("odd", OutcomeStatus::Discarded, Fate::Open, None),
        ];
        for (scope, status, _, _) in &cases {
            h.intend(scope, "s", None, &entry(*status)).unwrap();
            assert!(h.latest(scope).unwrap().is_none(), "a pending decision is not read");
        }
        assert!(h.session("s", 10).unwrap().is_empty());
        let settled = h.resolve(|id| cases.iter().find(|c| c.0 == id).unwrap().2).unwrap();
        assert_eq!(settled, 4);
        for (scope, _, _, want) in cases {
            let got = h.latest(scope).unwrap().map(|l| {
                let out = Decided::decode(&l.entry[..]).unwrap().outcome.unwrap();
                (out.status(), out.paths.len())
            });
            assert_eq!(got, want, "{scope}");
        }
        assert_eq!(h.resolve(|_| Fate::Gone).unwrap(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
