use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeDelta, TimeZone, Utc};
use mdbase::runtime_contracts::{
    ComposeOptions, ContractDocument, ContractSource, PolicySelector, RuntimeContracts,
    RuntimeRegistry,
};
#[cfg(feature = "sqlite")]
use mdbase_runtime::SqliteRuntimeStore;
use mdbase_runtime::{
    ActionDispatch, ActionProvider, ActionResponse, AdmitOutcome, AuthorizationDecision, Claim,
    Clock, DispatchAuthorizer, DispatchFailure, DispatchOutcome, InMemoryRuntimeStore, ManualClock,
    PreparedEvent, ProviderRegistry, RunStatus, Runtime, RuntimeConfig, RuntimeResult,
    RuntimeStore, StoreSnapshot, TimerClaim, TimerFireOutcome, TimerReconcileOutcome,
    TimerReconcileRequest, TimerRecord, TimerRequest,
};
use serde_json::{json, Value};
#[cfg(feature = "sqlite")]
use tempfile::tempdir;

#[derive(Default)]
struct AllowAuthorizer;

#[async_trait]
impl DispatchAuthorizer for AllowAuthorizer {
    async fn authorize(&self, _request: &ActionDispatch) -> AuthorizationDecision {
        AuthorizationDecision::Allow
    }
}

#[derive(Default)]
struct RecordingProvider {
    requests: Mutex<Vec<ActionDispatch>>,
    cancellations: Mutex<Vec<String>>,
    fail_unknown_once: Mutex<bool>,
    always_unknown: bool,
}

impl RecordingProvider {
    fn fail_unknown_once() -> Self {
        Self {
            fail_unknown_once: Mutex::new(true),
            ..Self::default()
        }
    }

    fn always_unknown() -> Self {
        Self {
            always_unknown: true,
            ..Self::default()
        }
    }

    fn requests(&self) -> Vec<ActionDispatch> {
        self.requests.lock().unwrap().clone()
    }

    fn cancellations(&self) -> Vec<String> {
        self.cancellations.lock().unwrap().clone()
    }
}

#[async_trait]
impl ActionProvider for RecordingProvider {
    async fn dispatch(&self, request: ActionDispatch) -> Result<ActionResponse, DispatchFailure> {
        self.requests.lock().unwrap().push(request.clone());
        let mut fail_once = self.fail_unknown_once.lock().unwrap();
        if self.always_unknown || *fail_once {
            *fail_once = false;
            return Err(DispatchFailure {
                code: "transport_lost".to_string(),
                message: "Provider response was lost.".to_string(),
                outcome: DispatchOutcome::Unknown,
            });
        }
        Ok(ActionResponse {
            output: request.input,
            receipt: Some(json!({"invocation_id": request.invocation_id})),
            emitted_events: Vec::new(),
        })
    }

    async fn cancel(&self, invocation_id: &str) -> Result<(), DispatchFailure> {
        self.cancellations
            .lock()
            .unwrap()
            .push(invocation_id.to_string());
        Ok(())
    }
}

#[tokio::test]
async fn executes_variables_conditions_and_for_each_deterministically() {
    let registry = registry(true);
    let store = Arc::new(InMemoryRuntimeStore::new());
    let provider = Arc::new(RecordingProvider::default());
    let (runtime, _) = runtime(store.clone(), provider.clone());

    let delivery = runtime
        .deliver_event(&registry, changed_event("evt_one", json!(["a", "b"])))
        .await
        .unwrap();
    assert_eq!(delivery.admitted_run_ids.len(), 1);
    let outcomes = runtime.drain(&registry, 10).await.unwrap();
    assert_eq!(outcomes.len(), 1);

    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.runs.len(), 1);
    let run = &snapshot.runs[0];
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(
        run.steps[0].outputs,
        vec![
            json!({"from_var": "a", "value": "a"}),
            json!({"from_var": "a", "value": "b"})
        ]
    );
    assert_eq!(run.attempts.len(), 2);
    assert_ne!(run.attempts[0].invocation_id, run.attempts[1].invocation_id);
    assert_eq!(provider.requests().len(), 2);
    let contracts = RuntimeContracts::new().unwrap();
    let validation = contracts.validate_contract(&ContractDocument::virtual_contract(
        run.materialized_contract(),
    ));
    assert!(validation.valid, "{:#?}", validation.diagnostics);
}

