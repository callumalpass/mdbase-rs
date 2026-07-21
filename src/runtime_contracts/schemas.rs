use std::collections::BTreeMap;
use std::sync::Arc;

use jsonschema::error::{ValidationError, ValidationErrorKind};
use jsonschema::{Draft, JSONSchema};
use serde_json::Value;

use super::model::{ContractDocument, ContractKind, RuntimeDiagnostic, ValidationResult};

const ACTION: &str = include_str!("../../schemas/v0.3/runtime/action.schema.json");
const CAPABILITY: &str = include_str!("../../schemas/v0.3/runtime/capability.schema.json");
const CHECKPOINT: &str = include_str!("../../schemas/v0.3/runtime/checkpoint.schema.json");
const DIAGNOSTIC: &str = include_str!("../../schemas/v0.3/runtime/diagnostic.schema.json");
const EVENT_ENVELOPE: &str = include_str!("../../schemas/v0.3/runtime/event-envelope.schema.json");
const EVENT: &str = include_str!("../../schemas/v0.3/runtime/event.schema.json");
const PROVIDER: &str = include_str!("../../schemas/v0.3/runtime/provider.schema.json");
const RUN: &str = include_str!("../../schemas/v0.3/runtime/run.schema.json");
const RUNTIME_POLICY: &str = include_str!("../../schemas/v0.3/runtime/runtime-policy.schema.json");
const WORKFLOW: &str = include_str!("../../schemas/v0.3/runtime/workflow.schema.json");

pub(crate) struct CanonicalValidators {
    contracts: BTreeMap<ContractKind, JSONSchema>,
    event_envelope: JSONSchema,
}

#[derive(Debug, Default)]
pub(crate) struct EmbeddedSchemas {
    pub action_input: Option<Arc<JSONSchema>>,
    pub action_output: Option<Arc<JSONSchema>>,
    pub event_payload: Option<Arc<JSONSchema>>,
}

impl CanonicalValidators {
    pub fn new() -> Result<Self, String> {
        let mut contracts = BTreeMap::new();
        for (kind, source) in [
            (ContractKind::Provider, PROVIDER),
            (ContractKind::Action, ACTION),
            (ContractKind::Event, EVENT),
            (ContractKind::Capability, CAPABILITY),
            (ContractKind::Workflow, WORKFLOW),
            (ContractKind::RuntimePolicy, RUNTIME_POLICY),
            (ContractKind::RuntimeRun, RUN),
            (ContractKind::RuntimeCheckpoint, CHECKPOINT),
            (ContractKind::RuntimeDiagnostic, DIAGNOSTIC),
        ] {
            let schema = parse_schema(kind.as_str(), source)?;
            contracts.insert(
                kind,
                compile(&schema)
                    .map_err(|error| format!("{} schema is invalid: {error}", kind.as_str()))?,
            );
        }
        let envelope = parse_schema("event-envelope", EVENT_ENVELOPE)?;
        let event_envelope = compile(&envelope)
            .map_err(|error| format!("event-envelope schema is invalid: {error}"))?;
        Ok(Self {
            contracts,
            event_envelope,
        })
    }

    pub fn validate_contract(&self, document: &ContractDocument) -> ValidationResult {
        self.prepare_contract(document).0
    }

    pub fn prepare_contract(
        &self,
        document: &ContractDocument,
    ) -> (ValidationResult, EmbeddedSchemas) {
        let Some(kind) = document.kind() else {
            return (
                ValidationResult::new(vec![RuntimeDiagnostic::error(
                    "invalid_contract_type",
                    "Runtime contract type is missing or unknown.",
                )
                .at_path(&document.path)]),
                EmbeddedSchemas::default(),
            );
        };
        let mut diagnostics = self
            .contracts
            .get(&kind)
            .map(|schema| validate(schema, &document.frontmatter, &document.path))
            .unwrap_or_else(|| {
                vec![RuntimeDiagnostic::error(
                    "unknown_schema",
                    format!("No canonical schema is registered for {}.", kind.as_str()),
                )
                .at_path(&document.path)]
            });
        let embedded = if kind.is_registry_contract() {
            let (embedded, embedded_diagnostics) = self.compile_embedded(document);
            diagnostics.extend(embedded_diagnostics);
            embedded
        } else {
            EmbeddedSchemas::default()
        };
        (ValidationResult::new(diagnostics), embedded)
    }

    pub fn compile_embedded(
        &self,
        document: &ContractDocument,
    ) -> (EmbeddedSchemas, Vec<RuntimeDiagnostic>) {
        let mut compiled = EmbeddedSchemas::default();
        let mut diagnostics = Vec::new();
        let Some(kind) = document.kind() else {
            return (compiled, diagnostics);
        };
        match kind {
            ContractKind::Action => {
                compile_slot(
                    document.frontmatter.pointer("/schemas/input"),
                    &format!("{}#/schemas/input", document.path),
                    &mut compiled.action_input,
                    &mut diagnostics,
                );
                let output = document.frontmatter.pointer("/schemas/output");
                if !matches!(output, None | Some(Value::Null)) {
                    compile_slot(
                        output,
                        &format!("{}#/schemas/output", document.path),
                        &mut compiled.action_output,
                        &mut diagnostics,
                    );
                }
            }
            ContractKind::Event => compile_slot(
                document.frontmatter.pointer("/schemas/payload"),
                &format!("{}#/schemas/payload", document.path),
                &mut compiled.event_payload,
                &mut diagnostics,
            ),
            _ => {}
        }
        (compiled, diagnostics)
    }

