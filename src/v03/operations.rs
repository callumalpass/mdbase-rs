use serde::{Deserialize, Serialize};
use serde_json::Value;

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

    pub(crate) fn collection(&self) -> &'a Collection {
        self.collection
    }

    pub fn read(&self, input: &Value) -> OperationResult {
        let request = match self.parse_read_request(input) {
            Ok(request) => request,
            Err(result) => return result,
        };
        typed_read_result(crate::operations::read::evaluate_typed_read(
            self.collection,
            &request,
            crate::operations::read::TypedReadSource::Filesystem,
        ))
    }

    /// Evaluate one provider-supplied exact record without filesystem discovery.
    pub(crate) fn read_record(
        &self,
        input: &Value,
        path: &str,
        document: &str,
        file_facts: &crate::operations::read::RecordFileFacts,
    ) -> OperationResult {
        let request = match self.parse_read_request(input) {
            Ok(request) => request,
            Err(result) => return result,
        };
        typed_read_result(crate::operations::read::evaluate_typed_read(
            self.collection,
            &request,
            crate::operations::read::TypedReadSource::Exact {
                canonical_path: path,
                document,
                file_facts,
            },
        ))
    }

    pub(crate) fn read_record_not_found(&self, input: &Value) -> OperationResult {
        let request = match self.parse_read_request(input) {
            Ok(request) => request,
            Err(result) => return result,
        };
        typed_read_result(crate::operations::read::evaluate_typed_read(
            self.collection,
            &request,
            crate::operations::read::TypedReadSource::Missing,
        ))
    }

    fn parse_read_request(
        &self,
        input: &Value,
    ) -> Result<crate::api::ReadRequest, OperationResult> {
        let parsed = crate::api::operations::ReadInput::parse(input)
            .map_err(|error| legacy_read_error(input, error))?;
        crate::operations::ensure_safe_relative_path(&parsed.path, self.collection.spec_profile)
            .map_err(|error| legacy_read_error(input, error))?;
        let path = crate::operations::readable_record_path(self.collection, &parsed.path)
            .map_err(|error| legacy_read_error(input, error))?;
        Ok(crate::api::ReadRequest {
            path,
            include_document: parsed.include_document,
        })
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
        let request = match self.parse_read_request(input) {
            Ok(request) => request,
            Err(result) => return result,
        };
        let read = typed_read_result(crate::operations::read::evaluate_typed_read(
            self.collection,
            &request,
            crate::operations::read::TypedReadSource::Filesystem,
        ));
        if !read.valid {
            return failed_result(read.diagnostics);
        }
        let persisted = read.result["frontmatter"].clone();
        let (types, failures) = self
            .collection
            .determine_types_for_path_checked(&persisted, Some(path));
        OperationResult {
            valid: true,
            result: serde_json::json!({"types": types}),
            diagnostics: match_failure_diagnostics(path, failures),
        }
    }

    pub fn validate(&self, input: &Value) -> OperationResult {
        let mut result = self.normalize("validate", input, self.collection.validate_op(input));
        for diagnostic in &mut result.diagnostics {
            if diagnostic.code == "invalid_frontmatter" && diagnostic.details.is_none() {
                if let Some(reason) = diagnostic.message.strip_prefix("Invalid frontmatter: ") {
                    diagnostic.details = Some(serde_json::json!({"reason": reason}));
                }
            }
        }
        result
    }

    pub fn query(&self, input: &Value) -> OperationResult {
        super::query::execute(self.collection, input)
    }

    /// Execute a query that a synchronous host can cancel cooperatively.
    pub fn query_cancellable(
        &self,
        input: &Value,
        cancellation: &crate::OperationCancellation,
    ) -> Result<OperationResult, crate::OperationCancelled> {
        super::query::execute_cancellable(self.collection, input, cancellation)
    }

    pub(crate) fn query_runtime_cancellable(
        &self,
        input: &Value,
        cancellation: &crate::OperationCancellation,
    ) -> Result<OperationResult, crate::OperationCancelled> {
        super::query::execute_runtime_cancellable(self.collection, input, cancellation)
    }

    /// Discover canonical and configured compatibility view sources.
    pub fn list_views(&self, input: &Value) -> OperationResult {
        crate::views::list(self.collection, input)
    }

    /// Resolve and execute one named saved view headlessly.
    pub fn execute_view(&self, input: &Value) -> OperationResult {
        crate::views::execute(self.collection, input)
    }

    /// Read a complete saved-view source document and its opaque revision.
    pub fn read_view_source(&self, input: &Value) -> OperationResult {
        crate::views::read_source(self.collection, input)
    }

    /// Create a complete saved-view source without replacing an existing file.
    pub fn create_view_source(&self, input: &Value) -> OperationResult {
        crate::views::create_source(self.collection, input)
    }

    /// Replace a saved-view source after validating its complete document.
    pub fn update_view_source(&self, input: &Value) -> OperationResult {
        crate::views::update_source(self.collection, input)
    }

    /// Delete a saved-view source, optionally guarded by its opaque revision.
    pub fn delete_view_source(&self, input: &Value) -> OperationResult {
        crate::views::delete_source(self.collection, input)
    }

    /// Execute a query with payload-free phase timings.
    pub fn query_profiled(&self, input: &Value) -> (OperationResult, super::QueryPerformance) {
        super::query::execute_profiled(self.collection, input)
    }

    /// Evaluate a portable expression against a record or explicit bindings.
    pub fn evaluate_cel(&self, input: &Value) -> OperationResult {
        if input.get("path").is_some() {
            crate::cel::evaluate_record(self.collection, input)
        } else {
            crate::cel::evaluate_bindings(input)
        }
    }

    /// Recursively evaluate only `{ "$expr": "..." }` workflow values.
    pub fn evaluate_workflow_input(&self, input: &Value) -> OperationResult {
        crate::cel::evaluate_workflow_template(input)
    }

    pub fn create(&self, input: &Value) -> OperationResult {
        super::batch::execute_single(self.collection, "create", input)
    }

    pub fn update(&self, input: &Value) -> OperationResult {
        super::batch::execute_single(self.collection, "update", input)
    }

    pub fn delete(&self, input: &Value) -> OperationResult {
        super::batch::execute_single(self.collection, "delete", input)
    }

    pub fn rename(&self, input: &Value) -> OperationResult {
        if input.get("simulate_before_ref_update").is_some()
            || input.get("last_known_ref_mtimes").is_some()
        {
            return failed_result(vec![Diagnostic::error(
                "invalid_request",
                "Internal concurrency simulation fields are not accepted by canonical operations.",
                None,
            )]);
        }
        super::batch::execute_single(self.collection, "rename", input)
    }

    /// Execute one mutation directly inside a disposable staging collection.
    /// Unlike [`Self::create`], [`Self::update`], [`Self::delete`], and
    /// [`Self::rename`], this method does not create a second collection-wide
    /// shadow copy before writing. It is intended for storage providers that
    /// have already materialized an isolated working set and own a durable
    /// transaction outside mdbase. The caller must discard that working set
    /// whenever this operation is invalid or the enclosing transaction does
    /// not commit. Ordinary filesystem callers should use the atomic mutation
    /// methods instead.
    pub fn execute_staged_mutation(&self, operation: &str, input: &Value) -> OperationResult {
        self.execute_mutation_direct(operation, input)
    }

    pub(super) fn execute_mutation_direct(
        &self,
        operation: &str,
        input: &Value,
    ) -> OperationResult {
        if input.get("simulate_before_ref_update").is_some()
            || input.get("last_known_ref_mtimes").is_some()
        {
            return failed_result(vec![Diagnostic::error(
                "invalid_request",
                "Internal concurrency simulation fields are not accepted by canonical operations.",
                None,
            )]);
        }
        match operation {
            "create" => self.create_direct(input),
            "update" => self.update_direct(input),
            "delete" => self.delete_direct(input),
            "rename" => self.rename_direct(input),
            _ => failed_result(vec![Diagnostic::error(
                "invalid_request",
                format!("Unsupported mutation operation '{operation}'."),
                None,
            )]),
        }
    }

    fn create_direct(&self, input: &Value) -> OperationResult {
        let (request, options) = match super::mutation_adapter::decode_create(input) {
            Ok(decoded) => decoded,
            Err(diagnostics) => return failed_result(diagnostics),
        };
        match crate::mutation::staged_create(self.collection, request, options) {
            Ok(record) => planned_operation_result(self.collection, record),
            Err(error) => typed_error_result(error),
        }
    }

    fn update_direct(&self, input: &Value) -> OperationResult {
        let (request, options) = match super::mutation_adapter::decode_update(input) {
            Ok(decoded) => decoded,
            Err(diagnostics) => return failed_result(diagnostics),
        };
        match crate::mutation::staged_update(self.collection, request, options) {
            Ok(record) => planned_operation_result(self.collection, record),
            Err(error) => typed_error_result(error),
        }
    }

    fn delete_direct(&self, input: &Value) -> OperationResult {
        let (request, options) = match super::mutation_adapter::decode_delete(input) {
            Ok(decoded) => decoded,
            Err(diagnostics) => return failed_result(diagnostics),
        };
        match crate::mutation::staged_delete(self.collection, request, options) {
            Ok(planned) => planned_delete_result(&planned),
            Err(error) => typed_error_result(error),
        }
    }

    fn rename_direct(&self, input: &Value) -> OperationResult {
        let (request, options, last_known_mtime) =
            match super::mutation_adapter::decode_rename(self.collection, input) {
                Ok(decoded) => decoded,
                Err(diagnostics) => return failed_result(diagnostics),
            };
        match crate::mutation::plan_rename(self.collection, request, options, last_known_mtime) {
            Ok(planned) => super::mutation_adapter::planned_rename_result(self.collection, planned),
            Err(error) => typed_error_result(error),
        }
    }

    /// Execute or dry-run a deterministic sequence of core mutations.
    pub fn batch(&self, input: &Value) -> OperationResult {
        super::batch::execute(self.collection, input)
    }

    pub fn list_types(&self, _input: &Value) -> OperationResult {
        let mut types = self
            .collection
            .types()
            .values()
            .map(|definition| {
                serde_json::json!({
                    "name": definition.name,
                    "path": definition.source_path,
                    "version": definition.version,
                    "description": definition.description,
                })
            })
            .collect::<Vec<_>>();
        types.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
        OperationResult {
            valid: true,
            result: serde_json::json!({"types": types}),
            diagnostics: Vec::new(),
        }
    }

    pub fn get_data_contracts(&self, input: &Value) -> OperationResult {
        let contracts = self
            .collection
            .list_data_contracts()
            .into_iter()
            .filter(|definition| {
                input
                    .get("contract")
                    .and_then(Value::as_str)
                    .is_none_or(|contract| definition.id == contract)
                    && input
                        .get("version")
                        .and_then(Value::as_str)
                        .is_none_or(|version| definition.version == version)
            })
            .collect::<Vec<_>>();
        let implementations = if let (Some(contract), Some(version)) = (
            input.get("contract").and_then(Value::as_str),
            input.get("version").and_then(Value::as_str),
        ) {
            self.collection
                .get_data_contract_implementations(contract, version)
        } else {
            contracts
                .iter()
                .flat_map(|definition| {
                    self.collection
                        .get_data_contract_implementations(&definition.id, &definition.version)
                })
                .collect()
        };
        OperationResult {
            valid: true,
            result: serde_json::json!({
                "contracts": contracts,
                "implementations": implementations,
            }),
            diagnostics: Vec::new(),
        }
    }

    pub fn get_contract_view(&self, input: &Value) -> OperationResult {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return failed_result(vec![Diagnostic::error(
                "invalid_request",
                "Contract projection requires path.",
                None,
            )]);
        };
        let Some(contract) = input.get("contract").and_then(Value::as_str) else {
            return failed_result(vec![Diagnostic::error(
                "invalid_request",
                "Contract projection requires contract.",
                None,
            )]);
        };
        let Some(version) = input.get("version").and_then(Value::as_str) else {
            return failed_result(vec![Diagnostic::error(
                "invalid_request",
                "Contract projection requires exact version.",
                None,
            )]);
        };
        let projected = self.collection.get_contract_view(
            path,
            contract,
            version,
            input.get("type").and_then(Value::as_str),
        );
        OperationResult {
            valid: projected.valid,
            result: serde_json::json!({
                "contract": projected.contract,
                "version": projected.version,
                "contract_digest": projected.contract_digest,
                "type": projected.type_name,
                "implementation_digest": projected.implementation_digest,
                "view": projected.view,
            }),
            diagnostics: projected
                .diagnostics
                .into_iter()
                .map(|diagnostic| Diagnostic {
                    severity: diagnostic.severity,
                    code: diagnostic.code,
                    message: diagnostic.message,
                    path: diagnostic.path,
                    field: diagnostic.field,
                    type_name: None,
                    schema_location: None,
                    details: None,
                })
                .collect(),
        }
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

    fn normalize(&self, _operation: &str, input: &Value, legacy: Value) -> OperationResult {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string);
        let diagnostics = collect_diagnostics(&legacy, path.as_deref(), "error");
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
}

