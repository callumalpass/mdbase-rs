//! SQLite setup and helpers.

use rusqlite::{Connection, Result};
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
