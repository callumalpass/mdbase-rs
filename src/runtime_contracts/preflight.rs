use std::collections::{BTreeMap, BTreeSet};

use semver::{Version, VersionReq};
use serde_json::Value;

use super::model::{
    ContractEntry, PreflightReport, RuntimeDiagnostic, ValidationResult, WorkflowResolution,
};
use super::registry::RuntimeRegistry;
use super::schemas::{validate_compiled, CanonicalValidators};

pub(crate) fn preflight(registry: &RuntimeRegistry) -> PreflightReport {
    let mut diagnostics = registry.diagnostics.clone();
    diagnostics.extend(validate_provider_listings(registry));
    diagnostics.extend(validate_policy_selectors(registry));
    let mut workflows = BTreeMap::new();

    for (id, entry) in &registry.workflows {
        let (resolution, workflow_diagnostics) = preflight_workflow(registry, id, entry);
        diagnostics.extend(workflow_diagnostics);
        workflows.insert(id.clone(), resolution);
    }
    sort_diagnostics(&mut diagnostics);
    diagnostics.dedup();
    PreflightReport {
        valid: !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == "error"),
        workflows,
        diagnostics,
    }
}

pub(crate) fn validate_event(
    validators: &CanonicalValidators,
    registry: &RuntimeRegistry,
    envelope: &Value,
) -> ValidationResult {
    let mut diagnostics = validators
        .validate_event_envelope_structure(envelope)
        .diagnostics;
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == "error")
    {
        return ValidationResult::new(diagnostics);
    }
    let event_id = envelope
        .get("type")
        .and_then(Value::as_str)
        .expect("canonical envelope requires string type");
    let Some(contract) = registry.events.get(event_id) else {
        diagnostics.push(unresolved("unresolved_event", event_id, None));
        return ValidationResult::new(diagnostics);
    };
    let expected_version = contract.contract.get("version").and_then(Value::as_u64);
    let actual_version = envelope.get("contract_version").and_then(Value::as_u64);
    if actual_version != expected_version {
        diagnostics.push(
            RuntimeDiagnostic::error(
                "runtime_contract_version_mismatch",
                format!(
                    "Event {event_id} declares contract version {}, but the registry provides {}.",
                    display_optional(actual_version),
                    display_optional(expected_version)
                ),
            )
            .for_id(event_id)
            .with_details(serde_json::json!({
                "expected": expected_version,
                "actual": actual_version,
            })),
        );
    }
    let expected_provider = contract.contract.get("provider").and_then(Value::as_str);
    let actual_provider = envelope.pointer("/source/provider").and_then(Value::as_str);
    if actual_provider.is_some() && actual_provider != expected_provider {
        diagnostics.push(
            RuntimeDiagnostic::error(
                "event_provider_mismatch",
                format!(
                    "Event {event_id} was delivered by provider {}, but its contract belongs to {}.",
                    display_optional(actual_provider),
                    display_optional(expected_provider)
                ),
            )
            .for_id(event_id),
        );
    }
    if let Some(schema) = registry.event_payloads.get(event_id) {
        diagnostics.extend(
            validate_compiled(
                schema,
                envelope.get("payload").unwrap_or(&Value::Null),
                "<event.payload>",
            )
            .diagnostics,
        );
    }
    ValidationResult::new(diagnostics)
}

pub(crate) fn validate_action_input(
    registry: &RuntimeRegistry,
    action_id: &str,
    input: &Value,
) -> ValidationResult {
    if !registry.actions.contains_key(action_id) {
        return ValidationResult::new(vec![unresolved("unresolved_action", action_id, None)]);
    }
    registry.action_inputs.get(action_id).map_or_else(
        || {
            ValidationResult::new(vec![RuntimeDiagnostic::error(
                "invalid_embedded_schema",
                format!("Action {action_id} has no compiled input schema."),
            )
            .for_id(action_id)])
        },
        |schema| validate_compiled(schema, input, &format!("<action:{action_id}.input>")),
    )
}

