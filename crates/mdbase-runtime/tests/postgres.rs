#![cfg(feature = "postgres")]

use chrono::{TimeDelta, Utc};
use mdbase_interop::{ExactContractReference, ImplementationIdentity};
use mdbase_runtime::{
    ConcurrencyPolicy, OnError, PlannedRun, PostgresRuntimeStore, PreparedEvent, RunStatus,
    RuntimeStore, TimerRecord, TimerStatus, POSTGRES_SCHEMA_VERSION,
};
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

fn planned_run(id: &str, event_id: &str, policy: ConcurrencyPolicy) -> PlannedRun {
    let now = Utc::now();
    PlannedRun {
        id: id.to_string(),
        workflow: "test.workflow".to_string(),
        workflow_version: "1.0.0".to_string(),
        workflow_revision: digest('a'),
        catalog_revision: digest('b'),
        policy_id: "test.policy".to_string(),
        policy_revision: digest('c'),
        trigger: "changed".to_string(),
        event_id: event_id.to_string(),
        event_type: "test.changed".to_string(),
        event_contract: contract("test.changed"),
        event_source: identity(),
        source_declaration_digest: digest('d'),
        event_cursor: 0,
        event: json!({"id": event_id, "type": "test.changed"}),
        executor: "postgres-test".to_string(),
        idempotency_key: format!("key:{id}"),
        idempotency_scope: "postgres-test".to_string(),
        concurrency_group: "test-group".to_string(),
        concurrency_policy: policy,
        replacement_blockers: Vec::new(),
        on_error: OnError::Stop,
        not_before: now,
        timeout_at: None,
        minimum_interval_ms: None,
        vars: json!({}),
        workflow_value: json!({}),
        steps: Vec::new(),
        created_at: now,
    }
}

fn prepared_run_event(id: &str, run: PlannedRun) -> PreparedEvent {
    let mut event = prepared_event(id);
    event.runs.push(run);
    event
}

fn digest(character: char) -> String {
    format!("sha256:{}", character.to_string().repeat(64))
}

fn contract(id: &str) -> ExactContractReference {
    ExactContractReference {
        id: id.to_string(),
        version: "1.0.0".to_string(),
        digest: digest('e'),
    }
}

fn identity() -> ImplementationIdentity {
    ImplementationIdentity {
        application: "postgres-test".to_string(),
        implementation: "mdbase-runtime".to_string(),
        version: "1.0.0".to_string(),
        instance_id: None,
    }
}