#[tokio::test]
async fn reconciles_a_timer_prefix_without_rescheduling_unchanged_or_fired_timers() {
    let store = Arc::new(InMemoryRuntimeStore::new());
    let provider = Arc::new(RecordingProvider::default());
    let (runtime, clock) = runtime(store.clone(), provider);
    let first_at = clock.now();
    let second_at = clock.now() + TimeDelta::hours(2);
    let request = |id: &str, fire_at| TimerRequest {
        id: id.to_string(),
        fire_at,
        event_type: "timer.fired".to_string(),
        contract_version: 1,
        payload: json!({"owner": "grant-a"}),
    };

    runtime
        .upsert_timer(request("grant-b:keep", second_at))
        .await
        .unwrap();
    let first = runtime
        .reconcile_timers(TimerReconcileRequest {
            id_prefix: "grant-a:".to_string(),
            timers: vec![
                request("grant-a:one", first_at),
                request("grant-a:two", second_at),
            ],
        })
        .await
        .unwrap();
    assert_eq!(
        first
            .timers
            .iter()
            .map(|timer| (timer.id.clone(), timer.generation))
            .collect::<Vec<_>>(),
        vec![
            ("grant-a:one".to_string(), 1),
            ("grant-a:two".to_string(), 1)
        ]
    );
    let fired = runtime.fire_due_timer(&registry(true)).await.unwrap();
    assert!(matches!(
        fired,
        TimerFireOutcome::Fired {
            ref timer_id,
            generation: 1,
            ..
        } if timer_id == "grant-a:one"
    ));

    let second = runtime
        .reconcile_timers(TimerReconcileRequest {
            id_prefix: "grant-a:".to_string(),
            timers: vec![
                request("grant-a:one", first_at),
                request("grant-a:three", second_at),
            ],
        })
        .await
        .unwrap();
    assert_eq!(second.cancelled_ids, vec!["grant-a:two"]);
    assert_eq!(second.timers[0].generation, 1);
    assert_eq!(second.timers[0].status, mdbase_runtime::TimerStatus::Fired);

    let moved = runtime
        .reconcile_timers(TimerReconcileRequest {
            id_prefix: "grant-a:".to_string(),
            timers: vec![request("grant-a:one", second_at)],
        })
        .await
        .unwrap();
    assert_eq!(moved.timers[0].generation, 2);
    let snapshot = store.snapshot().await.unwrap();
    assert!(snapshot
        .timers
        .iter()
        .any(|timer| timer.id == "grant-b:keep"));
    assert!(runtime
        .timers("grant-a:")
        .await
        .unwrap()
        .iter()
        .all(|timer| timer.id.starts_with("grant-a:")));
}

#[tokio::test]
async fn rejects_timer_reconciliation_outside_its_prefix() {
    let store = Arc::new(InMemoryRuntimeStore::new());
    let provider = Arc::new(RecordingProvider::default());
    let (runtime, clock) = runtime(store, provider);
    let error = runtime
        .reconcile_timers(TimerReconcileRequest {
            id_prefix: "grant-a:".to_string(),
            timers: vec![TimerRequest {
                id: "grant-b:timer".to_string(),
                fire_at: clock.now(),
                event_type: "timer.fired".to_string(),
                contract_version: 1,
                payload: json!({}),
            }],
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), "timer_outside_reconciliation_prefix");
}

#[tokio::test]
async fn duplicate_delivery_returns_original_cursor_without_new_runs() {
    let registry = registry(true);
    let store = Arc::new(InMemoryRuntimeStore::new());
    let (runtime, _) = runtime(store.clone(), Arc::new(RecordingProvider::default()));
    let event = changed_event("evt_duplicate", json!(["a"]));

    let first = runtime
        .deliver_event(&registry, event.clone())
        .await
        .unwrap();
    let second = runtime.deliver_event(&registry, event).await.unwrap();

    assert!(!first.duplicate);
    assert!(second.duplicate);
    assert_eq!(first.cursor, second.cursor);
    assert_eq!(store.snapshot().await.unwrap().runs.len(), 1);
}

#[tokio::test]
async fn skip_treats_queued_work_as_active_in_every_local_store() {
    for store in local_state_stores() {
        let mut registry = registry(true);
        registry
            .workflows
            .get_mut("test.workflow")
            .unwrap()
            .contract["run"]["concurrency"]["policy"] = json!("skip");
        let (runtime, _) = runtime(store.clone(), Arc::new(RecordingProvider::default()));

        let first = runtime
            .deliver_event(&registry, changed_event("evt_skip_first", json!(["a"])))
            .await
            .unwrap();
        let second = runtime
            .deliver_event(&registry, changed_event("evt_skip_second", json!(["b"])))
            .await
            .unwrap();

        assert_eq!(first.admitted_run_ids.len(), 1);
        assert!(second.admitted_run_ids.is_empty());
        assert_eq!(second.skipped_run_ids.len(), 1);
        assert_eq!(store.snapshot().await.unwrap().runs.len(), 1);
    }
}

