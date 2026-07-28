use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, TimeZone, Utc};
use mdbase_interop::{ExactContractReference, ImplementationIdentity};
#[cfg(feature = "postgres")]
use mdbase_runtime::PostgresRuntimeStore;
#[cfg(feature = "sqlite")]
use mdbase_runtime::SqliteRuntimeStore;
use mdbase_runtime::{
    ConcurrencyPolicy, InMemoryRuntimeStore, OnError, PlannedRun, PreparedEvent, RunStatus,
    RuntimeStore, TimerRecord, TimerStatus,
};
use serde_json::json;
#[cfg(feature = "postgres")]
use ulid::Ulid;

fn event(id: &str, at: chrono::DateTime<Utc>, runs: Vec<PlannedRun>) -> PreparedEvent {
    PreparedEvent {
        source_runtime: "contract".to_string(),
        event_id: id.to_string(),
        envelope: json!({"type": "contract.event", "id": id}),
        received_at: at,
        runs,
    }
}

fn run(
    id: &str,
    event_id: &str,
    group: &str,
    policy: ConcurrencyPolicy,
    at: chrono::DateTime<Utc>,
) -> PlannedRun {
    PlannedRun {
        id: id.to_string(),
        workflow: "contract.workflow".to_string(),
        workflow_version: "1.0.0".to_string(),
        workflow_revision: digest('a'),
        catalog_revision: digest('b'),
        policy_id: "contract.policy".to_string(),
        policy_revision: digest('c'),
        trigger: "contract".to_string(),
        event_id: event_id.to_string(),
        event_type: "contract.event".to_string(),
        event_contract: contract("contract.event"),
        event_source: identity(),
        source_declaration_digest: digest('d'),
        event_cursor: 0,
        event: json!({"id": event_id}),
        executor: "contract-executor".to_string(),
        idempotency_key: format!("contract:{id}"),
        idempotency_scope: "contract".to_string(),
        concurrency_group: group.to_string(),
        concurrency_policy: policy,
        replacement_blockers: Vec::new(),
        on_error: OnError::Stop,
        not_before: at,
        timeout_at: None,
        minimum_interval_ms: None,
        vars: json!({}),
        workflow_value: json!({}),
        steps: Vec::new(),
        created_at: at,
    }
}

fn timer(id: &str, at: chrono::DateTime<Utc>) -> TimerRecord {
    TimerRecord {
        id: id.to_string(),
        generation: 0,
        status: TimerStatus::Scheduled,
        fire_at: at,
        event_contract: contract("mdbase.runtime.timer.fired"),
        event_source: identity(),
        source_uri: "urn:test:timer".to_string(),
        subject: None,
        data: json!({"id": id}),
        created_at: at,
        updated_at: at,
        fired_at: None,
    }
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
        application: "contract-test".to_string(),
        implementation: "store-contract".to_string(),
        version: "1.0.0".to_string(),
        instance_id: None,
    }
}

