//! Bounded exact-record mutation planning for hosted providers.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::v03::OperationResult;
use crate::{Collection, SpecProfile};

use super::{
    CanonicalChange, CanonicalOperationOutcome, CanonicalRecordInput, CanonicalTypeSet,
    CatalogError, ChangeBatch, ChangeSet, CompiledCatalog, OperationKind, RecordChange,
    RecordChangeKind,
};
use crate::api::{CollectionPath, Revision};

const MAX_HOSTED_MUTATION_RECORDS: usize = 2_001;
const MAX_HOSTED_MUTATION_EXACT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedMutationRequest {
    pub operation: String,
    pub primary_stable_id: String,
    pub input: Value,
    #[serde(default)]
    pub records: Vec<CanonicalRecordInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedMutationChange {
    pub stable_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<CanonicalRecordInput>,
}

/// Legacy hosted mutation plan retained exactly for source compatibility.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedMutationPlan {
    pub result: OperationResult,
    pub primary_stable_id: String,
    pub changes: Vec<HostedMutationChange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedRecordChange {
    pub stable_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<CanonicalRecordInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<CanonicalRecordInput>,
    pub change: RecordChange,
}

/// Typed hosted mutation plan. It contains no compatibility wire result.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedHostedMutationPlan {
    pub operation: CanonicalOperationOutcome,
    pub primary_stable_id: String,
    pub changes: Vec<HostedRecordChange>,
    pub change_set: ChangeSet,
}

impl CompiledCatalog {
    pub fn hosted_mutation_requires_incoming_context(
        &self,
        operation: &str,
        input: &Value,
    ) -> bool {
        match operation {
            "rename" => input
                .get("update_refs")
                .and_then(Value::as_bool)
                .unwrap_or(self.collection.settings.rename_update_refs),
            "delete" => input
                .get("check_backlinks")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            _ => false,
        }
    }

    /// Execute one mutation against a bounded caller-supplied exact context.
    /// The disposable stage is not an authority: the returned write set must
    /// still be committed with provider-owned revision CAS and fencing.
    pub fn plan_hosted_mutation_typed(
        &self,
        request: &HostedMutationRequest,
    ) -> Result<TypedHostedMutationPlan, CatalogError> {
        if request.records.len() > MAX_HOSTED_MUTATION_RECORDS {
            return Err(mutation_error(
                "hosted_mutation_context_budget_exceeded",
                "Hosted mutation context exceeds its exact-record budget.",
            ));
        }
        let exact_bytes = request.records.iter().try_fold(0_usize, |total, record| {
            total.checked_add(record.document.len())
        });
        if exact_bytes.is_none_or(|bytes| bytes > MAX_HOSTED_MUTATION_EXACT_BYTES) {
            return Err(mutation_error(
                "hosted_mutation_context_byte_budget_exceeded",
                "Hosted mutation context exceeds its exact-byte budget.",
            ));
        }
        if !matches!(
            request.operation.as_str(),
            "create" | "update" | "delete" | "rename" | "batch"
        ) {
            return Err(mutation_error(
                "unsupported_hosted_mutation",
                "Hosted mutation operation is not in the closed mutation plan.",
            ));
        }
        let directory = tempfile::tempdir().map_err(|error| {
            mutation_error(
                "hosted_mutation_stage_failed",
                format!("Hosted mutation stage could not be created: {error}"),
            )
        })?;
        materialize_catalog_resources(directory.path(), self)?;
        let mut stable_by_path = BTreeMap::new();
        let mut before_by_stable = BTreeMap::new();
        for record in &request.records {
            let stable_id = record.stable_id.as_ref().ok_or_else(|| {
                mutation_error(
                    "hosted_mutation_identity_required",
                    "Every hosted mutation context record requires stable identity.",
                )
            })?;
            let path = self
                .collection
                .validate_record_path(&record.path)
                .map_err(|error| {
                    mutation_error(
                        "invalid_path",
                        format!("Hosted mutation record path is invalid: {error}"),
                    )
                })?;
            if stable_by_path
                .insert(path.as_str().to_string(), stable_id.clone())
                .is_some()
                || before_by_stable
                    .insert(stable_id.clone(), path.as_str().to_string())
                    .is_some()
            {
                return Err(mutation_error(
                    "hosted_mutation_context_ambiguous",
                    "Hosted mutation context contains duplicate path or stable identity.",
                ));
            }
            let destination = path.under(directory.path());
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(stage_io_error)?;
            }
            fs::write(destination, &record.document).map_err(stage_io_error)?;
        }

