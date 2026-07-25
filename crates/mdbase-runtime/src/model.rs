use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::{json, Map};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventJournalEntry {
    pub cursor: u64,
    pub source_runtime: String,
    pub event_id: String,
    pub envelope: Value,
    pub received_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlannedRun {
    pub id: String,
    pub workflow: String,
    pub workflow_version: u64,
    pub workflow_revision: String,
    pub registry_revision: String,
    pub policy_revision: Option<String>,
    pub trigger: String,
    pub event_id: String,
    pub event_type: String,
    pub event_cursor: u64,
    pub event: Value,
    pub executor: String,
    pub idempotency_key: String,
    pub idempotency_scope: String,
    pub concurrency_group: String,
    pub concurrency_policy: ConcurrencyPolicy,
    /// Runs whose cancellation was requested when this replacement was
    /// admitted. An indeterminate blocker keeps the replacement queued until
    /// an operator resolves that ambiguity.
    #[serde(default)]
    pub replacement_blockers: Vec<String>,
    pub on_error: OnError,
    pub not_before: DateTime<Utc>,
    #[serde(default)]
    pub timeout_at: Option<DateTime<Utc>>,
    pub minimum_interval_ms: Option<u64>,
    pub vars: Value,
    pub workflow_value: Value,
    pub steps: Vec<PlannedStep>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlannedStep {
    pub id: String,
    pub action: String,
    pub action_version: u64,
    pub action_revision: String,
    /// Canonical action contract used to validate and authorize this admitted
    /// plan. Older persisted runs may omit it and use the matching live
    /// registry entry as a compatibility fallback.
    #[serde(default)]
    pub action_contract: Value,
    pub condition: Option<String>,
    pub input: Value,
    pub for_each: Option<PlannedForEach>,
    pub idempotent: bool,
    pub cooperative_cancellation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlannedForEach {
    pub items: Value,
    pub binding: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrencyPolicy {
    Skip,
    Queue,
    Replace,
    Allow,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    Stop,
    Continue,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Waiting,
    Succeeded,
    Failed,
    Cancelled,
    Indeterminate,
}

impl RunStatus {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Indeterminate
        )
    }

    pub(crate) fn occupies_concurrency_group(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Waiting)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Skipped,
    Cancelled,
    TimedOut,
    Indeterminate,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionAttemptStatus {
    Dispatching,
    Succeeded,
    Failed,
    Indeterminate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionAttempt {
    pub step_id: String,
    pub item_index: Option<usize>,
    pub invocation_id: String,
    pub action: String,
    pub action_version: u64,
    pub attempt: u32,
    pub status: ActionAttemptStatus,
    pub idempotent: bool,
    pub input: Value,
    pub output: Option<Value>,
    pub receipt: Option<Value>,
    pub error: Option<RuntimeFailure>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StepRecord {
    pub id: String,
    pub action: String,
    pub status: StepStatus,
    pub outputs: Vec<Value>,
    pub error: Option<RuntimeFailure>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunRecord {
    pub plan: PlannedRun,
    pub status: RunStatus,
    pub revision: u64,
    pub next_step: usize,
    pub steps: Vec<StepRecord>,
    pub attempts: Vec<ActionAttempt>,
    pub active_attempt: Option<usize>,
    pub cancel_requested_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl RunRecord {
    pub fn admitted(plan: PlannedRun) -> Self {
        let created_at = plan.created_at;
        let steps = plan
            .steps
            .iter()
            .map(|step| StepRecord {
                id: step.id.clone(),
                action: step.action.clone(),
                status: StepStatus::Pending,
                outputs: Vec::new(),
                error: None,
                started_at: None,
                finished_at: None,
            })
            .collect();
        Self {
            plan,
            status: RunStatus::Queued,
            revision: 0,
            next_step: 0,
            steps,
            attempts: Vec::new(),
            active_attempt: None,
            cancel_requested_at: None,
            created_at,
            started_at: None,
            updated_at: created_at,
            finished_at: None,
        }
    }

    /// Portable `runtime_run` contract value for materialization or tooling.
    ///
    /// The durable store keeps a richer internal plan; this projection exposes
    /// only the profile-defined record and never serializes provider internals.
    pub fn materialized_contract(&self) -> Value {
        let mut value = Map::from_iter([
            ("type".to_string(), json!("runtime_run")),
            ("id".to_string(), json!(self.plan.id)),
            ("workflow".to_string(), json!(self.plan.workflow)),
            (
                "workflow_version".to_string(),
                json!(self.plan.workflow_version),
            ),
            (
                "workflow_revision".to_string(),
                json!(self.plan.workflow_revision),
            ),
            (
                "registry_revision".to_string(),
                json!(self.plan.registry_revision),
            ),
            ("trigger".to_string(), json!(self.plan.trigger)),
            ("event_id".to_string(), json!(self.plan.event_id)),
            ("event_type".to_string(), json!(self.plan.event_type)),
            ("event_cursor".to_string(), json!(self.plan.event_cursor)),
            ("executor".to_string(), json!(self.plan.executor)),
            (
                "idempotency_key".to_string(),
                json!(self.plan.idempotency_key),
            ),
            (
                "concurrency_group".to_string(),
                json!(self.plan.concurrency_group),
            ),
            ("status".to_string(), json!(self.status)),
            ("created_at".to_string(), json!(self.created_at)),
            ("updated_at".to_string(), json!(self.updated_at)),
        ]);
        insert_option(
            &mut value,
            "policy_revision",
            self.plan.policy_revision.clone().map(Value::String),
        );
        insert_option(
            &mut value,
            "timeout_at",
            self.plan.timeout_at.map(|at| json!(at)),
        );
        insert_option(
            &mut value,
            "started_at",
            self.started_at.map(|at| json!(at)),
        );
        insert_option(
            &mut value,
            "finished_at",
            self.finished_at.map(|at| json!(at)),
        );
        insert_option(
            &mut value,
            "cancel_requested_at",
            self.cancel_requested_at.map(|at| json!(at)),
        );
        value.insert(
            "steps".to_string(),
            Value::Array(
                self.steps
                    .iter()
                    .zip(&self.plan.steps)
                    .map(|(step, planned)| {
                        let attempts = self
                            .attempts
                            .iter()
                            .filter(|attempt| attempt.step_id == step.id)
                            .collect::<Vec<_>>();
                        let latest = attempts.last().copied();
                        let mut item = Map::from_iter([
                            ("id".to_string(), json!(step.id)),
                            ("action".to_string(), json!(step.action)),
                            ("action_version".to_string(), json!(planned.action_version)),
                            ("status".to_string(), json!(step.status)),
                        ]);
                        if attempts.len() == 1 {
                            let attempt = latest.expect("one attempt is present");
                            item.insert("invocation_id".to_string(), json!(attempt.invocation_id));
                            item.insert("attempt".to_string(), json!(attempt.attempt));
                            item.insert("input".to_string(), attempt.input.clone());
                            insert_option(&mut item, "receipt", attempt.receipt.clone());
                        }
                        let output = match step.outputs.as_slice() {
                            [] => None,
                            [output] if planned.for_each.is_none() => Some(output.clone()),
                            outputs => Some(Value::Array(outputs.to_vec())),
                        };
                        insert_option(&mut item, "output", output);
                        insert_option(
                            &mut item,
                            "error",
                            step.error.as_ref().map(|error| json!(error)),
                        );
                        insert_option(&mut item, "started_at", step.started_at.map(|at| json!(at)));
                        insert_option(
                            &mut item,
                            "finished_at",
                            step.finished_at.map(|at| json!(at)),
                        );
                        Value::Object(item)
                    })
                    .collect(),
            ),
        );
        Value::Object(value)
    }

    pub(crate) fn request_cancel(&mut self, now: DateTime<Utc>) -> bool {
        if self.status.terminal() {
            return false;
        }
        self.cancel_requested_at.get_or_insert(now);
        self.updated_at = now;
        if matches!(self.status, RunStatus::Queued | RunStatus::Waiting) {
            let current = self.next_step;
            if let Some(step) = self.steps.get_mut(current) {
                step.status = StepStatus::Cancelled;
                step.finished_at = Some(now);
            }
            for step in self.steps.iter_mut().skip(current.saturating_add(1)) {
                if step.status == StepStatus::Pending {
                    step.status = StepStatus::Skipped;
                    step.finished_at = Some(now);
                }
            }
            self.next_step = self.steps.len();
            self.status = RunStatus::Cancelled;
            self.finished_at = Some(now);
        }
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeFailure {
    pub code: String,
    pub message: String,
}

impl RuntimeFailure {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionDispatch {
    pub run_id: String,
    pub workflow: String,
    pub step_id: String,
    pub item_index: Option<usize>,
    pub invocation_id: String,
    pub attempt: u32,
    pub action: String,
    pub input: Value,
    pub event: Value,
    pub executor: String,
    pub correlation_id: String,
    pub causation_id: String,
    pub deadline: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionResponse {
    pub output: Value,
    #[serde(default)]
    pub receipt: Option<Value>,
    #[serde(default)]
    pub emitted_events: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CancellationOutcome {
    pub accepted: bool,
    pub terminal: bool,
    pub invocation_id: Option<String>,
    pub provider_notified: bool,
    pub provider_error: Option<RuntimeFailure>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DispatchOutcome {
    NotApplied,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DispatchFailure {
    pub code: String,
    pub message: String,
    pub outcome: DispatchOutcome,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TimerStatus {
    Scheduled,
    Firing,
    Fired,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TimerRecord {
    pub id: String,
    pub generation: u64,
    pub status: TimerStatus,
    pub fire_at: DateTime<Utc>,
    pub event_type: String,
    pub contract_version: u64,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub fired_at: Option<DateTime<Utc>>,
}

impl TimerRecord {
    pub fn materialized_contract(&self) -> Value {
        let mut value = Map::from_iter([
            ("type".to_string(), json!("runtime_timer")),
            ("id".to_string(), json!(self.id)),
            ("generation".to_string(), json!(self.generation)),
            ("status".to_string(), json!(self.status)),
            ("fire_at".to_string(), json!(self.fire_at)),
            (
                "event".to_string(),
                json!({
                    "type": self.event_type,
                    "contract_version": self.contract_version,
                    "payload": self.payload
                }),
            ),
            ("missed_run_policy".to_string(), json!("fire_once")),
            ("created_at".to_string(), json!(self.created_at)),
            ("updated_at".to_string(), json!(self.updated_at)),
        ]);
        insert_option(&mut value, "fired_at", self.fired_at.map(|at| json!(at)));
        Value::Object(value)
    }
}

fn insert_option(target: &mut Map<String, Value>, name: &str, value: Option<Value>) {
    if let Some(value) = value {
        target.insert(name.to_string(), value);
    }
}
