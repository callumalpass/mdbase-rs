//! Portable v0.3 expression host bindings built on the shared evaluator.

use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use super::{Diagnostic, OperationResult};
use crate::expressions::ast::Expr;
use crate::expressions::evaluator::{
    evaluate_with_limits, EvalContext, EvaluationClock, NoteNamespaceSource,
};
use crate::expressions::parser::Parser;
use crate::Collection;

pub(crate) const MAX_SOURCE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_AST_DEPTH: u32 = 128;
pub(crate) const MAX_EVALUATION_STEPS: u64 = 1_000_000;

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct CelFailure {
    pub code: String,
    pub message: String,
}

/// Stable error returned by the provider-neutral workflow CEL facade.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct WorkflowCelError {
    pub code: String,
    pub message: String,
}

/// Compile a workflow expression without evaluating it.
pub fn validate_runtime_expression(source: &str) -> Result<(), WorkflowCelError> {
    compile(source).map(|_| ()).map_err(WorkflowCelError::from)
}

/// Evaluate one workflow CEL expression with an injected operation clock.
pub fn evaluate_runtime_expression(
    source: &str,
    bindings: &Value,
    now: DateTime<Utc>,
    timezone: Option<&str>,
) -> Result<Value, WorkflowCelError> {
    let expression = compile(source).map_err(WorkflowCelError::from)?;
    let mut context = EvalContext::empty();
    context.frontmatter = bindings.clone();
    context.string_concat = false;
    let clock = EvaluationClock::from_utc(now, timezone).map_err(|message| WorkflowCelError {
        code: "invalid_timezone".to_string(),
        message,
    })?;
    evaluate_compiled(&expression, &context, &clock).map_err(WorkflowCelError::from)
}

/// Recursively evaluate canonical `{ "$expr": "..." }` workflow values with
/// an injected operation clock.
pub fn evaluate_runtime_template(
    template: &Value,
    bindings: &Value,
    now: DateTime<Utc>,
    timezone: Option<&str>,
) -> Result<Value, Vec<WorkflowCelError>> {
    let mut context = EvalContext::empty();
    context.frontmatter = bindings.clone();
    context.string_concat = false;
    let clock = EvaluationClock::from_utc(now, timezone).map_err(|message| {
        vec![WorkflowCelError {
            code: "invalid_timezone".to_string(),
            message,
        }]
    })?;
    let mut diagnostics = Vec::new();
    let value = evaluate_runtime_template_value(template, &context, &clock, &mut diagnostics);
    if diagnostics.is_empty() {
        Ok(value)
    } else {
        Err(diagnostics)
    }
}

impl From<CelFailure> for WorkflowCelError {
    fn from(value: CelFailure) -> Self {
        Self {
            code: value.code,
            message: value.message,
        }
    }
}

pub(crate) fn compile(source: &str) -> Result<Expr, CelFailure> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(CelFailure {
            code: "expression_source_limit_exceeded".to_string(),
            message: format!(
                "CEL source is {} bytes; the configured limit is {MAX_SOURCE_BYTES} bytes.",
                source.len()
            ),
        });
    }
    Parser::parse_with_max_depth(source, MAX_AST_DEPTH).map_err(|message| CelFailure {
        code: if message == "expression_depth_exceeded" {
            "expression_depth_exceeded".to_string()
        } else {
            "expression_compile_error".to_string()
        },
        message,
    })
}

pub(crate) fn operation_clock(timezone: Option<&str>) -> Result<EvaluationClock, CelFailure> {
    EvaluationClock::capture(timezone).map_err(|message| CelFailure {
        code: "invalid_timezone".to_string(),
        message,
    })
}

pub(crate) fn evaluate_compiled(
    expression: &Expr,
    context: &EvalContext,
    clock: &EvaluationClock,
) -> Result<Value, CelFailure> {
    evaluate_with_limits(
        expression,
        context,
        MAX_AST_DEPTH,
        MAX_EVALUATION_STEPS,
        clock,
    )
    .map_err(|error| CelFailure {
        code: error.code,
        message: error.message,
    })
}