#[tokio::test]
async fn queue_preserves_event_order_when_earlier_work_is_not_ready() {
    for store in local_state_stores() {
        let mut delayed_registry = registry(true);
        delayed_registry
            .workflows
            .get_mut("test.workflow")
            .unwrap()
            .contract["triggers"][0]["debounce"] = json!("1m");
        let ready_registry = registry(true);
        let provider = Arc::new(RecordingProvider::default());
        let (runtime, clock) = runtime(store, provider.clone());

        runtime
            .deliver_event(
                &delayed_registry,
                changed_event("evt_queue_first", json!(["a"])),
            )
            .await
            .unwrap();
        runtime
            .deliver_event(
                &ready_registry,
                changed_event("evt_queue_second", json!(["b"])),
            )
            .await
            .unwrap();

        assert_eq!(
            runtime.work_once(&ready_registry).await.unwrap(),
            mdbase_runtime::WorkerOutcome::Idle
        );
        clock.advance(TimeDelta::minutes(1));
        runtime.work_once(&ready_registry).await.unwrap();
        runtime.work_once(&ready_registry).await.unwrap();
        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].input["value"], json!("a"));
        assert_eq!(requests[1].input["value"], json!("b"));
    }
}

#[tokio::test]
async fn replace_cancels_queued_work_before_admitting_its_successor() {
    for store in local_state_stores() {
        let mut registry = registry(true);
        registry
            .workflows
            .get_mut("test.workflow")
            .unwrap()
            .contract["run"]["concurrency"]["policy"] = json!("replace");
        let provider = Arc::new(RecordingProvider::default());
        let (runtime, _) = runtime(store.clone(), provider.clone());

        let first = runtime
            .deliver_event(&registry, changed_event("evt_replace_first", json!(["a"])))
            .await
            .unwrap();
        let second = runtime
            .deliver_event(&registry, changed_event("evt_replace_second", json!(["b"])))
            .await
            .unwrap();

        assert_eq!(
            second.cancellation_requested_run_ids,
            first.admitted_run_ids
        );
        let replaced = runtime
            .get_run(&first.admitted_run_ids[0])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replaced.status, RunStatus::Cancelled);
        assert_eq!(
            replaced.steps[0].status,
            mdbase_runtime::StepStatus::Cancelled
        );

        let outcome = runtime.work_once(&registry).await.unwrap();
        assert!(matches!(
            outcome,
            mdbase_runtime::WorkerOutcome::Completed {
                status: RunStatus::Succeeded,
                ..
            }
        ));
        assert_eq!(provider.requests().len(), 1);
        assert_eq!(provider.requests()[0].input["value"], json!("b"));
    }
}

#[tokio::test]
async fn idempotent_unknown_dispatch_replays_the_same_invocation_after_lease_expiry() {
    let registry = registry(true);
    let store = Arc::new(InMemoryRuntimeStore::new());
    let provider = Arc::new(RecordingProvider::fail_unknown_once());
    let (runtime, clock) = runtime(store.clone(), provider.clone());
    runtime
        .deliver_event(&registry, changed_event("evt_replay", json!(["a"])))
        .await
        .unwrap();

    let failure = runtime.work_once(&registry).await.unwrap_err();
    assert_eq!(failure.code(), "action_provider_error");
    clock.advance(TimeDelta::seconds(31));
    let outcome = runtime.work_once(&registry).await.unwrap();
    assert!(matches!(
        outcome,
        mdbase_runtime::WorkerOutcome::Completed {
            status: RunStatus::Succeeded,
            ..
        }
    ));

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].invocation_id, requests[1].invocation_id);
    assert_eq!(requests[0].attempt, requests[1].attempt);
}

#[tokio::test]
async fn cancelled_cooperative_dispatch_is_not_replayed_during_recovery() {
    let registry = registry(true);
    let store = Arc::new(InMemoryRuntimeStore::new());
    let provider = Arc::new(RecordingProvider::fail_unknown_once());
    let (runtime, clock) = runtime(store.clone(), provider.clone());
    let delivery = runtime
        .deliver_event(
            &registry,
            changed_event("evt_cancel_recovery", json!(["a"])),
        )
        .await
        .unwrap();

    runtime.work_once(&registry).await.unwrap_err();
    let cancellation = runtime
        .cancel_run(&delivery.admitted_run_ids[0])
        .await
        .unwrap();
    assert!(cancellation.accepted);
    assert!(cancellation.provider_notified);
    clock.advance(TimeDelta::seconds(31));

    let recovered = runtime.work_once(&registry).await.unwrap();
    assert!(matches!(
        recovered,
        mdbase_runtime::WorkerOutcome::Completed {
            status: RunStatus::Indeterminate,
            ..
        }
    ));
    assert_eq!(provider.requests().len(), 1);
    assert!(!provider.cancellations().is_empty());
    let run = runtime
        .get_run(&delivery.admitted_run_ids[0])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        run.steps[0].status,
        mdbase_runtime::StepStatus::Indeterminate
    );
    assert_eq!(run.next_step, run.steps.len());
}