pub(super) fn typed_error_result(error: crate::api::MdbaseError) -> OperationResult {
    let diagnostics = error
        .diagnostics()
        .iter()
        .cloned()
        .map(|diagnostic| Diagnostic {
            severity: match diagnostic.severity {
                crate::api::Severity::Error => "error",
                crate::api::Severity::Warning => "warning",
                crate::api::Severity::Info => "info",
            }
            .to_string(),
            code: diagnostic.code.to_string(),
            message: diagnostic.message,
            path: diagnostic.path,
            field: diagnostic.field,
            type_name: diagnostic.type_name,
            schema_location: diagnostic.schema_location,
            details: diagnostic.details,
        })
        .collect::<Vec<_>>();
    if diagnostics.is_empty() {
        failed_result(vec![Diagnostic::error(
            "invalid_request",
            error.to_string(),
            None,
        )])
    } else {
        failed_result(diagnostics)
    }
}

pub(super) fn planned_delete_result(planned: &crate::mutation::PlannedDelete) -> OperationResult {
    let mut result = serde_json::to_value(planned.result()).expect("delete results serialize");
    if !planned.deleted {
        result["dry_run"] = Value::Bool(true);
        result["would_delete"] = Value::Bool(true);
    }
    if planned.broken_links.is_empty() {
        result
            .as_object_mut()
            .expect("delete results are objects")
            .remove("broken_links");
    }
    OperationResult {
        valid: true,
        result,
        diagnostics: Vec::new(),
    }
}