        let data_contracts = crate::data_contracts::DataContractRegistry::load_resolved(
            self.contracts.clone(),
            &self.collection.types,
        )
        .map_err(|error| mutation_error(error.code, error.message))?;
        let collection = Collection {
            root: directory.path().to_path_buf(),
            root_capability: Collection::capability_for_root(directory.path())
                .map_err(stage_io_error)?,
            spec_profile: SpecProfile::V03,
            settings: self.collection.settings.clone(),
            config_extensions: self.collection.config_extensions.clone(),
            types: self.collection.types.clone(),
            type_plans: self.collection.type_plans.clone(),
            type_warnings: self.collection.type_warnings.clone(),
            data_contracts,
        };
        let primary_before = before_by_stable.get(&request.primary_stable_id).cloned();
        let mut input = request.input.as_object().cloned().ok_or_else(|| {
            mutation_error(
                "invalid_mutation",
                "Hosted mutation input must be an object.",
            )
        })?;
        match request.operation.as_str() {
            "create" => {
                input.remove("types");
            }
            "update" => {
                let path = primary_before.as_ref().ok_or_else(|| {
                    mutation_error("record_not_found", "The hosted record does not exist.")
                })?;
                if let Some(patch) = input.remove("patch") {
                    input.insert("fields".to_string(), patch);
                }
                input.insert("path".to_string(), Value::String(path.clone()));
            }
            "rename" => {
                let from = primary_before.as_ref().ok_or_else(|| {
                    mutation_error("record_not_found", "The hosted record does not exist.")
                })?;
                let to = input
                    .get("path")
                    .or_else(|| input.get("to"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        mutation_error("invalid_mutation", "Hosted rename requires a destination.")
                    })?
                    .to_string();
                input.insert("from".to_string(), Value::String(from.clone()));
                input.insert("to".to_string(), Value::String(to));
            }
            "delete" => {
                let path = primary_before.as_ref().ok_or_else(|| {
                    mutation_error("record_not_found", "The hosted record does not exist.")
                })?;
                input.insert("path".to_string(), Value::String(path.clone()));
            }
            "batch" => {}
            _ => unreachable!(),
        }
        let operations = collection
            .v03_operations()
            .expect("compiled catalogs always use the canonical profile");
        let result = if request.operation == "batch" {
            operations.batch(&Value::Object(input.clone()))
        } else {
            operations.execute_staged_mutation(&request.operation, &Value::Object(input))
        };
        let operation = typed_mutation_outcome(&request.operation, result.clone())?;
        let partial_batch_applied = request
            .input
            .get("allow_partial")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && matches!(
                &operation.value,
                super::CanonicalOperationValue::Batch(Some(batch)) if batch.succeeded > 0
            );
        if !result.valid && !partial_batch_applied {
            return Ok(TypedHostedMutationPlan {
                operation,
                primary_stable_id: request.primary_stable_id.clone(),
                changes: Vec::new(),
                change_set: ChangeSet::None,
            });
        }
        let is_dry_run = request
            .input
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mutation_result = if is_dry_run {
            reset_mutation_stage(directory.path(), &request.records, self)?;
            let mut committed_input = request
                .input
                .as_object()
                .cloned()
                .expect("hosted mutation input was validated above");
            committed_input.insert("dry_run".to_string(), Value::Bool(false));
            match request.operation.as_str() {
                "update" => {
                    let path = primary_before
                        .as_ref()
                        .expect("hosted update target was validated above");
                    if let Some(patch) = committed_input.remove("patch") {
                        committed_input.insert("fields".to_string(), patch);
                    }
                    committed_input.insert("path".to_string(), Value::String(path.clone()));
                }
                "rename" => {
                    let from = primary_before
                        .as_ref()
                        .expect("hosted rename target was validated above");
                    let to = committed_input
                        .get("path")
                        .or_else(|| committed_input.get("to"))
                        .cloned()
                        .expect("hosted rename destination was validated above");
                    committed_input.insert("from".to_string(), Value::String(from.clone()));
                    committed_input.insert("to".to_string(), to);
                }
                "delete" => {
                    let path = primary_before
                        .as_ref()
                        .expect("hosted delete target was validated above");
                    committed_input.insert("path".to_string(), Value::String(path.clone()));
                }
                "create" => {
                    committed_input.remove("types");
                }
                "batch" => {
                    committed_input.insert("dry_run".to_string(), Value::Bool(false));
                }
                _ => unreachable!(),
            }
            let operations = collection
                .v03_operations()
                .expect("compiled catalogs always use the canonical profile");
            if request.operation == "batch" {
                operations.batch(&Value::Object(committed_input))
            } else {
                operations
                    .execute_staged_mutation(&request.operation, &Value::Object(committed_input))
            }
        } else {
            result.clone()
        };
        if !mutation_result.valid && !partial_batch_applied {
            return Err(mutation_error(
                "hosted_mutation_dry_run_mismatch",
                "Canonical dry-run and disposable mutation execution disagreed.",
            ));
        }
        let applied_operation = typed_mutation_outcome(&request.operation, mutation_result)?;