#[tokio::test]
async fn replacement_waits_when_cancelled_predecessor_is_indeterminate() {
    let mut registry = registry(true);
    registry
        .workflows
        .get_mut("test.workflow")
        .unwrap()
        .contract["run"]["concurrency"]["policy"] = json!("replace");
    let store = Arc::new(InMemoryRuntimeStore::new());
    let provider = Arc::new(RecordingProvider::fail_unknown_once());
    let (runtime, clock) = runtime(store, provider.clone());
    let first = runtime
        .deliver_event(
            &registry,
            changed_event("evt_replace_running", json!(["a"])),
        )
        .await
        .unwrap();
    runtime.work_once(&registry).await.unwrap_err();

    let replacement = runtime
        .deliver_event(
            &registry,
            changed_event("evt_replace_waiting", json!(["b"])),
        )
        .await
        .unwrap();
    assert_eq!(
        replacement.cancellation_requested_run_ids,
        first.admitted_run_ids
    );
    clock.advance(TimeDelta::seconds(31));
    let predecessor = runtime.work_once(&registry).await.unwrap();
    assert!(matches!(
        predecessor,
        mdbase_runtime::WorkerOutcome::Completed {
            status: RunStatus::Indeterminate,
            ..
        }
    ));

    assert_eq!(
        runtime.work_once(&registry).await.unwrap(),
        mdbase_runtime::WorkerOutcome::Idle
    );
    let replacement = runtime
        .get_run(&replacement.admitted_run_ids[0])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replacement.status, RunStatus::Queued);
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn unknown_non_idempotent_dispatch_is_indeterminate_and_never_replayed() {
    let registry = registry(false);
    let store = Arc::new(InMemoryRuntimeStore::new());
    let provider = Arc::new(RecordingProvider::always_unknown());
    let (runtime, clock) = runtime(store.clone(), provider.clone());
    runtime
        .deliver_event(&registry, changed_event("evt_indeterminate", json!(["a"])))
        .await
        .unwrap();

    let first = runtime.work_once(&registry).await.unwrap();
    assert!(matches!(
        first,
        mdbase_runtime::WorkerOutcome::Completed {
            status: RunStatus::Indeterminate,
            ..
        }
    ));
    clock.advance(TimeDelta::minutes(1));
    assert_eq!(
        runtime.work_once(&registry).await.unwrap(),
        mdbase_runtime::WorkerOutcome::Idle
    );
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn workflow_deadline_fails_before_a_new_effect_is_dispatched() {
    let mut registry = registry(true);
    registry
        .workflows
        .get_mut("test.workflow")
        .unwrap()
        .contract["run"]["limits"] = json!({"timeout": "1s", "max_items": 100});
    let store = Arc::new(InMemoryRuntimeStore::new());
    let provider = Arc::new(RecordingProvider::default());
    let (runtime, clock) = runtime(store.clone(), provider.clone());
    runtime
        .deliver_event(&registry, changed_event("evt_timeout", json!(["a"])))
        .await
        .unwrap();
    clock.advance(TimeDelta::seconds(2));

    let outcome = runtime.work_once(&registry).await.unwrap();
    assert!(matches!(
        outcome,
        mdbase_runtime::WorkerOutcome::Completed {
            status: RunStatus::Failed,
            ..
        }
    ));
    let run = &store.snapshot().await.unwrap().runs[0];
    assert_eq!(run.steps[0].status, mdbase_runtime::StepStatus::TimedOut);
    assert!(run.attempts.is_empty());
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn deterministic_expression_failures_are_committed_not_retried_by_lease() {
    let mut registry = registry(true);
    let workflow = &mut registry
        .workflows
        .get_mut("test.workflow")
        .unwrap()
        .contract;
    workflow["steps"][0]["input"] = json!({"value": {"$expr": "event.payload.items["}});
    workflow["steps"].as_array_mut().unwrap().push(json!({
        "id": "must-not-run",
        "action": "test.echo",
        "input": {"value": "unexpected"}
    }));
    let store = Arc::new(InMemoryRuntimeStore::new());
    let provider = Arc::new(RecordingProvider::default());
    let (runtime, clock) = runtime(store.clone(), provider.clone());
    runtime
        .deliver_event(
            &registry,
            changed_event("evt_expression_failure", json!(["a"])),
        )
        .await
        .unwrap();

    let outcome = runtime.work_once(&registry).await.unwrap();
    assert!(matches!(
        outcome,
        mdbase_runtime::WorkerOutcome::Completed {
            status: RunStatus::Failed,
            ..
        }
    ));
    clock.advance(TimeDelta::minutes(1));
    assert_eq!(
        runtime.work_once(&registry).await.unwrap(),
        mdbase_runtime::WorkerOutcome::Idle
    );
    assert!(provider.requests().is_empty());
    let run = &store.snapshot().await.unwrap().runs[0];
    assert_eq!(run.steps[0].status, mdbase_runtime::StepStatus::Failed);
    assert_eq!(run.steps[1].status, mdbase_runtime::StepStatus::Skipped);
    assert_eq!(run.next_step, run.steps.len());
}

#[tokio::test]
#[cfg(feature = "sqlite")]
async fn sqlite_store_survives_reopen_and_preserves_event_deduplication() {
    let registry = registry(true);
    let directory = tempdir().unwrap();
    let path = directory.path().join("runtime.sqlite");
    let event = changed_event("evt_sqlite", json!(["a"]));
    {
        let store = Arc::new(SqliteRuntimeStore::open(&path).unwrap());
        let (runtime, _) = runtime(store.clone(), Arc::new(RecordingProvider::default()));
        runtime
            .deliver_event(&registry, event.clone())
            .await
            .unwrap();
        runtime.drain(&registry, 10).await.unwrap();
        assert_eq!(
            store.snapshot().await.unwrap().runs[0].status,
            RunStatus::Succeeded
        );
    }
    {
        let store = Arc::new(SqliteRuntimeStore::open(&path).unwrap());
        let (runtime, _) = runtime(store.clone(), Arc::new(RecordingProvider::default()));
        let duplicate = runtime.deliver_event(&registry, event).await.unwrap();
        assert!(duplicate.duplicate);
        let snapshot = store.snapshot().await.unwrap();
        assert_eq!(snapshot.events.len(), 1);
        assert_eq!(snapshot.runs.len(), 1);
    }
}

#[tokio::test]
#[cfg(feature = "sqlite")]
async fn sqlite_timer_reconciliation_is_atomic_and_survives_reopen() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("runtime.sqlite");
    let fire_at = Utc.with_ymd_and_hms(2026, 7, 25, 9, 0, 0).single().unwrap();
    let timer = |id: &str| TimerRequest {
        id: id.to_string(),
        fire_at,
        event_type: "timer.fired".to_string(),
        contract_version: 1,
        payload: json!({"private": id}),
    };
    {
        let store = Arc::new(SqliteRuntimeStore::open(&path).unwrap());
        let (runtime, _) = runtime(store, Arc::new(RecordingProvider::default()));
        let outcome = runtime
            .reconcile_timers(TimerReconcileRequest {
                id_prefix: "grant-a:".to_string(),
                timers: vec![timer("grant-a:one"), timer("grant-a:two")],
            })
            .await
            .unwrap();
        assert_eq!(outcome.timers.len(), 2);
    }
    {
        let store = Arc::new(SqliteRuntimeStore::open(&path).unwrap());
        let (runtime, _) = runtime(store, Arc::new(RecordingProvider::default()));
        let outcome = runtime
            .reconcile_timers(TimerReconcileRequest {
                id_prefix: "grant-a:".to_string(),
                timers: vec![timer("grant-a:one")],
            })
            .await
            .unwrap();
        assert_eq!(outcome.timers[0].generation, 1);
        assert_eq!(outcome.cancelled_ids, vec!["grant-a:two"]);
        assert_eq!(runtime.timers("grant-a:").await.unwrap().len(), 2);
    }
}

#[tokio::test]
#[cfg(feature = "sqlite")]
async fn journal_retention_preserves_dedupe_tombstones_and_reports_reset() {
    let registry = registry(true);
    let store = Arc::new(SqliteRuntimeStore::in_memory().unwrap());
    let (runtime, _) = runtime(store.clone(), Arc::new(RecordingProvider::default()));
    let event = changed_event("evt_retained", json!(["a"]));
    let first = runtime
        .deliver_event(&registry, event.clone())
        .await
        .unwrap();
    assert_eq!(runtime.prune_events_through(first.cursor).await.unwrap(), 1);

    let page = runtime.events_after(0, 100).await.unwrap();
    assert!(page.reset_required);
    assert_eq!(page.retained_after, 1);
    assert_eq!(page.head, 1);
    assert!(page.events.is_empty());

    let duplicate = runtime.deliver_event(&registry, event).await.unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.cursor, first.cursor);
    assert_eq!(store.snapshot().await.unwrap().runs.len(), 1);
}

