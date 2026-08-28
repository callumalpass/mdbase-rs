use rusqlite::Connection;

use super::{store_error, SQLITE_SCHEMA_VERSION};
use crate::error::{RuntimeError, RuntimeResult};
use crate::model::{RunRecord, TimerRecord};

pub(super) fn migrate(connection: &mut Connection) -> RuntimeResult<()> {
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
    let transaction = connection.transaction().map_err(store_error)?;
    let mut version = installed;
    if version == 0 {
        transaction
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS runtime_events (
                    cursor INTEGER PRIMARY KEY AUTOINCREMENT,
                    source_runtime TEXT NOT NULL,
                    event_id TEXT NOT NULL,
                    envelope_json TEXT NOT NULL,
                    received_at TEXT NOT NULL,
                    UNIQUE(source_runtime, event_id)
                );

                CREATE TABLE IF NOT EXISTS runtime_event_dedup (
                    source_runtime TEXT NOT NULL,
                    event_id TEXT NOT NULL,
                    cursor INTEGER NOT NULL,
                    PRIMARY KEY(source_runtime, event_id)
                );

                INSERT OR IGNORE INTO runtime_event_dedup(source_runtime, event_id, cursor)
                    SELECT source_runtime, event_id, cursor FROM runtime_events;

                CREATE TABLE IF NOT EXISTS runtime_meta (
                    key TEXT PRIMARY KEY,
                    value INTEGER NOT NULL
                );

                INSERT OR IGNORE INTO runtime_meta(key, value) VALUES ('retained_after', 0);

                CREATE TABLE IF NOT EXISTS runtime_runs (
                    id TEXT PRIMARY KEY,
                    executor TEXT NOT NULL,
                    workflow TEXT NOT NULL,
                    trigger_id TEXT NOT NULL,
                    status TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    not_before TEXT NOT NULL,
                    idempotency_scope TEXT NOT NULL,
                    idempotency_key TEXT NOT NULL,
                    concurrency_group TEXT NOT NULL,
                    concurrency_policy TEXT NOT NULL,
                    lease_worker TEXT,
                    lease_token TEXT,
                    lease_expires_at TEXT,
                    revision INTEGER NOT NULL,
                    record_json TEXT NOT NULL,
                    UNIQUE(idempotency_scope, idempotency_key)
                );

                CREATE INDEX IF NOT EXISTS runtime_runs_claim
                    ON runtime_runs(executor, status, not_before, created_at);
                CREATE INDEX IF NOT EXISTS runtime_runs_group
                    ON runtime_runs(concurrency_group, status);
                CREATE INDEX IF NOT EXISTS runtime_runs_trigger
                    ON runtime_runs(workflow, trigger_id, created_at);

                CREATE TABLE IF NOT EXISTS runtime_timers (
                    id TEXT PRIMARY KEY,
                    generation INTEGER NOT NULL,
                    status TEXT NOT NULL,
                    fire_at TEXT NOT NULL,
                    lease_worker TEXT,
                    lease_token TEXT,
                    lease_expires_at TEXT,
                    record_json TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS runtime_timers_due
                    ON runtime_timers(status, fire_at);
                ",
            )
            .map_err(store_error)?;
        version = 1;
    }
    if version == 1 {
        // Runtime profile 0.2 made the persisted run and timer model
        // incompatible with profile 0.1. The missing exact contract evidence
        // cannot be reconstructed from old JSON, so this prerelease migration
        // explicitly resets runtime-owned execution state.
        transaction
            .execute_batch(
                "
                DELETE FROM runtime_timers;
                DELETE FROM runtime_runs;
                DELETE FROM runtime_event_dedup;
                DELETE FROM runtime_events;
                DELETE FROM runtime_meta;
                INSERT INTO runtime_meta(key, value) VALUES ('retained_after', 0);
                ",
            )
            .map_err(store_error)?;
        version = 2;
    }
    if version != SQLITE_SCHEMA_VERSION {
        return Err(RuntimeError::Store(format!(
            "SQLite runtime migration stopped at version {version}; expected {SQLITE_SCHEMA_VERSION}."
        )));
    }
    transaction
        .pragma_update(None, "user_version", SQLITE_SCHEMA_VERSION)
        .map_err(store_error)?;
    transaction.commit().map_err(store_error)
}

pub(super) fn validate_persisted_records(connection: &Connection) -> RuntimeResult<()> {
    let mut runs = connection
        .prepare("SELECT id, record_json FROM runtime_runs ORDER BY id")
        .map_err(store_error)?;
    let rows = runs
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(store_error)?;
    for row in rows {
        let (id, json) = row.map_err(store_error)?;
        serde_json::from_str::<RunRecord>(&json).map_err(|error| {
            RuntimeError::diagnostic(
                "invalid_persisted_runtime_record",
                format!("Runtime run {id} is incompatible with this build: {error}"),
            )
        })?;
    }
    drop(runs);

    let mut timers = connection
        .prepare("SELECT id, record_json FROM runtime_timers ORDER BY id")
        .map_err(store_error)?;
    let rows = timers
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(store_error)?;
    for row in rows {
        let (id, json) = row.map_err(store_error)?;
        serde_json::from_str::<TimerRecord>(&json).map_err(|error| {
            RuntimeError::diagnostic(
                "invalid_persisted_runtime_record",
                format!("Runtime timer {id} is incompatible with this build: {error}"),
            )
        })?;
    }
    Ok(())
}
