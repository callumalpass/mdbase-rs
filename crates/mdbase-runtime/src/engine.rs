use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use jsonschema::{Draft, JSONSchema};
use mdbase::v03::{evaluate_runtime_expression, evaluate_runtime_template};
use mdbase_interop::{ActionCancellation, ActionInvocation, ActionOutcome, ImplementationIdentity};
use serde_json::{json, Map, Value};

use crate::admission::AdmissionCatalog;
use crate::clock::{Clock, SystemClock};
use crate::error::{RuntimeError, RuntimeResult};
use crate::model::{
    ActionAttempt, ActionAttemptStatus, ActionDispatch, CancellationOutcome, DispatchOutcome,
    OnError, RunStatus, RuntimeFailure, StepStatus,
};
use crate::planner::{plan_event, stable_id};
use crate::provider::{
    AuthorizationDecision, DenyAllAuthorizer, DispatchAuthorizer, ProviderRegistry,
};
use crate::store::{AdmitOutcome, Claim, EventPage, PreparedEvent, RuntimeStore};

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub runtime_id: String,
    pub executor_id: String,
    pub worker_id: String,
    pub actor_id: String,
    pub actor_kind: String,
    pub identity: ImplementationIdentity,
    pub timezone: Option<String>,
    pub lease_duration: Duration,
    pub max_items: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            runtime_id: "mdbase-runtime".to_string(),
            executor_id: "local".to_string(),
            worker_id: format!("worker_{}", ulid::Ulid::new()),
            actor_id: "local-user".to_string(),
            actor_kind: "user".to_string(),
            identity: ImplementationIdentity {
                application: "mdbase".to_string(),
                implementation: "mdbase-runtime".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                instance_id: None,
            },
            timezone: None,
            lease_duration: Duration::from_secs(30),
            max_items: 1_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryOutcome {
    pub cursor: u64,
    pub duplicate: bool,
    pub admitted_run_ids: Vec<String>,
    pub skipped_run_ids: Vec<String>,
    /// Existing runs for which this delivery durably recorded cancellation
    /// intent before sending any best-effort cooperative cancellation signal.
    pub cancellation_requested_run_ids: Vec<String>,
}

