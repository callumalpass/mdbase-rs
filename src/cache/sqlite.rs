//! SQLite setup and helpers.

use rusqlite::{Connection, OpenFlags, Result};
use std::path::Path;

/// Open (or create) the cache database at `<root>/<cache_folder>/cache.db`.
pub(crate) fn open_cache_db(root: &Path, cache_folder: &str) -> Result<Connection> {
    let db_dir = root.join(cache_folder);
    std::fs::create_dir_all(&db_dir)
        .map_err(|_e| rusqlite::Error::InvalidPath(db_dir.join("cache.db")))?;
    let db_path = db_dir.join("cache.db");
    let conn = Connection::open(&db_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
    )?;
    init_schema(&conn)?;
    Ok(conn)
}

/// Open an existing cache read-only without schema initialization, DDL, or
/// logical cache writes. SQLite retains ownership of WAL shared-memory details.
pub(crate) fn open_cache_db_read_only_existing(
    root: &Path,
    cache_folder: &str,
) -> Result<Connection> {
    let db_path = root.join(cache_folder).join("cache.db");
    if !db_path.is_file() {
        return Err(rusqlite::Error::InvalidPath(db_path));
    }
    Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

/// Open an in-memory cache database (for testing).
#[allow(dead_code)]
pub(crate) fn open_cache_db_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    init_schema(&conn)?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(include_str!("schema.sql"))?;
    if !table_has_column(conn, "files", "ctime_ns")? {
        conn.execute_batch("ALTER TABLE files ADD COLUMN ctime_ns INTEGER;")?;
    }
    if !table_has_column(conn, "files", "source_revision")? {
        conn.execute_batch(
            "ALTER TABLE files ADD COLUMN source_revision TEXT NOT NULL DEFAULT '';",
        )?;
    }
    if !table_has_column(conn, "files", "failure_reason")? {
        conn.execute_batch("ALTER TABLE files ADD COLUMN failure_reason TEXT;")?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_files_failure_reason ON files(failure_reason) WHERE failure_reason IS NOT NULL;",
    )?;
    if !table_has_column(conn, "links", "source_revision")? {
        conn.execute_batch(
            "ALTER TABLE links ADD COLUMN source_revision TEXT NOT NULL DEFAULT '';",
        )?;
    }
    if !table_has_column(conn, "links", "resolved")? {
        conn.execute_batch("ALTER TABLE links ADD COLUMN resolved INTEGER NOT NULL DEFAULT 0;")?;
    }
    Ok(())
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for candidate in columns {
        if candidate? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_existing_open_does_not_initialize_or_change_logical_tokens() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join(".mdbase/cache");
        assert!(open_cache_db_read_only_existing(root.path(), ".mdbase/cache").is_err());
        assert!(!cache.exists());

        let connection = open_cache_db(root.path(), ".mdbase/cache").unwrap();
        connection
            .execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('query_snapshot', 'before')",
                [],
            )
            .unwrap();
        let schema_before: i64 = connection
            .query_row("PRAGMA schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_unique_values_path'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        drop(connection);
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(cache.join(format!("cache.db{suffix}")));
        }
        let read_only = open_cache_db_read_only_existing(root.path(), ".mdbase/cache").unwrap();
        assert_eq!(
            read_only
                .query_row(
                    "SELECT value FROM meta WHERE key = 'query_snapshot'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "before"
        );
        assert_eq!(
            read_only
                .query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            schema_before
        );
        drop(read_only);
        let verify = open_cache_db_read_only_existing(root.path(), ".mdbase/cache").unwrap();
        assert_eq!(
            verify
                .query_row(
                    "SELECT value FROM meta WHERE key = 'query_snapshot'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "before"
        );
        assert_eq!(
            verify
                .query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            schema_before
        );
    }

    #[test]
    fn read_only_transaction_holds_one_logical_snapshot_across_live_wal_writer() {
        let root = tempfile::tempdir().unwrap();
        let writer = open_cache_db(root.path(), ".mdbase/cache").unwrap();
        writer
            .execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('query_snapshot', 'first')",
                [],
            )
            .unwrap();
        let mut reader = open_cache_db_read_only_existing(root.path(), ".mdbase/cache").unwrap();
        let read = reader
            .transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)
            .unwrap();
        let first: String = read
            .query_row(
                "SELECT value FROM meta WHERE key = 'query_snapshot'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        writer
            .execute(
                "UPDATE meta SET value = 'second' WHERE key = 'query_snapshot'",
                [],
            )
            .unwrap();
        let coherent: String = read
            .query_row(
                "SELECT value FROM meta WHERE key = 'query_snapshot'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first, "first");
        assert_eq!(coherent, "first");
        drop(read);
        assert_eq!(
            reader
                .query_row(
                    "SELECT value FROM meta WHERE key = 'query_snapshot'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "second"
        );
    }
}
