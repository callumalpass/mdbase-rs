//! Version-neutral portable expression host bindings built on the shared evaluator.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use crate::diagnostic::Diagnostic;
use crate::expressions::evaluator::{EvalContext, EvaluationClock};
use crate::v03::OperationResult;
use crate::Collection;

mod host;
mod program;

pub(crate) use host::RESERVED;
pub(crate) use program::Program;

pub(crate) const MAX_SOURCE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_AST_DEPTH: u32 = 128;

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

/// Parse a standard CEL expression without evaluating it.
pub(crate) fn compile(source: &str) -> Result<Program, CelFailure> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(CelFailure {
            code: "expression_source_limit_exceeded".to_string(),
            message: format!(
                "CEL source is {} bytes; the configured limit is {MAX_SOURCE_BYTES} bytes.",
                source.len()
            ),
        });
    }
    Program::parse(source, MAX_AST_DEPTH).map_err(|message| CelFailure {
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
    program: &Program,
    context: &EvalContext,
    clock: &EvaluationClock,
) -> Result<Value, CelFailure> {
    host::evaluate(program, context, clock).map_err(|message| CelFailure {
        code: "expression_evaluation_error".to_string(),
        message,
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
    let request = match crate::api::ReadRequest::new(path) {
        Ok(request) => request,
        Err(error) => return failed("invalid_path", error.to_string(), Some(path.to_string())),
    };
    // Evaluation does not depend on validity: an invalid record still has
    // persisted and effective values (spec Chapter 04).
    let evaluation = crate::operations::read::evaluate_typed_read(
        collection,
        &request,
        crate::operations::read::TypedReadSource::Filesystem,
    );
    let Some(read) = evaluation.value else {
        let diagnostic = evaluation.diagnostics.first();
        return failed(
            diagnostic
                .map(|diagnostic| diagnostic.code.as_str())
                .unwrap_or("operation_failed"),
            diagnostic
                .map(|diagnostic| diagnostic.message.as_str())
                .unwrap_or("Record could not be read."),
            Some(path.to_string()),
        );
    };
    let program = match compile(source) {
        Ok(program) => program,
        Err(error) => {
            return failed(
                &error.code,
                format!("CEL expression did not compile: {}", error.message),
                Some(path.to_string()),
            )
        }
    };

    let effective = read.effective_frontmatter.clone();
    let raw = read.frontmatter.clone();
    let mut context = EvalContext::empty();
    context.frontmatter = enrich_record_bindings(&effective, &raw);
    context.raw_frontmatter = Some(raw);
    context.file_path = Some(path.to_string());
    context.body = Some(read.body);
    context.file_size = Some(read.file.size);
    context.file_mtime = Some(read.file.mtime);
    context.type_names = Some(read.types);
    context.types = Some(Arc::new(collection.types.clone()));
    context.string_concat = false;
    if program.facts().needs_link_graph {
        let graph = match collection.build_all_files_data() {
            Ok(files) => collection
                .build_link_graph(files)
                .map_err(|error| (error.code, error.message)),
            Err(error) => Err(("operation_failed".to_string(), error.to_string())),
        };
        match graph {
            Ok((linked, backlinks)) => {
                context.all_files = Some(Arc::new(linked));
                context.backlinks_index = Some(Arc::new(backlinks));
            }
            Err((code, message)) => return failed(&code, message, Some(path.to_string())),
        }
    }
    let clock = match operation_clock(
        input
            .get("timezone")
            .and_then(Value::as_str)
            .or(collection.settings.timezone.as_deref()),
    ) {
        Ok(clock) => clock,
        Err(error) => return failed(&error.code, error.message, Some(path.to_string())),
    };
    evaluate_program(&program, &context, &clock, Some(path))
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
    parsed: &Program,
    raw: &Value,
    path: &str,
    timezone: Option<&str>,
) -> Result<bool, CelFailure> {
    let clock = operation_clock(timezone)?;
    evaluate_match_expression_compiled_with_clock(parsed, raw, path, &clock)
}

pub(crate) fn evaluate_match_expression_compiled_with_clock(
    parsed: &Program,
    raw: &Value,
    path: &str,
    clock: &EvaluationClock,
) -> Result<bool, CelFailure> {
    let mut context = EvalContext::empty();
    context.frontmatter = enrich_record_bindings(raw, raw);
    context.raw_frontmatter = Some(raw.clone());
    context.file_path = Some(path.to_string());
    context.string_concat = false;
    let value = evaluate_compiled(parsed, &context, clock)?;
    Ok(value == Value::Bool(true))
}

/// Record bindings for a CEL host: unreserved effective fields at the top
/// level, `record` for effective values, and `raw` for persisted frontmatter.
pub(crate) fn enrich_record_bindings(effective: &Value, raw: &Value) -> Value {
    let record = effective.as_object().cloned().unwrap_or_default();
    let mut binding = record
        .iter()
        .filter(|(key, _)| !RESERVED.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Map<_, _>>();
    binding.insert("record".to_string(), Value::Object(record));
    binding.insert(
        "raw".to_string(),
        Value::Object(raw.as_object().cloned().unwrap_or_default()),
    );
    Value::Object(binding)
}

fn evaluate_source(
    source: &str,
    context: &EvalContext,
    clock: &EvaluationClock,
    path: Option<&str>,
) -> OperationResult {
    match compile(source) {
        Ok(program) => evaluate_program(&program, context, clock, path),
        Err(error) => failed(
            &error.code,
            format!("CEL expression did not compile: {}", error.message),
            path.map(String::from),
        ),
    }
}

fn evaluate_program(
    program: &Program,
    context: &EvalContext,
    clock: &EvaluationClock,
    path: Option<&str>,
) -> OperationResult {
    match evaluate_compiled(program, context, clock) {
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

    #[test]
    fn record_bindings_expose_record_and_raw_without_shadowing_system_names() {
        let bindings = enrich_record_bindings(
            &json!({"title": "Hello", "status": "open", "file": "frontmatter"}),
            &json!({"title": "Hello"}),
        );
        assert_eq!(bindings["status"], "open");
        assert_eq!(bindings["record"]["file"], "frontmatter");
        assert!(bindings["raw"].get("status").is_none());
        assert!(bindings.get("file").is_none());
        assert!(bindings.get("present").is_none() && bindings.get("note").is_none());
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

        assert_eq!(error.code, "expression_evaluation_error");
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
    fn missing_fields_are_null_but_missing_keys_are_errors() {
        let mut context = EvalContext::empty();
        context.frontmatter =
            enrich_record_bindings(&json!({"title": "A"}), &json!({"title": "A"}));
        let clock = EvaluationClock::capture(Some("UTC")).unwrap();
        let evaluate =
            |source: &str| evaluate_compiled(&compile(source).unwrap(), &context, &clock);
        assert_eq!(evaluate("note == null && !has(raw.note)").unwrap(), true);
        assert_eq!(
            evaluate("raw.note == null").unwrap_err().code,
            "expression_evaluation_error"
        );
        assert_eq!(evaluate(r#"raw.?note.orValue("none")"#).unwrap(), "none");
    }
}
