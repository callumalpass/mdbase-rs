#[cfg(feature = "postgres")]
use std::str::FromStr;
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
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
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

async fn race_gate() -> (Arc<tokio::sync::Barrier>, Arc<tokio::sync::Barrier>) {
    let gate = Arc::new(tokio::sync::Barrier::new(2));
    (gate.clone(), gate)
}

async fn assert_exact_full_and_firing_races(store: Arc<dyn RuntimeStore>) {
    let now = Utc.with_ymd_and_hms(2026, 8, 29, 0, 0, 0).single().unwrap();
    let later = now + TimeDelta::hours(1);

    // An absent exact member and an empty complete-set reconciliation must
    // linearize: generation 1 is either authoritative and scheduled, or it is
    // the exact generation cancelled by the complete set.
    let (exact_gate, full_gate) = race_gate().await;
    let exact_store = store.clone();
    let full_store = store.clone();
    let exact = async move {
        exact_gate.wait().await;
        exact_store
            .reconcile_timer_exact(timer("race:absent", later), now)
            .await
    };
    let full = async move {
        full_gate.wait().await;
        full_store
            .reconcile_timers("race:absent", Vec::new(), now)
            .await
    };
    let (exact, full) = tokio::join!(exact, full);
    let exact = exact.unwrap();
    let full = full.unwrap();
    assert_eq!(exact.generation, 1);
    let final_absent = store
        .timers("race:absent")
        .await
        .unwrap()
        .into_iter()
        .find(|timer| timer.id == "race:absent")
        .unwrap();
    let full_cancelled = full.cancelled_ids == ["race:absent"];
    assert_eq!(
        (full_cancelled, final_absent.status),
        if full_cancelled {
            (true, TimerStatus::Cancelled)
        } else {
            (false, TimerStatus::Scheduled)
        }
    );

    // The same race over an existing row must allocate generation 2 exactly
    // once; the complete set's cancellation boolean identifies the winner.
    store
        .reconcile_timer_exact(timer("race:existing", later), now)
        .await
        .unwrap();
    let mut changed = timer("race:existing", later);
    changed.data = json!({"writer": "exact"});
    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let exact_gate = gate.clone();
    let full_gate = gate;
    let exact_store = store.clone();
    let full_store = store.clone();
    let (exact, full) = tokio::join!(
        async move {
            exact_gate.wait().await;
            exact_store.reconcile_timer_exact(changed, now).await
        },
        async move {
            full_gate.wait().await;
            full_store
                .reconcile_timers("race:existing", Vec::new(), now)
                .await
        }
    );
    assert_eq!(exact.unwrap().generation, 2);
    let full = full.unwrap();
    let final_existing = store
        .timers("race:existing")
        .await
        .unwrap()
        .into_iter()
        .find(|timer| timer.id == "race:existing")
        .unwrap();
    assert_eq!(final_existing.generation, 2);
    assert_eq!(full.cancelled_ids, ["race:existing"]);
    assert!(matches!(
        final_existing.status,
        TimerStatus::Scheduled | TimerStatus::Cancelled
    ));

    // Exact reconciliation of a claimed timer and cancellation are both
    // generation-authoritative. Cancellation always succeeds; exact either
    // observes firing generation 1 or reschedules cancelled generation 2.
    let claimed_desired = timer("race:claimed", now);
    store
        .reconcile_timer_exact(claimed_desired.clone(), now)
        .await
        .unwrap();
    let claim = store
        .claim_due_timer("race-worker", now, Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.timer.id, "race:claimed");
    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let exact_gate = gate.clone();
    let cancel_gate = gate;
    let exact_store = store.clone();
    let cancel_store = store.clone();
    let (exact, cancelled) = tokio::join!(
        async move {
            exact_gate.wait().await;
            exact_store
                .reconcile_timer_exact(claimed_desired, now)
                .await
        },
        async move {
            cancel_gate.wait().await;
            cancel_store
                .cancel_timer("race:claimed", Some(1), now)
                .await
        }
    );
    let exact = exact.unwrap();
    assert!(cancelled.unwrap());
    let final_claimed = store
        .timers("race:claimed")
        .await
        .unwrap()
        .into_iter()
        .find(|timer| timer.id == "race:claimed")
        .unwrap();
    if exact.generation == 1 {
        assert_eq!(exact.status, TimerStatus::Firing);
        assert_eq!(final_claimed.status, TimerStatus::Cancelled);
        assert_eq!(final_claimed.generation, 1);
    } else {
        assert_eq!(exact.generation, 2);
        assert_eq!(final_claimed.status, TimerStatus::Scheduled);
        assert_eq!(final_claimed, exact);
    }
    let cleanup_cancelled = store
        .cancel_timer(
            "race:claimed",
            Some(final_claimed.generation),
            now + TimeDelta::seconds(1),
        )
        .await
        .unwrap();
    assert_eq!(
        cleanup_cancelled,
        final_claimed.status == TimerStatus::Scheduled
    );

    // A changed exact generation fences an in-flight fire. The fire either
    // commits first or reports a stable stale lease/generation; generation 2 is
    // authoritative in both serial orders.
    let firing_desired = timer("race:firing", now);
    store
        .reconcile_timer_exact(firing_desired.clone(), now)
        .await
        .unwrap();
    let claim = store
        .claim_due_timer("race-fire-worker", now, Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.timer.id, "race:firing");
    let mut fired = claim.timer.clone();
    fired.status = TimerStatus::Fired;
    fired.updated_at = now;
    fired.fired_at = Some(now);
    let mut replacement = firing_desired;
    replacement.data = json!({"generation": 2});
    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let fire_gate = gate.clone();
    let exact_gate = gate;
    let fire_store = store.clone();
    let exact_store = store.clone();
    let (fire, exact) = tokio::join!(
        async move {
            fire_gate.wait().await;
            fire_store
                .fire_timer(claim, fired, event("race-firing-event", now, Vec::new()))
                .await
        },
        async move {
            exact_gate.wait().await;
            exact_store.reconcile_timer_exact(replacement, now).await
        }
    );
    let exact = exact.unwrap();
    assert_eq!(exact.generation, 2);
    assert_eq!(exact.status, TimerStatus::Scheduled);
    if let Err(error) = fire {
        assert!(
            ["stale_lease", "stale_timer_lease", "stale_timer_generation"].contains(&error.code())
        );
    }
    let final_firing = store
        .timers("race:firing")
        .await
        .unwrap()
        .into_iter()
        .find(|timer| timer.id == "race:firing")
        .unwrap();
    assert_eq!(final_firing, exact);
}