fn planned_operation_result(
    collection: &Collection,
    record: crate::mutation::PlannedRecord,
) -> OperationResult {
    let path = record.path.to_string();
    let metadata = match std::fs::metadata(record.path.under(&collection.root)) {
        Ok(metadata) => metadata,
        Err(error) => {
            return failed_result(vec![Diagnostic::error(
                "io_error",
                format!("Failed to stat persisted record: {error}"),
                Some(path),
            )])
        }
    };
    match crate::mutation::project_record(collection, record, metadata) {
        Ok(outcome) => OperationResult {
            valid: true,
            result: serde_json::to_value(outcome.value).expect("record documents serialize"),
            diagnostics: outcome
                .diagnostics
                .into_iter()
                .map(|diagnostic| Diagnostic {
                    severity: match diagnostic.severity {
                        crate::api::Severity::Error => "error",
                        crate::api::Severity::Warning => "warning",
                        crate::api::Severity::Info => "info",
                    }
                    .to_string(),
                    code: diagnostic.code.to_string(),
                    message: diagnostic.message,
                    path: diagnostic.path,
                    field: diagnostic.field,
                    type_name: diagnostic.type_name,
                    schema_location: diagnostic.schema_location,
                    details: diagnostic.details,
                })
                .collect(),
        },
        Err(error) => failed_result(
            error
                .diagnostics()
                .iter()
                .cloned()
                .map(|diagnostic| Diagnostic {
                    severity: "error".to_string(),
                    code: diagnostic.code.to_string(),
                    message: diagnostic.message,
                    path: diagnostic.path,
                    field: diagnostic.field,
                    type_name: diagnostic.type_name,
                    schema_location: diagnostic.schema_location,
                    details: diagnostic.details,
                })
                .collect(),
        ),
    }
}