        let (after_by_stable, affected) = if request.operation == "batch" {
            let super::CanonicalOperationValue::Batch(Some(batch)) = &applied_operation.value
            else {
                return Err(mutation_error(
                    "hosted_mutation_plan_incomplete",
                    "Canonical batch execution omitted typed item evidence.",
                ));
            };
            let applied = batch_after_paths(request, &before_by_stable, batch)?;
            (applied.after_by_stable, applied.affected)
        } else {
            let primary_after = match request.operation.as_str() {
                "delete" => None,
                "rename" => request
                    .input
                    .get("path")
                    .or_else(|| request.input.get("to"))
                    .and_then(Value::as_str),
                "create" => request.input.get("path").and_then(Value::as_str),
                _ => primary_before.as_deref(),
            };
            let mut paths = before_by_stable.clone();
            if let Some(path) = primary_after {
                paths.insert(request.primary_stable_id.clone(), path.to_string());
            } else {
                paths.remove(&request.primary_stable_id);
            }
            let affected = before_by_stable
                .keys()
                .chain(paths.keys())
                .cloned()
                .collect::<BTreeSet<_>>();
            (paths, affected)
        };
        let mut changes = Vec::new();
        for stable_id in affected {
            let before_path = before_by_stable.get(&stable_id).cloned();
            let after_path = after_by_stable.get(&stable_id).cloned();
            let record = after_path
                .as_deref()
                .map(|path| {
                    collection
                        .snapshot_record(path)
                        .map(|record| CanonicalRecordInput {
                            stable_id: Some(stable_id.clone()),
                            path: record.path,
                            file_size: record.document.len() as u64,
                            document: record.document,
                            file_mtime: None,
                        })
                })
                .transpose()
                .map_err(|error| {
                    mutation_error(
                        "hosted_mutation_plan_incomplete",
                        format!("Canonical mutation output could not be captured: {error}"),
                    )
                })?;
            let before = request
                .records
                .iter()
                .find(|record| record.stable_id.as_deref() == Some(stable_id.as_str()))
                .cloned();
            if before
                .as_ref()
                .map(|record| (&record.path, &record.document))
                == record
                    .as_ref()
                    .map(|record| (&record.path, &record.document))
            {
                continue;
            }
            let change =
                hosted_record_change(self, before.as_ref(), record.as_ref(), &request.operation)?;
            changes.push(HostedRecordChange {
                stable_id,
                before_path,
                before,
                after: record,
                change,
            });
        }
        changes.sort_by_key(|change| change.stable_id != request.primary_stable_id);
        let change_set = ChangeSet::Exact(
            ChangeBatch::new(
                changes
                    .iter()
                    .cloned()
                    .map(|change| CanonicalChange::Record(change.change))
                    .collect(),
            )
            .map_err(provider_change_error)?,
        );
        Ok(TypedHostedMutationPlan {
            operation,
            primary_stable_id: request.primary_stable_id.clone(),
            changes,
            change_set,
        })
    }

    /// Legacy compatibility entry point. The wire envelope is projected only
    /// here, after typed planning is complete.
    pub fn plan_hosted_mutation(
        &self,
        request: &HostedMutationRequest,
    ) -> Result<HostedMutationPlan, CatalogError> {
        let plan = self.plan_hosted_mutation_typed(request)?;
        Ok(HostedMutationPlan {
            result: plan.operation.to_v03(),
            primary_stable_id: plan.primary_stable_id,
            changes: plan
                .changes
                .into_iter()
                .map(|change| HostedMutationChange {
                    stable_id: change.stable_id,
                    before_path: change.before_path,
                    record: change.after,
                })
                .collect(),
        })
    }
}

struct BatchAppliedPaths {
    after_by_stable: BTreeMap<String, String>,
    affected: BTreeSet<String>,
}

