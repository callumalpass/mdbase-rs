use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, TimeDelta, Utc};
use mdbase::v03::{evaluate_runtime_expression, evaluate_runtime_template};
use regex::Regex;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::admission::{AdmissionCatalog, AdmittedWorkflow};
use crate::engine::RuntimeConfig;
use crate::error::{RuntimeError, RuntimeResult};
use crate::model::{ConcurrencyPolicy, OnError, PlannedForEach, PlannedRun, PlannedStep};

pub(crate) fn plan_event(
    catalog: &AdmissionCatalog,
    event: &Value,
    now: DateTime<Utc>,
    config: &RuntimeConfig,
) -> RuntimeResult<Vec<PlannedRun>> {
    let event_id = required_string(event, "id")?;
    let event_type = required_string(event, "type")?;
    let mut plans = Vec::new();

    for admitted in catalog.admit_event(event)? {
        let workflow = &admitted.workflow;
        let workflow_id = required_string(workflow, "id")?;
        let workflow_version = required_string(workflow, "version")?.to_string();
        if let Some(selected_executor) = catalog.selected_executor(workflow_id) {
            if selected_executor != config.executor_id {
                continue;
            }
        }
        let trigger = &admitted.trigger;
        let trigger_id = required_string(trigger, "id")?;
        let mut bindings = json!({
            "event": event,
            "workflow": {
                "id": workflow_id,
                "version": workflow_version
            },
            "trigger": trigger,
            "vars": {},
            "steps": {}
        });
        let vars = evaluate_vars(
            workflow.get("vars"),
            &mut bindings,
            now,
            config.timezone.as_deref(),
        )?;
        bindings["vars"] = vars.clone();
        if !condition_matches(
            trigger.get("if"),
            &bindings,
            now,
            config.timezone.as_deref(),
        )? || !condition_matches(
            workflow.get("if"),
            &bindings,
            now,
            config.timezone.as_deref(),
        )? {
            continue;
        }

        let idempotency_key = match workflow.pointer("/run/idempotency/key") {
            Some(value) => evaluated_string(
                value,
                &bindings,
                now,
                config.timezone.as_deref(),
                "idempotency key",
            )?,
            None => format!("{workflow_id}:{event_id}:{trigger_id}"),
        };
        let idempotency_scope = format!("{}:{workflow_id}", config.executor_id);
        let concurrency_group = match workflow.pointer("/run/concurrency/group") {
            Some(value) => evaluated_string(
                value,
                &bindings,
                now,
                config.timezone.as_deref(),
                "concurrency group",
            )?,
            None => workflow_id.to_string(),
        };
        let concurrency_policy = match workflow
            .pointer("/run/concurrency/policy")
            .and_then(Value::as_str)
            .unwrap_or("allow")
        {
            "skip" => ConcurrencyPolicy::Skip,
            "queue" => ConcurrencyPolicy::Queue,
            "replace" => ConcurrencyPolicy::Replace,
            "allow" => ConcurrencyPolicy::Allow,
            value => {
                return Err(RuntimeError::diagnostic(
                    "invalid_workflow",
                    format!("Unsupported concurrency policy {value}."),
                ))
            }
        };
        let on_error = match workflow
            .pointer("/run/on_error")
            .and_then(Value::as_str)
            .unwrap_or("stop")
        {
            "stop" => OnError::Stop,
            "continue" => OnError::Continue,
            value => {
                return Err(RuntimeError::diagnostic(
                    "invalid_workflow",
                    format!("Unsupported on_error policy {value}."),
                ))
            }
        };
        let debounce_ms = trigger
            .get("debounce")
            .and_then(Value::as_str)
            .map(parse_duration_ms)
            .transpose()?
            .unwrap_or(0);
        let minimum_interval_ms = trigger
            .get("minimum_interval")
            .and_then(Value::as_str)
            .map(parse_duration_ms)
            .transpose()?;
        let not_before = now
            + TimeDelta::milliseconds(i64::try_from(debounce_ms).map_err(|_| {
                RuntimeError::diagnostic(
                    "duration_out_of_range",
                    "Trigger debounce exceeds the supported range.",
                )
            })?);
        let timeout_ms = workflow
            .pointer("/run/limits/timeout")
            .and_then(Value::as_str)
            .map(parse_duration_ms)
            .transpose()?;
        let timeout_at = timeout_ms
            .map(|timeout| {
                i64::try_from(timeout)
                    .map(TimeDelta::milliseconds)
                    .map(|timeout| now + timeout)
            })
            .transpose()
            .map_err(|_| {
                RuntimeError::diagnostic(
                    "duration_out_of_range",
                    "Workflow timeout exceeds the supported range.",
                )
            })?;
        let steps = plan_steps(workflow, &admitted)?;
        let run_id = stable_id(
            "run",
            &format!(
                "{}:{workflow_id}:{trigger_id}:{event_id}:{}",
                config.executor_id, idempotency_key
            ),
        );
        plans.push(PlannedRun {
            id: run_id,
            workflow: workflow_id.to_string(),
            workflow_version,
            workflow_revision: admitted.workflow_revision,
            catalog_revision: catalog.revision().to_string(),
            policy_id: admitted.policy_id,
            policy_revision: admitted.policy_revision,
            trigger: trigger_id.to_string(),
            event_id: event_id.to_string(),
            event_type: event_type.to_string(),
            event_contract: admitted.event_contract,
            event_source: admitted.event_source,
            source_declaration_digest: admitted.source_declaration_digest,
            event_cursor: 0,
            event: event.clone(),
            executor: config.executor_id.clone(),
            idempotency_key,
            idempotency_scope,
            concurrency_group,
            concurrency_policy,
            replacement_blockers: Vec::new(),
            on_error,
            not_before,
            timeout_at,
            minimum_interval_ms,
            vars,
            workflow_value: workflow.clone(),
            steps,
            created_at: now,
        });
    }
    Ok(plans)
}