pub(crate) fn validate_action_output(
    registry: &RuntimeRegistry,
    action_id: &str,
    output: &Value,
) -> ValidationResult {
    let Some(action) = registry.actions.get(action_id) else {
        return ValidationResult::new(vec![unresolved("unresolved_action", action_id, None)]);
    };
    if action
        .contract
        .pointer("/schemas/output")
        .is_none_or(Value::is_null)
    {
        return ValidationResult::new(Vec::new());
    }
    registry.action_outputs.get(action_id).map_or_else(
        || {
            ValidationResult::new(vec![RuntimeDiagnostic::error(
                "invalid_embedded_schema",
                format!("Action {action_id} has no compiled output schema."),
            )
            .for_id(action_id)])
        },
        |schema| validate_compiled(schema, output, &format!("<action:{action_id}.output>")),
    )
}

pub(crate) fn preflight_action_contract(
    registry: &RuntimeRegistry,
    action: &Value,
    context: &Value,
) -> ValidationResult {
    let action_id = action
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("<pinned-action>");
    let mut diagnostics = validate_dispatch_context(context);
    let required = action_capabilities(action);
    if required.is_empty() {
        return ValidationResult::new(diagnostics);
    }
    let Some(policy) = selected_policy(registry) else {
        diagnostics.push(
            RuntimeDiagnostic::error(
                "policy_not_selected",
                format!(
                    "Pinned action {action_id} has effects but no unique runtime policy is selected."
                ),
            )
            .for_id(action_id),
        );
        return ValidationResult::new(diagnostics);
    };
    for capability in required {
        if policy_mode(policy, &capability) != Some("allow") {
            diagnostics.push(
                RuntimeDiagnostic::error(
                    "capability_denied",
                    format!("Selected policy does not explicitly allow capability {capability}."),
                )
                .for_id(capability),
            );
        }
    }
    ValidationResult::new(diagnostics)
}

/// Pure dispatch preflight. This does not grant access or invoke a handler;
/// hosts such as mdbase-connect remain the final authorization boundary.
pub(crate) fn preflight_action(
    registry: &RuntimeRegistry,
    action_id: &str,
    context: &Value,
) -> ValidationResult {
    let Some(action) = registry.actions.get(action_id) else {
        return ValidationResult::new(vec![unresolved("unresolved_action", action_id, None)]);
    };
    let mut diagnostics = validate_dispatch_context(context);
    let required = action_capabilities(&action.contract);
    if required.is_empty() {
        return ValidationResult::new(diagnostics);
    }
    let Some(policy) = selected_policy(registry) else {
        diagnostics.push(
            RuntimeDiagnostic::error(
                "policy_not_selected",
                format!("Action {action_id} has effects but no unique runtime policy is selected."),
            )
            .for_id(action_id),
        );
        return ValidationResult::new(diagnostics);
    };
    for capability in required {
        if policy
            .contract
            .pointer(&format!(
                "/capabilities/{}/mode",
                escape_pointer(&capability)
            ))
            .and_then(Value::as_str)
            != Some("allow")
        {
            diagnostics.push(
                RuntimeDiagnostic::error(
                    "capability_denied",
                    format!("Selected policy does not explicitly allow capability {capability}."),
                )
                .for_id(capability),
            );
        }
    }
    ValidationResult::new(diagnostics)
}

