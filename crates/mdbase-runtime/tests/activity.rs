use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeDelta, TimeZone, Utc};
use mdbase::runtime_contracts::{
    ComposeOptions, ContractDocument, ContractSource, PolicySelector, RuntimeContracts,
};
use mdbase::watch::WatchEvent;
use mdbase_runtime::{
    ActionDispatch, ActionProvider, ActionResponse, AuthorizationDecision, DispatchAuthorizer,
    DispatchFailure, InMemoryRuntimeStore, ManualClock, ProviderRegistry, RunStatus, Runtime,
    RuntimeConfig, StatusTransitionActivity, WorkerOutcome,
};
use serde_json::{json, Value};

#[derive(Default)]
struct Allow;

#[async_trait]
impl DispatchAuthorizer for Allow {
    async fn authorize(&self, _request: &ActionDispatch) -> AuthorizationDecision {
        AuthorizationDecision::Allow
    }
}

#[derive(Default)]
struct ArchiveProvider {
    requests: Mutex<Vec<ActionDispatch>>,
}

#[async_trait]
impl ActionProvider for ArchiveProvider {
    async fn dispatch(&self, request: ActionDispatch) -> Result<ActionResponse, DispatchFailure> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(ActionResponse {
            output: json!({"archived": true, "path": request.input["path"]}),
            receipt: Some(json!({"revision": request.input["if_revision"]})),
            emitted_events: Vec::new(),
        })
    }
}

#[tokio::test]
async fn watch_status_activity_debounces_and_replaces_by_record() {
    let activity = StatusTransitionActivity {
        id: "tasknotes.auto_archive.done".to_string(),
        name: "Auto-archive completed tasks".to_string(),
        record_type: "task".to_string(),
        status_field: "status".to_string(),
        status_value: "done".to_string(),
        action: "tasknotes.task.archive".to_string(),
        delay: Duration::from_secs(5 * 60),
    };
    let registry = registry(activity.workflow_contract().unwrap());
    let provider = Arc::new(ArchiveProvider::default());
    let providers = ProviderRegistry::default();
    providers.register("tasknotes.task.archive", provider.clone());
    let clock = ManualClock::new(
        Utc.with_ymd_and_hms(2026, 7, 26, 10, 0, 0)
            .single()
            .unwrap(),
    );
    let runtime = Runtime::builder(Arc::new(InMemoryRuntimeStore::new()))
        .providers(providers)
        .authorizer(Arc::new(Allow))
        .clock(Arc::new(clock.clone()))
        .config(RuntimeConfig {
            runtime_id: "local".to_string(),
            executor_id: "local".to_string(),
            worker_id: "activity-worker".to_string(),
            ..RuntimeConfig::default()
        })
        .build()
        .unwrap();

    let first = runtime
        .deliver_watch_event(
            &registry,
            changed_event("rev-one", "First title"),
            Some("tasks"),
        )
        .await
        .unwrap();
    assert_eq!(first.admitted_run_ids.len(), 1);
    assert_eq!(
        runtime.work_once(&registry).await.unwrap(),
        WorkerOutcome::Idle
    );

    clock.advance(TimeDelta::minutes(1));
    let second = runtime
        .deliver_watch_event(
            &registry,
            changed_event("rev-two", "Revised title"),
            Some("tasks"),
        )
        .await
        .unwrap();
    assert_eq!(second.admitted_run_ids.len(), 1);

    clock.advance(TimeDelta::minutes(5));
    assert!(matches!(
        runtime.work_once(&registry).await.unwrap(),
        WorkerOutcome::Completed {
            status: RunStatus::Succeeded,
            ..
        }
    ));
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].input["path"], "tasks/one.md");
    assert_eq!(requests[0].input["if_revision"], "rev-two");
    assert_eq!(requests[0].input["status_value"], "done");
}

fn changed_event(revision: &str, title: &str) -> WatchEvent {
    WatchEvent {
        event_type: "mdbase.record.modified".to_string(),
        sequence: 1,
        occurred_at: "2026-07-26T10:00:00Z".to_string(),
        payload: json!({
            "path": "tasks/one.md",
            "before": {"status": "open"},
            "after": {"status": "done", "title": title},
            "changed_fields": ["status", "title"],
            "previous_revision": "rev-old",
            "revision": revision,
            "types": ["task"]
        }),
    }
}

fn registry(workflow: Value) -> mdbase::runtime_contracts::RuntimeRegistry {
    let documents = vec![
        json!({
            "type": "provider",
            "id": "mdbase.watch",
            "version": 1,
            "name": "mdbase Watch",
            "provider_version": "1.0.0",
            "contracts": {
                "events": [
                    "mdbase.record.created",
                    "mdbase.record.modified",
                    "mdbase.record.renamed"
                ]
            }
        }),
        json!({
            "type": "event",
            "id": "mdbase.record.created",
            "version": 1,
            "provider": "mdbase.watch",
            "name": "Record created",
            "schemas": {
                "dialect": "json-schema-2020-12",
                "payload": {"type": "object"}
            }
        }),
        json!({
            "type": "event",
            "id": "mdbase.record.modified",
            "version": 1,
            "provider": "mdbase.watch",
            "name": "Record modified",
            "schemas": {
                "dialect": "json-schema-2020-12",
                "payload": {"type": "object"}
            }
        }),
        json!({
            "type": "event",
            "id": "mdbase.record.renamed",
            "version": 1,
            "provider": "mdbase.watch",
            "name": "Record renamed",
            "schemas": {
                "dialect": "json-schema-2020-12",
                "payload": {"type": "object"}
            }
        }),
        json!({
            "type": "provider",
            "id": "tasknotes",
            "version": 1,
            "name": "TaskNotes",
            "provider_version": "1.0.0",
            "contracts": {"actions": ["tasknotes.task.archive"]}
        }),
        json!({
            "type": "action",
            "id": "tasknotes.task.archive",
            "version": 1,
            "provider": "tasknotes",
            "name": "Archive a task",
            "schemas": {
                "dialect": "json-schema-2020-12",
                "input": {"type": "object"},
                "output": {"type": "object"}
            },
            "dispatch": {"idempotency": "invocation_id"}
        }),
        json!({
            "type": "runtime_policy",
            "id": "local.policy",
            "version": 1,
            "name": "Local policy",
            "executors": {"default": "local"}
        }),
        workflow,
    ]
    .into_iter()
    .map(ContractDocument::virtual_contract)
    .collect();
    let contracts = RuntimeContracts::new().unwrap();
    let registry = contracts.compose(
        vec![ContractSource::built_in(documents)],
        &ComposeOptions {
            selected_policies: vec![PolicySelector::Id("local.policy".to_string())],
        },
    );
    assert!(registry.valid(), "{:#?}", registry.diagnostics);
    assert!(contracts.preflight(&registry).valid);
    registry
}