#[tokio::test]
async fn admitted_runs_keep_their_pinned_action_when_the_registry_changes() {
    let admitted_registry = registry(true);
    let store = Arc::new(InMemoryRuntimeStore::new());
    let provider = Arc::new(RecordingProvider::default());
    let (runtime, _) = runtime(store.clone(), provider.clone());
    runtime
        .deliver_event(
            &admitted_registry,
            changed_event("evt_pinned", json!(["a"])),
        )
        .await
        .unwrap();

    let mut changed = registry(true);
    changed.actions.get_mut("test.echo").unwrap().contract["name"] =
        json!("Replacement echo definition");
    let outcome = runtime.work_once(&changed).await.unwrap();
    assert!(matches!(
        outcome,
        mdbase_runtime::WorkerOutcome::Completed {
            status: RunStatus::Succeeded,
            ..
        }
    ));
    let run = &store.snapshot().await.unwrap().runs[0];
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn cancelling_queued_work_is_immediately_durable_and_terminal() {
    let registry = registry(true);
    let store = Arc::new(InMemoryRuntimeStore::new());
    let (runtime, _) = runtime(store.clone(), Arc::new(RecordingProvider::default()));
    let delivery = runtime
        .deliver_event(&registry, changed_event("evt_cancel_queued", json!(["a"])))
        .await
        .unwrap();
    let outcome = runtime
        .cancel_run(&delivery.admitted_run_ids[0])
        .await
        .unwrap();

    assert!(outcome.accepted);
    assert!(outcome.terminal);
    assert_eq!(
        store.snapshot().await.unwrap().runs[0].status,
        RunStatus::Cancelled
    );
    assert_eq!(
        runtime.work_once(&registry).await.unwrap(),
        mdbase_runtime::WorkerOutcome::Idle
    );
}

#[tokio::test]
async fn overdue_one_shot_timer_fires_once_with_a_stable_generation() {
    let registry = registry(true);
    let store = Arc::new(InMemoryRuntimeStore::new());
    let (runtime, clock) = runtime(store.clone(), Arc::new(RecordingProvider::default()));
    let scheduled = runtime
        .upsert_timer(TimerRequest {
            id: "timer_due".to_string(),
            fire_at: clock.now() + TimeDelta::minutes(5),
            event_type: "timer.fired".to_string(),
            contract_version: 1,
            payload: json!({"items": ["timer"]}),
        })
        .await
        .unwrap();
    assert_eq!(scheduled.generation, 1);
    clock.advance(TimeDelta::hours(1));

    let fired = runtime.fire_due_timer(&registry).await.unwrap();
    assert!(matches!(
        fired,
        TimerFireOutcome::Fired { generation: 1, .. }
    ));
    assert_eq!(
        runtime.fire_due_timer(&registry).await.unwrap(),
        TimerFireOutcome::Idle
    );
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(
        snapshot.timers[0].status,
        mdbase_runtime::TimerStatus::Fired
    );
    let contracts = RuntimeContracts::new().unwrap();
    let validation = contracts.validate_contract(&ContractDocument::virtual_contract(
        snapshot.timers[0].materialized_contract(),
    ));
    assert!(validation.valid, "{:#?}", validation.diagnostics);
}

#[tokio::test]
async fn crash_after_provider_success_replays_idempotently_with_the_same_invocation() {
    let registry = registry(true);
    let inner = Arc::new(InMemoryRuntimeStore::new());
    let fault_store = Arc::new(FailCommitStore::new(inner.clone(), 3));
    let provider = Arc::new(RecordingProvider::default());
    let (runtime, clock) = runtime(fault_store, provider.clone());
    runtime
        .deliver_event(&registry, changed_event("evt_commit_crash", json!(["a"])))
        .await
        .unwrap();

    let failure = runtime.work_once(&registry).await.unwrap_err();
    assert_eq!(failure.code(), "runtime_store_error");
    let interrupted = &inner.snapshot().await.unwrap().runs[0];
    assert_eq!(interrupted.attempts.len(), 1);
    assert_eq!(
        interrupted.attempts[0].status,
        mdbase_runtime::ActionAttemptStatus::Dispatching
    );

    clock.advance(TimeDelta::seconds(31));
    let recovered = runtime.work_once(&registry).await.unwrap();
    assert!(matches!(
        recovered,
        mdbase_runtime::WorkerOutcome::Completed {
            status: RunStatus::Succeeded,
            ..
        }
    ));
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].invocation_id, requests[1].invocation_id);
}