fn preflight_workflow(
    registry: &RuntimeRegistry,
    workflow_id: &str,
    entry: &ContractEntry,
) -> (WorkflowResolution, Vec<RuntimeDiagnostic>) {
    let workflow = &entry.contract;
    let mut diagnostics = Vec::new();
    let mut resolution = WorkflowResolution {
        workflow: workflow_id.to_string(),
        ..WorkflowResolution::default()
    };
    duplicates(
        workflow.get("triggers"),
        "duplicate_trigger",
        workflow_id,
        &mut diagnostics,
    );
    duplicates(
        workflow.get("steps"),
        "duplicate_step",
        workflow_id,
        &mut diagnostics,
    );
    validate_workflow_expressions(entry, workflow_id, &mut diagnostics);
    resolve_requires(
        registry,
        workflow.get("requires"),
        workflow_id,
        &mut resolution,
        &mut diagnostics,
    );

    for trigger in array(workflow.get("triggers")) {
        let Some(event_id) = trigger.get("event").and_then(Value::as_str) else {
            continue;
        };
        if let Some(event) = registry.events.get(event_id) {
            resolution.events.insert(event_id.to_string());
            if let Some(provider) = event.contract.get("provider").and_then(Value::as_str) {
                resolution.providers.insert(provider.to_string());
            }
        } else {
            diagnostics.push(unresolved("unresolved_event", event_id, Some(workflow_id)));
        }
    }

    for step in array(workflow.get("steps")) {
        resolve_requires(
            registry,
            step.get("requires"),
            workflow_id,
            &mut resolution,
            &mut diagnostics,
        );
        let Some(action_id) = step.get("action").and_then(Value::as_str) else {
            continue;
        };
        let Some(action) = registry.actions.get(action_id) else {
            diagnostics.push(unresolved(
                "unresolved_action",
                action_id,
                Some(workflow_id),
            ));
            continue;
        };
        resolution.actions.insert(action_id.to_string());
        if let Some(provider) = action.contract.get("provider").and_then(Value::as_str) {
            resolution.providers.insert(provider.to_string());
        }
        resolve_requires(
            registry,
            action.contract.get("requires"),
            action_id,
            &mut resolution,
            &mut diagnostics,
        );
        for capability in action_capabilities(&action.contract) {
            resolve_capability(
                registry,
                &capability,
                action_id,
                &mut resolution,
                &mut diagnostics,
            );
        }
        for event_id in strings(action.contract.get("emits")) {
            if registry.events.contains_key(event_id) {
                resolution.events.insert(event_id.to_string());
            } else {
                diagnostics.push(unresolved(
                    "unresolved_emitted_event",
                    event_id,
                    Some(action_id),
                ));
            }
        }
    }

    let required_capabilities = resolution.capabilities.clone();
    if !required_capabilities.is_empty() {
        match selected_policy(registry) {
            Some(policy) => {
                for capability in required_capabilities {
                    if policy_mode(policy, &capability) == Some("deny") {
                        diagnostics.push(
                            RuntimeDiagnostic::error(
                                "capability_denied",
                                format!("Capability {capability} is denied by runtime policy."),
                            )
                            .for_id(capability),
                        );
                    }
                }
            }
            None => diagnostics.push(
                RuntimeDiagnostic::error(
                    "policy_not_selected",
                    format!(
                        "Workflow {workflow_id} can dispatch effectful actions but no unique runtime policy is selected."
                    ),
                )
                .for_id(workflow_id),
            ),
        }
    }

    if workflow
        .pointer("/run/execution/mode")
        .and_then(Value::as_str)
        == Some("single_executor")
    {
        resolution.executor = selected_executor(registry, workflow_id);
        if resolution.executor.is_none() {
            diagnostics.push(
                RuntimeDiagnostic::error(
                    "executor_not_selected",
                    format!(
                        "Workflow {workflow_id} uses single_executor but no policy selects an executor."
                    ),
                )
                .for_id(workflow_id),
            );
        }
    } else {
        resolution.executor = selected_executor(registry, workflow_id);
    }
    resolution.valid = !diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == "error");
    (resolution, diagnostics)
}