async fn assert_store_contract(store: Arc<dyn RuntimeStore>) {
    let now = Utc.with_ymd_and_hms(2026, 7, 26, 1, 0, 0).single().unwrap();

    let first = store
        .admit_event(event(
            "event-first",
            now,
            vec![run(
                "run-first",
                "event-first",
                "replace-group",
                ConcurrencyPolicy::Skip,
                now,
            )],
        ))
        .await
        .unwrap();
    assert!(!first.duplicate);
    let duplicate = store
        .admit_event(event("event-first", now, Vec::new()))
        .await
        .unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.cursor, first.cursor);

    let skipped = store
        .admit_event(event(
            "event-skipped",
            now,
            vec![run(
                "run-skipped",
                "event-skipped",
                "replace-group",
                ConcurrencyPolicy::Skip,
                now,
            )],
        ))
        .await
        .unwrap();
    assert_eq!(skipped.skipped_run_ids, ["run-skipped"]);
    let replacement = store
        .admit_event(event(
            "event-replacement",
            now,
            vec![run(
                "run-replacement",
                "event-replacement",
                "replace-group",
                ConcurrencyPolicy::Replace,
                now,
            )],
        ))
        .await
        .unwrap();
    assert_eq!(replacement.cancellation_requested_run_ids, ["run-first"]);
    assert_eq!(
        store.get_run("run-first").await.unwrap().unwrap().status,
        RunStatus::Cancelled
    );

    let queue_ready = now + TimeDelta::minutes(1);
    let mut older = run(
        "run-older",
        "event-older",
        "queue-group",
        ConcurrencyPolicy::Queue,
        now,
    );
    older.executor = "queue-executor".to_string();
    older.not_before = queue_ready;
    store
        .admit_event(event("event-older", now, vec![older]))
        .await
        .unwrap();
    let mut newer = run(
        "run-newer",
        "event-newer",
        "queue-group",
        ConcurrencyPolicy::Queue,
        now,
    );
    newer.executor = "queue-executor".to_string();
    store
        .admit_event(event("event-newer", now, vec![newer]))
        .await
        .unwrap();
    assert!(store
        .claim_run("queue-executor", "worker", now, Duration::from_secs(30),)
        .await
        .unwrap()
        .is_none());
    let mut claim = store
        .claim_run(
            "queue-executor",
            "worker",
            queue_ready,
            Duration::from_secs(30),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.run.plan.id, "run-older");
    let stale = claim.clone();
    claim.run.status = RunStatus::Succeeded;
    claim.run.finished_at = Some(queue_ready);
    claim.run.updated_at = queue_ready;
    store
        .commit_run(claim, vec![event("event-emitted", queue_ready, Vec::new())])
        .await
        .unwrap();
    let stale_error = store.commit_run(stale, Vec::new()).await.unwrap_err();
    assert_eq!(stale_error.code(), "stale_lease");

    store
        .admit_event(event(
            "event-cancel",
            now,
            vec![run(
                "run-cancel",
                "event-cancel",
                "cancel-group",
                ConcurrencyPolicy::Allow,
                now,
            )],
        ))
        .await
        .unwrap();
    assert!(store.request_cancel("run-cancel", now).await.unwrap());
    assert_eq!(
        store.get_run("run-cancel").await.unwrap().unwrap().status,
        RunStatus::Cancelled
    );

    let later = now + TimeDelta::hours(1);
    let reconciled = store
        .reconcile_timers(
            "contract:",
            vec![timer("contract:one", later), timer("contract:two", later)],
            now,
        )
        .await
        .unwrap();
    assert_eq!(reconciled.timers.len(), 2);
    let reconciled = store
        .reconcile_timers("contract:", vec![reconciled.timers[0].clone()], now)
        .await
        .unwrap();
    assert_eq!(reconciled.cancelled_ids, ["contract:two"]);

    store.upsert_timer(timer("due", now)).await.unwrap();
    let timer_claim = store
        .claim_due_timer("timer-worker", now, Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    assert!(store
        .claim_due_timer("other-worker", now, Duration::from_secs(30))
        .await
        .unwrap()
        .is_none());
    let mut fired = timer_claim.timer.clone();
    fired.status = TimerStatus::Fired;
    fired.fired_at = Some(now);
    store
        .fire_timer(
            timer_claim,
            fired,
            event("event-timer-fired", now, Vec::new()),
        )
        .await
        .unwrap();

    let head = store.events_after(0, 100).await.unwrap().head;
    assert_eq!(store.prune_events_through(head).await.unwrap(), head);
    let expired = store.events_after(0, 100).await.unwrap();
    assert!(expired.reset_required);
    assert_eq!(expired.retained_after, head);
    assert!(
        store
            .admit_event(event("event-first", now, Vec::new()))
            .await
            .unwrap()
            .duplicate
    );

    let snapshot = store.snapshot().await.unwrap();
    assert!(snapshot.events.is_empty());
    assert!(snapshot
        .runs
        .iter()
        .any(|record| record.plan.id == "run-replacement"));
    assert!(snapshot
        .timers
        .iter()
        .any(|record| record.id == "due" && record.status == TimerStatus::Fired));
}

#[tokio::test]
async fn memory_store_satisfies_the_shared_contract() {
    assert_store_contract(Arc::new(InMemoryRuntimeStore::new())).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_store_satisfies_the_shared_contract() {
    assert_store_contract(Arc::new(SqliteRuntimeStore::in_memory().unwrap())).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_store_satisfies_the_shared_contract() {
    let Ok(database_url) = std::env::var("MDBASE_RUNTIME_TEST_DATABASE_URL") else {
        assert_ne!(
            std::env::var("MDBASE_RUNTIME_REQUIRE_POSTGRES").as_deref(),
            Ok("1"),
            "shared PostgreSQL contract is required but its database URL is missing"
        );
        eprintln!("skipping shared PostgreSQL contract: no database URL configured");
        return;
    };
    let namespace = format!("contract:{}", Ulid::new());
    let store = PostgresRuntimeStore::connect(&database_url, namespace)
        .await
        .unwrap();
    assert_store_contract(Arc::new(store)).await;
}
