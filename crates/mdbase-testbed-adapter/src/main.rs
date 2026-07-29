use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeDelta, TimeZone, Utc};
use mdbase::Collection;
use mdbase_interop::{ExactContractReference, ImplementationIdentity};
use mdbase_runtime::{
    canonical_digest, ActionDispatch, ActionInvocation, ActionOutcome, ActionProvider,
    AdmissionCatalog, AuthorizationDecision, Clock, DispatchAuthorizer, DispatchFailure,
    DispatchOutcome, InMemoryRuntimeStore, ManualClock, ProviderBinding, ProviderRegistry,
    RunStatus, Runtime, RuntimeConfig, RuntimeStore,
};
use serde_json::{json, Value};
use ulid::Ulid;

const CORE_SCENARIO: &str = "core.shared-contract-consumers";
const CRASH_SCENARIO: &str = "runtime.crash-recovery";
const FENCING_SCENARIO: &str = "runtime.competing-workers";

fn implementation() -> Value {
    json!({
        "id": "mdbase-rs",
        "name": "mdbase Rust core and durable runtime",
        "version": "0.4.0-rc.3",
        "language": "Rust",
        "target": "native",
        "x-runtime-version": mdbase_runtime::VERSION
    })
}

#[tokio::main]
async fn main() {
    if let Err(error) = execute().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn execute() -> Result<(), String> {
    match std::env::args().nth(1).as_deref() {
        Some("describe") => write(&json!({
            "kind": "mdbase.testbed.adapter",
            "protocol_version": "0.1",
            "implementation": implementation(),
            "profiles": ["core_read", "runtime/0.2"],
            "roles": [
                "contract_store",
                "record_consumer",
                "runtime",
                "runtime_store",
                "action_provider"
            ],
            "scenarios": [CORE_SCENARIO, CRASH_SCENARIO, FENCING_SCENARIO]
        })),
        Some("run") => {
            let request = read_request()?;
            if request["kind"] != "mdbase.testbed.run" || request["protocol_version"] != "0.1" {
                return Err("Unsupported or invalid mdbase testbed run request.".to_string());
            }
            let scenario_id = request
                .pointer("/scenario/id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Testbed run request is missing scenario.id.".to_string())?;
            let entries = match scenario_id {
                CORE_SCENARIO => shared_contract_consumers(&request)?,
                CRASH_SCENARIO => crash_recovery(&request).await?,
                FENCING_SCENARIO => competing_workers(&request).await?,
                _ => return Err(format!("Unsupported testbed scenario {scenario_id}.")),
            };
            write(&json!({
                "kind": "mdbase.testbed.transcript",
                "protocol_version": "0.1",
                "scenario_id": scenario_id,
                "implementation": implementation(),
                "entries": entries
            }))
        }
        _ => Err("Usage: mdbase-testbed-adapter describe|run".to_string()),
    }
}

fn shared_contract_consumers(request: &Value) -> Result<Vec<Value>, String> {
    let contract = fixture(request, "contract.example-note")?;
    let type_file = fixture(request, "type.shared-note")?;
    let record = fixture(request, "record.shared-note")?;
    let root = std::env::temp_dir().join(format!("mdbase-testbed-rs-{}", Ulid::new()));
    let result = (|| {
        write_file(
            &root,
            "mdbase.yaml",
            "spec_version: \"0.3.0\"\nsettings:\n  types_folder: _types\n  contracts_folder: _contracts\n  explicit_type_keys: [type]\n",
        )?;
        write_file(&root, "_contracts/example.note.md", &markdown(&contract)?)?;
        write_file(&root, "_types/shared-note.md", &markdown(&type_file)?)?;
        write_file(&root, "shared.md", &markdown(&record)?)?;

        let collection = Collection::open(&root).map_err(|error| error.to_string())?;
        let loaded = collection
            .list_data_contracts()
            .into_iter()
            .find(|candidate| {
                candidate.id == contract["id"] && candidate.version == contract["version"]
            })
            .ok_or_else(|| "The neutral record contract was not loaded.".to_string())?;
        let implementation = collection
            .get_data_contract_implementations(&loaded.id, &loaded.version)
            .into_iter()
            .find(|candidate| candidate.type_name == type_file["name"])
            .ok_or_else(|| "The neutral type implementation was not loaded.".to_string())?;
        let alpha = collection.get_contract_view("shared.md", &loaded.id, &loaded.version, None);
        let beta = collection.get_contract_view("shared.md", &loaded.id, &loaded.version, None);
        if !alpha.valid || !beta.valid {
            return Err(format!(
                "Contract view projection failed: {:?} {:?}",
                alpha.diagnostics, beta.diagnostics
            ));
        }

        Ok(vec![
            entry(
                1,
                "arrange",
                "contract-store",
                "contract.load",
                "succeeded",
                json!({"contract": loaded.id, "version": loaded.version}),
            ),
            entry(
                2,
                "arrange",
                "contract-store",
                "type.load",
                "succeeded",
                json!({
                    "type": implementation.type_name,
                    "implements": implementation.contract
                }),
            ),
            entry(
                3,
                "act",
                "consumer-alpha",
                "contract-view.read",
                "succeeded",
                json!({"contract": alpha.contract, "view": alpha.view}),
            ),
            entry(
                4,
                "act",
                "consumer-beta",
                "contract-view.read",
                "succeeded",
                json!({"contract": beta.contract, "view": beta.view}),
            ),
            entry(
                5,
                "observe",
                "testbed",
                "contract-view.compare",
                "succeeded",
                json!({
                    "consumers": 2,
                    "same_contract": alpha.contract == beta.contract
                        && alpha.contract_digest == beta.contract_digest,
                    "same_view": alpha.view == beta.view
                }),
            ),
        ])
    })();
    let _ = fs::remove_dir_all(root);
    result
}

async fn crash_recovery(request: &Value) -> Result<Vec<Value>, String> {
    let context = RuntimeContext::from_request(request)?;
    let store = Arc::new(InMemoryRuntimeStore::new());
    let provider = Arc::new(RecoveringProvider::default());
    let clock = ManualClock::new(
        Utc.with_ymd_and_hms(2026, 7, 29, 0, 0, 0)
            .single()
            .expect("fixed testbed instant"),
    );
    let alpha = context.runtime(
        store.clone(),
        provider.clone(),
        clock.clone(),
        "worker-alpha",
    )?;
    let delivery = alpha
        .deliver_event(&context.catalog, context.event("evt-crash-1"))
        .await
        .map_err(|error| error.to_string())?;
    let first = alpha
        .work_once()
        .await
        .expect_err("the testbed provider intentionally loses its first outcome");
    if first.code() != "action_provider_error" {
        return Err(format!(
            "Expected action_provider_error at the crash boundary, got {}.",
            first.code()
        ));
    }
    clock.advance(TimeDelta::seconds(31));
    let beta = context.runtime(store.clone(), provider.clone(), clock, "worker-beta")?;
    beta.work_once().await.map_err(|error| error.to_string())?;
    let requests = provider.requests();
    let snapshot = store.snapshot().await.map_err(|error| error.to_string())?;
    let run = snapshot
        .runs
        .first()
        .ok_or_else(|| "Recovered runtime has no run.".to_string())?;
    let same_attempt = requests.len() == 2 && requests[0].attempt_id == requests[1].attempt_id;
    let same_invocation =
        requests.len() == 2 && requests[0].invocation_id == requests[1].invocation_id;

    Ok(vec![
        entry(
            1,
            "arrange",
            "runtime",
            "event.admit",
            "succeeded",
            json!({
                "event_id": "evt-crash-1",
                "runs": delivery.admitted_run_ids.len()
            }),
        ),
        entry(
            2,
            "act",
            "worker-alpha",
            "action.dispatch",
            "indeterminate",
            json!({"provider_effects": provider.effects()}),
        ),
        entry(
            3,
            "recover",
            "worker-beta",
            "lease.recover",
            "succeeded",
            json!({
                "same_attempt_id": same_attempt,
                "same_invocation_id": same_invocation
            }),
        ),
        entry(
            4,
            "observe",
            "runtime-store",
            "run.inspect",
            "succeeded",
            json!({
                "dispatches": requests.len(),
                "logical_effects": provider.effects(),
                "status": run_status(&run.status)
            }),
        ),
    ])
}

async fn competing_workers(request: &Value) -> Result<Vec<Value>, String> {
    let context = RuntimeContext::from_request(request)?;
    let store = Arc::new(InMemoryRuntimeStore::new());
    let provider = Arc::new(RecoveringProvider::without_failure());
    let clock = ManualClock::new(
        Utc.with_ymd_and_hms(2026, 7, 29, 0, 0, 0)
            .single()
            .expect("fixed testbed instant"),
    );
    let runtime = context.runtime(store.clone(), provider, clock.clone(), "admission-worker")?;
    let delivery = runtime
        .deliver_event(&context.catalog, context.event("evt-fence-1"))
        .await
        .map_err(|error| error.to_string())?;
    let first = store
        .claim_run(
            "testbed-runtime",
            "worker-alpha",
            clock.now(),
            Duration::from_secs(30),
        )
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "worker-alpha did not acquire the testbed run.".to_string())?;
    let blocked = store
        .claim_run(
            "testbed-runtime",
            "worker-beta",
            clock.now(),
            Duration::from_secs(30),
        )
        .await
        .map_err(|error| error.to_string())?;
    clock.advance(TimeDelta::seconds(31));
    let winner = store
        .claim_run(
            "testbed-runtime",
            "worker-beta",
            clock.now(),
            Duration::from_secs(30),
        )
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "worker-beta did not recover the expired lease.".to_string())?;
    let stale = store
        .commit_run(first, Vec::new())
        .await
        .expect_err("the expired worker must be fenced");
    let snapshot = store.snapshot().await.map_err(|error| error.to_string())?;

    Ok(vec![
        entry(
            1,
            "arrange",
            "runtime",
            "event.admit",
            "succeeded",
            json!({
                "event_id": "evt-fence-1",
                "runs": delivery.admitted_run_ids.len()
            }),
        ),
        entry(
            2,
            "act",
            "worker-alpha",
            "run.claim",
            "succeeded",
            json!({"claims": 1}),
        ),
        entry(
            3,
            "act",
            "worker-beta",
            "run.claim",
            "skipped",
            json!({"claims": usize::from(blocked.is_some())}),
        ),
        entry(
            4,
            "recover",
            "worker-alpha",
            "run.commit",
            "rejected",
            json!({"code": stale.code()}),
        ),
        entry(
            5,
            "observe",
            "runtime-store",
            "run.inspect",
            "succeeded",
            json!({
                "concurrent_claims": 1,
                "runs": snapshot.runs.len(),
                "winner": winner.worker
            }),
        ),
    ])
}

