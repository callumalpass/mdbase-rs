//! Portable v0.3 expression host bindings built on the shared evaluator.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{json, Map, Value};

use super::{Diagnostic, OperationResult};
use crate::expressions::evaluator::{evaluate, EvalContext, NoteNamespaceSource};
use crate::expressions::parser::Parser;
use crate::Collection;

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
        .get("frontmatter")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let raw = read
        .get("raw_frontmatter")
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
    evaluate_source(source, &context, Some(path))
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
    evaluate_source(source, &context, None)
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
    let mut diagnostics = Vec::new();
    let value = evaluate_template_value(template, &context, &mut diagnostics);
    let valid = !diagnostics
        .iter()
        .any(|diagnostic: &Diagnostic| diagnostic.severity == "error");
    OperationResult {
        valid,
        result: json!({"value": value}),
        diagnostics,
    }
}

pub(crate) fn evaluate_match_expression(
    source: &str,
    raw: &Value,
    path: &str,
) -> Result<bool, String> {
    let parsed = Parser::parse(source)?;
    let mut context = EvalContext::empty();
    let known = raw.as_object().into_iter().flat_map(|object| object.keys());
    context.frontmatter = enrich_record_bindings(raw, raw, known);
    context.raw_frontmatter = Some(raw.clone());
    context.note_namespace_source = NoteNamespaceSource::Effective;
    context.file_path = Some(path.to_string());
    context.string_concat = false;
    let value = evaluate(&parsed, &context).map_err(|error| error.message)?;
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

fn known_fields(collection: &Collection, type_names: &[String]) -> BTreeSet<String> {
    type_names
        .iter()
        .filter_map(|type_name| collection.types.get(type_name))
        .flat_map(|type_definition| type_definition.fields.keys().cloned())
        .collect()
}

fn evaluate_source(source: &str, context: &EvalContext, path: Option<&str>) -> OperationResult {
    let expression = match Parser::parse(source) {
        Ok(expression) => expression,
        Err(error) => {
            return failed(
                "expression_compile_error",
                format!("CEL expression did not compile: {error}"),
                path.map(String::from),
            )
        }
    };
    match evaluate(&expression, context) {
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
            let result = evaluate_source(source, context, None);
            diagnostics.extend(result.diagnostics);
            result.result.get("value").cloned().unwrap_or(Value::Null)
        }
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        evaluate_template_value(value, context, diagnostics),
                    )
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| evaluate_template_value(value, context, diagnostics))
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
