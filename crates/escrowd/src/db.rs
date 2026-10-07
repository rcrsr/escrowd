//! The daemon's SQLite files (`journal.sqlite`, `history.sqlite`, each scope's
//! `meta.sqlite`): one way to open them and one way to upgrade them.
//!
//! Each file's schema has a version in `PRAGMA user_version`. A file lists its
//! upgrade steps in order; `migrate` runs the ones a file has not had, in one
//! transaction, and refuses a file a newer escrowd wrote, so a downgrade fails at
//! start instead of misreading rows.

use std::io;
use std::path::Path;

use rusqlite::{Connection, Transaction};

/// How each commit reaches the disk (WAL mode in both).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    /// Every commit is fsynced: it survives a power loss.
    Full,
    /// Commits survive a crash of the daemon, not of the host.
    Normal,
}

/// One upgrade: from the version before it to the next.
pub type Step = fn(&Transaction) -> rusqlite::Result<()>;

pub fn sql(e: rusqlite::Error) -> io::Error {
    io::Error::other(e)
}

pub fn open(path: &Path, durability: Durability) -> io::Result<Connection> {
    let db = Connection::open(path).map_err(sql)?;
    let sync = match durability {
        Durability::Full => "FULL",
        Durability::Normal => "NORMAL",
    };
    // synchronous first, so the switch of a new file to WAL runs under it.
    db.execute_batch(&format!("PRAGMA synchronous = {sync}; PRAGMA journal_mode = WAL;"))
        .map_err(sql)?;
    Ok(db)
}

/// Bring `db` (`name` in errors) to version `steps.len()`.
pub fn migrate(db: &mut Connection, name: &str, steps: &[Step]) -> io::Result<()> {
    let have: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(sql)?;
    let want = steps.len() as i64;
    if have > want {
        return Err(io::Error::other(format!(
            "{name}: schema version {have} is from a newer escrowd (this one knows {want})"
        )));
    }
    if have == want {
        return Ok(());
    }
    // One transaction: a new file costs one sync, not one per table.
    let tx = db.transaction().map_err(sql)?;
    for step in &steps[have as usize..] {
        step(&tx).map_err(sql)?;
    }
    tx.pragma_update(None, "user_version", want).map_err(sql)?;
    tx.commit().map_err(sql)
}

/// `table` exists in `db`.
pub fn has_table(db: &Connection, table: &str) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
}

/// `table` has a column `column`.
pub fn has_column(db: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    db.prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2")?
        .exists([table, column])
}

#[cfg(test)]
pub(crate) fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("escrowd-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_t(tx: &Transaction) -> rusqlite::Result<()> {
        tx.execute_batch("CREATE TABLE t (a INTEGER)")
    }

    fn add_b(tx: &Transaction) -> rusqlite::Result<()> {
        tx.execute_batch("ALTER TABLE t ADD COLUMN b INTEGER")
    }

    #[test]
    fn migrate_runs_each_missing_step_once_and_refuses_a_newer_file() {
        let dir = temp_dir("db-migrate");
        let path = dir.join("x.sqlite");
        let mut db = open(&path, Durability::Normal).unwrap();
        migrate(&mut db, "x", &[create_t]).unwrap();
        migrate(&mut db, "x", &[create_t]).unwrap(); // nothing to do
        assert!(!has_column(&db, "t", "b").unwrap());
        migrate(&mut db, "x", &[create_t, add_b]).unwrap();
        assert!(has_column(&db, "t", "b").unwrap());
        let v: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 2);
        let err = migrate(&mut db, "x", &[create_t]).unwrap_err();
        assert!(err.to_string().contains("newer escrowd"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_step_leaves_the_file_as_it_was() {
        fn broken(tx: &Transaction) -> rusqlite::Result<()> {
            tx.execute_batch("CREATE TABLE u (a INTEGER); SELECT * FROM missing")
        }
        let dir = temp_dir("db-rollback");
        let mut db = open(&dir.join("x.sqlite"), Durability::Full).unwrap();
        migrate(&mut db, "x", &[create_t]).unwrap();
        assert!(migrate(&mut db, "x", &[create_t, broken]).is_err());
        assert!(!has_table(&db, "u").unwrap());
        let v: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
