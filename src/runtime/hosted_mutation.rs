//! Bounded exact-record mutation planning for hosted providers.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::v03::OperationResult;
use crate::{Collection, SpecProfile};

use super::{CanonicalRecordInput, CatalogError, CompiledCatalog};

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedMutationPlan {
    pub result: OperationResult,
    pub primary_stable_id: String,
    pub changes: Vec<HostedMutationChange>,
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
    pub fn plan_hosted_mutation(
        &self,
        request: &HostedMutationRequest,
    ) -> Result<HostedMutationPlan, CatalogError> {
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
            "create" | "update" | "delete" | "rename"
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
            _ => unreachable!(),
        }
        let result = collection
            .v03_operations()
            .expect("compiled catalogs always use the canonical profile")
            .execute_staged_mutation(&request.operation, &Value::Object(input));
        if !result.valid {
            return Ok(HostedMutationPlan {
                result,
                primary_stable_id: request.primary_stable_id.clone(),
                changes: Vec::new(),
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
                _ => unreachable!(),
            }
            collection
                .v03_operations()
                .expect("compiled catalogs always use the canonical profile")
                .execute_staged_mutation(&request.operation, &Value::Object(committed_input))
        } else {
            result.clone()
        };
        if !mutation_result.valid {
            return Err(mutation_error(
                "hosted_mutation_dry_run_mismatch",
                "Canonical dry-run and disposable mutation execution disagreed.",
            ));
        }

        let mut affected = BTreeSet::from([request.primary_stable_id.clone()]);
        if let Some(references) = mutation_result
            .result
            .get("references_updated")
            .and_then(Value::as_array)
        {
            for reference in references {
                let path = reference
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        mutation_error(
                            "hosted_mutation_plan_incomplete",
                            "Canonical reference update omitted its source path.",
                        )
                    })?;
                let stable_id = stable_by_path.get(path).ok_or_else(|| {
                    mutation_error(
                        "hosted_mutation_plan_incomplete",
                        "Canonical mutation changed a record outside its supplied context.",
                    )
                })?;
                affected.insert(stable_id.clone());
            }
        }

        let primary_after = match request.operation.as_str() {
            "delete" => None,
            "rename" => result.result.get("to").and_then(Value::as_str),
            _ => result
                .result
                .get("path")
                .and_then(Value::as_str)
                .or(primary_before.as_deref()),
        };
        let mut changes = Vec::new();
        for stable_id in affected {
            let before_path = before_by_stable.get(&stable_id).cloned();
            let after_path = if stable_id == request.primary_stable_id {
                primary_after.map(str::to_string)
            } else {
                before_path.clone()
            };
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
            changes.push(HostedMutationChange {
                stable_id,
                before_path,
                record,
            });
        }
        changes.sort_by_key(|change| change.stable_id != request.primary_stable_id);
        Ok(HostedMutationPlan {
            result,
            primary_stable_id: request.primary_stable_id.clone(),
            changes,
        })
    }
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