fn runtime(
    store: Arc<dyn RuntimeStore>,
    provider: Arc<RecordingProvider>,
) -> (Runtime, ManualClock) {
    let clock = ManualClock::new(Utc.with_ymd_and_hms(2026, 7, 24, 0, 0, 0).single().unwrap());
    let providers = ProviderRegistry::default();
    providers.register("test.echo", provider);
    let runtime = Runtime::new(
        store,
        providers,
        Arc::new(AllowAuthorizer),
        Arc::new(clock.clone()),
        RuntimeConfig {
            runtime_id: "test-runtime".to_string(),
            executor_id: "local".to_string(),
            worker_id: "worker_one".to_string(),
            actor_id: "test-user".to_string(),
            actor_kind: "user".to_string(),
            timezone: Some("UTC".to_string()),
            lease_duration: Duration::from_secs(30),
            max_items: 100,
        },
    )
    .unwrap();
    (runtime, clock)
}

fn local_state_stores() -> Vec<Arc<dyn RuntimeStore>> {
    #[cfg(feature = "sqlite")]
    {
        vec![
            Arc::new(InMemoryRuntimeStore::new()),
            Arc::new(SqliteRuntimeStore::in_memory().unwrap()),
        ]
    }
    #[cfg(not(feature = "sqlite"))]
    {
        vec![Arc::new(InMemoryRuntimeStore::new())]
    }
}