pub(crate) fn evaluate_record(collection: &Collection, input: &Value) -> OperationResult {
    let Some(path) = input.get("path").and_then(Value::as_str) else {
        return failed(
            "invalid_request",
            "Record CEL evaluation requires path.",
            None,
        );
    };
    let Some(source) = input.get("expression").and_then(Value::as_str) else {
        return failed(
            "invalid_request",
            "CEL evaluation requires expression.",
            Some(path.to_string()),
        );
    };
    let read = collection.read(&json!({"path": path}));
    if let Some(error) = read.get("error") {
        return failed(
            error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("operation_failed"),
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Record could not be read."),
            Some(path.to_string()),
        );
    }

    let effective = read
        .get("effective_frontmatter")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let raw = read
        .get("frontmatter")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let type_names = read
        .get("types")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect::<Vec<_>>();
    let known_fields = known_fields(collection, &type_names);
    let mut context = EvalContext::empty();
    context.frontmatter = enrich_record_bindings(&effective, &raw, known_fields.iter());
    context.raw_frontmatter = Some(raw);
    context.file_path = Some(path.to_string());
    context.body = read.get("body").and_then(Value::as_str).map(String::from);
    context.file_size = read.pointer("/file/size").and_then(Value::as_u64);
    context.file_mtime = read
        .pointer("/file/mtime")
        .and_then(Value::as_str)
        .map(String::from);
    context.type_names = Some(type_names);
    context.types = Some(Arc::new(collection.types.clone()));
    context.note_namespace_source = NoteNamespaceSource::Effective;
    context.string_concat = false;
    let clock = match operation_clock(collection.settings.timezone.as_deref()) {
        Ok(clock) => clock,
        Err(error) => return failed(&error.code, error.message, Some(path.to_string())),
    };
    evaluate_source(source, &context, &clock, Some(path))
}

pub(crate) fn evaluate_bindings(input: &Value) -> OperationResult {
    let Some(source) = input.get("expression").and_then(Value::as_str) else {
        return failed(
            "invalid_request",
            "CEL evaluation requires expression.",
            None,
        );
    };
    let mut context = EvalContext::empty();
    context.frontmatter = input.get("bindings").cloned().unwrap_or_else(|| json!({}));
    context.string_concat = false;
    let clock = match operation_clock(input.get("timezone").and_then(Value::as_str)) {
        Ok(clock) => clock,
        Err(error) => return failed(&error.code, error.message, None),
    };
    evaluate_source(source, &context, &clock, None)
}

pub(crate) fn evaluate_workflow_template(input: &Value) -> OperationResult {
    let Some(template) = input.get("template") else {
        return failed(
            "invalid_request",
            "Workflow input evaluation requires template.",
            None,
        );
    };
    let mut context = EvalContext::empty();
    context.frontmatter = input.get("bindings").cloned().unwrap_or_else(|| json!({}));
    context.string_concat = false;
    let clock = match operation_clock(input.get("timezone").and_then(Value::as_str)) {
        Ok(clock) => clock,
        Err(error) => return failed(&error.code, error.message, None),
    };
    let mut diagnostics = Vec::new();
    let value = evaluate_template_value(template, &context, &clock, &mut diagnostics);
    let valid = !diagnostics
        .iter()
        .any(|diagnostic: &Diagnostic| diagnostic.severity == "error");
    OperationResult {
        valid,
        result: json!({"value": value}),
        diagnostics,
    }
}

#[allow(dead_code)]
pub(crate) fn evaluate_match_expression(
    source: &str,
    raw: &Value,
    path: &str,
    timezone: Option<&str>,
) -> Result<bool, CelFailure> {
    let parsed = compile(source)?;
    evaluate_match_expression_compiled(&parsed, raw, path, timezone)
}

#[allow(dead_code)]
pub(crate) fn evaluate_match_expression_compiled(
    parsed: &Expr,
    raw: &Value,
    path: &str,
    timezone: Option<&str>,
) -> Result<bool, CelFailure> {
    let clock = operation_clock(timezone)?;
    evaluate_match_expression_compiled_with_clock(parsed, raw, path, &clock)
}

pub(crate) fn evaluate_match_expression_compiled_with_clock(
    parsed: &Expr,
    raw: &Value,
    path: &str,
    clock: &EvaluationClock,
) -> Result<bool, CelFailure> {
    let mut context = EvalContext::empty();
    let known = raw.as_object().into_iter().flat_map(|object| object.keys());
    context.frontmatter = enrich_record_bindings(raw, raw, known);
    context.raw_frontmatter = Some(raw.clone());
    context.note_namespace_source = NoteNamespaceSource::Effective;
    context.file_path = Some(path.to_string());
    context.string_concat = false;
    let value = evaluate_compiled(parsed, &context, clock)?;
    Ok(value == Value::Bool(true))
}