struct RuntimeContext {
    catalog: AdmissionCatalog,
    event_contract: ExactContractReference,
    source: ImplementationIdentity,
    provider_declaration_digest: String,
}

impl RuntimeContext {
    fn from_request(request: &Value) -> Result<Self, String> {
        let event_contract = fixture(request, "contract.record-changed")?;
        let action_contract = fixture(request, "contract.record-annotate")?;
        let workflow = fixture(request, "workflow.annotate-changed")?;
        let policy = fixture(request, "policy.allow-record-write")?;
        let event_reference = exact(&event_contract)?;
        let action_reference = exact(&action_contract)?;
        let source = identity("testbed-source");
        let provider = identity("provider-alpha");
        let source_declaration = declaration(json!({
            "kind": "mdbase.event-source",
            "profile_version": "0.1",
            "declaration_id": "testbed.events",
            "source": source,
            "contracts": [{
                "requirement": {
                    "id": event_reference.id,
                    "version": event_reference.version
                },
                "resolved": event_reference
            }]
        }))?;
        let provider_declaration = declaration(json!({
            "kind": "mdbase.action-provider",
            "profile_version": "0.1",
            "declaration_id": "testbed.actions",
            "provider": provider,
            "handlers": [{
                "handler_id": "annotate",
                "requirement": {
                    "id": action_reference.id,
                    "version": action_reference.version
                },
                "resolved": action_reference,
                "idempotency": {"mode": "request"},
                "cancellation": "cooperative"
            }]
        }))?;
        let provider_declaration_digest = provider_declaration["declaration_digest"]
            .as_str()
            .ok_or_else(|| "Provider declaration has no digest.".to_string())?
            .to_string();
        let catalog = AdmissionCatalog::new(
            vec![event_contract, action_contract],
            vec![source_declaration],
            vec![provider_declaration],
            vec![workflow],
            policy,
        )
        .map_err(|error| error.to_string())?;
        Ok(Self {
            catalog,
            event_contract: event_reference,
            source,
            provider_declaration_digest,
        })
    }