fn registry(idempotent: bool) -> RuntimeRegistry {
    let dispatch = if idempotent {
        json!({"idempotency": "invocation_id", "cancellation": "cooperative"})
    } else {
        json!({"idempotency": "none", "cancellation": "none"})
    };
    let documents = vec![
        contract(json!({
            "type": "provider",
            "id": "test",
            "version": 1,
            "name": "Test provider",
            "provider_version": "1.0.0",
            "contracts": {
                "events": ["test.changed"],
                "actions": ["test.echo"]
            }
        })),
        contract(json!({
            "type": "provider",
            "id": "mdbase.timer",
            "version": 1,
            "name": "Timer provider",
            "provider_version": "1.0.0",
            "contracts": {
                "events": ["timer.fired"]
            }
        })),
        contract(json!({
            "type": "event",
            "id": "test.changed",
            "version": 1,
            "provider": "test",
            "name": "Changed",
            "schemas": {
                "dialect": "json-schema-2020-12",
                "payload": {
                    "type": "object",
                    "required": ["items"],
                    "properties": {
                        "items": {"type": "array"}
                    }
                }
            }
        })),
        contract(json!({
            "type": "event",
            "id": "timer.fired",
            "version": 1,
            "provider": "mdbase.timer",
            "name": "Timer fired",
            "schemas": {
                "dialect": "json-schema-2020-12",
                "payload": {
                    "type": "object",
                    "required": ["timer_id", "generation", "scheduled_at", "fired_at", "late_by_ms", "data"],
                    "properties": {
                        "timer_id": {"type": "string"},
                        "generation": {"type": "integer"},
                        "scheduled_at": {"type": "string", "format": "date-time"},
                        "fired_at": {"type": "string", "format": "date-time"},
                        "late_by_ms": {"type": "integer"},
                        "data": {"type": "object"}
                    }
                }
            }
        })),
        contract(json!({
            "type": "action",
            "id": "test.echo",
            "version": 1,
            "provider": "test",
            "name": "Echo",
            "schemas": {
                "dialect": "json-schema-2020-12",
                "input": {"type": "object"},
                "output": {"type": "object"}
            },
            "dispatch": dispatch
        })),
        contract(json!({
            "type": "runtime_policy",
            "id": "local.policy",
            "version": 1,
            "name": "Local policy",
            "executors": {"default": "local"}
        })),
        contract(json!({
            "type": "workflow",
            "id": "test.workflow",
            "version": 1,
            "name": "Test workflow",
            "enabled": true,
            "vars": {
                "second": {"$expr": "vars.first"},
                "first": {"$expr": "event.payload.items[0]"}
            },
            "triggers": [
                {
                    "id": "changed",
                    "event": "test.changed",
                    "if": {"$expr": "event.payload.items.length > 0"}
                }
            ],
            "steps": [
                {
                    "id": "echo",
                    "action": "test.echo",
                    "for_each": {
                        "items": {"$expr": "event.payload.items"},
                        "as": "entry"
                    },
                    "input": {
                        "value": {"$expr": "entry"},
                        "from_var": {"$expr": "vars.second"}
                    }
                }
            ],
            "run": {
                "execution": {"mode": "single_executor"},
                "concurrency": {"policy": "queue"},
                "on_error": "stop"
            }
        })),
    ];
    let runtime = RuntimeContracts::new().unwrap();
    let registry = runtime.compose(
        vec![ContractSource::built_in(documents)],
        &ComposeOptions {
            selected_policies: vec![PolicySelector::Id("local.policy".to_string())],
        },
    );
    assert!(registry.valid(), "{:#?}", registry.diagnostics);
    let preflight = runtime.preflight(&registry);
    assert!(preflight.valid, "{:#?}", preflight.diagnostics);
    registry
}

