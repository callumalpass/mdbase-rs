use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeDelta, TimeZone, Utc};
use mdbase_interop::{ExactContractReference, ImplementationIdentity};
#[cfg(feature = "sqlite")]
use mdbase_runtime::SqliteRuntimeStore;
use mdbase_runtime::{
    canonical_digest, ActionCancellation, ActionDispatch, ActionInvocation, ActionOutcome,
    ActionProvider, AdmissionCatalog, AdmitOutcome, AuthorizationDecision, Claim, Clock,
    DispatchAuthorizer, DispatchFailure, DispatchOutcome, InMemoryRuntimeStore, ManualClock,
    PreparedEvent, ProviderBinding, ProviderRegistry, RunStatus, Runtime, RuntimeConfig,
    RuntimeResult, RuntimeStore, StoreSnapshot, TimerClaim, TimerFireOutcome,
    TimerReconcileOutcome, TimerReconcileRequest, TimerRecord, TimerRequest,
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
    requests: Mutex<Vec<ActionInvocation>>,
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

    fn requests(&self) -> Vec<ActionInvocation> {
        self.requests.lock().unwrap().clone()
    }

    fn cancellations(&self) -> Vec<String> {
        self.cancellations.lock().unwrap().clone()
    }
}

#[async_trait]
impl ActionProvider for RecordingProvider {
    async fn dispatch(&self, request: ActionInvocation) -> Result<ActionOutcome, DispatchFailure> {
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
        Ok(ActionOutcome {
            kind: "mdbase.action.outcome".to_string(),
            profile_version: "0.1".to_string(),
            outcome_id: format!("out_{}", request.attempt_id),
            request_id: request.request_id,
            invocation_id: request.invocation_id,
            attempt_id: request.attempt_id,
            contract: request.contract,
            provider: request.provider,
            provider_declaration_digest: request.provider_declaration_digest,
            status: "succeeded".to_string(),
            completed_at: "2026-07-24T00:00:00Z".to_string(),
            output: Some(request.input),
            error: None,
        })
    }

    async fn cancel(&self, request: ActionCancellation) -> Result<(), DispatchFailure> {
        self.cancellations.lock().unwrap().push(request.request_id);
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
    let outcomes = runtime.drain(10).await.unwrap();
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
    assert_eq!(provider.requests()[0].contract.id, "test.echo");
    assert_eq!(provider.requests()[0].handler_id, "echo");
    assert!(provider.requests()[0]
        .provider_declaration_digest
        .starts_with("sha256:"));
    assert_eq!(
        run.materialized_contract()["admitted_plan"]["profile_version"],
        "0.2"
    );
    mdbase_runtime::validate_runtime_record(&run.materialized_contract()).unwrap();
}

#[tokio::test]
async fn admission_rejects_ambiguous_action_providers() {
    let store = Arc::new(InMemoryRuntimeStore::new());
    let (runtime, _) = runtime(store, Arc::new(RecordingProvider::default()));
    let error = runtime
        .deliver_event(
            &ambiguous_registry(),
            changed_event("evt_ambiguous", json!(["a"])),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "ambiguous_provider");
}

#[tokio::test]
async fn trigger_admits_portable_record_type_membership_expression() {
    let registry = registry_with(true, |workflow| {
        workflow["triggers"][0]["if"]["$expr"] = json!(r#""pickle_request" in event.data.types"#);
    });
    let store = Arc::new(InMemoryRuntimeStore::new());
    let (runtime, _) = runtime(store, Arc::new(RecordingProvider::default()));
    let mut event = changed_event("evt_pickle_request", json!(["request.md"]));
    event["data"]["types"] = json!(["pickle_request"]);

    let delivery = runtime.deliver_event(&registry, event).await.unwrap();

    assert_eq!(delivery.admitted_run_ids.len(), 1);
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
        contract: timer_contract_reference(),
        source: test_identity(),
        source_uri: "urn:test:runtime".to_string(),
        subject: None,
        data: json!({"owner": "grant-a"}),
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
                contract: timer_contract_reference(),
                source: test_identity(),
                source_uri: "urn:test:runtime".to_string(),
                subject: None,
                data: json!({}),
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
        let registry = registry_with(true, |workflow| {
            workflow["run"]["concurrency"]["policy"] = json!("skip");
        });
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
        let delayed_registry = registry_with(true, |workflow| {
            workflow["triggers"][0]["debounce"] = json!("1m");
        });
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
            runtime.work_once().await.unwrap(),
            mdbase_runtime::WorkerOutcome::Idle
        );
        clock.advance(TimeDelta::minutes(1));
        runtime.work_once().await.unwrap();
        runtime.work_once().await.unwrap();
        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].input["value"], json!("a"));
        assert_eq!(requests[1].input["value"], json!("b"));
    }
}