fn validate_workflow_expressions(
    entry: &ContractEntry,
    workflow_id: &str,
    diagnostics: &mut Vec<RuntimeDiagnostic>,
) {
    let workflow = &entry.contract;
    for (path, value) in [
        ("/vars", workflow.get("vars")),
        ("/if", workflow.get("if")),
        (
            "/run/idempotency/key",
            workflow.pointer("/run/idempotency/key"),
        ),
        (
            "/run/concurrency/group",
            workflow.pointer("/run/concurrency/group"),
        ),
    ] {
        if let Some(value) = value {
            validate_expression_value(entry, workflow_id, value, path, diagnostics);
        }
    }
    for (index, trigger) in array(workflow.get("triggers")).enumerate() {
        if let Some(value) = trigger.get("if") {
            validate_expression_value(
                entry,
                workflow_id,
                value,
                &format!("/triggers/{index}/if"),
                diagnostics,
            );
        }
    }
    for (index, step) in array(workflow.get("steps")).enumerate() {
        for (suffix, value) in [
            ("/if", step.get("if")),
            ("/input", step.get("input")),
            ("/for_each/items", step.pointer("/for_each/items")),
        ] {
            if let Some(value) = value {
                validate_expression_value(
                    entry,
                    workflow_id,
                    value,
                    &format!("/steps/{index}{suffix}"),
                    diagnostics,
                );
            }
        }
    }
}

fn validate_expression_value(
    entry: &ContractEntry,
    workflow_id: &str,
    value: &Value,
    path: &str,
    diagnostics: &mut Vec<RuntimeDiagnostic>,
) {
    match value {
        Value::Object(object) if object.len() == 1 && object.contains_key("$expr") => {
            let Some(source) = object.get("$expr").and_then(Value::as_str) else {
                return;
            };
            if let Err(error) = crate::v03::validate_runtime_expression(source) {
                let expression_path = format!("{path}/$expr");
                let source_path = entry
                    .origins
                    .first()
                    .map(|origin| format!("{}#{expression_path}", origin.location))
                    .unwrap_or_else(|| expression_path.clone());
                diagnostics.push(
                    RuntimeDiagnostic::error(
                        error.code,
                        format!(
                            "Workflow {workflow_id} expression at {expression_path} failed to compile: {}",
                            error.message
                        ),
                    )
                    .for_id(workflow_id)
                    .at_path(source_path),
                );
            }
        }
        Value::Object(object) => {
            for (key, nested) in object {
                validate_expression_value(
                    entry,
                    workflow_id,
                    nested,
                    &format!("{path}/{}", escape_pointer(key)),
                    diagnostics,
                );
            }
        }
        Value::Array(values) => {
            for (index, nested) in values.iter().enumerate() {
                validate_expression_value(
                    entry,
                    workflow_id,
                    nested,
                    &format!("{path}/{index}"),
                    diagnostics,
                );
            }
        }
        _ => {}
    }
}

fn validate_provider_listings(registry: &RuntimeRegistry) -> Vec<RuntimeDiagnostic> {
    let mut diagnostics = Vec::new();
    for (provider_id, provider) in &registry.providers {
        for (list, contracts) in [
            ("events", &registry.events),
            ("actions", &registry.actions),
            ("workflows", &registry.workflows),
        ] {
            for id in strings(provider.contract.pointer(&format!("/contracts/{list}"))) {
                match contracts.get(id) {
                    None => diagnostics.push(unresolved(
                        &format!("unresolved_provider_{}", list.trim_end_matches('s')),
                        id,
                        Some(provider_id),
                    )),
                    Some(contract)
                        if matches!(list, "events" | "actions")
                            && contract.contract.get("provider").and_then(Value::as_str)
                                != Some(provider_id.as_str()) =>
                    {
                        diagnostics.push(
                            RuntimeDiagnostic::error(
                                "provider_contract_mismatch",
                                format!(
                                    "Provider {provider_id} advertises {id}, but the contract names another provider."
                                ),
                            )
                            .for_id(id),
                        );
                    }
                    Some(_) => {}
                }
            }
        }
        for capability in strings(provider.contract.pointer("/contracts/capabilities")) {
            if !registry.capability_ids.contains(capability) {
                diagnostics.push(unresolved(
                    "unresolved_provider_capability",
                    capability,
                    Some(provider_id),
                ));
            }
        }

        for (kind, contracts) in [("event", &registry.events), ("action", &registry.actions)] {
            let advertised = provider.contract.pointer(&format!("/contracts/{kind}s"));
            let advertised = strings(advertised).collect::<BTreeSet<_>>();
            for (id, contract) in contracts {
                if contract.contract.get("provider").and_then(Value::as_str)
                    == Some(provider_id.as_str())
                    && !advertised.contains(id.as_str())
                {
                    diagnostics.push(
                        RuntimeDiagnostic::error(
                            "provider_contract_mismatch",
                            format!(
                                "Provider {provider_id} does not advertise its {kind} contract {id}."
                            ),
                        )
                        .for_id(id),
                    );
                }
            }
        }
    }
    diagnostics
}