    fn runtime(
        &self,
        store: Arc<dyn RuntimeStore>,
        provider: Arc<RecoveringProvider>,
        clock: ManualClock,
        worker: &str,
    ) -> Result<Runtime, String> {
        let providers = ProviderRegistry::default();
        providers.register(
            ProviderBinding {
                provider_declaration_digest: self.provider_declaration_digest.clone(),
                handler_id: "annotate".to_string(),
            },
            provider,
        );
        Runtime::new(
            store,
            providers,
            Arc::new(AllowAuthorizer),
            Arc::new(clock),
            RuntimeConfig {
                runtime_id: "testbed-runtime".to_string(),
                executor_id: "testbed-runtime".to_string(),
                worker_id: worker.to_string(),
                actor_id: "testbed".to_string(),
                actor_kind: "test".to_string(),
                identity: identity("runtime"),
                timezone: Some("UTC".to_string()),
                lease_duration: Duration::from_secs(30),
                max_items: 100,
            },
        )
        .map_err(|error| error.to_string())
    }

    fn event(&self, id: &str) -> Value {
        json!({
            "specversion": "1.0",
            "id": id,
            "source": "urn:mdbase:testbed:source",
            "type": self.event_contract.id,
            "time": "2026-07-29T00:00:00Z",
            "datacontenttype": "application/json",
            "dataschema": format!(
                "urn:mdbase:contract:{}:{}:{}",
                self.event_contract.id,
                self.event_contract.version,
                self.event_contract.digest
            ),
            "data": {"record_id": "note-1", "revision": 1},
            "mdbaseprofile": "0.1",
            "mdbasecontractversion": self.event_contract.version,
            "mdbasecontractdigest": self.event_contract.digest,
            "mdbaseapplication": self.source.application,
            "mdbaseimplementation": self.source.implementation,
            "mdbaseimplementationversion": self.source.version,
            "mdbaseinstanceid": self.source.instance_id
        })
    }
}