    pub fn validate_event_envelope_structure(&self, envelope: &Value) -> ValidationResult {
        ValidationResult::new(validate(&self.event_envelope, envelope, "<event>"))
    }
}

pub(crate) fn validate_compiled(
    schema: &JSONSchema,
    value: &Value,
    path: &str,
) -> ValidationResult {
    ValidationResult::new(validate(schema, value, path))
}

fn compile_slot(
    schema: Option<&Value>,
    path: &str,
    slot: &mut Option<Arc<JSONSchema>>,
    diagnostics: &mut Vec<RuntimeDiagnostic>,
) {
    let Some(schema) = schema else {
        return;
    };
    if let Some(reference) = external_reference(schema) {
        diagnostics.push(
            RuntimeDiagnostic::error(
                "invalid_embedded_schema",
                format!("External JSON Schema reference is not supported: {reference}"),
            )
            .at_path(path),
        );
        return;
    }
    match compile(schema) {
        Ok(value) => *slot = Some(Arc::new(value)),
        Err(error) => diagnostics
            .push(RuntimeDiagnostic::error("invalid_embedded_schema", error).at_path(path)),
    }
}

fn parse_schema(name: &str, source: &str) -> Result<Value, String> {
    serde_json::from_str(source).map_err(|error| format!("{name} schema is invalid JSON: {error}"))
}

fn compile(schema: &Value) -> Result<JSONSchema, String> {
    JSONSchema::options()
        .with_draft(Draft::Draft202012)
        .should_validate_formats(true)
        .compile(schema)
        .map_err(|error| error.to_string())
}

fn validate(schema: &JSONSchema, value: &Value, path: &str) -> Vec<RuntimeDiagnostic> {
    match schema.validate(value) {
        Ok(()) => Vec::new(),
        Err(errors) => errors
            .map(|error| validation_diagnostic(error, path))
            .collect(),
    }
}

fn validation_diagnostic(error: ValidationError<'_>, path: &str) -> RuntimeDiagnostic {
    let field = match &error.kind {
        ValidationErrorKind::Required { property } => property.as_str().map(str::to_string),
        ValidationErrorKind::AdditionalProperties { unexpected }
        | ValidationErrorKind::UnevaluatedProperties { unexpected } => unexpected.first().cloned(),
        _ => {
            let pointer = error.instance_path.to_string();
            (!pointer.is_empty()).then_some(pointer)
        }
    };
    RuntimeDiagnostic {
        severity: "error".to_string(),
        code: diagnostic_code(&error.kind).to_string(),
        message: error.to_string(),
        path: Some(path.to_string()),
        id: None,
        field,
        details: Some(serde_json::json!({
            "instance_path": error.instance_path.to_string(),
            "schema_path": error.schema_path.to_string(),
        })),
    }
}

fn diagnostic_code(kind: &ValidationErrorKind) -> &'static str {
    match kind {
        ValidationErrorKind::Required { .. } => "schema_required",
        ValidationErrorKind::AdditionalProperties { .. } => "schema_additional_properties",
        ValidationErrorKind::UnevaluatedProperties { .. } => "schema_unevaluated_properties",
        ValidationErrorKind::Type { .. } => "schema_type",
        ValidationErrorKind::Constant { .. } => "schema_const",
        ValidationErrorKind::Enum { .. } => "schema_enum",
        ValidationErrorKind::Pattern { .. } => "schema_pattern",
        ValidationErrorKind::MinLength { .. } => "schema_min_length",
        ValidationErrorKind::MaxLength { .. } => "schema_max_length",
        ValidationErrorKind::Minimum { .. } => "schema_minimum",
        ValidationErrorKind::Maximum { .. } => "schema_maximum",
        ValidationErrorKind::MultipleOf { .. } => "schema_multiple_of",
        ValidationErrorKind::ExclusiveMinimum { .. } => "schema_exclusive_minimum",
        ValidationErrorKind::ExclusiveMaximum { .. } => "schema_exclusive_maximum",
        ValidationErrorKind::MinItems { .. } => "schema_min_items",
        ValidationErrorKind::MaxItems { .. } => "schema_max_items",
        ValidationErrorKind::UniqueItems => "schema_unique_items",
        ValidationErrorKind::OneOfMultipleValid | ValidationErrorKind::OneOfNotValid => {
            "schema_one_of"
        }
        ValidationErrorKind::AnyOf => "schema_any_of",
        ValidationErrorKind::Not { .. } => "schema_not",
        ValidationErrorKind::Format { .. } => "format_invalid",
        _ => "schema_invalid",
    }
}

fn external_reference(value: &Value) -> Option<&str> {
    match value {
        Value::Array(values) => values.iter().find_map(external_reference),
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                if !reference.starts_with('#') {
                    return Some(reference);
                }
            }
            object.values().find_map(external_reference)
        }
        _ => None,
    }
}
