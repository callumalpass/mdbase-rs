#![cfg(feature = "postgres")]

use chrono::Utc;
use mdbase_runtime::{PostgresRuntimeStore, PreparedEvent, RuntimeStore, TimerRecord, TimerStatus};
use serde_json::json;
use ulid::Ulid;

fn prepared_event(id: &str) -> PreparedEvent {
    PreparedEvent {
        source_runtime: "postgres-test".to_string(),
        event_id: id.to_string(),
        envelope: json!({
            "type": "test.changed",
            "contract_version": 1,
            "id": id,
            "occurred_at": Utc::now(),
            "source": {"runtime": "postgres-test", "provider": "test"},
            "payload": {"private": "must remain in the authority"}
        }),
        received_at: Utc::now(),
        runs: Vec::new(),
    }
}

/// Set `MDBASE_RUNTIME_TEST_DATABASE_URL` to run this against a disposable or
/// dedicated PostgreSQL database. A unique namespace makes parallel test runs
/// independent without requiring database-level creation privileges.
#[tokio::test]
async fn postgres_store_preserves_dedupe_retention_timers_and_namespace_fencing() {
    let Ok(database_url) = std::env::var("MDBASE_RUNTIME_TEST_DATABASE_URL") else {
        eprintln!("skipping live PostgreSQL runtime test: no database URL configured");
        return;
    };
    let test_id = Ulid::new().to_string();
    let first = PostgresRuntimeStore::connect(&database_url, format!("test:{test_id}:a"))
        .await
        .unwrap();
    let second = PostgresRuntimeStore::new(first.pool().clone(), format!("test:{test_id}:b"))
        .await
        .unwrap();

    let admitted = first
        .admit_event(prepared_event("event-one"))
        .await
        .unwrap();
    assert_eq!(admitted.cursor, 1);
    assert!(!admitted.duplicate);
    assert!(second.snapshot().await.unwrap().events.is_empty());

    assert_eq!(
        first.prune_events_through(admitted.cursor).await.unwrap(),
        1
    );
    let expired = first.events_after(0, 100).await.unwrap();
    assert!(expired.reset_required);
    assert_eq!(expired.retained_after, 1);
    assert_eq!(expired.head, 1);

    let duplicate = first
        .admit_event(prepared_event("event-one"))
        .await
        .unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.cursor, admitted.cursor);

    let concurrent = (0..16)
        .map(|_| {
            let store = first.clone();
            tokio::spawn(async move {
                store
                    .admit_event(prepared_event("event-concurrent"))
                    .await
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    let mut original = 0;
    let mut cursor = None;
    for task in concurrent {
        let outcome = task.await.unwrap();
        original += usize::from(!outcome.duplicate);
        assert_eq!(*cursor.get_or_insert(outcome.cursor), outcome.cursor);
    }
    assert_eq!(original, 1);

    let now = Utc::now();
    let timer = first
        .upsert_timer(TimerRecord {
            id: "wake-up".to_string(),
            generation: 0,
            status: TimerStatus::Scheduled,
            fire_at: now,
            event_type: "timer.fired".to_string(),
            contract_version: 1,
            payload: json!({"purpose": "notification"}),
            created_at: now,
            updated_at: now,
            fired_at: None,
        })
        .await
        .unwrap();
    assert_eq!(timer.generation, 1);
    let claim = first
        .claim_due_timer("worker-a", now, std::time::Duration::from_secs(30))
        .await
        .unwrap()
        .expect("due timer");
    assert!(first
        .claim_due_timer("worker-b", now, std::time::Duration::from_secs(30))
        .await
        .unwrap()
        .is_none());

    let mut fired = claim.timer.clone();
    fired.status = TimerStatus::Fired;
    fired.fired_at = Some(now);
    fired.updated_at = now;
    let outcome = first
        .fire_timer(claim, fired, prepared_event("timer-event-one"))
        .await
        .unwrap();
    assert_eq!(outcome.cursor, 3);
    assert_eq!(
        first.snapshot().await.unwrap().timers[0].status,
        TimerStatus::Fired
    );
}