impl From<AdmitOutcome> for DeliveryOutcome {
    fn from(value: AdmitOutcome) -> Self {
        Self {
            cursor: value.cursor,
            duplicate: value.duplicate,
            admitted_run_ids: value.admitted_run_ids,
            skipped_run_ids: value.skipped_run_ids,
            cancellation_requested_run_ids: value.cancellation_requested_run_ids,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerOutcome {
    Idle,
    Completed { run_id: String, status: RunStatus },
    Deferred { run_id: String, reason: String },
}

pub struct Runtime {
    pub(crate) store: Arc<dyn RuntimeStore>,
    providers: ProviderRegistry,
    authorizer: Arc<dyn DispatchAuthorizer>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) config: RuntimeConfig,
}

pub struct RuntimeBuilder {
    store: Arc<dyn RuntimeStore>,
    providers: ProviderRegistry,
    authorizer: Arc<dyn DispatchAuthorizer>,
    clock: Arc<dyn Clock>,
    config: RuntimeConfig,
}

impl RuntimeBuilder {
    pub fn new(store: Arc<dyn RuntimeStore>) -> Self {
        Self {
            store,
            providers: ProviderRegistry::default(),
            authorizer: Arc::new(DenyAllAuthorizer),
            clock: Arc::new(SystemClock),
            config: RuntimeConfig::default(),
        }
    }

    pub fn providers(mut self, providers: ProviderRegistry) -> Self {
        self.providers = providers;
        self
    }

    pub fn authorizer(mut self, authorizer: Arc<dyn DispatchAuthorizer>) -> Self {
        self.authorizer = authorizer;
        self
    }

    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn config(mut self, config: RuntimeConfig) -> Self {
        self.config = config;
        self
    }

    pub fn build(self) -> RuntimeResult<Runtime> {
        Runtime::new(
            self.store,
            self.providers,
            self.authorizer,
            self.clock,
            self.config,
        )
    }
}

impl Runtime {
    pub fn builder(store: Arc<dyn RuntimeStore>) -> RuntimeBuilder {
        RuntimeBuilder::new(store)
    }

    pub fn new(
        store: Arc<dyn RuntimeStore>,
        providers: ProviderRegistry,
        authorizer: Arc<dyn DispatchAuthorizer>,
        clock: Arc<dyn Clock>,
        config: RuntimeConfig,
    ) -> RuntimeResult<Self> {
        Ok(Self {
            store,
            providers,
            authorizer,
            clock,
            config,
        })
    }

    pub fn providers(&self) -> &ProviderRegistry {
        &self.providers
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    pub async fn deliver_event(
        &self,
        catalog: &AdmissionCatalog,
        event: Value,
    ) -> RuntimeResult<DeliveryOutcome> {
        let prepared = self.prepare_event(catalog, event, self.clock.now())?;
        self.deliver_prepared_event(prepared).await
    }

    /// Admit a plan prepared by another conformant admission authority.
    ///
    /// This is the durable boundary used when Connect or a remote coordinator
    /// owns contract resolution. The immutable plan must already contain exact
    /// contract, source, provider, declaration, and handler evidence.
    pub async fn deliver_prepared_event(
        &self,
        prepared: PreparedEvent,
    ) -> RuntimeResult<DeliveryOutcome> {
        let admitted = self.store.admit_event(prepared).await?;
        for run_id in &admitted.cancellation_requested_run_ids {
            self.notify_cancellation_request(run_id).await;
        }
        Ok(admitted.into())
    }

    pub async fn work_once(&self) -> RuntimeResult<WorkerOutcome> {
        let now = self.clock.now();
        let Some(claim) = self
            .store
            .claim_run(
                &self.config.executor_id,
                &self.config.worker_id,
                now,
                self.config.lease_duration,
            )
            .await?
        else {
            return Ok(WorkerOutcome::Idle);
        };
        self.execute_claim(claim).await
    }

    pub async fn drain(&self, max_runs: usize) -> RuntimeResult<Vec<WorkerOutcome>> {
        let mut outcomes = Vec::new();
        for _ in 0..max_runs {
            let outcome = self.work_once().await?;
            if outcome == WorkerOutcome::Idle {
                break;
            }
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    pub async fn events_after(&self, after: u64, limit: usize) -> RuntimeResult<EventPage> {
        self.store.events_after(after, limit).await
    }

    pub async fn get_run(&self, id: &str) -> RuntimeResult<Option<crate::model::RunRecord>> {
        self.store.get_run(id).await
    }

    pub async fn prune_events_through(&self, cursor: u64) -> RuntimeResult<u64> {
        self.store.prune_events_through(cursor).await
    }

    /// Persist cancellation intent before optionally notifying a cooperative
    /// provider. The returned outcome distinguishes a durable request from the
    /// best-effort provider signal.
    pub async fn cancel_run(&self, id: &str) -> RuntimeResult<CancellationOutcome> {
        if !self.store.request_cancel(id, self.clock.now()).await? {
            return Ok(CancellationOutcome {
                accepted: false,
                terminal: true,
                invocation_id: None,
                provider_notified: false,
                provider_error: None,
            });
        }
        let Some(run) = self.store.get_run(id).await? else {
            return Err(RuntimeError::Store(
                "cancelled run disappeared from the runtime store".to_string(),
            ));
        };
        Ok(self.cancellation_outcome(&run).await)
    }

    async fn notify_cancellation_request(&self, id: &str) {
        if let Ok(Some(run)) = self.store.get_run(id).await {
            let _ = self.cancellation_outcome(&run).await;
        }
    }

    async fn cancellation_outcome(&self, run: &crate::model::RunRecord) -> CancellationOutcome {
        if run.status.terminal() {
            return CancellationOutcome {
                accepted: true,
                terminal: true,
                invocation_id: None,
                provider_notified: false,
                provider_error: None,
            };
        }
        let Some(active) = run.active_attempt.and_then(|index| run.attempts.get(index)) else {
            return CancellationOutcome {
                accepted: true,
                terminal: false,
                invocation_id: None,
                provider_notified: false,
                provider_error: None,
            };
        };
        let cooperative = run
            .plan
            .steps
            .get(run.next_step)
            .is_some_and(|step| step.cooperative_cancellation);
        if !cooperative {
            return CancellationOutcome {
                accepted: true,
                terminal: false,
                invocation_id: Some(active.invocation_id.clone()),
                provider_notified: false,
                provider_error: None,
            };
        }
        let Some(planned) = run.plan.steps.get(run.next_step) else {
            return CancellationOutcome {
                accepted: true,
                terminal: false,
                invocation_id: Some(active.invocation_id.clone()),
                provider_notified: false,
                provider_error: Some(RuntimeFailure::new(
                    "invalid_run_state",
                    "Active attempt has no corresponding admitted step.",
                )),
            };
        };
        let Some(provider) = self
            .providers
            .get(&planned.provider_declaration_digest, &planned.handler_id)
        else {
            return CancellationOutcome {
                accepted: true,
                terminal: false,
                invocation_id: Some(active.invocation_id.clone()),
                provider_notified: false,
                provider_error: Some(RuntimeFailure::new(
                    "unsupported_action_handler",
                    format!(
                        "No live handler is registered for declaration {} handler {}.",
                        planned.provider_declaration_digest, planned.handler_id
                    ),
                )),
            };
        };
        let notification = tokio::time::timeout(
            self.config.lease_duration,
            provider.cancel(ActionCancellation {
                kind: "mdbase.action.cancel".to_string(),
                profile_version: "0.1".to_string(),
                cancellation_id: stable_id(
                    "cancel",
                    &format!("{}:{}", run.plan.id, active.invocation_id),
                ),
                request_id: stable_id(
                    "req",
                    &format!(
                        "{}:{}:{}",
                        run.plan.id,
                        active.step_id,
                        active.item_index.unwrap_or(0)
                    ),
                ),
                caller: self.config.identity.clone(),
                requested_at: self.clock.now().to_rfc3339(),
                reason: Some("runtime_run_cancelled".to_string()),
            }),
        )
        .await;
        let provider_error = match notification {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(RuntimeFailure::new(error.code, error.message)),
            Err(_) => Some(RuntimeFailure::new(
                "action_cancellation_timed_out",
                "The provider did not acknowledge cancellation before the lease interval.",
            )),
        };
        CancellationOutcome {
            accepted: true,
            terminal: false,
            invocation_id: Some(active.invocation_id.clone()),
            provider_notified: provider_error.is_none(),
            provider_error,
        }
    }

    pub(crate) fn prepare_event(
        &self,
        catalog: &AdmissionCatalog,
        event: Value,
        received_at: DateTime<Utc>,
    ) -> RuntimeResult<PreparedEvent> {
        let source_runtime = event
            .get("source")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RuntimeError::diagnostic(
                    "invalid_runtime_event",
                    "Structured CloudEvent source is required.",
                )
            })?
            .to_string();
        let event_id = event
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RuntimeError::diagnostic("invalid_runtime_event", "Event id is required.")
            })?
            .to_string();
        let runs = plan_event(catalog, &event, received_at, &self.config)?;
        Ok(PreparedEvent {
            source_runtime,
            event_id,
            envelope: event,
            received_at,
            runs,
        })
    }

    async fn execute_claim(&self, mut claim: Claim) -> RuntimeResult<WorkerOutcome> {
        if claim.run.cancel_requested_at.is_some() {
            if let Some(active) = claim.run.active_attempt {
                let cooperative = claim
                    .run
                    .plan
                    .steps
                    .get(claim.run.next_step)
                    .is_some_and(|step| step.cooperative_cancellation);
                if cooperative
                    && claim.run.attempts[active].status == ActionAttemptStatus::Dispatching
                {
                    let _ = self.cancellation_outcome(&claim.run).await;
                    claim = self
                        .indeterminate_active_attempt(
                            claim,
                            active,
                            "action_cancellation_outcome_indeterminate",
                            "A cooperative action was cancelled while its dispatch outcome was not durable.",
                        )
                        .await?;
                    return Ok(WorkerOutcome::Completed {
                        run_id: claim.run.plan.id,
                        status: RunStatus::Indeterminate,
                    });
                }
            } else {
                claim = self.finish_cancelled(claim).await?;
                return Ok(WorkerOutcome::Completed {
                    run_id: claim.run.plan.id,
                    status: RunStatus::Cancelled,
                });
            }
        }
        if claim.run.active_attempt.is_none() && self.deadline_elapsed(&claim.run) {
            claim = self.timeout_run(claim).await?;
            return Ok(WorkerOutcome::Completed {
                run_id: claim.run.plan.id,
                status: claim.run.status,
            });
        }
        if let Some(active) = claim.run.active_attempt {
            if claim.run.attempts[active].status == ActionAttemptStatus::Dispatching {
                if !claim.run.attempts[active].idempotent {
                    claim = self
                        .indeterminate_active_attempt(
                            claim,
                            active,
                            "action_outcome_indeterminate",
                            "A non-idempotent dispatch was interrupted before its outcome became durable.",
                        )
                        .await?;
                    return Ok(WorkerOutcome::Completed {
                        run_id: claim.run.plan.id,
                        status: RunStatus::Indeterminate,
                    });
                }
                claim = self.dispatch_active_attempt(claim, active).await?;
                if claim.run.status.terminal() {
                    return Ok(WorkerOutcome::Completed {
                        run_id: claim.run.plan.id,
                        status: claim.run.status,
                    });
                }
                if claim.run.steps[claim.run.next_step].status == StepStatus::Failed {
                    claim = self.advance_failed_step(claim).await?;
                    if claim.run.status.terminal() {
                        return Ok(WorkerOutcome::Completed {
                            run_id: claim.run.plan.id,
                            status: claim.run.status,
                        });
                    }
                }
            }
        }

        if self.deadline_elapsed(&claim.run) {
            claim = self.timeout_run(claim).await?;
            return Ok(WorkerOutcome::Completed {
                run_id: claim.run.plan.id,
                status: claim.run.status,
            });
        }

        while claim.run.next_step < claim.run.plan.steps.len() {
            let step_index = claim.run.next_step;
            if self.deadline_elapsed(&claim.run) {
                claim = self.timeout_run(claim).await?;
                break;
            }
            if claim.run.cancel_requested_at.is_some() {
                claim = self.finish_cancelled(claim).await?;
                return Ok(WorkerOutcome::Completed {
                    run_id: claim.run.plan.id,
                    status: RunStatus::Cancelled,
                });
            }

            let planned = claim.run.plan.steps[step_index].clone();
            let activation = bindings(&claim.run, None);
            if let Some(condition) = &planned.condition {
                let matched = match evaluate_runtime_expression(
                    condition,
                    &activation,
                    self.clock.now(),
                    self.config.timezone.as_deref(),
                ) {
                    Ok(value) => value == Value::Bool(true),
                    Err(error) => {
                        claim = self
                            .advance_after_failure(
                                claim,
                                RuntimeFailure::new(error.code, error.message),
                            )
                            .await?;
                        if claim.run.status.terminal() {
                            break;
                        }
                        continue;
                    }
                };
                if !matched {
                    let now = self.clock.now();
                    claim.run.steps[step_index].status = StepStatus::Skipped;
                    claim.run.steps[step_index].finished_at = Some(now);
                    claim.run.next_step += 1;
                    claim.run.updated_at = now;
                    claim = self.store.commit_run(claim, Vec::new()).await?;
                    continue;
                }
            }

            let items = match &planned.for_each {
                Some(for_each) => {
                    let value = match evaluate_runtime_template(
                        &for_each.items,
                        &activation,
                        self.clock.now(),
                        self.config.timezone.as_deref(),
                    ) {
                        Ok(value) => value,
                        Err(errors) => {
                            claim = self
                                .advance_after_failure(
                                    claim,
                                    runtime_failure(expression_errors(errors)),
                                )
                                .await?;
                            if claim.run.status.terminal() {
                                break;
                            }
                            continue;
                        }
                    };
                    let Some(items) = value.as_array().cloned() else {
                        claim = self
                            .advance_after_failure(
                                claim,
                                RuntimeFailure::new(
                                    "workflow_expression_type",
                                    format!(
                                        "for_each for step {} must evaluate to a list.",
                                        planned.id
                                    ),
                                ),
                            )
                            .await?;
                        if claim.run.status.terminal() {
                            break;
                        }
                        continue;
                    };
                    items
                }
                None => vec![Value::Null],
            };
            if items.len() > self.config.max_items {
                claim = self
                    .advance_after_failure(
                        claim,
                        RuntimeFailure::new(
                            "workflow_item_limit",
                            format!(
                                "Step {} expanded {} items; limit is {}.",
                                planned.id,
                                items.len(),
                                self.config.max_items
                            ),
                        ),
                    )
                    .await?;
                if claim.run.status.terminal() {
                    break;
                }
                continue;
            }

            claim.run.steps[step_index].status = StepStatus::Running;
            claim.run.steps[step_index]
                .started_at
                .get_or_insert_with(|| self.clock.now());
            claim.run.updated_at = self.clock.now();
            claim = self.store.commit_run(claim, Vec::new()).await?;

            let completed_items = claim.run.steps[step_index].outputs.len();
            for (item_index, item) in items.into_iter().enumerate().skip(completed_items) {
                let item_index_value = planned.for_each.as_ref().map(|_| item_index);
                let mut item_bindings = bindings(&claim.run, None);
                if let Some(for_each) = &planned.for_each {
                    item_bindings[&for_each.binding] = item;
                }
                let input = match evaluate_runtime_template(
                    &planned.input,
                    &item_bindings,
                    self.clock.now(),
                    self.config.timezone.as_deref(),
                ) {
                    Ok(input) => input,
                    Err(errors) => {
                        claim = self
                            .fail_step(claim, runtime_failure(expression_errors(errors)))
                            .await?;
                        break;
                    }
                };

                let pinned_action = match self.pinned_action(&planned) {
                    Ok(action) => action,
                    Err(error) => {
                        claim = self.fail_step(claim, runtime_failure(error)).await?;
                        break;
                    }
                };
                if let Err(message) = validate_action_value(pinned_action, "input_schema", &input) {
                    claim = self
                        .fail_step(claim, RuntimeFailure::new("action_input_invalid", message))
                        .await?;
                    break;
                }

                let attempt_number = claim
                    .run
                    .attempts
                    .iter()
                    .filter(|attempt| {
                        attempt.step_id == planned.id && attempt.item_index == item_index_value
                    })
                    .count() as u32
                    + 1;
                let invocation_id = stable_id(
                    "inv",
                    &format!(
                        "{}:{}:{}",
                        claim.run.plan.id,
                        planned.id,
                        item_index_value.unwrap_or(0)
                    ),
                );
                let dispatch = self.dispatch_value(
                    &claim,
                    &planned,
                    item_index_value,
                    invocation_id.clone(),
                    attempt_number,
                    input.clone(),
                );
                if let Err(error) = self.authorize(&dispatch).await {
                    claim = self.fail_step(claim, runtime_failure(error)).await?;
                    break;
                }

                let attempt = ActionAttempt {
                    step_id: planned.id.clone(),
                    item_index: item_index_value,
                    invocation_id,
                    action: planned.action.clone(),
                    action_version: planned.action_version.clone(),
                    attempt: attempt_number,
                    status: ActionAttemptStatus::Dispatching,
                    idempotent: planned.idempotent,
                    input,
                    output: None,
                    receipt: None,
                    error: None,
                    started_at: self.clock.now(),
                    finished_at: None,
                };
                claim.run.attempts.push(attempt);
                let active = claim.run.attempts.len() - 1;
                claim.run.active_attempt = Some(active);
                claim.run.updated_at = self.clock.now();
                claim = self.store.commit_run(claim, Vec::new()).await?;
                claim = self.dispatch_active_attempt(claim, active).await?;
                if claim.run.status.terminal()
                    || claim.run.steps[step_index].status == StepStatus::Failed
                {
                    break;
                }
            }

            if claim.run.status.terminal() {
                break;
            }
            if claim.run.steps[step_index].status == StepStatus::Failed {
                claim = self.advance_failed_step(claim).await?;
                if claim.run.status.terminal() {
                    break;
                }
                continue;
            }

            let now = self.clock.now();
            claim.run.steps[step_index].status = StepStatus::Succeeded;
            claim.run.steps[step_index].finished_at = Some(now);
            claim.run.next_step += 1;
            claim.run.updated_at = now;
            claim = self.store.commit_run(claim, Vec::new()).await?;
        }

        if !claim.run.status.terminal() && claim.run.next_step >= claim.run.plan.steps.len() {
            let now = self.clock.now();
            claim.run.status = if claim
                .run
                .steps
                .iter()
                .any(|step| step.status == StepStatus::Failed)
            {
                RunStatus::Failed
            } else {
                RunStatus::Succeeded
            };
            claim.run.finished_at = Some(now);
            claim.run.updated_at = now;
            claim = self.store.commit_run(claim, Vec::new()).await?;
        }
        Ok(WorkerOutcome::Completed {
            run_id: claim.run.plan.id,
            status: claim.run.status,
        })
    }

    async fn dispatch_active_attempt(
        &self,
        mut claim: Claim,
        active: usize,
    ) -> RuntimeResult<Claim> {
        let attempt = claim.run.attempts[active].clone();
        let planned = claim
            .run
            .plan
            .steps
            .get(claim.run.next_step)
            .cloned()
            .ok_or_else(|| {
                RuntimeError::diagnostic(
                    "invalid_run_state",
                    "Active attempt has no corresponding planned step.",
                )
            })?;
        let pinned_action = match self.pinned_action(&planned) {
            Ok(action) => action,
            Err(error) => {
                return self
                    .fail_active_attempt(claim, active, runtime_failure(error))
                    .await
            }
        };
        let dispatch = self.dispatch_value(
            &claim,
            &planned,
            attempt.item_index,
            attempt.invocation_id.clone(),
            attempt.attempt,
            attempt.input.clone(),
        );
        if let Err(error) = self.authorize(&dispatch).await {
            return self
                .fail_active_attempt(claim, active, runtime_failure(error))
                .await;
        }
        let Some(provider) = self
            .providers
            .get(&planned.provider_declaration_digest, &planned.handler_id)
        else {
            return self
                .fail_active_attempt(
                    claim,
                    active,
                    RuntimeFailure::new(
                        "unsupported_action_handler",
                        format!(
                            "No live handler is registered for declaration {} handler {}.",
                            planned.provider_declaration_digest, planned.handler_id
                        ),
                    ),
                )
                .await;
        };

        let provider_result = if let Some(deadline) = claim.run.plan.timeout_at {
            let remaining = (deadline - self.clock.now())
                .to_std()
                .unwrap_or(Duration::ZERO);
            match tokio::time::timeout(remaining, provider.dispatch(dispatch.invocation.clone()))
                .await
            {
                Ok(result) => result,
                Err(_) => Err(crate::model::DispatchFailure {
                    code: "action_dispatch_timed_out".to_string(),
                    message: "The workflow deadline elapsed while the action was dispatching."
                        .to_string(),
                    outcome: DispatchOutcome::Unknown,
                }),
            }
        } else {
            provider.dispatch(dispatch.invocation.clone()).await
        }
        .and_then(|outcome| normalize_outcome(&dispatch.invocation, outcome));

        match provider_result {
            Ok((output, receipt)) => {
                if let Err(message) = validate_action_value(pinned_action, "output_schema", &output)
                {
                    claim.run.attempts[active].status = ActionAttemptStatus::Failed;
                    claim.run.attempts[active].error =
                        Some(RuntimeFailure::new("action_output_invalid", &message));
                    claim.run.attempts[active].receipt = Some(receipt);
                    claim.run.attempts[active].finished_at = Some(self.clock.now());
                    claim.run.active_attempt = None;
                    return self
                        .fail_step(claim, RuntimeFailure::new("action_output_invalid", message))
                        .await;
                }

                let now = self.clock.now();
                claim.run.attempts[active].status = ActionAttemptStatus::Succeeded;
                claim.run.attempts[active].output = Some(output.clone());
                claim.run.attempts[active].receipt = Some(receipt);
                claim.run.attempts[active].finished_at = Some(now);
                if self.deadline_elapsed(&claim.run) {
                    let step_index = claim.run.next_step;
                    claim.run.steps[step_index].status = StepStatus::TimedOut;
                    claim.run.steps[step_index].error = Some(RuntimeFailure::new(
                        "workflow_timed_out",
                        "The action completed after the workflow deadline.",
                    ));
                    claim.run.steps[step_index].finished_at = Some(now);
                    skip_remaining_steps(&mut claim.run, step_index.saturating_add(1), now);
                    claim.run.active_attempt = None;
                    claim.run.next_step = claim.run.steps.len();
                    claim.run.status = RunStatus::Failed;
                    claim.run.finished_at = Some(now);
                    claim.run.updated_at = now;
                    return self.store.commit_run(claim, Vec::new()).await;
                }
                claim.run.steps[claim.run.next_step].outputs.push(output);
                claim.run.active_attempt = None;
                claim.run.updated_at = now;
                self.store.commit_run(claim, Vec::new()).await
            }
            Err(failure) if failure.outcome == DispatchOutcome::NotApplied => {
                claim.run.attempts[active].status = ActionAttemptStatus::Failed;
                claim.run.attempts[active].error =
                    Some(RuntimeFailure::new(&failure.code, &failure.message));
                claim.run.attempts[active].finished_at = Some(self.clock.now());
                claim.run.active_attempt = None;
                if self.deadline_elapsed(&claim.run) {
                    return self.timeout_run(claim).await;
                }
                self.fail_step(claim, RuntimeFailure::new(failure.code, failure.message))
                    .await
            }
            Err(failure) if attempt.idempotent => Err(RuntimeError::Provider(format!(
                "{}: {} (safe to replay {})",
                failure.code, failure.message, attempt.invocation_id
            ))),
            Err(failure) => {
                let now = self.clock.now();
                claim.run.attempts[active].status = ActionAttemptStatus::Indeterminate;
                claim.run.attempts[active].error =
                    Some(RuntimeFailure::new(&failure.code, &failure.message));
                claim.run.attempts[active].finished_at = Some(now);
                let step_index = claim.run.next_step;
                claim.run.steps[step_index].status = StepStatus::Indeterminate;
                claim.run.steps[step_index].error = Some(RuntimeFailure::new(
                    "action_outcome_indeterminate",
                    failure.message,
                ));
                claim.run.steps[step_index].finished_at = Some(now);
                skip_remaining_steps(&mut claim.run, step_index.saturating_add(1), now);
                claim.run.next_step = claim.run.steps.len();
                claim.run.status = RunStatus::Indeterminate;
                claim.run.finished_at = Some(now);
                claim.run.updated_at = now;
                self.store.commit_run(claim, Vec::new()).await
            }
        }
    }

    async fn authorize(&self, dispatch: &ActionDispatch) -> RuntimeResult<()> {
        match self.authorizer.authorize(dispatch).await {
            AuthorizationDecision::Allow => Ok(()),
            AuthorizationDecision::Deny { code, message } => {
                Err(RuntimeError::diagnostic(code, message))
            }
        }
    }

    fn pinned_action<'a>(
        &self,
        planned: &'a crate::model::PlannedStep,
    ) -> RuntimeResult<&'a Value> {
        let action = &planned.action_contract;
        if action.get("id").and_then(Value::as_str) != Some(planned.action.as_str())
            || action.get("version").and_then(Value::as_str)
                != Some(planned.action_version.as_str())
            || mdbase_interop::contract_digest(action).as_deref()
                != Ok(planned.action_digest.as_str())
        {
            return Err(RuntimeError::diagnostic(
                "pinned_action_corrupt",
                format!(
                    "Pinned action {} does not match its revision.",
                    planned.action
                ),
            ));
        }
        Ok(action)
    }

    fn dispatch_value(
        &self,
        claim: &Claim,
        planned: &crate::model::PlannedStep,
        item_index: Option<usize>,
        invocation_id: String,
        attempt: u32,
        input: Value,
    ) -> ActionDispatch {
        let correlation_id = claim
            .run
            .plan
            .event
            .get("correlationid")
            .and_then(Value::as_str)
            .unwrap_or(&claim.run.plan.event_id)
            .to_string();
        let request_id = stable_id(
            "req",
            &format!(
                "{}:{}:{}",
                claim.run.plan.id,
                planned.id,
                item_index.unwrap_or(0)
            ),
        );
        let attempt_id = stable_id("attempt", &format!("{request_id}:{attempt}"));
        let contract = mdbase_interop::ExactContractReference {
            id: planned.action.clone(),
            version: planned.action_version.clone(),
            digest: planned.action_digest.clone(),
        };
        let invocation = ActionInvocation {
            kind: "mdbase.action.invocation".to_string(),
            profile_version: "0.1".to_string(),
            invocation_id: invocation_id.clone(),
            attempt_id,
            request_id,
            contract: contract.clone(),
            caller: self.config.identity.clone(),
            provider: planned.provider.clone(),
            provider_declaration_digest: planned.provider_declaration_digest.clone(),
            handler_id: planned.handler_id.clone(),
            admitted_at: self.clock.now().to_rfc3339(),
            correlation_id: Some(correlation_id.clone()),
            causation_id: Some(claim.run.plan.event_id.clone()),
            subject: claim
                .run
                .plan
                .event
                .get("subject")
                .and_then(Value::as_str)
                .map(str::to_string),
            idempotency_key: planned
                .idempotent
                .then(|| format!("{}:{item_index:?}", claim.run.plan.idempotency_key)),
            deadline: claim.run.plan.timeout_at.map(|value| value.to_rfc3339()),
            authorization_context: None,
            input: input.clone(),
        };
        ActionDispatch {
            invocation,
            run_id: claim.run.plan.id.clone(),
            workflow: claim.run.plan.workflow.clone(),
            step_id: planned.id.clone(),
            item_index,
            invocation_id,
            attempt,
            action: planned.action.clone(),
            contract,
            provider: planned.provider.clone(),
            provider_declaration_digest: planned.provider_declaration_digest.clone(),
            handler_id: planned.handler_id.clone(),
            input,
            event: claim.run.plan.event.clone(),
            executor: self.config.executor_id.clone(),
            correlation_id,
            causation_id: claim.run.plan.event_id.clone(),
            deadline: claim.run.plan.timeout_at,
        }
    }

    async fn fail_step(&self, mut claim: Claim, failure: RuntimeFailure) -> RuntimeResult<Claim> {
        let index = claim.run.next_step;
        let now = self.clock.now();
        claim.run.steps[index].status = StepStatus::Failed;
        claim.run.steps[index].error = Some(failure);
        claim.run.steps[index].finished_at = Some(now);
        claim.run.active_attempt = None;
        claim.run.updated_at = now;
        self.store.commit_run(claim, Vec::new()).await
    }

    async fn advance_after_failure(
        &self,
        claim: Claim,
        failure: RuntimeFailure,
    ) -> RuntimeResult<Claim> {
        let mut claim = self.fail_step(claim, failure).await?;
        claim = self.advance_failed_step(claim).await?;
        Ok(claim)
    }

    async fn advance_failed_step(&self, mut claim: Claim) -> RuntimeResult<Claim> {
        let now = self.clock.now();
        if claim.run.plan.on_error == OnError::Stop {
            let remaining_start = claim.run.next_step.saturating_add(1);
            skip_remaining_steps(&mut claim.run, remaining_start, now);
            claim.run.next_step = claim.run.steps.len();
            claim.run.status = RunStatus::Failed;
            claim.run.finished_at = Some(now);
        } else {
            claim.run.next_step += 1;
        }
        claim.run.updated_at = now;
        self.store.commit_run(claim, Vec::new()).await
    }

    async fn finish_cancelled(&self, mut claim: Claim) -> RuntimeResult<Claim> {
        let now = self.clock.now();
        let step_index = claim.run.next_step;
        if let Some(step) = claim.run.steps.get_mut(step_index) {
            step.status = StepStatus::Cancelled;
            step.finished_at = Some(now);
        }
        skip_remaining_steps(&mut claim.run, step_index.saturating_add(1), now);
        claim.run.active_attempt = None;
        claim.run.next_step = claim.run.steps.len();
        claim.run.status = RunStatus::Cancelled;
        claim.run.finished_at = Some(now);
        claim.run.updated_at = now;
        self.store.commit_run(claim, Vec::new()).await
    }

    async fn indeterminate_active_attempt(
        &self,
        mut claim: Claim,
        active: usize,
        code: &str,
        message: &str,
    ) -> RuntimeResult<Claim> {
        let now = self.clock.now();
        let failure = RuntimeFailure::new(code, message);
        claim.run.attempts[active].status = ActionAttemptStatus::Indeterminate;
        claim.run.attempts[active].error = Some(failure.clone());
        claim.run.attempts[active].finished_at = Some(now);
        let step_index = claim.run.next_step;
        claim.run.steps[step_index].status = StepStatus::Indeterminate;
        claim.run.steps[step_index].error = Some(failure);
        claim.run.steps[step_index].finished_at = Some(now);
        skip_remaining_steps(&mut claim.run, step_index.saturating_add(1), now);
        claim.run.active_attempt = None;
        claim.run.next_step = claim.run.steps.len();
        claim.run.status = RunStatus::Indeterminate;
        claim.run.finished_at = Some(now);
        claim.run.updated_at = now;
        self.store.commit_run(claim, Vec::new()).await
    }

    async fn fail_active_attempt(
        &self,
        mut claim: Claim,
        active: usize,
        failure: RuntimeFailure,
    ) -> RuntimeResult<Claim> {
        claim.run.attempts[active].status = ActionAttemptStatus::Failed;
        claim.run.attempts[active].error = Some(failure.clone());
        claim.run.attempts[active].finished_at = Some(self.clock.now());
        claim.run.active_attempt = None;
        self.fail_step(claim, failure).await
    }

    fn deadline_elapsed(&self, run: &crate::model::RunRecord) -> bool {
        run.plan
            .timeout_at
            .is_some_and(|deadline| self.clock.now() >= deadline)
    }

    async fn timeout_run(&self, mut claim: Claim) -> RuntimeResult<Claim> {
        let now = self.clock.now();
        let step_index = claim.run.next_step;
        if let Some(step) = claim.run.steps.get_mut(step_index) {
            step.status = StepStatus::TimedOut;
            step.error = Some(RuntimeFailure::new(
                "workflow_timed_out",
                "The workflow exceeded its execution deadline.",
            ));
            step.finished_at = Some(now);
        }
        skip_remaining_steps(&mut claim.run, step_index.saturating_add(1), now);
        claim.run.active_attempt = None;
        claim.run.next_step = claim.run.steps.len();
        claim.run.status = RunStatus::Failed;
        claim.run.finished_at = Some(now);
        claim.run.updated_at = now;
        self.store.commit_run(claim, Vec::new()).await
    }
}