#[tokio::test]
async fn replace_cancels_queued_work_before_admitting_its_successor() {
    for store in local_state_stores() {
        let registry = registry_with(true, |workflow| {
            workflow["run"]["concurrency"]["policy"] = json!("replace");
        });
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

        let outcome = runtime.work_once().await.unwrap();
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

    let failure = runtime.work_once().await.unwrap_err();
    assert_eq!(failure.code(), "action_provider_error");
    clock.advance(TimeDelta::seconds(31));
    let outcome = runtime.work_once().await.unwrap();
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
    assert_eq!(requests[0].attempt_id, requests[1].attempt_id);
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

    runtime.work_once().await.unwrap_err();
    let cancellation = runtime
        .cancel_run(&delivery.admitted_run_ids[0])
        .await
        .unwrap();
    assert!(cancellation.accepted);
    assert!(cancellation.provider_notified);
    clock.advance(TimeDelta::seconds(31));

    let recovered = runtime.work_once().await.unwrap();
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
    let registry = registry_with(true, |workflow| {
        workflow["run"]["concurrency"]["policy"] = json!("replace");
    });
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
    runtime.work_once().await.unwrap_err();

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
    let predecessor = runtime.work_once().await.unwrap();
    assert!(matches!(
        predecessor,
        mdbase_runtime::WorkerOutcome::Completed {
            status: RunStatus::Indeterminate,
            ..
        }
    ));

    assert_eq!(
        runtime.work_once().await.unwrap(),
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

    let first = runtime.work_once().await.unwrap();
    assert!(matches!(
        first,
        mdbase_runtime::WorkerOutcome::Completed {
            status: RunStatus::Indeterminate,
            ..
        }
    ));
    clock.advance(TimeDelta::minutes(1));
    assert_eq!(
        runtime.work_once().await.unwrap(),
        mdbase_runtime::WorkerOutcome::Idle
    );
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn workflow_deadline_fails_before_a_new_effect_is_dispatched() {
    let registry = registry_with(true, |workflow| {
        workflow["run"]["limits"] = json!({"timeout": "1s", "max_items": 100});
    });
    let store = Arc::new(InMemoryRuntimeStore::new());
    let provider = Arc::new(RecordingProvider::default());
    let (runtime, clock) = runtime(store.clone(), provider.clone());
    runtime
        .deliver_event(&registry, changed_event("evt_timeout", json!(["a"])))
        .await
        .unwrap();
    clock.advance(TimeDelta::seconds(2));

    let outcome = runtime.work_once().await.unwrap();
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
    let registry = registry_with(true, |workflow| {
        workflow["steps"][0]["input"] = json!({"value": {"$expr": "event.data.items["}});
        workflow["steps"].as_array_mut().unwrap().push(json!({
            "id": "must-not-run",
            "action": {"id": "test.echo", "version": "^1.0.0"},
            "input": {"value": "unexpected"}
        }));
    });
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

    let outcome = runtime.work_once().await.unwrap();
    assert!(matches!(
        outcome,
        mdbase_runtime::WorkerOutcome::Completed {
            status: RunStatus::Failed,
            ..
        }
    ));
    clock.advance(TimeDelta::minutes(1));
    assert_eq!(
        runtime.work_once().await.unwrap(),
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
        runtime.drain(10).await.unwrap();
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
        contract: timer_contract_reference(),
        source: test_identity(),
        source_uri: "urn:test:runtime".to_string(),
        subject: None,
        data: json!({"private": id}),
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

    let _changed = registry(true);
    let outcome = runtime.work_once().await.unwrap();
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
        runtime.work_once().await.unwrap(),
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
            contract: timer_contract_reference(),
            source: test_identity(),
            source_uri: "urn:test:runtime".to_string(),
            subject: None,
            data: json!({"items": ["timer"]}),
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
    assert_eq!(
        snapshot.timers[0].materialized_contract()["event"]["contract"]["version"],
        "1.0.0"
    );
    mdbase_runtime::validate_runtime_record(&snapshot.timers[0].materialized_contract()).unwrap();
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

    let failure = runtime.work_once().await.unwrap_err();
    assert_eq!(failure.code(), "runtime_store_error");
    let interrupted = &inner.snapshot().await.unwrap().runs[0];
    assert_eq!(interrupted.attempts.len(), 1);
    assert_eq!(
        interrupted.attempts[0].status,
        mdbase_runtime::ActionAttemptStatus::Dispatching
    );

    clock.advance(TimeDelta::seconds(31));
    let recovered = runtime.work_once().await.unwrap();
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
    let action = exact(&action_contract("test.echo"));
    for idempotent in [true, false] {
        let declaration = provider_declaration(idempotent, action.clone());
        providers.register(
            ProviderBinding {
                provider_declaration_digest: declaration["declaration_digest"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                handler_id: "echo".to_string(),
            },
            provider.clone(),
        );
    }
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
            identity: test_identity(),
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

fn registry(idempotent: bool) -> AdmissionCatalog {
    registry_with(idempotent, |_| {})
}

fn ambiguous_registry() -> AdmissionCatalog {
    let changed = event_contract(
        "test.changed",
        json!({
            "type": "object",
            "required": ["items"],
            "properties": {
                "items": {"type": "array"},
                "types": {"type": "array", "items": {"type": "string"}}
            }
        }),
    );
    let echo = action_contract("test.echo");
    let resolved = exact(&echo);
    let first = provider_declaration(true, resolved.clone());
    let mut second = provider_declaration(true, resolved);
    second["provider"]["implementation"] = json!("another-provider");
    second.as_object_mut().unwrap().remove("declaration_digest");
    second = declaration(second);
    AdmissionCatalog::new(
        vec![changed.clone(), echo],
        vec![source_declaration(test_identity(), vec![exact(&changed)])],
        vec![first, second],
        vec![json!({
            "type": "runtime_workflow",
            "id": "test.ambiguous",
            "version": "1.0.0",
            "name": "Ambiguous provider",
            "enabled": true,
            "triggers": [{
                "id": "changed",
                "event": {"id": "test.changed", "version": "^1.0.0"}
            }],
            "steps": [{
                "id": "echo",
                "action": {"id": "test.echo", "version": "^1.0.0"},
                "input": {}
            }]
        })],
        json!({
            "type": "runtime_policy",
            "id": "local.policy",
            "version": "1.0.0",
            "enabled": true,
            "executors": {"default": "local"},
            "grants": []
        }),
    )
    .unwrap()
}

fn registry_with(idempotent: bool, mutate: impl FnOnce(&mut Value)) -> AdmissionCatalog {
    let changed = event_contract(
        "test.changed",
        json!({
            "type": "object",
            "required": ["items"],
            "properties": {
                "items": {"type": "array"},
                "types": {"type": "array", "items": {"type": "string"}}
            }
        }),
    );
    let timer = event_contract(
        "timer.fired",
        json!({
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
        }),
    );
    let echo = action_contract("test.echo");
    let source = source_declaration(test_identity(), vec![exact(&changed), exact(&timer)]);
    let provider = provider_declaration(idempotent, exact(&echo));
    let mut workflow = json!({
        "type": "runtime_workflow",
        "id": "test.workflow",
        "version": "1.0.0",
        "name": "Test workflow",
        "enabled": true,
        "vars": {
            "second": {"$expr": "vars.first"},
            "first": {"$expr": "event.data.items[0]"}
        },
        "triggers": [{
            "id": "changed",
            "event": {"id": "test.changed", "version": "^1.0.0"},
            "if": {"$expr": "event.data.items.length > 0"}
        }],
        "steps": [{
            "id": "echo",
            "action": {"id": "test.echo", "version": "^1.0.0"},
            "for_each": {
                "items": {"$expr": "event.data.items"},
                "as": "entry"
            },
            "input": {
                "value": {"$expr": "entry"},
                "from_var": {"$expr": "vars.second"}
            }
        }],
        "run": {
            "concurrency": {"policy": "queue"},
            "on_error": "stop"
        }
    });
    mutate(&mut workflow);
    AdmissionCatalog::new(
        vec![changed, timer, echo],
        vec![source],
        vec![provider],
        vec![workflow],
        json!({
            "type": "runtime_policy",
            "id": "local.policy",
            "version": "1.0.0",
            "name": "Local policy",
            "enabled": true,
            "executors": {"default": "local"},
            "grants": []
        }),
    )
    .unwrap()
}

fn event_contract(id: &str, schema: Value) -> Value {
    json!({
        "kind": "mdbase.contract",
        "contract_type": "event",
        "id": id,
        "version": "1.0.0",
        "data_schema": {"dialect": "json-schema-2020-12", "value": schema}
    })
}

fn action_contract(id: &str) -> Value {
    json!({
        "kind": "mdbase.contract",
        "contract_type": "action",
        "id": id,
        "version": "1.0.0",
        "input_schema": {
            "dialect": "json-schema-2020-12",
            "value": {"type": "object"}
        },
        "output_schema": {
            "dialect": "json-schema-2020-12",
            "value": {"type": "object"}
        }
    })
}

fn timer_contract_reference() -> ExactContractReference {
    exact(&event_contract(
        "timer.fired",
        json!({
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
        }),
    ))
}

fn exact(contract: &Value) -> ExactContractReference {
    ExactContractReference {
        id: contract["id"].as_str().unwrap().to_string(),
        version: contract["version"].as_str().unwrap().to_string(),
        digest: mdbase_interop::contract_digest(contract).unwrap(),
    }
}

fn test_identity() -> ImplementationIdentity {
    ImplementationIdentity {
        application: "test".to_string(),
        implementation: "runtime-test".to_string(),
        version: "1.0.0".to_string(),
        instance_id: Some("local".to_string()),
    }
}

fn source_declaration(
    source: ImplementationIdentity,
    contracts: Vec<ExactContractReference>,
) -> Value {
    declaration(json!({
        "kind": "mdbase.event-source",
        "profile_version": "0.1",
        "declaration_id": "test.events",
        "source": source,
        "contracts": contracts.into_iter().map(|resolved| json!({
            "requirement": {"id": resolved.id, "version": resolved.version},
            "resolved": resolved
        })).collect::<Vec<_>>()
    }))
}

fn provider_declaration(idempotent: bool, resolved: ExactContractReference) -> Value {
    declaration(json!({
        "kind": "mdbase.action-provider",
        "profile_version": "0.1",
        "declaration_id": if idempotent { "test.actions.idempotent" } else { "test.actions.direct" },
        "provider": test_identity(),
        "handlers": [{
            "handler_id": "echo",
            "requirement": {"id": resolved.id, "version": resolved.version},
            "resolved": resolved,
            "idempotency": {"mode": if idempotent { "request" } else { "none" }},
            "cancellation": if idempotent { "cooperative" } else { "none" }
        }]
    }))
}

fn declaration(mut value: Value) -> Value {
    value["declaration_digest"] = Value::String(canonical_digest(&value).unwrap());
    value
}

fn changed_event(id: &str, items: Value) -> Value {
    let contract = exact(&event_contract(
        "test.changed",
        json!({
            "type": "object",
            "required": ["items"],
            "properties": {
                "items": {"type": "array"},
                "types": {"type": "array", "items": {"type": "string"}}
            }
        }),
    ));
    json!({
        "specversion": "1.0",
        "id": id,
        "source": "urn:test:runtime",
        "type": "test.changed",
        "time": "2026-07-24T00:00:00Z",
        "datacontenttype": "application/json",
        "dataschema": format!("urn:mdbase:contract:{}:{}:{}", contract.id, contract.version, contract.digest),
        "data": {"items": items},
        "mdbaseprofile": "0.1",
        "mdbasecontractversion": contract.version,
        "mdbasecontractdigest": contract.digest,
        "mdbaseapplication": "test",
        "mdbaseimplementation": "runtime-test",
        "mdbaseimplementationversion": "1.0.0",
        "mdbaseinstanceid": "local"
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