pub(crate) fn enrich_record_bindings<'a>(
    effective: &Value,
    raw: &Value,
    known_fields: impl Iterator<Item = &'a String>,
) -> Value {
    let record = effective.as_object().cloned().unwrap_or_default();
    let raw_object = raw.as_object().cloned().unwrap_or_default();
    let mut binding = record.clone();
    for reserved in [
        "record",
        "raw",
        "present",
        "note",
        "this",
        "old",
        "operation",
        "event",
        "workflow",
        "trigger",
        "steps",
        "vars",
        "item",
    ] {
        binding.remove(reserved);
    }

    let mut names = BTreeSet::new();
    names.extend(known_fields.cloned());
    names.extend(record.keys().cloned());
    names.extend(raw_object.keys().cloned());
    let raw_presence = names
        .iter()
        .map(|field| (field.clone(), Value::Bool(raw_object.contains_key(field))))
        .collect::<Map<_, _>>();
    let record_presence = names
        .iter()
        .map(|field| (field.clone(), Value::Bool(record.contains_key(field))))
        .collect::<Map<_, _>>();

    binding.insert("record".to_string(), Value::Object(record.clone()));
    binding.insert("note".to_string(), Value::Object(record));
    binding.insert("raw".to_string(), Value::Object(raw_object));
    binding.insert(
        "present".to_string(),
        json!({
            "raw": raw_presence,
            "record": record_presence,
        }),
    );
    Value::Object(binding)
}

pub(crate) fn known_fields(collection: &Collection, type_names: &[String]) -> BTreeSet<String> {
    type_names
        .iter()
        .filter_map(|type_name| collection.types.get(type_name))
        .flat_map(|type_definition| type_definition.fields.keys().cloned())
        .collect()
}

fn evaluate_source(
    source: &str,
    context: &EvalContext,
    clock: &EvaluationClock,
    path: Option<&str>,
) -> OperationResult {
    let expression = match compile(source) {
        Ok(expression) => expression,
        Err(error) => {
            return failed(
                &error.code,
                format!("CEL expression did not compile: {}", error.message),
                path.map(String::from),
            )
        }
    };
    match evaluate_compiled(&expression, context, clock) {
        Ok(value) => OperationResult {
            valid: true,
            result: json!({"value": value}),
            diagnostics: Vec::new(),
        },
        Err(error) => OperationResult {
            valid: true,
            result: json!({"value": null}),
            diagnostics: vec![Diagnostic {
                severity: "warning".to_string(),
                code: "expression_evaluation_error".to_string(),
                message: error.message,
                path: path.map(String::from),
                field: None,
                type_name: None,
                schema_location: None,
                details: Some(json!({"evaluator_code": error.code})),
            }],
        },
    }
}

fn evaluate_template_value(
    value: &Value,
    context: &EvalContext,
    clock: &EvaluationClock,
    diagnostics: &mut Vec<Diagnostic>,
) -> Value {
    match value {
        Value::Object(object) if object.len() == 1 && object.contains_key("$expr") => {
            let Some(source) = object.get("$expr").and_then(Value::as_str) else {
                diagnostics.push(Diagnostic::error(
                    "expression_compile_error",
                    "$expr must contain a string.",
                    None,
                ));
                return Value::Null;
            };
            let result = evaluate_source(source, context, clock, None);
            diagnostics.extend(result.diagnostics);
            result.result.get("value").cloned().unwrap_or(Value::Null)
        }
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        evaluate_template_value(value, context, clock, diagnostics),
                    )
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| evaluate_template_value(value, context, clock, diagnostics))
                .collect(),
        ),
        value => value.clone(),
    }
}

fn evaluate_runtime_template_value(
    value: &Value,
    context: &EvalContext,
    clock: &EvaluationClock,
    diagnostics: &mut Vec<WorkflowCelError>,
) -> Value {
    match value {
        Value::Object(object) if object.len() == 1 && object.contains_key("$expr") => {
            let Some(source) = object.get("$expr").and_then(Value::as_str) else {
                diagnostics.push(WorkflowCelError {
                    code: "expression_compile_error".to_string(),
                    message: "$expr must contain a string.".to_string(),
                });
                return Value::Null;
            };
            let expression = match compile(source) {
                Ok(expression) => expression,
                Err(error) => {
                    diagnostics.push(error.into());
                    return Value::Null;
                }
            };
            match evaluate_compiled(&expression, context, clock) {
                Ok(value) => value,
                Err(error) => {
                    diagnostics.push(error.into());
                    Value::Null
                }
            }
        }
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        evaluate_runtime_template_value(value, context, clock, diagnostics),
                    )
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| evaluate_runtime_template_value(value, context, clock, diagnostics))
                .collect(),
        ),
        value => value.clone(),
    }
}