async fn assert_exact_timer_reconcile_contract(store: Arc<dyn RuntimeStore>) {
    let now = Utc.with_ymd_and_hms(2026, 8, 29, 0, 0, 0).single().unwrap();
    let later = now + TimeDelta::hours(1);

    let scheduled_desired = timer("exact:scheduled", later);
    let scheduled = store
        .reconcile_timer_exact(scheduled_desired.clone(), now)
        .await
        .unwrap();
    assert_eq!(scheduled.generation, 1);
    let identical_scheduled = store
        .reconcile_timer_exact(scheduled_desired.clone(), now + TimeDelta::minutes(1))
        .await
        .unwrap();
    assert_eq!(identical_scheduled, scheduled);

    let mut changed_desired = scheduled_desired.clone();
    changed_desired.fire_at += TimeDelta::hours(1);
    let changed = store
        .reconcile_timer_exact(changed_desired, now + TimeDelta::minutes(2))
        .await
        .unwrap();
    assert_eq!(changed.generation, 2);
    assert_eq!(changed.status, TimerStatus::Scheduled);
    assert_eq!(changed.created_at, scheduled.created_at);
    assert_eq!(changed.fired_at, None);

    let cancelled_desired = timer("exact:cancelled", later);
    let cancelled = store
        .reconcile_timer_exact(cancelled_desired.clone(), now)
        .await
        .unwrap();
    assert!(!store
        .cancel_timer(&cancelled.id, Some(cancelled.generation + 1), now)
        .await
        .unwrap());
    assert!(store
        .cancel_timer(&cancelled.id, Some(cancelled.generation), now)
        .await
        .unwrap());
    let rescheduled = store
        .reconcile_timer_exact(cancelled_desired, now + TimeDelta::minutes(1))
        .await
        .unwrap();
    assert_eq!(rescheduled.generation, 2);
    assert_eq!(rescheduled.status, TimerStatus::Scheduled);
    assert_eq!(rescheduled.created_at, cancelled.created_at);
    assert_eq!(rescheduled.fired_at, None);

    let firing_desired = timer("exact:firing", now);
    store
        .reconcile_timer_exact(firing_desired.clone(), now)
        .await
        .unwrap();
    let firing = store
        .claim_due_timer("exact-worker", now, Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(firing.timer.id, firing_desired.id);
    let identical_firing = store
        .reconcile_timer_exact(firing_desired.clone(), now + TimeDelta::seconds(1))
        .await
        .unwrap();
    assert_eq!(identical_firing, firing.timer);
    assert!(store
        .claim_due_timer("other-worker", now, Duration::from_secs(30))
        .await
        .unwrap()
        .is_none());

    let mut fired = firing.timer.clone();
    fired.status = TimerStatus::Fired;
    fired.updated_at = now + TimeDelta::seconds(2);
    fired.fired_at = Some(now + TimeDelta::seconds(2));
    store
        .fire_timer(firing, fired.clone(), event("exact-fired", now, Vec::new()))
        .await
        .unwrap();
    let identical_fired = store
        .reconcile_timer_exact(firing_desired, now + TimeDelta::seconds(3))
        .await
        .unwrap();
    assert_eq!(identical_fired, fired);

    let short = store
        .reconcile_timer_exact(timer("prefix:a", later), now)
        .await
        .unwrap();
    let long = store
        .reconcile_timer_exact(timer("prefix:ab", later), now)
        .await
        .unwrap();
    let mut changed_short = timer("prefix:a", later);
    changed_short.data = json!({"changed": true});
    let replaced_short = store
        .reconcile_timer_exact(changed_short, now + TimeDelta::minutes(1))
        .await
        .unwrap();
    assert_eq!(replaced_short.generation, short.generation + 1);
    let prefix_timers = store.timers("prefix:").await.unwrap();
    assert_eq!(prefix_timers.len(), 2);
    assert_eq!(
        prefix_timers
            .iter()
            .find(|timer| timer.id == "prefix:ab")
            .unwrap(),
        &long
    );

    let concurrent_id = "exact:concurrent";
    let initial = store
        .reconcile_timer_exact(timer(concurrent_id, later), now)
        .await
        .unwrap();
    let mut exact_desired = timer(concurrent_id, later);
    exact_desired.data = json!({"writer": "exact"});
    let mut upsert_desired = timer(concurrent_id, later);
    upsert_desired.data = json!({"writer": "upsert"});
    let exact_store = store.clone();
    let upsert_store = store.clone();
    let cancel_store = store.clone();
    let (exact_result, upsert_result, cancel_result) = tokio::join!(
        exact_store.reconcile_timer_exact(exact_desired, now + TimeDelta::minutes(1)),
        upsert_store.upsert_timer(upsert_desired),
        cancel_store.cancel_timer(concurrent_id, Some(initial.generation), now)
    );
    exact_result.unwrap();
    upsert_result.unwrap();
    cancel_result.unwrap();
    let concurrent = store
        .timers(concurrent_id)
        .await
        .unwrap()
        .into_iter()
        .find(|timer| timer.id == concurrent_id)
        .unwrap();
    assert_eq!(concurrent.generation, 3);
    assert_eq!(concurrent.status, TimerStatus::Scheduled);

    assert_exact_full_and_firing_races(store).await;
}

#[tokio::test]
async fn memory_store_reconciles_exact_timers() {
    assert_exact_timer_reconcile_contract(Arc::new(InMemoryRuntimeStore::new())).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_store_reconciles_exact_timers() {
    assert_exact_timer_reconcile_contract(Arc::new(SqliteRuntimeStore::in_memory().unwrap())).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_store_reconciles_exact_timers() {
    let Ok(database_url) = std::env::var("MDBASE_RUNTIME_TEST_DATABASE_URL") else {
        assert_ne!(
            std::env::var("MDBASE_RUNTIME_REQUIRE_POSTGRES").as_deref(),
            Ok("1"),
            "PostgreSQL exact timer contract is required but its database URL is missing"
        );
        eprintln!("skipping PostgreSQL exact timer contract: no database URL configured");
        return;
    };
    let namespace = format!("exact-timer-contract:{}", Ulid::new());
    let store = PostgresRuntimeStore::connect(&database_url, namespace)
        .await
        .unwrap();
    assert_exact_timer_reconcile_contract(Arc::new(store)).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_absent_exact_and_full_payloads_linearize_generations() {
    let Ok(database_url) = std::env::var("MDBASE_RUNTIME_TEST_DATABASE_URL") else {
        assert_ne!(
            std::env::var("MDBASE_RUNTIME_REQUIRE_POSTGRES").as_deref(),
            Ok("1"),
            "PostgreSQL exact/full race is required but its database URL is missing"
        );
        eprintln!("skipping PostgreSQL exact/full race: no database URL configured");
        return;
    };
    let suffix = Ulid::new().to_string().to_lowercase();
    let namespace = format!("absent-exact-full:{suffix}");
    let sequence = format!("timer_insert_arrivals_{suffix}");
    let function = format!("timer_insert_gate_{suffix}");
    let trigger = format!("timer_insert_gate_{suffix}");
    let key_bytes = Ulid::new().to_bytes();
    let key = i64::from_be_bytes([
        key_bytes[0],
        key_bytes[1],
        key_bytes[2],
        key_bytes[3],
        key_bytes[4],
        key_bytes[5],
        key_bytes[6],
        key_bytes[7],
    ]);
    let store = PostgresRuntimeStore::connect(&database_url, namespace.clone())
        .await
        .unwrap();
    // Give the full writer a single-connection pool so its backend PID is
    // stable and pg_stat_activity can prove which statement is waiting.
    let full_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(PgConnectOptions::from_str(&database_url).unwrap())
        .await
        .unwrap();
    let full_store = PostgresRuntimeStore::new(full_pool, namespace.clone())
        .await
        .unwrap();

    let mut coordinator = None;
    let mut gate_held = false;
    let mut exact_task = None;
    let mut full_task = None;
    let body: Result<(), String> = async {
        let full_backend_pid = sqlx::query_scalar::<_, i32>("SELECT pg_backend_pid()")
            .fetch_one(full_store.pool())
            .await
            .map_err(|error| format!("query full-writer backend PID: {error}"))?;

        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SEQUENCE {sequence}")))
            .execute(store.pool())
            .await
            .map_err(|error| format!("create insert-arrival sequence: {error}"))?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               IF NEW.namespace = '{namespace}' THEN
                 PERFORM nextval('{sequence}');
                 PERFORM pg_advisory_xact_lock_shared({key});
               END IF;
               RETURN NEW;
             END $$"
        )))
        .execute(store.pool())
        .await
        .map_err(|error| format!("create insert-gate function: {error}"))?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TRIGGER {trigger} BEFORE INSERT ON mdbase_runtime_timers
             FOR EACH ROW EXECUTE FUNCTION {function}()"
        )))
        .execute(store.pool())
        .await
        .map_err(|error| format!("create insert-gate trigger: {error}"))?;

        coordinator = Some(
            store
                .pool()
                .acquire()
                .await
                .map_err(|error| format!("acquire advisory-gate connection: {error}"))?,
        );
        let gate_connection = coordinator
            .as_mut()
            .ok_or("advisory-gate connection missing after acquisition")?;
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(key)
            .execute(&mut **gate_connection)
            .await
            .map_err(|error| format!("acquire advisory gate: {error}"))?;
        gate_held = true;

        let now = Utc::now();
        let mut exact_desired = timer("absent:shared", now + TimeDelta::hours(1));
        exact_desired.data = json!({"writer": "exact"});
        let mut full_desired = timer("absent:shared", now + TimeDelta::hours(2));
        full_desired.data = json!({"writer": "full"});
        let exact_store = store.clone();
        exact_task = Some(tokio::spawn(async move {
            exact_store
                .reconcile_timer_exact(exact_desired, now)
                .await
        }));

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let arrivals = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                    "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM {sequence}"
                )))
                .fetch_one(store.pool())
                .await
                .map_err(|error| format!("query exact insert arrival: {error}"))?;
                if arrivals >= 1 {
                    return Ok::<_, String>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| "exact reconciliation did not reach the test-only insert gate".to_string())??;

        full_task = Some(tokio::spawn(async move {
            full_store
                .reconcile_timers("absent:", vec![full_desired], now)
                .await
        }));
        // Prove the full writer has reached the shared namespace lock and is
        // waiting behind exact. Under the old per-ID exact lock it instead reaches
        // the INSERT trigger gate, which the arrival check rejects.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let waiting = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (
                         SELECT 1
                         FROM pg_stat_activity
                         WHERE pid = $1
                           AND state = 'active'
                           AND wait_event_type = 'Lock'
                           AND wait_event = 'advisory'
                           AND query = 'SELECT pg_advisory_xact_lock(hashtextextended($1 || '':timers'', 0))'
                     )",
                )
                .bind(full_backend_pid)
                .fetch_one(store.pool())
                .await
                .map_err(|error| format!("query full-writer wait state: {error}"))?;
                if waiting {
                    return Ok::<_, String>(());
                }
                let arrivals = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                    "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM {sequence}"
                )))
                .fetch_one(store.pool())
                .await
                .map_err(|error| format!("query full-writer insert arrival: {error}"))?;
                if arrivals != 1 {
                    return Err(
                        "full reconciliation reached INSERT without waiting at the timer namespace lock"
                            .to_string(),
                    );
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| "full reconciliation did not wait at the timer namespace lock".to_string())??;

        let unlocked = sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
            .bind(key)
            .fetch_one(&mut **coordinator.as_mut().ok_or("advisory-gate connection missing")?)
            .await
            .map_err(|error| format!("release advisory gate: {error}"))?;
        if !unlocked {
            return Err("advisory gate was not held by its coordinator".to_string());
        }
        gate_held = false;

        let exact = exact_task
            .take()
            .ok_or("exact task missing")?
            .await
            .map_err(|error| format!("join exact task: {error}"))?
            .map_err(|error| format!("exact reconciliation: {error}"))?;
        let mut full_timers = full_task
            .take()
            .ok_or("full task missing")?
            .await
            .map_err(|error| format!("join full task: {error}"))?
            .map_err(|error| format!("full reconciliation: {error}"))?
            .timers;
        let full = full_timers
            .pop()
            .ok_or("full reconciliation returned no timer")?;
        let mut generations = [exact.generation, full.generation];
        generations.sort();
        if generations != [1, 2] {
            return Err(format!("expected generations [1, 2], got {generations:?}"));
        }
        if exact.data == full.data {
            return Err("exact and full payloads unexpectedly match".to_string());
        }
        let final_timer = store
            .timers("absent:")
            .await
            .map_err(|error| format!("query final timer: {error}"))?
            .pop()
            .ok_or("final timer missing")?;
        if final_timer.generation != 2 {
            return Err(format!(
                "expected final generation 2, got {}",
                final_timer.generation
            ));
        }
        if final_timer != exact && final_timer != full {
            return Err("final timer matches neither serialized writer".to_string());
        }
        Ok(())
    }
    .await;

    let mut failures = body.err().into_iter().collect::<Vec<_>>();

    // Settle every held resource before DDL cleanup. A failed unlock closes the
    // coordinator backend so a session-level advisory lock cannot leak.
    if let Some(mut connection) = coordinator.take() {
        if gate_held {
            match sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
                .bind(key)
                .fetch_one(&mut *connection)
                .await
            {
                Ok(true) => {}
                Ok(false) => failures.push(
                    "cleanup could not release advisory gate because it was not held".to_string(),
                ),
                Err(error) => {
                    failures.push(format!("cleanup failed to release advisory gate: {error}"));
                    connection.close_on_drop();
                }
            }
        }
    }
    if let Some(task) = exact_task.take() {
        if !task.is_finished() {
            task.abort();
        }
        if let Err(error) = task.await {
            if !error.is_cancelled() {
                failures.push(format!("cleanup failed to join exact task: {error}"));
            }
        }
    }
    if let Some(task) = full_task.take() {
        if !task.is_finished() {
            task.abort();
        }
        if let Err(error) = task.await {
            if !error.is_cancelled() {
                failures.push(format!("cleanup failed to join full task: {error}"));
            }
        }
    }

    // Use a new pool/backend and attempt every drop even if an earlier one
    // fails, then verify the catalogs before reporting the guarded failure.
    match PgPoolOptions::new()
        .max_connections(1)
        .connect_with(PgConnectOptions::from_str(&database_url).unwrap())
        .await
    {
        Ok(cleanup_pool) => {
            for (object, statement) in [
                (
                    "trigger",
                    format!("DROP TRIGGER IF EXISTS {trigger} ON mdbase_runtime_timers"),
                ),
                ("function", format!("DROP FUNCTION IF EXISTS {function}()")),
                ("sequence", format!("DROP SEQUENCE IF EXISTS {sequence}")),
            ] {
                if let Err(error) = sqlx::query(sqlx::AssertSqlSafe(statement))
                    .execute(&cleanup_pool)
                    .await
                {
                    failures.push(format!("cleanup failed to drop {object}: {error}"));
                }
            }
            let trigger_exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM pg_trigger WHERE tgname = $1 AND NOT tgisinternal)",
            )
            .bind(&trigger)
            .fetch_one(&cleanup_pool)
            .await;
            let function_exists =
                sqlx::query_scalar::<_, bool>("SELECT to_regprocedure($1) IS NOT NULL")
                    .bind(format!("{function}()"))
                    .fetch_one(&cleanup_pool)
                    .await;
            let sequence_exists =
                sqlx::query_scalar::<_, bool>("SELECT to_regclass($1) IS NOT NULL")
                    .bind(&sequence)
                    .fetch_one(&cleanup_pool)
                    .await;
            for (object, exists) in [
                ("trigger", trigger_exists),
                ("function", function_exists),
                ("sequence", sequence_exists),
            ] {
                match exists {
                    Ok(false) => {}
                    Ok(true) => failures.push(format!("cleanup left {object} in the catalog")),
                    Err(error) => failures.push(format!(
                        "cleanup could not verify {object} removal: {error}"
                    )),
                }
            }
            cleanup_pool.close().await;
        }
        Err(error) => failures.push(format!("connect fresh cleanup backend: {error}")),
    }

    if !failures.is_empty() {
        panic!("{}", failures.join("\n"));
    }
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_full_reconcile_and_multi_expiry_claim_do_not_deadlock() {
    let Ok(database_url) = std::env::var("MDBASE_RUNTIME_TEST_DATABASE_URL") else {
        assert_ne!(
            std::env::var("MDBASE_RUNTIME_REQUIRE_POSTGRES").as_deref(),
            Ok("1"),
            "PostgreSQL reconcile/claim race is required but its database URL is missing"
        );
        eprintln!("skipping PostgreSQL reconcile/claim race: no database URL configured");
        return;
    };
    let options = PgConnectOptions::from_str(&database_url).unwrap().options([
        ("enable_indexscan", "off"),
        ("enable_indexonlyscan", "off"),
        ("enable_bitmapscan", "off"),
        ("deadlock_timeout", "100ms"),
    ]);
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .unwrap();
    let suffix = Ulid::new().to_string().to_lowercase();
    let namespace = format!("reconcile-expiry-race:{suffix}");
    let function = format!("timer_expiry_delay_{suffix}");
    let trigger = format!("timer_expiry_delay_{suffix}");
    let store = PostgresRuntimeStore::new(pool, namespace.clone())
        .await
        .unwrap();
    let now = Utc::now();

    // Descending heap order plus forced sequential scans makes the historical
    // unordered UPDATE acquire rows opposite to full reconciliation's id order.
    for index in (0..64).rev() {
        store
            .upsert_timer(timer(
                &format!("expiry:{index:03}"),
                now - TimeDelta::minutes(1),
            ))
            .await
            .unwrap();
    }
    sqlx::query(
        "UPDATE mdbase_runtime_timers
         SET lease_worker = 'expired-worker', lease_token = id, lease_expires_at = $2
         WHERE namespace = $1",
    )
    .bind(&namespace)
    .bind(now - TimeDelta::seconds(1))
    .execute(store.pool())
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF OLD.namespace = '{namespace}'
              AND OLD.lease_token IS NOT NULL
              AND NEW.lease_token IS NULL
              AND NEW.status = OLD.status THEN
             PERFORM pg_sleep(0.002);
           END IF;
           RETURN NEW;
         END $$"
    )))
    .execute(store.pool())
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER {trigger} BEFORE UPDATE ON mdbase_runtime_timers
         FOR EACH ROW EXECUTE FUNCTION {function}()"
    )))
    .execute(store.pool())
    .await
    .unwrap();

    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let full_gate = gate.clone();
    let claim_gate = gate;
    let full_store = store.clone();
    let claim_store = store.clone();
    let (full, claim) = tokio::time::timeout(Duration::from_secs(10), async move {
        tokio::join!(
            async move {
                full_gate.wait().await;
                full_store
                    .reconcile_timers("expiry:", Vec::new(), now)
                    .await
            },
            async move {
                claim_gate.wait().await;
                claim_store
                    .claim_due_timer("race-worker", now, Duration::from_secs(30))
                    .await
            }
        )
    })
    .await
    .expect("reconcile/expiry race timed out");
    let full = full.expect("full reconciliation must not deadlock or abort");
    claim.expect("lease expiry/claim must not deadlock or abort");
    assert_eq!(full.cancelled_ids.len(), 64);
    let final_timers = store.timers("expiry:").await.unwrap();
    assert_eq!(final_timers.len(), 64);
    assert!(final_timers
        .iter()
        .all(|timer| timer.status == TimerStatus::Cancelled));

    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP TRIGGER {trigger} ON mdbase_runtime_timers"
    )))
    .execute(store.pool())
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP FUNCTION {function}()")))
        .execute(store.pool())
        .await
        .unwrap();
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_timer_generation_exhaustion_rolls_back() {
    use mdbase_runtime::TIMER_GENERATION_MAX;

    let Ok(database_url) = std::env::var("MDBASE_RUNTIME_TEST_DATABASE_URL") else {
        assert_ne!(
            std::env::var("MDBASE_RUNTIME_REQUIRE_POSTGRES").as_deref(),
            Ok("1"),
            "PostgreSQL timer exhaustion contract is required but its database URL is missing"
        );
        eprintln!("skipping PostgreSQL timer exhaustion contract: no database URL configured");
        return;
    };
    let namespace = format!("timer-exhaustion-contract:{}", Ulid::new());
    let store = PostgresRuntimeStore::connect(&database_url, namespace)
        .await
        .unwrap();
    let now = Utc::now();
    let exhausted = store.upsert_timer(timer("max:timer", now)).await.unwrap();
    store.upsert_timer(timer("max:omitted", now)).await.unwrap();
    let mut exhausted_at_max = exhausted;
    exhausted_at_max.generation = TIMER_GENERATION_MAX;
    sqlx::query(
        "UPDATE mdbase_runtime_timers SET generation = $3, record_json = $4
         WHERE namespace = $1 AND id = $2",
    )
    .bind(store.namespace())
    .bind("max:timer")
    .bind(i64::MAX)
    .bind(serde_json::to_value(&exhausted_at_max).unwrap())
    .execute(store.pool())
    .await
    .unwrap();
    let before = store.snapshot().await.unwrap().timers;
    let mut changed = exhausted_at_max;
    changed.data = json!({"changed": true});

    for error in [
        store.upsert_timer(changed.clone()).await.unwrap_err(),
        store
            .reconcile_timer_exact(changed.clone(), now)
            .await
            .unwrap_err(),
        store
            .reconcile_timers("max:", vec![changed], now)
            .await
            .unwrap_err(),
    ] {
        assert_eq!(error.code(), "timer_generation_exhausted");
        assert_eq!(store.snapshot().await.unwrap().timers, before);
    }
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
