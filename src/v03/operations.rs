use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::Diagnostic;
use crate::{Collection, SpecProfile};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationResult {
    pub valid: bool,
    pub result: Value,
    pub diagnostics: Vec<Diagnostic>,
}

pub struct Operations<'a> {
    collection: &'a Collection,
}

impl<'a> Operations<'a> {
    pub(crate) fn new(collection: &'a Collection) -> Result<Self, Box<Diagnostic>> {
        if collection.spec_profile != SpecProfile::V03 {
            return Err(Box::new(Diagnostic::error(
                "unsupported_profile",
                "The v0.3 operation facade requires a v0.3 collection.",
                Some("mdbase.yaml".to_string()),
            )));
        }
        Ok(Self { collection })
    }

    pub fn read(&self, input: &Value) -> OperationResult {
        self.normalize("read", input, self.collection.read(input))
    }

    pub fn validate(&self, input: &Value) -> OperationResult {
        self.normalize("validate", input, self.collection.validate_op(input))
    }

    pub fn query(&self, input: &Value) -> OperationResult {
        self.normalize("query", input, self.collection.query(input))
    }

    pub fn create(&self, input: &Value) -> OperationResult {
        if let Some(result) = invalid_revision_input(input) {
            return result;
        }
        self.normalize("create", input, self.collection.create(input))
    }

    pub fn update(&self, input: &Value) -> OperationResult {
        if let Some(result) = invalid_revision_input(input) {
            return result;
        }
        self.normalize("update", input, self.collection.update(input))
    }

    pub fn delete(&self, input: &Value) -> OperationResult {
        if let Some(result) = invalid_revision_input(input) {
            return result;
        }
        self.normalize("delete", input, self.collection.delete(input))
    }

    pub fn rename(&self, input: &Value) -> OperationResult {
        if let Some(result) = invalid_revision_input(input) {
            return result;
        }
        self.normalize("rename", input, self.collection.rename(input))
    }

    fn normalize(&self, operation: &str, input: &Value, legacy: Value) -> OperationResult {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string);
        let validation_severity =
            if operation == "read" && self.collection.settings.default_validation == "warn" {
                "warning"
            } else {
                "error"
            };
        let mut diagnostics = collect_diagnostics(&legacy, path.as_deref(), validation_severity);
        let has_error = diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == "error");
        let mut valid = legacy
            .get("valid")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| legacy.get("error").is_none());
        if has_error {
            valid = false;
        }

        let mut result = legacy.as_object().cloned().unwrap_or_default();
        for envelope_key in ["valid", "error", "issues", "validation", "warnings"] {
            result.remove(envelope_key);
        }

        if valid && matches!(operation, "read" | "create" | "update" | "rename") {
            let persisted_path = persisted_path(operation, input, &result);
            if let Some(persisted_path) = persisted_path {
                self.hydrate_persisted_result(
                    &persisted_path,
                    operation != "read",
                    &mut result,
                    &mut diagnostics,
                );
            }
        }

        let valid = valid
            && !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == "error");
        OperationResult {
            valid,
            result: Value::Object(result),
            diagnostics,
        }
    }

    fn hydrate_persisted_result(
        &self,
        path: &str,
        replace_frontmatter: bool,
        result: &mut Map<String, Value>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let full_path = self.collection.root.join(path);
        match std::fs::read(&full_path) {
            Ok(bytes) => {
                result.insert(
                    "revision".to_string(),
                    Value::String(super::revision(&bytes)),
                );
            }
            Err(error) => {
                diagnostics.push(Diagnostic::error(
                    "file_not_found",
                    format!("Failed to read persisted record: {error}"),
                    Some(path.to_string()),
                ));
                return;
            }
        }

        let read = self.collection.read(&serde_json::json!({"path": path}));
        if replace_frontmatter {
            if let Some(raw_frontmatter) = read.get("raw_frontmatter") {
                result.insert("frontmatter".to_string(), raw_frontmatter.clone());
            }
        }
        if let Some(types) = read.get("types") {
            result.insert("types".to_string(), types.clone());
        }
        result.insert("path".to_string(), Value::String(path.to_string()));
    }
}

fn invalid_revision_input(input: &Value) -> Option<OperationResult> {
    let revision = input.get("if_revision")?;
    if revision.is_string() {
        return None;
    }
    Some(OperationResult {
        valid: false,
        result: serde_json::json!({}),
        diagnostics: vec![Diagnostic::error(
            "invalid_request",
            "if_revision must be an opaque string token.",
            input
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_string),
        )],
    })
}

fn persisted_path(operation: &str, input: &Value, result: &Map<String, Value>) -> Option<String> {
    let key = if operation == "rename" { "to" } else { "path" };
    result
        .get(key)
        .or_else(|| input.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn collect_diagnostics(
    value: &Value,
    fallback_path: Option<&str>,
    validation_severity: &str,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for pointer in ["/issues", "/error/issues"] {
        if let Some(issues) = value.pointer(pointer).and_then(Value::as_array) {
            diagnostics.extend(
                issues
                    .iter()
                    .map(|issue| diagnostic_from_value(issue, "error", fallback_path)),
            );
        }
    }
    if let Some(issues) = value
        .pointer("/validation/issues")
        .and_then(Value::as_array)
    {
        diagnostics.extend(issues.iter().map(|issue| {
            let mut diagnostic = diagnostic_from_value(issue, validation_severity, fallback_path);
            diagnostic.severity = validation_severity.to_string();
            diagnostic
        }));
    }
    if let Some(warnings) = value.get("warnings").and_then(Value::as_array) {
        diagnostics.extend(
            warnings
                .iter()
                .map(|warning| diagnostic_from_value(warning, "warning", fallback_path)),
        );
    }
    if diagnostics.is_empty() {
        if let Some(error) = value.get("error") {
            diagnostics.push(diagnostic_from_value(error, "error", fallback_path));
        }
    }
    deduplicate_diagnostics(diagnostics)
}

fn diagnostic_from_value(
    value: &Value,
    default_severity: &str,
    fallback_path: Option<&str>,
) -> Diagnostic {
    Diagnostic {
        severity: value
            .get("severity")
            .and_then(Value::as_str)
            .unwrap_or(default_severity)
            .to_string(),
        code: value
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("operation_failed")
            .to_string(),
        message: value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Operation failed.")
            .to_string(),
        path: value
            .get("path")
            .and_then(Value::as_str)
            .or(fallback_path)
            .map(str::to_string),
        field: value
            .get("field")
            .and_then(Value::as_str)
            .map(str::to_string),
        type_name: value
            .get("type")
            .or_else(|| value.get("type_name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        schema_location: value
            .get("schema_location")
            .and_then(Value::as_str)
            .map(str::to_string),
        details: value.get("details").cloned(),
    }
}

fn deduplicate_diagnostics(diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut result = Vec::new();
    for diagnostic in diagnostics {
        if !result.contains(&diagnostic) {
            result.push(diagnostic);
        }
    }
    result
}