fn validate_policy_selectors(registry: &RuntimeRegistry) -> Vec<RuntimeDiagnostic> {
    let mut diagnostics = Vec::new();
    if registry.selected_policy_ids.len() > 1 {
        diagnostics.push(RuntimeDiagnostic::error(
            "policy_not_selected",
            "More than one runtime policy is selected.",
        ));
    }
    for (policy_id, policy) in &registry.policies {
        if policy.contract.get("enabled") == Some(&Value::Bool(false)) {
            continue;
        }
        if let Some(workflows) = policy
            .contract
            .pointer("/executors/workflows")
            .and_then(Value::as_object)
        {
            for workflow_id in workflows.keys() {
                if !registry.workflows.contains_key(workflow_id) {
                    diagnostics.push(unresolved(
                        "unresolved_policy_workflow",
                        workflow_id,
                        Some(policy_id),
                    ));
                }
            }
        }
    }
    diagnostics
}

fn resolve_requires(
    registry: &RuntimeRegistry,
    requires: Option<&Value>,
    source: &str,
    resolution: &mut WorkflowResolution,
    diagnostics: &mut Vec<RuntimeDiagnostic>,
) {
    for capability in strings(requires.and_then(|value| value.get("capabilities"))) {
        resolve_capability(registry, capability, source, resolution, diagnostics);
    }
    for requirement in array(requires.and_then(|value| value.get("providers"))) {
        let (provider_id, range) = match requirement {
            Value::String(id) => (id.as_str(), None),
            Value::Object(value) => (
                value.get("id").and_then(Value::as_str).unwrap_or(""),
                value.get("version").and_then(Value::as_str),
            ),
            _ => continue,
        };
        if !registry.provider_ids.contains(provider_id) {
            diagnostics.push(unresolved("unresolved_provider", provider_id, Some(source)));
            continue;
        }
        resolution.providers.insert(provider_id.to_string());
        if let Some(range) = range {
            let actual = registry
                .providers
                .get(provider_id)
                .and_then(|provider| provider.contract.get("provider_version"))
                .and_then(Value::as_str);
            if !version_satisfies(actual, range) {
                diagnostics.push(
                    RuntimeDiagnostic::error(
                        "provider_version_mismatch",
                        format!("Provider {provider_id} does not satisfy {range}."),
                    )
                    .for_id(provider_id)
                    .with_details(serde_json::json!({
                        "required": range,
                        "actual": actual,
                    })),
                );
            }
        }
    }
}

fn resolve_capability(
    registry: &RuntimeRegistry,
    capability: &str,
    source: &str,
    resolution: &mut WorkflowResolution,
    diagnostics: &mut Vec<RuntimeDiagnostic>,
) {
    if registry.capability_ids.contains(capability) {
        resolution.capabilities.insert(capability.to_string());
    } else {
        diagnostics.push(unresolved(
            "unresolved_capability",
            capability,
            Some(source),
        ));
    }
}