fn contract(value: Value) -> ContractDocument {
    ContractDocument::virtual_contract(value)
}

fn changed_event(id: &str, items: Value) -> Value {
    json!({
        "type": "test.changed",
        "contract_version": 1,
        "id": id,
        "occurred_at": "2026-07-24T00:00:00Z",
        "source": {
            "runtime": "test-runtime",
            "provider": "test"
        },
        "payload": {
            "items": items
        }
    })
}

struct FailCommitStore {
    inner: Arc<InMemoryRuntimeStore>,
    fail_on: usize,
    commits: AtomicUsize,
}

impl FailCommitStore {
    fn new(inner: Arc<InMemoryRuntimeStore>, fail_on: usize) -> Self {
        Self {
            inner,
            fail_on,
            commits: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl RuntimeStore for FailCommitStore {
    async fn admit_event(&self, event: PreparedEvent) -> RuntimeResult<AdmitOutcome> {
        self.inner.admit_event(event).await
    }

    async fn claim_run(
        &self,
        executor: &str,
        worker: &str,
        now: chrono::DateTime<Utc>,
        lease_for: Duration,
    ) -> RuntimeResult<Option<Claim>> {
        self.inner.claim_run(executor, worker, now, lease_for).await
    }

    async fn commit_run(&self, claim: Claim, emitted: Vec<PreparedEvent>) -> RuntimeResult<Claim> {
        let commit = self.commits.fetch_add(1, Ordering::SeqCst) + 1;
        if commit == self.fail_on {
            return Err(mdbase_runtime::RuntimeError::Store(
                "injected commit failure".to_string(),
            ));
        }
        self.inner.commit_run(claim, emitted).await
    }

    async fn get_run(&self, id: &str) -> RuntimeResult<Option<mdbase_runtime::RunRecord>> {
        self.inner.get_run(id).await
    }

    async fn events_after(
        &self,
        after: u64,
        limit: usize,
    ) -> RuntimeResult<mdbase_runtime::EventPage> {
        self.inner.events_after(after, limit).await
    }

    async fn prune_events_through(&self, cursor: u64) -> RuntimeResult<u64> {
        self.inner.prune_events_through(cursor).await
    }

    async fn request_cancel(&self, id: &str, now: chrono::DateTime<Utc>) -> RuntimeResult<bool> {
        self.inner.request_cancel(id, now).await
    }

    async fn upsert_timer(&self, timer: TimerRecord) -> RuntimeResult<TimerRecord> {
        self.inner.upsert_timer(timer).await
    }

    async fn cancel_timer(
        &self,
        id: &str,
        generation: Option<u64>,
        now: chrono::DateTime<Utc>,
    ) -> RuntimeResult<bool> {
        self.inner.cancel_timer(id, generation, now).await
    }

    async fn reconcile_timers(
        &self,
        id_prefix: &str,
        desired: Vec<TimerRecord>,
        now: chrono::DateTime<Utc>,
    ) -> RuntimeResult<TimerReconcileOutcome> {
        self.inner.reconcile_timers(id_prefix, desired, now).await
    }

    async fn timers(&self, id_prefix: &str) -> RuntimeResult<Vec<TimerRecord>> {
        self.inner.timers(id_prefix).await
    }

    async fn claim_due_timer(
        &self,
        worker: &str,
        now: chrono::DateTime<Utc>,
        lease_for: Duration,
    ) -> RuntimeResult<Option<TimerClaim>> {
        self.inner.claim_due_timer(worker, now, lease_for).await
    }

    async fn fire_timer(
        &self,
        claim: TimerClaim,
        fired: TimerRecord,
        event: PreparedEvent,
    ) -> RuntimeResult<AdmitOutcome> {
        self.inner.fire_timer(claim, fired, event).await
    }

    async fn snapshot(&self) -> RuntimeResult<StoreSnapshot> {
        self.inner.snapshot().await
    }
}
