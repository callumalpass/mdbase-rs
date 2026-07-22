use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::lifecycle::LifecycleEvent;
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
        let mut result = self.normalize("read", input, self.collection.read(input));
        self.attach_match_diagnostics(&mut result);
        result
    }

    /// Resolve explicit or inferred type membership for one record.
    pub fn get_types(&self, input: &Value) -> OperationResult {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return failed_result(vec![Diagnostic::error(
                "invalid_request",
                "Type matching requires path.",
                None,
            )]);
        };
        let read = self.collection.read(&serde_json::json!({"path": path}));
        if let Some(error) = read.get("error") {
            return failed_result(vec![Diagnostic::error(
                error
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("operation_failed"),
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Record could not be read."),
                Some(path.to_string()),
            )]);
        }
        let raw = read
            .get("raw_frontmatter")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let (types, failures) = self
            .collection
            .determine_types_for_path_checked(&raw, Some(path));
        OperationResult {
            valid: true,
            result: serde_json::json!({"types": types}),
            diagnostics: match_failure_diagnostics(path, failures),
        }
    }

    pub fn validate(&self, input: &Value) -> OperationResult {
        self.normalize("validate", input, self.collection.validate_op(input))
    }

    pub fn query(&self, input: &Value) -> OperationResult {
        super::query::execute(self.collection, input)
    }

    /// Discover canonical and configured compatibility view sources.
    pub fn list_views(&self, input: &Value) -> OperationResult {
        crate::views::list(self.collection, input)
    }

    /// Resolve and execute one named saved view headlessly.
    pub fn execute_view(&self, input: &Value) -> OperationResult {
        crate::views::execute(self.collection, input)
    }

    /// Execute a query and return payload-free phase timings for local
    /// profiling and host observability.
    pub fn query_profiled(&self, input: &Value) -> (OperationResult, super::QueryPerformance) {
        super::query::execute_profiled(self.collection, input)
    }

    /// Evaluate a portable expression against either a record or explicit
    /// workflow bindings.
    pub fn evaluate_cel(&self, input: &Value) -> OperationResult {
        if input.get("path").is_some() {
            super::cel::evaluate_record(self.collection, input)
        } else {
            super::cel::evaluate_bindings(input)
        }
    }

    /// Recursively evaluate only `{ "$expr": "..." }` workflow values.
    pub fn evaluate_workflow_input(&self, input: &Value) -> OperationResult {
        super::cel::evaluate_workflow_template(input)
    }

    pub fn create(&self, input: &Value) -> OperationResult {
        if let Some(result) = invalid_revision_input(input) {
            return result;
        }
        let input = match self.prepare_create(input) {
            Ok(input) => input,
            Err(diagnostics) => return failed_result(diagnostics),
        };
        self.normalize("create", &input, self.collection.create(&input))
    }

    pub fn update(&self, input: &Value) -> OperationResult {
        if let Some(result) = invalid_revision_input(input) {
            return result;
        }
        let input = match self.prepare_update(input) {
            Ok(input) => input,
            Err(diagnostics) => return failed_result(diagnostics),
        };
        self.normalize("update", &input, self.collection.update(&input))
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

    /// Execute or dry-run a deterministic sequence of core mutations.
    pub fn batch(&self, input: &Value) -> OperationResult {
        super::batch::execute(self.collection, input)
    }

    pub fn read_type(&self, input: &Value) -> OperationResult {
        self.collection.read_type_file(input)
    }

    pub fn create_type(&self, input: &Value) -> OperationResult {
        self.collection.create_type_file(input)
    }

    pub fn update_type(&self, input: &Value) -> OperationResult {
        self.collection.update_type_file(input)
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
        if let Err(error) = crate::operations::ensure_no_symlink_components(
            &self.collection.root,
            path,
            self.collection.spec_profile,
        ) {
            diagnostics.push(diagnostic_from_value(
                error.get("error").unwrap_or(&error),
                "error",
                Some(path),
            ));
            return;
        }
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

    fn prepare_create(&self, input: &Value) -> Result<Value, Vec<Diagnostic>> {
        let mut normalized = input.as_object().cloned().unwrap_or_default();
        let mut draft = input
            .get("frontmatter")
            .or_else(|| input.get("fields"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(type_name) = input.get("type").and_then(Value::as_str) {
            draft
                .entry("type".to_string())
                .or_insert_with(|| Value::String(type_name.to_string()));
        }
        let path = input.get("path").and_then(Value::as_str).unwrap_or("");
        let type_names = create_type_membership(self.collection, input, &draft, path);
        let lifecycle_draft = self.collection.apply_v03_lifecycle(
            LifecycleEvent::Create,
            &type_names,
            draft,
            None,
            path,
        )?;
        ensure_membership_unchanged(self.collection, &type_names, &lifecycle_draft, path)?;
        normalized.insert("frontmatter".to_string(), Value::Object(lifecycle_draft));
        Ok(Value::Object(normalized))
    }

    fn prepare_update(&self, input: &Value) -> Result<Value, Vec<Diagnostic>> {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return Ok(input.clone());
        };
        let read = self.collection.read(&serde_json::json!({"path": path}));
        if read.get("error").is_some() {
            return Ok(input.clone());
        }
        let old = read
            .get("raw_frontmatter")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let patch = input
            .get("patch")
            .or_else(|| input.get("fields"))
            .or_else(|| input.get("frontmatter"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut draft = old.clone();
        apply_patch(&mut draft, &patch, &self.collection.settings.write_nulls);
        let type_names = self
            .collection
            .determine_types_for_path(&Value::Object(draft.clone()), Some(path));
        let lifecycle_draft = self.collection.apply_v03_lifecycle(
            LifecycleEvent::Update,
            &type_names,
            draft,
            Some(&old),
            path,
        )?;
        ensure_membership_unchanged(self.collection, &type_names, &lifecycle_draft, path)?;

        let mut normalized = input.as_object().cloned().unwrap_or_default();
        normalized.remove("patch");
        normalized.remove("frontmatter");
        normalized.insert(
            "fields".to_string(),
            Value::Object(diff_frontmatter(&old, &lifecycle_draft)),
        );
        Ok(Value::Object(normalized))
    }

    fn attach_match_diagnostics(&self, result: &mut OperationResult) {
        let Some(path) = result.result.get("path").and_then(Value::as_str) else {
            return;
        };
        let Some(raw) = result.result.get("raw_frontmatter") else {
            return;
        };
        let (_, failures) = self
            .collection
            .determine_types_for_path_checked(raw, Some(path));
        result
            .diagnostics
            .extend(match_failure_diagnostics(path, failures));
    }
}

fn match_failure_diagnostics(
    path: &str,
    failures: Vec<(String, super::cel::CelFailure)>,
) -> Vec<Diagnostic> {
    failures
        .into_iter()
        .map(|(type_name, failure)| Diagnostic {
            severity: "warning".to_string(),
            code: "expression_evaluation_error".to_string(),
            message: format!(
                "Type '{type_name}' match expression failed: {}",
                failure.message
            ),
            path: Some(path.to_string()),
            field: Some("match.expr".to_string()),
            type_name: Some(type_name),
            schema_location: None,
            details: Some(serde_json::json!({
                "context": "match",
                "evaluator_code": failure.code,
            })),
        })
        .collect()
}

fn create_type_membership(
    collection: &Collection,
    input: &Value,
    draft: &Map<String, Value>,
    path: &str,
) -> Vec<String> {
    let mut type_names = Vec::new();
    if let Some(type_name) = input.get("type").and_then(Value::as_str) {
        type_names.push(type_name.to_lowercase());
    }
    for type_name in collection.determine_types_for_path(&Value::Object(draft.clone()), Some(path))
    {
        if !type_names.contains(&type_name) {
            type_names.push(type_name);
        }
    }
    type_names
}

fn ensure_membership_unchanged(
    collection: &Collection,
    before: &[String],
    draft: &Map<String, Value>,
    path: &str,
) -> Result<(), Vec<Diagnostic>> {
    let after = collection.determine_types_for_path(&Value::Object(draft.clone()), Some(path));
    let mut before_sorted = before.to_vec();
    let mut after_sorted = after;
    before_sorted.sort();
    before_sorted.dedup();
    after_sorted.sort();
    after_sorted.dedup();
    if before_sorted == after_sorted {
        return Ok(());
    }
    let mut diagnostic = Diagnostic::error(
        "type_membership_changed",
        "Lifecycle policy changed the record's matched type membership.",
        Some(path.to_string()),
    );
    diagnostic.details = Some(serde_json::json!({
        "before": before_sorted,
        "after": after_sorted,
    }));
    Err(vec![diagnostic])
}

fn apply_patch(draft: &mut Map<String, Value>, patch: &Map<String, Value>, write_nulls: &str) {
    for (field, value) in patch {
        if value.is_null() && write_nulls == "omit" {
            draft.remove(field);
        } else {
            draft.insert(field.clone(), value.clone());
        }
    }
}

fn diff_frontmatter(before: &Map<String, Value>, after: &Map<String, Value>) -> Map<String, Value> {
    let mut fields = Map::new();
    for (field, value) in after {
        if before.get(field) != Some(value) {
            fields.insert(field.clone(), value.clone());
        }
    }
    for field in before.keys() {
        if !after.contains_key(field) {
            fields.insert(field.clone(), Value::Null);
        }
    }
    fields
}

fn failed_result(diagnostics: Vec<Diagnostic>) -> OperationResult {
    OperationResult {
        valid: false,
        result: serde_json::json!({}),
        diagnostics,
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