fn batch_after_paths(
    request: &HostedMutationRequest,
    before_by_stable: &BTreeMap<String, String>,
    batch: &crate::api::BatchResult,
) -> Result<BatchAppliedPaths, CatalogError> {
    use crate::api::BatchOperationResult;

    let request_items = request
        .input
        .get("operations")
        .and_then(Value::as_array)
        .ok_or_else(|| mutation_error("invalid_request", "Hosted batch requires operations."))?;
    let successful_creates = batch
        .operations
        .iter()
        .filter(|item| item.valid && item.kind == "create")
        .count();
    let mut stable_by_path = before_by_stable
        .iter()
        .map(|(stable, path)| (path.clone(), stable.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut affected = BTreeSet::new();

    for (position, item) in batch.operations.iter().enumerate() {
        if item.index != position {
            return Err(mutation_error(
                "hosted_mutation_plan_incomplete",
                "Canonical batch item evidence is not in request order.",
            ));
        }
        if !item.valid {
            continue;
        }
        let request_item = request_items.get(item.index).ok_or_else(|| {
            mutation_error(
                "hosted_mutation_plan_incomplete",
                "Canonical batch item evidence has no matching request item.",
            )
        })?;
        match (item.kind.as_str(), &item.result) {
            ("create", BatchOperationResult::Record(record)) => {
                let input = request_item.get("input").unwrap_or(&Value::Null);
                let stable_id = request_item
                    .get("stable_id")
                    .or_else(|| input.get("stable_id"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        (successful_creates == 1).then(|| request.primary_stable_id.clone())
                    })
                    .ok_or_else(|| {
                        mutation_error(
                            "hosted_mutation_identity_required",
                            "Every successful hosted batch create requires stable identity evidence.",
                        )
                    })?;
                let path = record.path.as_str().to_string();
                if stable_by_path.insert(path, stable_id.clone()).is_some() {
                    return Err(mutation_error(
                        "hosted_mutation_plan_incomplete",
                        "A successful hosted batch create replaced an existing identity.",
                    ));
                }
                affected.insert(stable_id);
            }
            ("update", BatchOperationResult::Record(record)) => {
                affected.insert(stable_at_path(&stable_by_path, record.path.as_str())?);
            }
            ("delete", BatchOperationResult::Delete(deleted)) if deleted.deleted => {
                let stable_id = stable_by_path
                    .remove(deleted.path.as_str())
                    .ok_or_else(|| {
                        mutation_error(
                            "hosted_mutation_plan_incomplete",
                            "A successful hosted batch delete has no current stable identity.",
                        )
                    })?;
                affected.insert(stable_id);
            }
            ("rename", BatchOperationResult::Rename(renamed)) => {
                let from = renamed.result.from.as_str();
                let to = renamed.result.to.as_str().to_string();
                let stable_id = stable_by_path.remove(from).ok_or_else(|| {
                    mutation_error(
                        "hosted_mutation_plan_incomplete",
                        "A successful hosted batch rename has no current source identity.",
                    )
                })?;
                if stable_by_path.insert(to, stable_id.clone()).is_some() {
                    return Err(mutation_error(
                        "hosted_mutation_plan_incomplete",
                        "A successful hosted batch rename replaced another identity.",
                    ));
                }
                affected.insert(stable_id.clone());
                for reference in &renamed.result.references_updated {
                    let path = reference.path.as_deref().ok_or_else(|| {
                        mutation_error(
                            "hosted_mutation_plan_incomplete",
                            "Canonical batch reference evidence omitted its source path.",
                        )
                    })?;
                    affected.insert(if path == from {
                        stable_id.clone()
                    } else {
                        stable_at_path(&stable_by_path, path)?
                    });
                }
            }
            _ => {
                return Err(mutation_error(
                    "hosted_mutation_plan_incomplete",
                    format!(
                        "Successful canonical batch item {} has mismatched '{}' evidence.",
                        item.index, item.kind
                    ),
                ))
            }
        }
    }

    Ok(BatchAppliedPaths {
        after_by_stable: stable_by_path
            .into_iter()
            .map(|(path, stable)| (stable, path))
            .collect(),
        affected,
    })
}

fn stable_at_path(
    stable_by_path: &BTreeMap<String, String>,
    path: &str,
) -> Result<String, CatalogError> {
    stable_by_path.get(path).cloned().ok_or_else(|| {
        mutation_error(
            "hosted_mutation_plan_incomplete",
            format!("Successful canonical batch evidence references unknown path '{path}'."),
        )
    })
}

fn typed_mutation_outcome(
    operation: &str,
    result: OperationResult,
) -> Result<CanonicalOperationOutcome, CatalogError> {
    let kind = match operation {
        "create" => OperationKind::Create,
        "update" => OperationKind::Update,
        "delete" => OperationKind::Delete,
        "rename" => OperationKind::Rename,
        "batch" => OperationKind::Batch,
        _ => return Err(mutation_error("unsupported_hosted_mutation", operation)),
    };
    CanonicalOperationOutcome::hosted_wire_edge(kind, result).map_err(provider_change_error)
}

fn hosted_record_change(
    catalog: &CompiledCatalog,
    before: Option<&CanonicalRecordInput>,
    after: Option<&CanonicalRecordInput>,
    operation: &str,
) -> Result<RecordChange, CatalogError> {
    let before = before
        .map(|record| catalog.classify_record(record))
        .transpose()?;
    let after = after
        .map(|record| catalog.classify_record(record))
        .transpose()?;
    let kind = match (before.as_ref(), after.as_ref(), operation) {
        (None, Some(_), _) => RecordChangeKind::Created,
        (Some(_), None, _) => RecordChangeKind::Deleted,
        (Some(before), Some(after), _) if before.path != after.path => RecordChangeKind::Renamed,
        (Some(_), Some(_), _) => RecordChangeKind::Updated,
        _ => {
            return Err(mutation_error(
                "hosted_mutation_plan_incomplete",
                "Mutation produced no record transition.",
            ))
        }
    };
    let path = after
        .as_ref()
        .or(before.as_ref())
        .expect("one side exists")
        .path
        .clone();
    let changed_fields = match (before.as_ref(), after.as_ref()) {
        (Some(before), Some(after)) => {
            super::diff::canonical_field_changes(&before.frontmatter, &after.frontmatter)
        }
        (Some(before), None) => super::diff::canonical_present_fields(&before.frontmatter),
        (None, Some(after)) => super::diff::canonical_present_fields(&after.frontmatter),
        _ => unreachable!(),
    }
    .map_err(provider_change_error)?;
    Ok(RecordChange {
        kind,
        path: CollectionPath::new(path)
            .map_err(|error| mutation_error("invalid_path", error.to_string()))?,
        from: (kind == RecordChangeKind::Renamed)
            .then(|| CollectionPath::new(before.as_ref().expect("rename has before").path.clone()))
            .transpose()
            .map_err(|error| mutation_error("invalid_path", error.to_string()))?,
        before_revision: before
            .as_ref()
            .map(|record| Revision::parse(record.revision.clone()))
            .transpose()
            .map_err(|error| mutation_error("invalid_revision", error.to_string()))?,
        after_revision: after
            .as_ref()
            .map(|record| Revision::parse(record.revision.clone()))
            .transpose()
            .map_err(|error| mutation_error("invalid_revision", error.to_string()))?,
        before_types: CanonicalTypeSet::new(
            before
                .iter()
                .flat_map(|record| record.types.iter().cloned()),
        ),
        after_types: CanonicalTypeSet::new(
            after.iter().flat_map(|record| record.types.iter().cloned()),
        ),
        changed_fields,
        body_changed: before.as_ref().map(|record| record.body.as_str())
            != after.as_ref().map(|record| record.body.as_str()),
    })
}

fn provider_change_error(error: super::ProviderError) -> CatalogError {
    mutation_error(error.code(), error.to_string())
}

fn reset_mutation_stage(
    root: &std::path::Path,
    records: &[CanonicalRecordInput],
    catalog: &CompiledCatalog,
) -> Result<(), CatalogError> {
    for entry in fs::read_dir(root).map_err(stage_io_error)? {
        let path = entry.map_err(stage_io_error)?.path();
        if path.is_dir() {
            fs::remove_dir_all(path).map_err(stage_io_error)?;
        } else {
            fs::remove_file(path).map_err(stage_io_error)?;
        }
    }
    materialize_catalog_resources(root, catalog)?;
    for record in records {
        let path = catalog
            .collection
            .validate_record_path(&record.path)
            .map_err(|error| {
                mutation_error(
                    "invalid_path",
                    format!("Hosted mutation record path is invalid: {error}"),
                )
            })?;
        let destination = path.under(root);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(stage_io_error)?;
        }
        fs::write(destination, &record.document).map_err(stage_io_error)?;
    }
    Ok(())
}

fn materialize_catalog_resources(
    root: &std::path::Path,
    catalog: &CompiledCatalog,
) -> Result<(), CatalogError> {
    fs::write(root.join("mdbase.yaml"), &catalog.configuration_document).map_err(stage_io_error)?;
    for resource in &catalog.type_resources {
        let path = root.join(&resource.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(stage_io_error)?;
        }
        let mut definition = resource.definition.clone();
        if let Some(schema) = definition.get_mut("schema").and_then(Value::as_object_mut) {
            schema.remove("$ref");
            schema.insert("value".to_string(), resource.schema.clone());
        }
        let yaml = serde_yaml::to_string(&definition).map_err(|error| {
            mutation_error(
                "hosted_mutation_stage_failed",
                format!("Hosted type resource could not be staged: {error}"),
            )
        })?;
        fs::write(path, format!("---\n{yaml}---\n")).map_err(stage_io_error)?;
    }
    Ok(())
}

fn mutation_error(code: impl Into<String>, message: impl Into<String>) -> CatalogError {
    CatalogError {
        code: code.into(),
        message: message.into(),
    }
}

fn stage_io_error(error: std::io::Error) -> CatalogError {
    mutation_error(
        "hosted_mutation_stage_failed",
        format!("Hosted mutation stage could not be written: {error}"),
    )
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::runtime::{CatalogInput, ResolvedTypeResource};

    fn catalog() -> CompiledCatalog {
        CompiledCatalog::compile(CatalogInput {
            resource_revision: "catalog-1".to_string(),
            configuration_document: "spec_version: 0.3.0\nsettings:\n  default_validation: warn\n"
                .to_string(),
            types: vec![ResolvedTypeResource {
                path: "_types/task.md".to_string(),
                revision: "type-1".to_string(),
                definition: json!({
                    "kind": "mdbase.type",
                    "name": "task",
                    "version": 1,
                    "match": {"path_glob": "tasks/*.md"},
                    "schema": {
                        "dialect": "json-schema-2020-12",
                        "value": {"type": "object"}
                    }
                }),
                schema: json!({"type": "object"}),
            }],
            contracts: Vec::new(),
        })
        .unwrap()
    }

    fn record(stable_id: &str, path: &str, document: &str) -> CanonicalRecordInput {
        CanonicalRecordInput {
            stable_id: Some(stable_id.to_string()),
            path: path.to_string(),
            document: document.to_string(),
            file_size: document.len() as u64,
            file_mtime: None,
        }
    }

    #[test]
    fn plans_create_update_and_delete_from_bounded_exact_context() {
        let catalog = catalog();
        assert!(!catalog
            .hosted_mutation_requires_incoming_context("rename", &json!({"update_refs": false})));
        assert!(catalog.hosted_mutation_requires_incoming_context("rename", &json!({})));
        let created = catalog
            .plan_hosted_mutation(&HostedMutationRequest {
                operation: "create".to_string(),
                primary_stable_id: "record-1".to_string(),
                input: json!({
                    "path": "tasks/one.md",
                    "type": "task",
                    "frontmatter": {"status": "open"},
                    "body": "Hello\n"
                }),
                records: Vec::new(),
            })
            .unwrap();
        assert!(created.result.valid);
        let created_record = created.changes[0].record.clone().unwrap();
        assert!(created_record.document.contains("status: open"));

        let revision = catalog.classify_record(&created_record).unwrap().revision;
        let updated = catalog
            .plan_hosted_mutation(&HostedMutationRequest {
                operation: "update".to_string(),
                primary_stable_id: "record-1".to_string(),
                input: json!({"patch": {"status": "done"}, "if_revision": revision}),
                records: vec![created_record.clone()],
            })
            .unwrap();
        assert!(updated.result.valid);
        assert!(updated.changes[0]
            .record
            .as_ref()
            .unwrap()
            .document
            .contains("status: done"));

        let deleted = catalog
            .plan_hosted_mutation(&HostedMutationRequest {
                operation: "delete".to_string(),
                primary_stable_id: "record-1".to_string(),
                input: json!({"if_revision": revision}),
                records: vec![created_record],
            })
            .unwrap();
        assert!(deleted.result.valid);
        assert!(deleted.changes[0].record.is_none());
    }

    #[test]
    fn typed_batch_returns_exact_created_and_updated_record_evidence() {
        let catalog = catalog();
        let existing = record(
            "record-1",
            "tasks/one.md",
            "---\nstatus: open\n---\nBefore\n",
        );
        let revision = catalog.classify_record(&existing).unwrap().revision;
        let planned = catalog
            .plan_hosted_mutation_typed(&HostedMutationRequest {
                operation: "batch".to_string(),
                primary_stable_id: "record-2".to_string(),
                input: json!({
                    "operations": [
                        {"kind": "update", "input": {"path": "tasks/one.md", "fields": {"status": "done"}, "if_revision": revision}},
                        {"kind": "create", "stable_id": "record-2", "input": {"path": "tasks/two.md", "type": "task", "body": "Created"}}
                    ]
                }),
                records: vec![existing],
            })
            .unwrap();
        assert!(planned.operation.valid, "{:#?}", planned.operation);
        assert!(matches!(
            planned.operation.value,
            super::super::CanonicalOperationValue::Batch(Some(_))
        ));
        assert_eq!(planned.changes.len(), 2);
        assert!(matches!(planned.change_set, ChangeSet::Exact(_)));
        assert!(planned.changes.iter().all(|change| change.after.is_some()));
    }

    #[test]
    fn typed_batch_path_evidence_handles_dependencies_repeated_paths_and_rename_chains() {
        use crate::api::{
            BatchItemResult, BatchOperationResult, BatchRenameResult, BatchResult, RecordDocument,
            RecordFile, RenameResult,
        };

        fn document(path: &str) -> RecordDocument {
            RecordDocument {
                path: CollectionPath::new(path).unwrap(),
                revision: Revision::parse(crate::v03::revision(path.as_bytes())).unwrap(),
                types: vec!["task".to_string()],
                frontmatter: json!({}),
                effective_frontmatter: json!({}),
                body: String::new(),
                document: Some(String::new()),
                file: RecordFile {
                    name: path.rsplit('/').next().unwrap().to_string(),
                    folder: path
                        .rsplit_once('/')
                        .map_or("", |value| value.0)
                        .to_string(),
                    size: 0,
                    mtime: String::new(),
                },
            }
        }
        fn item(index: usize, kind: &str, result: BatchOperationResult) -> BatchItemResult {
            BatchItemResult {
                index,
                kind: kind.to_string(),
                valid: true,
                result,
                diagnostics: Vec::new(),
            }
        }
        fn rename(index: usize, from: &str, to: &str) -> BatchItemResult {
            item(
                index,
                "rename",
                BatchOperationResult::Rename(BatchRenameResult {
                    result: RenameResult {
                        document: document(to),
                        from: CollectionPath::new(from).unwrap(),
                        to: CollectionPath::new(to).unwrap(),
                        references_updated: Vec::new(),
                    },
                    partial_updates: None,
                }),
            )
        }

        let request = HostedMutationRequest {
            operation: "batch".to_string(),
            primary_stable_id: "new-id".to_string(),
            input: json!({
                "allow_partial": true,
                "operations": [
                    {"kind": "create", "stable_id": "new-id", "input": {"type": "task"}},
                    {"kind": "update", "input": {"path": "generated.md"}},
                    {"kind": "rename", "input": {"from": "generated.md", "to": "generated-one.md"}},
                    {"kind": "create", "stable_id": "failed-generated", "input": {"type": "task"}},
                    {"kind": "rename", "input": {"from": "generated-one.md", "to": "generated-two.md"}},
                    {"kind": "rename", "input": {"from": "existing.md", "to": "existing-one.md"}},
                    {"kind": "rename", "input": {"from": "existing-one.md", "to": "existing-two.md"}},
                    {"kind": "delete", "input": {"path": "missing.md"}}
                ]
            }),
            records: Vec::new(),
        };
        let failed = BatchItemResult {
            index: 3,
            kind: "create".to_string(),
            valid: false,
            result: BatchOperationResult::default(),
            diagnostics: Vec::new(),
        };
        let failed_delete = BatchItemResult {
            index: 7,
            kind: "delete".to_string(),
            valid: false,
            result: BatchOperationResult::default(),
            diagnostics: Vec::new(),
        };
        let batch = BatchResult {
            operations: vec![
                item(
                    0,
                    "create",
                    BatchOperationResult::Record(document("generated.md")),
                ),
                item(
                    1,
                    "update",
                    BatchOperationResult::Record(document("generated.md")),
                ),
                rename(2, "generated.md", "generated-one.md"),
                failed,
                rename(4, "generated-one.md", "generated-two.md"),
                rename(5, "existing.md", "existing-one.md"),
                rename(6, "existing-one.md", "existing-two.md"),
                failed_delete,
            ],
            succeeded: 6,
            failed: 2,
            preflight: false,
            dry_run: false,
        };
        let before = BTreeMap::from([("existing-id".to_string(), "existing.md".to_string())]);
        let applied = batch_after_paths(&request, &before, &batch).unwrap();
        assert_eq!(
            applied.after_by_stable,
            BTreeMap::from([
                ("existing-id".to_string(), "existing-two.md".to_string()),
                ("new-id".to_string(), "generated-two.md".to_string()),
            ])
        );
        assert_eq!(
            applied.affected,
            BTreeSet::from(["existing-id".to_string(), "new-id".to_string()])
        );
        assert!(!applied.after_by_stable.contains_key("failed-generated"));
    }

    #[test]
    fn partial_batch_changes_follow_only_successful_typed_items_in_request_order() {
        let catalog = catalog();
        let a = record("record-a", "tasks/a.md", "---\nstatus: open\n---\nA\n");
        let b = record("record-b", "tasks/b.md", "---\nstatus: open\n---\nB\n");
        let c = record("record-c", "tasks/c.md", "---\nstatus: open\n---\nC\n");
        let d = record("record-d", "tasks/d.md", "---\nstatus: open\n---\nD\n");
        let e = record("record-e", "tasks/e.md", "---\nstatus: open\n---\nE\n");
        let a_revision = catalog.classify_record(&a).unwrap().revision;
        let stale = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let planned = catalog
            .plan_hosted_mutation_typed(&HostedMutationRequest {
                operation: "batch".to_string(),
                primary_stable_id: "record-new".to_string(),
                input: json!({
                    "allow_partial": true,
                    "operations": [
                        {"kind": "create", "stable_id": "record-new", "input": {"path": "tasks/new.md", "type": "task", "frontmatter": {"status": "new"}, "body": "New"}},
                        {"kind": "create", "stable_id": "failed-create", "input": {"path": "tasks/d.md", "type": "task"}},
                        {"kind": "update", "input": {"path": "tasks/a.md", "fields": {"status": "done"}, "if_revision": a_revision}},
                        {"kind": "update", "input": {"path": "tasks/e.md", "fields": {"status": "wrong"}, "if_revision": stale}},
                        {"kind": "delete", "input": {"path": "tasks/missing.md"}},
                        {"kind": "delete", "input": {"path": "tasks/c.md"}},
                        {"kind": "rename", "input": {"from": "tasks/b.md", "to": "tasks/b-two.md", "update_refs": false}},
                        {"kind": "rename", "input": {"from": "tasks/missing-rename.md", "to": "tasks/nope.md", "update_refs": false}},
                        {"kind": "create", "stable_id": "failed-generated", "input": {"type": "task", "frontmatter": {"id": "generated-but-invalid"}}}
                    ]
                }),
                records: vec![a.clone(), b.clone(), c.clone(), d, e],
            })
            .unwrap();

        let super::super::CanonicalOperationValue::Batch(Some(batch)) = &planned.operation.value
        else {
            panic!(
                "partial batch must retain typed item evidence: {:#?}",
                planned.operation
            )
        };
        assert_eq!(batch.operations.len(), 9);
        assert_eq!(batch.succeeded, 4);
        assert_eq!(batch.failed, 5);
        assert_eq!(planned.changes.len(), 4);
        assert_eq!(
            planned.change_set,
            ChangeSet::Exact(
                ChangeBatch::new(
                    planned
                        .changes
                        .iter()
                        .cloned()
                        .map(|change| CanonicalChange::Record(change.change))
                        .collect()
                )
                .unwrap()
            )
        );
        assert!(planned.changes.iter().all(|change| !matches!(
            change.stable_id.as_str(),
            "failed-create" | "failed-generated"
        )));

        let created = planned
            .changes
            .iter()
            .find(|change| change.stable_id == "record-new")
            .unwrap();
        assert!(created.before.is_none());
        let created_after = created.after.as_ref().unwrap();
        assert_eq!(created_after.path, "tasks/new.md");
        assert!(created_after.document.contains("status: new"));
        assert_eq!(
            created.change.after_revision.as_ref().unwrap().to_string(),
            catalog.classify_record(created_after).unwrap().revision
        );

        let updated = planned
            .changes
            .iter()
            .find(|change| change.stable_id == "record-a")
            .unwrap();
        assert_eq!(updated.before.as_ref(), Some(&a));
        assert!(updated
            .after
            .as_ref()
            .unwrap()
            .document
            .contains("status: done"));
        assert_eq!(
            updated.change.before_revision.as_ref().unwrap().to_string(),
            a_revision
        );

        let deleted = planned
            .changes
            .iter()
            .find(|change| change.stable_id == "record-c")
            .unwrap();
        assert_eq!(deleted.before.as_ref(), Some(&c));
        assert!(deleted.after.is_none());

        let renamed = planned
            .changes
            .iter()
            .find(|change| change.stable_id == "record-b")
            .unwrap();
        assert_eq!(renamed.before.as_ref(), Some(&b));
        assert_eq!(renamed.after.as_ref().unwrap().path, "tasks/b-two.md");
        assert_eq!(renamed.change.kind, RecordChangeKind::Renamed);
        assert_eq!(renamed.change.from.as_ref().unwrap().as_str(), "tasks/b.md");
    }

    #[test]
    fn rename_rewrites_only_supplied_incoming_reference_records() {
        let catalog = catalog();
        let primary = record("record-1", "tasks/one.md", "---\ntitle: One\n---\nBody\n");
        let revision = catalog.classify_record(&primary).unwrap().revision;
        let reference = record(
            "record-2",
            "notes/ref.md",
            "---\ntitle: Ref\nlinks:\n  nested:\n    - '[[../tasks/one]]'\nsummary: 'See [[../tasks/one]] and [[../tasks/one#part]].'\n---\nSee [[../tasks/one]].\n",
        );
        let renamed = catalog
            .plan_hosted_mutation(&HostedMutationRequest {
                operation: "rename".to_string(),
                primary_stable_id: "record-1".to_string(),
                input: json!({
                    "path": "tasks/moved.md",
                    "if_revision": revision,
                    "update_refs": true
                }),
                records: vec![primary, reference],
            })
            .unwrap();
        assert!(renamed.result.valid);
        assert_eq!(renamed.changes.len(), 2);
        assert_eq!(
            renamed.changes[0].record.as_ref().unwrap().path,
            "tasks/moved.md"
        );
        assert!(renamed.changes[1]
            .record
            .as_ref()
            .unwrap()
            .document
            .contains("moved"));
        assert!(!renamed.changes[1]
            .record
            .as_ref()
            .unwrap()
            .document
            .contains("tasks/one"));
    }
}
