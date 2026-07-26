#![cfg(feature = "sqlite")]

use std::fs;

use mdbase_runtime::{RuntimeStore, SqliteRuntimeStore, SQLITE_SCHEMA_VERSION};
use rusqlite::Connection;

#[test]
fn fresh_sqlite_stores_record_the_latest_schema_version() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.sqlite");
    let store = SqliteRuntimeStore::open(&path).unwrap();
    drop(store);

    let connection = Connection::open(path).unwrap();
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
        .unwrap();
    assert_eq!(version, SQLITE_SCHEMA_VERSION);
}

#[tokio::test]
async fn unversioned_sqlite_data_is_migrated_without_losing_events() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.sqlite");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "
            CREATE TABLE runtime_events (
                cursor INTEGER PRIMARY KEY AUTOINCREMENT,
                source_runtime TEXT NOT NULL,
                event_id TEXT NOT NULL,
                envelope_json TEXT NOT NULL,
                received_at TEXT NOT NULL,
                UNIQUE(source_runtime, event_id)
            );
            INSERT INTO runtime_events
                (source_runtime, event_id, envelope_json, received_at)
            VALUES
                ('legacy', 'event-one', '{}', '2026-01-01T00:00:00.000Z');
            ",
        )
        .unwrap();
    drop(connection);

    let store = SqliteRuntimeStore::open(&path).unwrap();
    let page = store.events_after(0, 10).await.unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].event_id, "event-one");
    assert_eq!(page.head, 1);
}

#[test]
fn newer_sqlite_schema_versions_fail_closed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.sqlite");
    let connection = Connection::open(&path).unwrap();
    connection
        .pragma_update(None, "user_version", SQLITE_SCHEMA_VERSION + 1)
        .unwrap();
    drop(connection);

    let error = match SqliteRuntimeStore::open(path) {
        Ok(_) => panic!("newer schema must be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "runtime_schema_too_new");
}

#[test]
fn failed_sqlite_migrations_do_not_advance_the_version_or_leave_partial_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.sqlite");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("CREATE TABLE runtime_events (cursor INTEGER PRIMARY KEY);")
        .unwrap();
    drop(connection);

    assert!(SqliteRuntimeStore::open(&path).is_err());
    let connection = Connection::open(&path).unwrap();
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
        .unwrap();
    assert_eq!(version, 0);
    let runs_table: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'runtime_runs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(runs_table, 0);

    // Make it explicit that cleanup is handled by the temporary directory and
    // the failed migration did not create sidecar data elsewhere.
    assert!(fs::metadata(path).unwrap().is_file());
}
