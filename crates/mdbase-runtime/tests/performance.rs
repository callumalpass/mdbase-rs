use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
#[cfg(feature = "sqlite")]
use mdbase_runtime::SqliteRuntimeStore;
use mdbase_runtime::{InMemoryRuntimeStore, PreparedEvent, RuntimeStore};
use serde_json::json;
#[cfg(feature = "sqlite")]
use tempfile::tempdir;

#[tokio::test]
async fn event_journals_have_bounded_reference_throughput() {
    let now = Utc.with_ymd_and_hms(2026, 7, 24, 0, 0, 0).single().unwrap();

    let memory = InMemoryRuntimeStore::new();
    let started = Instant::now();
    for index in 0..10_000 {
        memory
            .admit_event(event(index, now))
            .await
            .expect("in-memory admission");
    }
    let memory_elapsed = started.elapsed();
    assert!(
        memory_elapsed < Duration::from_secs(10),
        "10,000 in-memory admissions took {:?}",
        memory_elapsed
    );
    eprintln!(
        "runtime admission profile: memory=10,000/{memory_elapsed:?} ({:.0}/s)",
        10_000.0 / memory_elapsed.as_secs_f64()
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_event_journal_has_bounded_reference_throughput() {
    let now = Utc.with_ymd_and_hms(2026, 7, 24, 0, 0, 0).single().unwrap();
    let directory = tempdir().unwrap();
    let sqlite = SqliteRuntimeStore::open(directory.path().join("runtime.sqlite")).unwrap();
    let started = Instant::now();
    for index in 0..2_000 {
        sqlite
            .admit_event(event(index, now))
            .await
            .expect("SQLite admission");
    }
    let sqlite_elapsed = started.elapsed();
    assert!(
        sqlite_elapsed < Duration::from_secs(20),
        "2,000 SQLite admissions took {:?}",
        sqlite_elapsed
    );
    eprintln!(
        "runtime admission profile: sqlite=2,000/{sqlite_elapsed:?} ({:.0}/s)",
        2_000.0 / sqlite_elapsed.as_secs_f64()
    );
}

fn event(index: usize, received_at: chrono::DateTime<Utc>) -> PreparedEvent {
    let id = format!("evt_{index}");
    PreparedEvent {
        source_runtime: "performance".to_string(),
        event_id: id.clone(),
        envelope: json!({"id": id, "payload": {"index": index}}),
        received_at,
        runs: Vec::new(),
    }
}