fn skip_remaining_steps(run: &mut crate::model::RunRecord, start: usize, now: DateTime<Utc>) {
    for step in run.steps.iter_mut().skip(start) {
        if step.status == StepStatus::Pending {
            step.status = StepStatus::Skipped;
            step.finished_at = Some(now);
        }
    }
}

fn bindings(run: &crate::model::RunRecord, item: Option<(&str, Value)>) -> Value {
    let steps = run
        .steps
        .iter()
        .map(|step| {
            let output = match step.outputs.as_slice() {
                [] => Value::Null,
                [one] => one.clone(),
                many => Value::Array(many.to_vec()),
            };
            (
                step.id.clone(),
                json!({
                    "status": step.status,
                    "output": output
                }),
            )
        })
        .collect::<Map<_, _>>();
    let mut value = json!({
        "event": run.plan.event,
        "workflow": {
            "id": run.plan.workflow,
            "version": run.plan.workflow_version
        },
        "trigger": {
            "id": run.plan.trigger,
            "event": run.plan.event_type
        },
        "vars": run.plan.vars,
        "steps": steps
    });
    if let Some((name, item)) = item {
        value[name] = item;
    }
    value
}

fn expression_errors(errors: Vec<mdbase::v03::WorkflowCelError>) -> RuntimeError {
    RuntimeError::diagnostic(
        "workflow_expression_error",
        errors
            .into_iter()
            .map(|error| format!("{}: {}", error.code, error.message))
            .collect::<Vec<_>>()
            .join("; "),
    )
}