fn match_failure_diagnostics(
    path: &str,
    failures: Vec<(String, crate::cel::CelFailure)>,
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

fn typed_read_result(evaluation: crate::operations::read::TypedReadEvaluation) -> OperationResult {
    OperationResult {
        valid: evaluation.valid,
        result: evaluation
            .value
            .map(|value| serde_json::to_value(value).expect("record documents serialize"))
            .unwrap_or_else(|| serde_json::json!({})),
        diagnostics: evaluation
            .diagnostics
            .into_iter()
            .map(|diagnostic| Diagnostic {
                severity: match diagnostic.severity {
                    crate::api::Severity::Error => "error",
                    crate::api::Severity::Warning => "warning",
                    crate::api::Severity::Info => "info",
                }
                .to_string(),
                code: diagnostic.code.to_string(),
                message: diagnostic.message,
                path: diagnostic.path,
                field: diagnostic.field,
                type_name: diagnostic.type_name,
                schema_location: diagnostic.schema_location,
                details: diagnostic.details,
            })
            .collect(),
    }
}

fn legacy_read_error(input: &Value, error: Value) -> OperationResult {
    OperationResult {
        valid: false,
        result: serde_json::json!({}),
        diagnostics: collect_diagnostics(
            &error,
            input.get("path").and_then(Value::as_str),
            "error",
        ),
    }
}

fn failed_result(diagnostics: Vec<Diagnostic>) -> OperationResult {
    OperationResult {
        valid: false,
        result: serde_json::json!({}),
        diagnostics,
    }
}

pub(super) fn collect_diagnostics(
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

pub(super) fn diagnostic_from_value(
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