fn failed(code: &str, message: impl Into<String>, path: Option<String>) -> OperationResult {
    OperationResult {
        valid: false,
        result: json!({}),
        diagnostics: vec![Diagnostic::error(code, message, path)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expressions::evaluator::evaluate;

    #[test]
    fn presence_maps_include_known_missing_fields() {
        let known = BTreeSet::from(["status".to_string(), "title".to_string()]);
        let bindings = enrich_record_bindings(
            &json!({"title": "Hello", "status": "open"}),
            &json!({"title": "Hello"}),
            known.iter(),
        );
        assert_eq!(bindings["present"]["raw"]["status"], false);
        assert_eq!(bindings["present"]["record"]["status"], true);
    }

    #[test]
    fn workflow_templates_only_evaluate_expression_objects() {
        let result = evaluate_workflow_template(&json!({
            "bindings": {"event": {"payload": {"path": "task.md"}}},
            "template": {
                "evaluated": {"$expr": "event.payload.path"},
                "literal": "event.payload.path",
                "nested": [{"$expr": "event.payload.path"}],
            }
        }));
        assert!(result.valid, "{result:#?}");
        assert_eq!(result.result["value"]["evaluated"], "task.md");
        assert_eq!(result.result["value"]["literal"], "event.payload.path");
        assert_eq!(result.result["value"]["nested"][0], "task.md");
    }

    #[test]
    fn runtime_membership_supports_notification_criteria_and_maps() {
        let now = "2026-07-26T00:00:00Z".parse().unwrap();
        let bindings = json!({
            "event": {
                "payload": {
                    "types": ["pickle_request"],
                    "path": "requests/test.md"
                }
            },
            "metadata": {
                "status": "pending"
            }
        });

        assert_eq!(
            evaluate_runtime_expression(
                r#""pickle_request" in event.payload.types"#,
                &bindings,
                now,
                Some("UTC"),
            )
            .unwrap(),
            true
        );
        assert_eq!(
            evaluate_runtime_expression(
                r#""missing_type" in event.payload.types"#,
                &bindings,
                now,
                Some("UTC"),
            )
            .unwrap(),
            false
        );
        assert_eq!(
            evaluate_runtime_expression(
                r#""status" in metadata && event.payload.path == "requests/test.md""#,
                &bindings,
                now,
                Some("UTC"),
            )
            .unwrap(),
            true
        );
    }

    #[test]
    fn runtime_membership_rejects_an_invalid_right_operand() {
        let error = evaluate_runtime_expression(
            r#""pickle_request" in "pickle_request""#,
            &json!({}),
            "2026-07-26T00:00:00Z".parse().unwrap(),
            Some("UTC"),
        )
        .unwrap_err();

        assert_eq!(error.code, "type_error");
        assert_eq!(error.message, "Right operand of 'in' must be a list or map");
    }

    #[test]
    fn runtime_cel_supports_presence_and_comprehension_macros() {
        let now = "2026-07-26T00:00:00Z".parse().unwrap();
        let bindings = json!({
            "event": {
                "payload": {
                    "nullable": null,
                    "types": ["pickle_request", "urgent"]
                }
            }
        });

        assert_eq!(
            evaluate_runtime_expression(
                "has(event.payload.nullable) && !has(event.payload.missing)",
                &bindings,
                now,
                Some("UTC"),
            )
            .unwrap(),
            true
        );
        assert_eq!(
            evaluate_runtime_expression(
                r#"event.payload.types.exists(t, t == "pickle_request")
                    && event.payload.types.all(t, t != "")
                    && event.payload.types.exists_one(t, t == "urgent")
                    && event.payload.types.filter(t, t == "urgent").size() == 1
                    && event.payload.types.map(t, t).size() == 2
                    && event.payload.types.map(t, t == "urgent", t)[0] == "urgent""#,
                &bindings,
                now,
                Some("UTC"),
            )
            .unwrap(),
            true
        );
    }

    #[test]
    fn v03_note_namespace_does_not_change_legacy_note_resolution() {
        let known = BTreeSet::from(["title".to_string()]);
        let mut v03 = EvalContext::empty();
        v03.frontmatter = enrich_record_bindings(
            &json!({"title": "Effective"}),
            &json!({"title": "Raw"}),
            known.iter(),
        );
        v03.raw_frontmatter = Some(json!({"title": "Raw"}));
        v03.note_namespace_source = NoteNamespaceSource::Effective;
        let expression = Parser::parse("note.title").unwrap();
        assert_eq!(evaluate(&expression, &v03).unwrap(), "Effective");

        let mut legacy = EvalContext::empty();
        legacy.frontmatter = json!({"note": {"title": "Persisted object"}});
        legacy.raw_frontmatter = Some(json!({"title": "Legacy raw"}));
        assert_eq!(evaluate(&expression, &legacy).unwrap(), "Legacy raw");
    }
}