fn runtime_failure(error: RuntimeError) -> RuntimeFailure {
    RuntimeFailure::new(error.code(), error.to_string())
}

fn validate_action_value(
    artifact: &Value,
    schema_field: &str,
    value: &Value,
) -> Result<(), String> {
    let schema = artifact
        .pointer(&format!("/{schema_field}/value"))
        .ok_or_else(|| format!("Pinned action artifact has no inline {schema_field}."))?;
    let compiled = JSONSchema::options()
        .with_draft(Draft::Draft202012)
        .compile(schema)
        .map_err(|error| format!("Pinned {schema_field} does not compile: {error}"))?;
    compiled.validate(value).map_err(|errors| {
        errors
            .map(|error| format!("{}: {error}", error.instance_path))
            .collect::<Vec<_>>()
            .join("; ")
    })
}

fn normalize_outcome(
    invocation: &ActionInvocation,
    outcome: ActionOutcome,
) -> Result<(Value, Value), crate::model::DispatchFailure> {
    let evidence_matches = outcome.kind == "mdbase.action.outcome"
        && outcome.profile_version == "0.1"
        && outcome.request_id == invocation.request_id
        && outcome.invocation_id == invocation.invocation_id
        && outcome.attempt_id == invocation.attempt_id
        && outcome.contract == invocation.contract
        && outcome.provider == invocation.provider
        && outcome.provider_declaration_digest == invocation.provider_declaration_digest;
    if !evidence_matches {
        return Err(crate::model::DispatchFailure {
            code: "action_outcome_evidence_mismatch".to_string(),
            message: "Provider outcome does not match the admitted invocation evidence."
                .to_string(),
            outcome: DispatchOutcome::Unknown,
        });
    }
    let receipt = serde_json::to_value(&outcome).expect("portable action outcome serializes");
    match outcome.status.as_str() {
        "succeeded" => outcome
            .output
            .map(|output| (output, receipt))
            .ok_or_else(|| crate::model::DispatchFailure {
                code: "action_output_missing".to_string(),
                message: "Successful action outcome has no output.".to_string(),
                outcome: DispatchOutcome::Unknown,
            }),
        "indeterminate" => Err(crate::model::DispatchFailure {
            code: outcome
                .error
                .as_ref()
                .map_or("action_outcome_indeterminate", |error| error.code.as_str())
                .to_string(),
            message: outcome.error.as_ref().map_or_else(
                || "Provider reported an indeterminate outcome.".to_string(),
                |error| error.message.clone(),
            ),
            outcome: DispatchOutcome::Unknown,
        }),
        status => Err(crate::model::DispatchFailure {
            code: outcome
                .error
                .as_ref()
                .map_or("action_failed", |error| error.code.as_str())
                .to_string(),
            message: outcome.error.as_ref().map_or_else(
                || format!("Provider reported action status {status}."),
                |error| error.message.clone(),
            ),
            outcome: DispatchOutcome::NotApplied,
        }),
    }
}