#[derive(Default)]
struct AllowAuthorizer;

#[async_trait]
impl DispatchAuthorizer for AllowAuthorizer {
    async fn authorize(&self, _request: &ActionDispatch) -> AuthorizationDecision {
        AuthorizationDecision::Allow
    }
}

#[derive(Default)]
struct RecoveringProvider {
    requests: Mutex<Vec<ActionInvocation>>,
    outcomes: Mutex<HashMap<String, ActionOutcome>>,
    effects: AtomicUsize,
    lose_first: AtomicBool,
}

impl RecoveringProvider {
    fn without_failure() -> Self {
        Self {
            lose_first: AtomicBool::new(true),
            ..Self::default()
        }
    }

    fn requests(&self) -> Vec<ActionInvocation> {
        self.requests.lock().expect("request lock").clone()
    }

    fn effects(&self) -> usize {
        self.effects.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ActionProvider for RecoveringProvider {
    async fn dispatch(&self, request: ActionInvocation) -> Result<ActionOutcome, DispatchFailure> {
        self.requests
            .lock()
            .expect("request lock")
            .push(request.clone());
        if let Some(recorded) = self
            .outcomes
            .lock()
            .expect("outcome lock")
            .get(&request.invocation_id)
            .cloned()
        {
            return Ok(recorded);
        }
        self.effects.fetch_add(1, Ordering::SeqCst);
        let outcome = ActionOutcome {
            kind: "mdbase.action.outcome".to_string(),
            profile_version: "0.1".to_string(),
            outcome_id: format!("out_{}", request.attempt_id),
            request_id: request.request_id.clone(),
            invocation_id: request.invocation_id.clone(),
            attempt_id: request.attempt_id.clone(),
            contract: request.contract.clone(),
            provider: request.provider.clone(),
            provider_declaration_digest: request.provider_declaration_digest.clone(),
            status: "succeeded".to_string(),
            completed_at: "2026-07-29T00:00:00Z".to_string(),
            output: Some(json!({
                "record_id": request.input["record_id"],
                "applied": true
            })),
            error: None,
        };
        self.outcomes
            .lock()
            .expect("outcome lock")
            .insert(request.invocation_id.clone(), outcome.clone());
        if !self.lose_first.swap(true, Ordering::SeqCst) {
            return Err(DispatchFailure {
                code: "transport_lost".to_string(),
                message: "The provider effect committed but its response was lost.".to_string(),
                outcome: DispatchOutcome::Unknown,
            });
        }
        Ok(outcome)
    }
}

fn exact(contract: &Value) -> Result<ExactContractReference, String> {
    Ok(ExactContractReference {
        id: required_string(contract, "id")?.to_string(),
        version: required_string(contract, "version")?.to_string(),
        digest: mdbase_interop::contract_digest(contract)?,
    })
}

fn declaration(mut value: Value) -> Result<Value, String> {
    value["declaration_digest"] =
        Value::String(canonical_digest(&value).map_err(|error| error.to_string())?);
    Ok(value)
}

fn fixture(request: &Value, id: &str) -> Result<Value, String> {
    request
        .pointer(&format!("/fixtures/{}/value", pointer_segment(id)))
        .cloned()
        .ok_or_else(|| format!("Scenario request is missing fixture {id}."))
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Fixture is missing string field {field}."))
}

fn identity(application: &str) -> ImplementationIdentity {
    ImplementationIdentity {
        application: application.to_string(),
        implementation: format!("{application}.testbed"),
        version: "1.0.0".to_string(),
        instance_id: Some(format!("{application}-instance")),
    }
}

fn entry(
    sequence: usize,
    phase: &str,
    actor: &str,
    operation: &str,
    outcome: &str,
    facts: Value,
) -> Value {
    json!({
        "sequence": sequence,
        "phase": phase,
        "actor": actor,
        "operation": operation,
        "outcome": outcome,
        "facts": facts
    })
}

fn run_status(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Queued => "queued",
        RunStatus::Running => "running",
        RunStatus::Waiting => "waiting",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        RunStatus::Indeterminate => "indeterminate",
    }
}

fn markdown(value: &Value) -> Result<String, String> {
    serde_yaml::to_string(value)
        .map(|yaml| format!("---\n{yaml}---\n"))
        .map_err(|error| error.to_string())
}

fn write_file(root: &Path, relative: &str, content: &str) -> Result<(), String> {
    let path = root.join(relative);
    fs::create_dir_all(
        path.parent()
            .ok_or_else(|| format!("{relative} has no parent."))?,
    )
    .map_err(|error| error.to_string())?;
    fs::write(path, content).map_err(|error| error.to_string())
}

fn pointer_segment(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn read_request() -> Result<Value, String> {
    let mut source = String::new();
    io::stdin()
        .read_to_string(&mut source)
        .map_err(|error| error.to_string())?;
    serde_json::from_str(&source).map_err(|error| error.to_string())
}

fn write(value: &Value) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string(value).map_err(|error| error.to_string())?
    );
    Ok(())
}