/// Set `MDBASE_RUNTIME_TEST_DATABASE_URL` to run this against a disposable or
/// dedicated PostgreSQL database. A unique namespace makes parallel test runs
/// independent without requiring database-level creation privileges.
#[tokio::test]
async fn postgres_store_preserves_dedupe_retention_timers_and_namespace_fencing() {
    let Ok(database_url) = std::env::var("MDBASE_RUNTIME_TEST_DATABASE_URL") else {
        assert_ne!(
            std::env::var("MDBASE_RUNTIME_REQUIRE_POSTGRES").as_deref(),
            Ok("1"),
            "live PostgreSQL is required but MDBASE_RUNTIME_TEST_DATABASE_URL is missing"
        );
        eprintln!("skipping live PostgreSQL runtime test: no database URL configured");
        return;
    };
    let test_id = Ulid::new().to_string();
    let first = PostgresRuntimeStore::connect(&database_url, format!("test:{test_id}:a"))
        .await
        .unwrap();
    assert_eq!(
        first.schema_version().await.unwrap(),
        POSTGRES_SCHEMA_VERSION
    );
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
    let later = now + TimeDelta::hours(1);
    let reconciled = first
        .reconcile_timers(
            "grant-a:",
            vec![
                TimerRecord {
                    id: "grant-a:one".to_string(),
                    generation: 0,
                    status: TimerStatus::Scheduled,
                    fire_at: later,
                    event_contract: contract("mdbase.runtime.timer.fired"),
                    event_source: identity(),
                    source_uri: "urn:test:timer".to_string(),
                    subject: None,
                    data: json!({"purpose": "notification"}),
                    created_at: now,
                    updated_at: now,
                    fired_at: None,
                },
                TimerRecord {
                    id: "grant-a:two".to_string(),
                    generation: 0,
                    status: TimerStatus::Scheduled,
                    fire_at: later,
                    event_contract: contract("mdbase.runtime.timer.fired"),
                    event_source: identity(),
                    source_uri: "urn:test:timer".to_string(),
                    subject: None,
                    data: json!({"purpose": "notification"}),
                    created_at: now,
                    updated_at: now,
                    fired_at: None,
                },
            ],
            now,
        )
        .await
        .unwrap();
    assert_eq!(reconciled.timers.len(), 2);
    let reconciled = first
        .reconcile_timers("grant-a:", vec![reconciled.timers[0].clone()], now)
        .await
        .unwrap();
    assert_eq!(reconciled.cancelled_ids, vec!["grant-a:two"]);

    let timer = first
        .upsert_timer(TimerRecord {
            id: "wake-up".to_string(),
            generation: 0,
            status: TimerStatus::Scheduled,
            fire_at: now,
            event_contract: contract("mdbase.runtime.timer.fired"),
            event_source: identity(),
            source_uri: "urn:test:timer".to_string(),
            subject: None,
            data: json!({"purpose": "notification"}),
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
    let snapshot = first.snapshot().await.unwrap();
    assert_eq!(
        snapshot
            .timers
            .iter()
            .find(|timer| timer.id == "wake-up")
            .unwrap()
            .status,
        TimerStatus::Fired
    );

    let state_store = PostgresRuntimeStore::new(
        first.pool().clone(),
        format!("test:{test_id}:state-machine"),
    )
    .await
    .unwrap();
    let first_run = planned_run(
        "run-skip-first",
        "event-skip-first",
        ConcurrencyPolicy::Skip,
    );
    state_store
        .admit_event(prepared_run_event("event-skip-first", first_run))
        .await
        .unwrap();
    let skipped = state_store
        .admit_event(prepared_run_event(
            "event-skip-second",
            planned_run(
                "run-skip-second",
                "event-skip-second",
                ConcurrencyPolicy::Skip,
            ),
        ))
        .await
        .unwrap();
    assert_eq!(skipped.skipped_run_ids, ["run-skip-second"]);

    let replaced = state_store
        .admit_event(prepared_run_event(
            "event-replace",
            planned_run(
                "run-replacement",
                "event-replace",
                ConcurrencyPolicy::Replace,
            ),
        ))
        .await
        .unwrap();
    assert_eq!(replaced.cancellation_requested_run_ids, ["run-skip-first"]);
    let snapshot = state_store.snapshot().await.unwrap();
    assert_eq!(
        snapshot
            .runs
            .iter()
            .find(|run| run.plan.id == "run-skip-first")
            .unwrap()
            .status,
        RunStatus::Cancelled
    );

    let queue_store =
        PostgresRuntimeStore::new(first.pool().clone(), format!("test:{test_id}:queue-order"))
            .await
            .unwrap();
    let mut older = planned_run(
        "run-queue-older",
        "event-queue-older",
        ConcurrencyPolicy::Queue,
    );
    let queue_now = older.created_at;
    older.not_before = queue_now + TimeDelta::minutes(1);
    queue_store
        .admit_event(prepared_run_event("event-queue-older", older))
        .await
        .unwrap();
    let mut newer = planned_run(
        "run-queue-newer",
        "event-queue-newer",
        ConcurrencyPolicy::Queue,
    );
    newer.created_at = queue_now;
    newer.not_before = queue_now;
    queue_store
        .admit_event(prepared_run_event("event-queue-newer", newer))
        .await
        .unwrap();
    assert!(queue_store
        .claim_run(
            "postgres-test",
            "worker-a",
            queue_now,
            std::time::Duration::from_secs(30),
        )
        .await
        .unwrap()
        .is_none());
    let claim = queue_store
        .claim_run(
            "postgres-test",
            "worker-a",
            queue_now + TimeDelta::minutes(1),
            std::time::Duration::from_secs(30),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.run.plan.id, "run-queue-older");

    // Profile 0.1 persisted Rust values under schema version 1 even though
    // profile 0.2 cannot deserialize them safely. The v2 migration makes the
    // prerelease reset explicit and leaves the store immediately reusable.
    sqlx::query("UPDATE mdbase_runtime_schema SET version = 1 WHERE singleton = TRUE")
        .execute(first.pool())
        .await
        .unwrap();
    PostgresRuntimeStore::prepare(first.pool()).await.unwrap();
    assert_eq!(
        first.schema_version().await.unwrap(),
        POSTGRES_SCHEMA_VERSION
    );
    for table in [
        "mdbase_runtime_events",
        "mdbase_runtime_event_dedup",
        "mdbase_runtime_runs",
        "mdbase_runtime_timers",
        "mdbase_runtime_meta",
    ] {
        let count = match table {
            "mdbase_runtime_events" => {
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mdbase_runtime_events")
                    .fetch_one(first.pool())
                    .await
                    .unwrap()
            }
            "mdbase_runtime_event_dedup" => {
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mdbase_runtime_event_dedup")
                    .fetch_one(first.pool())
                    .await
                    .unwrap()
            }
            "mdbase_runtime_runs" => {
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mdbase_runtime_runs")
                    .fetch_one(first.pool())
                    .await
                    .unwrap()
            }
            "mdbase_runtime_timers" => {
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mdbase_runtime_timers")
                    .fetch_one(first.pool())
                    .await
                    .unwrap()
            }
            "mdbase_runtime_meta" => {
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mdbase_runtime_meta")
                    .fetch_one(first.pool())
                    .await
                    .unwrap()
            }
            _ => unreachable!(),
        };
        assert_eq!(count, 0, "{table} was not reset");
    }
    let reset_store =
        PostgresRuntimeStore::new(first.pool().clone(), format!("test:{test_id}:after-v2"))
            .await
            .unwrap();
    assert_eq!(
        reset_store
            .admit_event(prepared_event("event-after-v2"))
            .await
            .unwrap()
            .cursor,
        1
    );
    sqlx::query(
        "INSERT INTO mdbase_runtime_timers
            (namespace, id, generation, status, fire_at, record_json)
         VALUES ($1, 'broken', 1, 'scheduled', now(), '{}'::jsonb)",
    )
    .bind(format!("test:{test_id}:invalid"))
    .execute(first.pool())
    .await
    .unwrap();
    let error = PostgresRuntimeStore::prepare(first.pool())
        .await
        .unwrap_err();
    assert_eq!(error.code(), "invalid_persisted_runtime_record");
    sqlx::query("DELETE FROM mdbase_runtime_timers WHERE id = 'broken'")
        .execute(first.pool())
        .await
        .unwrap();
}