fn plan_steps(workflow: &Value, admitted: &AdmittedWorkflow) -> RuntimeResult<Vec<PlannedStep>> {
    workflow
        .get("steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|step| {
            let id = required_string(step, "id")?.to_string();
            let binding = admitted
                .steps
                .iter()
                .find(|binding| binding.id == id)
                .ok_or_else(|| {
                    RuntimeError::diagnostic(
                        "unresolved_action",
                        format!("Admitted plan has no action binding for step {id}."),
                    )
                })?;
            let condition = step
                .pointer("/if/$expr")
                .and_then(Value::as_str)
                .map(str::to_string);
            let for_each = step.get("for_each").map(|value| PlannedForEach {
                items: value.get("items").cloned().unwrap_or(Value::Null),
                binding: value
                    .get("as")
                    .and_then(Value::as_str)
                    .unwrap_or("item")
                    .to_string(),
            });
            Ok(PlannedStep {
                id,
                action: binding.contract.id.clone(),
                action_version: binding.contract.version.clone(),
                action_digest: binding.contract.digest.clone(),
                action_contract: binding.artifact.clone(),
                provider: binding.provider.clone(),
                provider_declaration_digest: binding.provider_declaration_digest.clone(),
                handler_id: binding.handler_id.clone(),
                condition,
                input: step.get("input").cloned().unwrap_or_else(|| json!({})),
                for_each,
                idempotent: binding.idempotent,
                cooperative_cancellation: binding.cooperative_cancellation,
            })
        })
        .collect()
}

