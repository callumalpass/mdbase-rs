use std::collections::BTreeMap;
use std::sync::OnceLock;

use jsonschema::{Draft, JSONSchema};
use serde_json::Value;

use crate::{RuntimeError, RuntimeResult};

const SOURCES: [(&str, &str); 10] = [
    (
        "runtime_workflow",
        include_str!("../schemas/runtime-workflow.schema.json"),
    ),
    (
        "runtime_policy",
        include_str!("../schemas/runtime-policy.schema.json"),
    ),
    (
        "runtime_provider_registration",
        include_str!("../schemas/runtime-provider-registration.schema.json"),
    ),
    (
        "runtime_capability_grant",
        include_str!("../schemas/runtime-capability-grant.schema.json"),
    ),
    (
        "runtime_run",
        include_str!("../schemas/runtime-run.schema.json"),
    ),
    (
        "runtime_action_attempt",
        include_str!("../schemas/runtime-action-attempt.schema.json"),
    ),
    (
        "runtime_checkpoint",
        include_str!("../schemas/runtime-checkpoint.schema.json"),
    ),
    (
        "runtime_timer",
        include_str!("../schemas/runtime-timer.schema.json"),
    ),
    (
        "runtime_diagnostic",
        include_str!("../schemas/runtime-diagnostic.schema.json"),
    ),
    (
        "runtime_dead_letter",
        include_str!("../schemas/runtime-dead-letter.schema.json"),
    ),
];

/// Validate an ordinary record from the standard Runtime 0.2 pack.
///
/// Core mdbase normally performs this validation before projection. This
/// helper lets hosts validate imported or materialized runtime records without
/// inventing a second record model.
pub fn validate_runtime_record(value: &Value) -> RuntimeResult<()> {
    let record_type = value.get("type").and_then(Value::as_str).ok_or_else(|| {
        RuntimeError::diagnostic(
            "invalid_runtime_record",
            "Runtime record requires a string type.",
        )
    })?;
    let schema = schemas().get(record_type).ok_or_else(|| {
        RuntimeError::diagnostic(
            "unknown_runtime_record_type",
            format!("{record_type} is not in the standard Runtime 0.2 pack."),
        )
    })?;
    schema.validate(value).map_err(|errors| {
        RuntimeError::diagnostic(
            "invalid_runtime_record",
            errors
                .map(|error| format!("{}: {error}", error.instance_path))
                .collect::<Vec<_>>()
                .join("; "),
        )
    })
}

fn schemas() -> &'static BTreeMap<&'static str, JSONSchema> {
    static SCHEMAS: OnceLock<BTreeMap<&'static str, JSONSchema>> = OnceLock::new();
    SCHEMAS.get_or_init(|| {
        SOURCES
            .into_iter()
            .map(|(record_type, source)| {
                let value: Value =
                    serde_json::from_str(source).expect("embedded runtime schema is JSON");
                let schema = JSONSchema::options()
                    .with_draft(Draft::Draft202012)
                    .compile(&value)
                    .expect("embedded runtime schema compiles");
                (record_type, schema)
            })
            .collect()
    })
}
