use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};

use super::{store_error, timestamp, SQLITE_SCHEMA_VERSION};
use crate::error::{RuntimeError, RuntimeResult};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SqliteRecoveryState {
    pub due_timers: bool,
    pub pending_runs: bool,
}

impl SqliteRecoveryState {
    pub fn has_work(self) -> bool {
        self.due_timers || self.pending_runs
    }
}

/// Inspect an existing runtime store without creating or migrating it.
pub fn inspect_sqlite_recovery(
    path: impl AsRef<Path>,
    now: DateTime<Utc>,
) -> RuntimeResult<SqliteRecoveryState> {
    let path = path.as_ref();
    if !path.is_file() {
        return Ok(SqliteRecoveryState::default());
    }
    let connection =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(store_error)?;
    let installed = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
        .map_err(store_error)?;
    if installed > SQLITE_SCHEMA_VERSION {
        return Err(RuntimeError::diagnostic(
            "runtime_schema_too_new",
            format!(
                "SQLite runtime schema version {installed} is newer than supported version {SQLITE_SCHEMA_VERSION}."
            ),
        ));
    }
    let table_exists = |name: &str| -> RuntimeResult<bool> {
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                [name],
                |row| row.get(0),
            )
            .map_err(store_error)
    };
    let due_timers = table_exists("runtime_timers")?
        && connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM runtime_timers
                    WHERE status IN ('scheduled', 'firing') AND fire_at <= ?1
                 )",
                [timestamp(now)],
                |row| row.get(0),
            )
            .map_err(store_error)?;
    let pending_runs = table_exists("runtime_runs")?
        && connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM runtime_runs
                    WHERE status IN ('queued', 'running')
                      AND not_before <= ?1
                      AND (lease_token IS NULL OR lease_expires_at <= ?1)
                 )",
                [timestamp(now)],
                |row| row.get(0),
            )
            .map_err(store_error)?;
    Ok(SqliteRecoveryState {
        due_timers,
        pending_runs,
    })
}
