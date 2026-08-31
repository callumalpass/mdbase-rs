use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::lifecycle::LifecycleEvent;
use super::Diagnostic;
use crate::frontmatter::parser::{
    json_to_yaml_mapping, parse_document_for_rewrite, yaml_mapping_to_json, FrontmatterState,
    ParsedDocument,
};
use crate::frontmatter::serializer;
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
        let mut result = self.normalize("read", input, self.collection.read(input));
        self.attach_match_diagnostics(&mut result);
        result
    }

    /// Evaluate a provider-supplied exact record using this collection's
    /// compiled catalog without reading or discovering filesystem records.
    pub(crate) fn read_record(
        &self,
        input: &Value,
        path: &str,
        document: &str,
        file_facts: &crate::operations::read::RecordFileFacts,
    ) -> OperationResult {
        let parsed = match crate::api::operations::ReadInput::parse(input) {
            Ok(parsed) => parsed,
            Err(error) => return self.normalize_without_hydration("read", input, error),
        };
        if let Err(error) =
            crate::operations::ensure_safe_relative_path(&parsed.path, self.collection.spec_profile)
        {
            return self.normalize_without_hydration("read", input, error);
        }
        let requested = match crate::operations::readable_record_path(self.collection, &parsed.path)
        {
            Ok(path) => path,
            Err(error) => return self.normalize_without_hydration("read", input, error),
        };
        if requested.as_str() != path {
            return failed_result(vec![Diagnostic::error(
                "record_identity_mismatch",
                "The supplied record does not match the requested canonical path.",
                Some(parsed.path),
            )]);
        }
        let evaluated = self.collection.read_document(
            requested.as_str(),
            document,
            file_facts,
            parsed.include_document,
        );
        let mut result = self.normalize_without_hydration("read", input, evaluated);
        self.attach_match_diagnostics(&mut result);
        result
    }

    pub(crate) fn read_record_not_found(&self, input: &Value) -> OperationResult {
        let parsed = match crate::api::operations::ReadInput::parse(input) {
            Ok(parsed) => parsed,
            Err(error) => return self.normalize_without_hydration("read", input, error),
        };
        if let Err(error) =
            crate::operations::ensure_safe_relative_path(&parsed.path, self.collection.spec_profile)
        {
            return self.normalize_without_hydration("read", input, error);
        }
        let requested = match crate::operations::readable_record_path(self.collection, &parsed.path)
        {
            Ok(path) => path,
            Err(error) => return self.normalize_without_hydration("read", input, error),
        };
        self.normalize_without_hydration(
            "read",
            input,
            crate::errors::op_error(
                crate::errors::FILE_NOT_FOUND,
                &format!("File not found: {}", requested.as_str()),
            ),
        )
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
        let persisted = read
            .get("frontmatter")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
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
    ///
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
        if let Some(result) = invalid_revision_input(input) {
            return result;
        }
        let (input, membership) = match self.prepare_create(input) {
            Ok(prepared) => prepared,
            Err(diagnostics) => return failed_result(diagnostics),
        };
        self.normalize(
            "create",
            &input,
            self.collection.create_v03_prepared(&input, membership),
        )
    }

    fn update_direct(&self, input: &Value) -> OperationResult {
        if let Some(result) = invalid_revision_input(input) {
            return result;
        }
        let (input, membership) = match self.prepare_update(input) {
            Ok(prepared) => prepared,
            Err(diagnostics) => return failed_result(diagnostics),
        };
        self.normalize(
            "update",
            &input,
            self.collection.update_v03_prepared(&input, membership),
        )
    }

    fn delete_direct(&self, input: &Value) -> OperationResult {
        if let Some(result) = invalid_revision_input(input) {
            return result;
        }
        self.normalize("delete", input, self.collection.delete(input))
    }

    fn rename_direct(&self, input: &Value) -> OperationResult {
        if let Some(result) = invalid_revision_input(input) {
            return result;
        }
        self.normalize("rename", input, self.collection.rename(input))
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

    fn normalize(&self, operation: &str, input: &Value, legacy: Value) -> OperationResult {
        self.normalize_inner(operation, input, legacy, true)
    }

    fn normalize_without_hydration(
        &self,
        operation: &str,
        input: &Value,
        legacy: Value,
    ) -> OperationResult {
        self.normalize_inner(operation, input, legacy, false)
    }

    fn normalize_inner(
        &self,
        operation: &str,
        input: &Value,
        legacy: Value,
        hydrate_from_filesystem: bool,
    ) -> OperationResult {
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

        if hydrate_from_filesystem
            && valid
            && !input
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && matches!(operation, "read" | "create" | "update" | "rename")
        {
            let persisted_path = persisted_path(operation, input, &result);
            if let Some(persisted_path) = persisted_path {
                let include_document = input
                    .get("include_document")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || input.get("document").is_some();
                self.hydrate_persisted_result(
                    &persisted_path,
                    include_document,
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
        include_document: bool,
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
        let read = self.collection.read(&serde_json::json!({
            "path": path,
            "include_document": include_document,
        }));
        if let Some(error) = read.get("error") {
            if error.get("code").and_then(Value::as_str) == Some("invalid_frontmatter") {
                match self.collection.snapshot_record(path) {
                    Ok(record) if record.frontmatter_error.is_some() => {
                        result.insert("path".to_string(), Value::String(record.path));
                        result.insert("revision".to_string(), Value::String(record.revision));
                        result.insert("types".to_string(), serde_json::json!(record.types));
                        result.insert("frontmatter".to_string(), Value::Object(record.frontmatter));
                        result.insert("effective_frontmatter".to_string(), serde_json::json!({}));
                        result.insert("body".to_string(), Value::String(record.body));
                        if include_document {
                            result.insert("document".to_string(), Value::String(record.document));
                        }
                        return;
                    }
                    Ok(_) => {}
                    Err(snapshot_error) => {
                        diagnostics.push(Diagnostic::error(
                            snapshot_error.code(),
                            snapshot_error.to_string(),
                            Some(path.to_string()),
                        ));
                        return;
                    }
                }
            }
            diagnostics.push(diagnostic_from_value(error, "error", Some(path)));
            return;
        }
        for key in [
            "path",
            "revision",
            "types",
            "frontmatter",
            "effective_frontmatter",
            "body",
            "document",
            "file",
        ] {
            if let Some(value) = read.get(key) {
                result.insert(key.to_string(), value.clone());
            }
        }
    }

    fn prepare_create(
        &self,
        input: &Value,
    ) -> Result<(Value, super::write_membership::ResolvedWriteMembership), Vec<Diagnostic>> {
        let mut normalized = input.as_object().cloned().unwrap_or_default();
        let mut draft = input
            .get("frontmatter")
            .or_else(|| input.get("fields"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        // An explicit path is canonicalized before it can affect membership.
        // A derived path intentionally starts path-independent; the designated
        // type selects its path pattern and the core create performs the final,
        // authoritative canonical-path revalidation.
        let canonical_path = match input.get("path").and_then(Value::as_str) {
            Some(path) => match crate::operations::mutation_record_path(self.collection, path) {
                Ok(path) => Some(path.to_string()),
                Err(error) => return Err(collect_diagnostics(&error, Some(path), "error")),
            },
            None => None,
        };
        if let Some(path) = &canonical_path {
            normalized.insert("path".to_string(), Value::String(path.clone()));
        }
        let path = canonical_path.as_deref().unwrap_or("");
        if let Some(candidate) = classify_raw_candidate(input, path)? {
            draft = candidate.frontmatter;
            normalized.insert("body".to_string(), Value::String(candidate.document.body));
        }
        let membership = super::write_membership::ResolvedWriteMembership::resolve_create(
            self.collection,
            input,
            &mut draft,
            path,
        )?;
        let lifecycle_draft = self.collection.apply_v03_lifecycle(
            LifecycleEvent::Create,
            membership.types(),
            draft,
            None,
            path,
        )?;
        normalized.insert("frontmatter".to_string(), Value::Object(lifecycle_draft));
        Ok((Value::Object(normalized), membership))
    }

    fn prepare_update(
        &self,
        input: &Value,
    ) -> Result<(Value, super::write_membership::ResolvedWriteMembership), Vec<Diagnostic>> {
        let Some(raw_path) = input.get("path").and_then(Value::as_str) else {
            return Err(vec![Diagnostic::error(
                "invalid_request",
                "Update requires path.",
                None,
            )]);
        };
        let path = match crate::operations::mutation_record_path(self.collection, raw_path) {
            Ok(path) => path.to_string(),
            Err(error) => return Err(collect_diagnostics(&error, Some(raw_path), "error")),
        };
        let candidate = classify_raw_candidate(input, &path)?;
        if let Err(error) = crate::operations::ensure_no_symlink_components(
            &self.collection.root,
            &path,
            self.collection.spec_profile,
        ) {
            return Err(collect_diagnostics(&error, Some(&path), "error"));
        }
        let full_path = self.collection.root.join(&path);
        if let Err(error) = crate::operations::ensure_regular_record_file(&full_path, &path) {
            return Err(collect_diagnostics(&error, Some(&path), "error"));
        }
        let loaded = crate::record_load::load_record(self.collection, &path).map_err(|_| {
            vec![Diagnostic::error(
                "file_read_failed",
                "Record could not be read.",
                Some(path.clone()),
            )]
        })?;
        let (old, prepared_revision) = match loaded {
            crate::record_load::RecordLoadOutcome::Parsed {
                raw_frontmatter,
                facts,
                ..
            } => (
                raw_frontmatter.as_object().cloned().unwrap_or_default(),
                Some(Value::String(facts.revision)),
            ),
            crate::record_load::RecordLoadOutcome::Invalid { facts, state, .. } => {
                if candidate.is_none() {
                    return Err(vec![invalid_record_diagnostic(
                        &path,
                        state.reason().as_str(),
                    )]);
                }
                (Map::new(), Some(Value::String(facts.revision)))
            }
        };
        let draft = if let Some(candidate) = candidate.as_ref() {
            candidate.frontmatter.clone()
        } else {
            let patch = input
                .get("patch")
                .or_else(|| input.get("fields"))
                .or_else(|| input.get("frontmatter"))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let mut draft = old.clone();
            apply_patch(&mut draft, &patch, &self.collection.settings.write_nulls);
            draft
        };
        let membership = super::write_membership::ResolvedWriteMembership::resolve_update(
            self.collection,
            &draft,
            &path,
        )?;
        let lifecycle_draft = self.collection.apply_v03_lifecycle(
            LifecycleEvent::Update,
            membership.types(),
            draft.clone(),
            Some(&old),
            &path,
        )?;
        let mut normalized = input.as_object().cloned().unwrap_or_default();
        normalized.insert("path".to_string(), Value::String(path.clone()));
        // Preparation read and locked core reread form one optimistic operation.
        // When the caller omitted a precondition, carry the prepared revision so
        // an intervening write cannot be merged into stale preparation.
        if normalized.get("if_revision").is_none() {
            if let Some(revision) = prepared_revision {
                normalized.insert("if_revision".to_string(), revision);
            }
        }
        if let Some(candidate) = candidate {
            if lifecycle_draft != draft {
                let canonical = json_to_yaml_mapping(&Value::Object(lifecycle_draft.clone()));
                let frontmatter = candidate
                    .document
                    .frontmatter
                    .as_ref()
                    .and_then(serde_yaml::Value::as_mapping)
                    .map_or(canonical, |authored| {
                        serializer::reconcile_json_object(authored, &lifecycle_draft)
                    });
                let document = serializer::serialize_document_with_bom(
                    candidate.had_bom,
                    &frontmatter,
                    &candidate.document.body,
                )
                .map_err(|error| {
                    vec![Diagnostic::error(
                        crate::errors::FRONTMATTER_SERIALIZATION_FAILED,
                        error.to_string(),
                        Some(path.clone()),
                    )]
                })?;
                normalized.insert("document".to_string(), Value::String(document));
            }
            normalized.insert("include_document".to_string(), Value::Bool(true));
        } else {
            normalized.remove("patch");
            normalized.remove("frontmatter");
            normalized.insert(
                "fields".to_string(),
                Value::Object(diff_frontmatter(&old, &lifecycle_draft)),
            );
        }
        Ok((Value::Object(normalized), membership))
    }

    fn attach_match_diagnostics(&self, result: &mut OperationResult) {
        let Some(path) = result.result.get("path").and_then(Value::as_str) else {
            return;
        };
        let Some(persisted) = result.result.get("frontmatter") else {
            return;
        };
        let (_, failures) = self
            .collection
            .determine_types_for_path_checked(persisted, Some(path));
        result
            .diagnostics
            .extend(match_failure_diagnostics(path, failures));
    }
}

fn invalid_record_diagnostic(path: &str, reason: &str) -> Diagnostic {
    let mut diagnostic = Diagnostic::error(
        "invalid_frontmatter",
        format!("Invalid frontmatter: {reason}"),
        Some(path.to_string()),
    );
    diagnostic.details = Some(serde_json::json!({"reason": reason}));
    diagnostic
}

struct RawCandidate {
    document: ParsedDocument,
    had_bom: bool,
    frontmatter: Map<String, Value>,
}

fn classify_raw_candidate(
    input: &Value,
    path: &str,
) -> Result<Option<RawCandidate>, Vec<Diagnostic>> {
    let Some(source) = input.get("document").and_then(Value::as_str) else {
        return Ok(None);
    };
    let (document, had_bom) = parse_document_for_rewrite(source);
    let frontmatter = match document.frontmatter_state() {
        FrontmatterState::Absent => Map::new(),
        FrontmatterState::Mapping(mapping) => yaml_mapping_to_json(mapping)
            .as_object()
            .cloned()
            .unwrap_or_default(),
        FrontmatterState::InvalidYaml => {
            return Err(vec![Diagnostic::error(
                "invalid_frontmatter",
                "Failed to parse replacement document YAML frontmatter.",
                Some(path.to_string()),
            )]);
        }
        FrontmatterState::Null | FrontmatterState::NonMapping(_) => {
            return Err(vec![Diagnostic::error(
                "invalid_frontmatter",
                "Replacement document frontmatter must be a YAML mapping.",
                Some(path.to_string()),
            )]);
        }
    };
    Ok(Some(RawCandidate {
        document,
        had_bom,
        frontmatter,
    }))
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
