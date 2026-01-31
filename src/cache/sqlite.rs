//! SQLite setup and helpers.

use rusqlite::{Connection, Result};
use std::path::Path;

/// Open (or create) the cache database at `<root>/<cache_folder>/cache.db`.
pub(crate) fn open_cache_db(root: &Path, cache_folder: &str) -> Result<Connection> {
    let db_dir = root.join(cache_folder);
    std::fs::create_dir_all(&db_dir).map_err(|_e| {
        rusqlite::Error::InvalidPath(db_dir.join("cache.db"))
    })?;
    let db_path = db_dir.join("cache.db");
    let conn = Connection::open(&db_path)?;
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
    // Migration: add ctime_ns if missing (idempotent for existing DBs)
    let _ = conn.execute_batch("ALTER TABLE files ADD COLUMN ctime_ns INTEGER;");
    Ok(())
}