fn duplicates(
    values: Option<&Value>,
    code: &str,
    workflow: &str,
    diagnostics: &mut Vec<RuntimeDiagnostic>,
) {
    let mut seen = BTreeSet::new();
    for value in array(values) {
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            continue;
        };
        if !seen.insert(id) {
            diagnostics.push(
                RuntimeDiagnostic::error(
                    code,
                    format!("{id} is duplicated in workflow {workflow}."),
                )
                .for_id(id),
            );
        }
    }
}

fn selected_policy(registry: &RuntimeRegistry) -> Option<&ContractEntry> {
    registry.selected_policy()
}

fn selected_executor(registry: &RuntimeRegistry, workflow_id: &str) -> Option<String> {
    let policy = selected_policy(registry)?;
    policy
        .contract
        .pointer(&format!(
            "/executors/workflows/{}",
            escape_pointer(workflow_id)
        ))
        .or_else(|| policy.contract.pointer("/executors/default"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn policy_mode<'a>(policy: &'a ContractEntry, capability: &str) -> Option<&'a str> {
    policy
        .contract
        .pointer(&format!(
            "/capabilities/{}/mode",
            escape_pointer(capability)
        ))
        .and_then(Value::as_str)
}

fn action_capabilities(action: &Value) -> BTreeSet<String> {
    strings(action.pointer("/requires/capabilities"))
        .chain(strings(action.get("effects")))
        .map(str::to_string)
        .collect()
}

fn validate_dispatch_context(context: &Value) -> Vec<RuntimeDiagnostic> {
    let required = [
        "/actor/id",
        "/actor/kind",
        "/run_id",
        "/correlation_id",
        "/executor",
    ];
    let missing = required
        .into_iter()
        .filter(|pointer| {
            context
                .pointer(pointer)
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        })
        .collect::<Vec<_>>();
    let origin_missing = !context
        .get("origin")
        .is_some_and(|origin| origin.is_object());
    if missing.is_empty() && !origin_missing {
        Vec::new()
    } else {
        vec![RuntimeDiagnostic::error(
            "invalid_dispatch_context",
            "Dispatch context is missing required provenance fields.",
        )
        .with_details(serde_json::json!({
            "missing": missing,
            "origin_missing": origin_missing,
        }))]
    }
}

fn version_satisfies(actual: Option<&str>, required: &str) -> bool {
    let (Some(actual), Ok(requirement)) = (actual, VersionReq::parse(required)) else {
        return false;
    };
    Version::parse(actual)
        .map(|version| requirement.matches(&version))
        .unwrap_or(false)
}

fn unresolved(code: &str, id: &str, source: Option<&str>) -> RuntimeDiagnostic {
    RuntimeDiagnostic::error(
        code,
        format!(
            "{id} could not be resolved{}.",
            source
                .map(|source| format!(" from {source}"))
                .unwrap_or_default()
        ),
    )
    .for_id(id)
}

fn array(value: Option<&Value>) -> impl Iterator<Item = &Value> {
    value.and_then(Value::as_array).into_iter().flatten()
}

fn strings(value: Option<&Value>) -> impl Iterator<Item = &str> {
    array(value).filter_map(Value::as_str)
}

fn escape_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn display_optional<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "<missing>".to_string(), |value| value.to_string())
}

fn sort_diagnostics(diagnostics: &mut [RuntimeDiagnostic]) {
    diagnostics.sort_by(|left, right| {
        (
            left.path.as_deref().unwrap_or(""),
            left.id.as_deref().unwrap_or(""),
            left.code.as_str(),
            left.field.as_deref().unwrap_or(""),
            left.message.as_str(),
        )
            .cmp(&(
                right.path.as_deref().unwrap_or(""),
                right.id.as_deref().unwrap_or(""),
                right.code.as_str(),
                right.field.as_deref().unwrap_or(""),
                right.message.as_str(),
            ))
    });
}
