use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, TimeDelta, Utc};
use mdbase::runtime_contracts::RuntimeRegistry;
use mdbase::v03::{evaluate_runtime_expression, evaluate_runtime_template};
use regex::Regex;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::engine::RuntimeConfig;
use crate::error::{RuntimeError, RuntimeResult};
use crate::model::{ConcurrencyPolicy, OnError, PlannedForEach, PlannedRun, PlannedStep};

pub(crate) fn plan_event(
    registry: &RuntimeRegistry,
    workflow_ids: &[String],
    event: &Value,
    now: DateTime<Utc>,
    config: &RuntimeConfig,
) -> RuntimeResult<Vec<PlannedRun>> {
    if !registry.valid() {
        return Err(RuntimeError::diagnostic(
            "runtime_registry_invalid",
            "The effective runtime registry contains errors.",
        ));
    }
    let event_id = required_string(event, "id")?;
    let event_type = required_string(event, "type")?;
    let registry_revision = registry.revision();
    let selected_policy = registry.selected_policy();
    let policy_revision = selected_policy.map(|entry| revision(&entry.contract));
    let selected_policy_value = selected_policy.map(|entry| &entry.contract);
    let mut plans = Vec::new();

    for workflow_id in workflow_ids {
        let Some(entry) = registry.workflows.get(workflow_id) else {
            continue;
        };
        let workflow = &entry.contract;
        if workflow.get("enabled") == Some(&Value::Bool(false)) {
            continue;
        }
        let workflow_version =
            workflow
                .get("version")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    RuntimeError::diagnostic(
                        "invalid_workflow",
                        format!("Workflow {workflow_id} has no integer version."),
                    )
                })?;
        let mode = workflow
            .pointer("/run/execution/mode")
            .and_then(Value::as_str)
            .unwrap_or("single_executor");
        let selected_executor = selected_executor(selected_policy_value, workflow_id);
        if mode == "single_executor"
            && selected_executor.as_deref() != Some(config.executor_id.as_str())
        {
            continue;
        }

        let triggers = workflow
            .get("triggers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for trigger in triggers {
            if trigger.get("event").and_then(Value::as_str) != Some(event_type) {
                continue;
            }
            let trigger_id = required_string(&trigger, "id")?;
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
            let idempotency_scope = if mode == "broadcast" {
                format!("{}:{}", config.executor_id, workflow_id)
            } else {
                config.executor_id.clone()
            };
            let concurrency_group = match workflow.pointer("/run/concurrency/group") {
                Some(value) => evaluated_string(
                    value,
                    &bindings,
                    now,
                    config.timezone.as_deref(),
                    "concurrency group",
                )?,
                None => workflow_id.clone(),
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
            let workflow_timeout = workflow
                .pointer("/run/limits/timeout")
                .and_then(Value::as_str)
                .map(parse_duration_ms)
                .transpose()?;
            let policy_timeout = selected_policy_value
                .and_then(|policy| policy.pointer("/limits/workflow_timeout"))
                .and_then(Value::as_str)
                .map(parse_duration_ms)
                .transpose()?;
            let timeout_ms = match (workflow_timeout, policy_timeout) {
                (Some(workflow), Some(policy)) => Some(workflow.min(policy)),
                (workflow, policy) => workflow.or(policy),
            };
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
            let steps = plan_steps(registry, workflow, workflow_id)?;
            let run_id = stable_id(
                "run",
                &format!(
                    "{}:{workflow_id}:{trigger_id}:{event_id}:{}",
                    config.executor_id, idempotency_key
                ),
            );
            plans.push(PlannedRun {
                id: run_id,
                workflow: workflow_id.clone(),
                workflow_version,
                workflow_revision: revision(workflow),
                registry_revision: registry_revision.clone(),
                policy_revision: policy_revision.clone(),
                trigger: trigger_id.to_string(),
                event_id: event_id.to_string(),
                event_type: event_type.to_string(),
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
    }
    Ok(plans)
}

fn plan_steps(
    registry: &RuntimeRegistry,
    workflow: &Value,
    workflow_id: &str,
) -> RuntimeResult<Vec<PlannedStep>> {
    workflow
        .get("steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|step| {
            let id = required_string(step, "id")?.to_string();
            let action = required_string(step, "action")?.to_string();
            let contract = registry.actions.get(&action).ok_or_else(|| {
                RuntimeError::diagnostic(
                    "unresolved_action",
                    format!("Workflow {workflow_id} references unknown action {action}."),
                )
            })?;
            let action_version = contract
                .contract
                .get("version")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    RuntimeError::diagnostic(
                        "invalid_action",
                        format!("Action {action} has no integer version."),
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
                action: action.clone(),
                action_version,
                action_revision: revision(&contract.contract),
                action_contract: contract.contract.clone(),
                condition,
                input: step.get("input").cloned().unwrap_or_else(|| json!({})),
                for_each,
                idempotent: contract
                    .contract
                    .pointer("/dispatch/idempotency")
                    .and_then(Value::as_str)
                    == Some("invocation_id"),
                cooperative_cancellation: contract
                    .contract
                    .pointer("/dispatch/cancellation")
                    .and_then(Value::as_str)
                    == Some("cooperative"),
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

fn selected_executor(policy: Option<&Value>, workflow_id: &str) -> Option<String> {
    policy
        .and_then(|policy| {
            policy
                .pointer(&format!(
                    "/executors/workflows/{}",
                    workflow_id.replace('~', "~0").replace('/', "~1")
                ))
                .and_then(Value::as_str)
                .or_else(|| policy.pointer("/executors/default").and_then(Value::as_str))
        })
        .map(str::to_string)
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

pub(crate) fn revision(value: &Value) -> String {
    let canonical = serde_json::to_vec(value).expect("JSON value serialization cannot fail");
    format!("sha256:{:x}", Sha256::digest(canonical))
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