fn evaluate_vars(
    vars: Option<&Value>,
    bindings: &mut Value,
    now: DateTime<Utc>,
    timezone: Option<&str>,
) -> RuntimeResult<Value> {
    let Some(vars) = vars.and_then(Value::as_object) else {
        return Ok(json!({}));
    };
    let dependency_pattern = Regex::new(r"\bvars\.([A-Za-z_][A-Za-z0-9_]*)\b")
        .expect("variable dependency regex is valid");
    let known = vars.keys().cloned().collect::<BTreeSet<_>>();
    let mut dependencies = BTreeMap::<String, BTreeSet<String>>::new();
    for (name, value) in vars {
        let source = serde_json::to_string(value)?;
        dependencies.insert(
            name.clone(),
            dependency_pattern
                .captures_iter(&source)
                .filter_map(|capture| capture.get(1).map(|value| value.as_str().to_string()))
                .filter(|dependency| known.contains(dependency))
                .collect(),
        );
    }

    let mut result = Map::new();
    while result.len() < vars.len() {
        let ready = dependencies
            .iter()
            .filter(|(name, deps)| {
                !result.contains_key(*name)
                    && deps
                        .iter()
                        .all(|dependency| result.contains_key(dependency))
            })
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        if ready.is_empty() {
            return Err(RuntimeError::diagnostic(
                "circular_workflow_variable",
                "Workflow variables contain a dependency cycle.",
            ));
        }
        for name in ready {
            bindings["vars"] = Value::Object(result.clone());
            let value = evaluate_runtime_template(
                vars.get(&name).expect("ready variable must exist"),
                bindings,
                now,
                timezone,
            )
            .map_err(cel_errors)?;
            result.insert(name, value);
        }
    }
    Ok(Value::Object(result))
}

fn condition_matches(
    condition: Option<&Value>,
    bindings: &Value,
    now: DateTime<Utc>,
    timezone: Option<&str>,
) -> RuntimeResult<bool> {
    let Some(source) = condition
        .and_then(|value| value.get("$expr"))
        .and_then(Value::as_str)
    else {
        return Ok(true);
    };
    Ok(evaluate_runtime_expression(source, bindings, now, timezone)
        .map_err(|error| RuntimeError::diagnostic(error.code, error.message))?
        == Value::Bool(true))
}

fn evaluated_string(
    value: &Value,
    bindings: &Value,
    now: DateTime<Utc>,
    timezone: Option<&str>,
    label: &str,
) -> RuntimeResult<String> {
    let value = evaluate_runtime_template(value, bindings, now, timezone).map_err(cel_errors)?;
    value.as_str().map(str::to_string).ok_or_else(|| {
        RuntimeError::diagnostic(
            "workflow_expression_type",
            format!("Evaluated {label} must be a string."),
        )
    })
}

fn required_string<'a>(value: &'a Value, key: &str) -> RuntimeResult<&'a str> {
    value.get(key).and_then(Value::as_str).ok_or_else(|| {
        RuntimeError::diagnostic(
            "invalid_runtime_value",
            format!("Runtime value requires string {key}."),
        )
    })
}

fn parse_duration_ms(value: &str) -> RuntimeResult<u64> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .ok_or_else(|| RuntimeError::diagnostic("invalid_duration", value))?;
    let amount = value[..split]
        .parse::<u64>()
        .map_err(|_| RuntimeError::diagnostic("invalid_duration", value))?;
    let multiplier = match &value[split..] {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return Err(RuntimeError::diagnostic("invalid_duration", value)),
    };
    amount.checked_mul(multiplier).ok_or_else(|| {
        RuntimeError::diagnostic(
            "duration_out_of_range",
            "Duration exceeds u64 milliseconds.",
        )
    })
}

pub(crate) fn stable_id(prefix: &str, source: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(source.as_bytes()));
    format!("{prefix}_{}", &digest[..26])
}

fn cel_errors(errors: Vec<mdbase::v03::WorkflowCelError>) -> RuntimeError {
    let message = errors
        .into_iter()
        .map(|error| format!("{}: {}", error.code, error.message))
        .collect::<Vec<_>>()
        .join("; ");
    RuntimeError::diagnostic("workflow_expression_error", message)
}
