//! Provider-neutral bounded point-validation planning for hosted authorities.

use std::collections::BTreeSet;
use std::fs;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::v03::OperationResult;
use crate::{Collection, SpecProfile};

use super::{
    CanonicalRecordInput, CatalogError, CompiledCatalog, ResolutionLookupKey, SemanticProjection,
    SemanticProjectionFacts,
};

const MAX_HOSTED_VALIDATION_RECORDS: usize = 2_001;
const MAX_HOSTED_VALIDATION_EXACT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedValidationRequirementKind {
    Identity,
    UniqueField,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HostedValidationRequirement {
    pub kind: HostedValidationRequirementKind,
    pub type_name: String,
    pub field_reference: String,
    pub comparable_value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedValidationPlan {
    pub catalog_revision: String,
    pub target_stable_id: String,
    pub target_path: String,
    pub input: Value,
    pub requirements: Vec<HostedValidationRequirement>,
    #[serde(default)]
    pub resolution_lookups: Vec<ResolutionLookupKey>,
}

impl HostedValidationPlan {
    /// Return true only when a current semantic projection can conflict with
    /// at least one canonical uniqueness requirement. False is a pruning proof.
    pub fn projection_may_conflict(&self, projection: &SemanticProjection) -> bool {
        self.facts_may_conflict(&projection.facts)
    }

    pub fn facts_may_conflict(&self, facts: &SemanticProjectionFacts) -> bool {
        self.requirements.iter().any(|requirement| {
            crate::field_references::get_value(
                &Value::Object(facts.effective_frontmatter.clone()),
                &requirement.field_reference,
            )
            .and_then(comparable_value)
            .as_deref()
                == Some(requirement.comparable_value.as_str())
        })
    }
}

impl CompiledCatalog {
    /// Compile cross-record uniqueness requirements for one exact validation
    /// target. The host may stream current projections through the returned
    /// plan and fetch exact records only for possible conflicts.
    pub fn plan_hosted_validation(
        &self,
        input: &Value,
        target: &CanonicalRecordInput,
    ) -> Result<HostedValidationPlan, CatalogError> {
        let target_stable_id = target.stable_id.clone().ok_or_else(|| {
            validation_error(
                "hosted_validation_identity_required",
                "Hosted validation requires stable target identity.",
            )
        })?;
        if input.get("path").and_then(Value::as_str) != Some(target.path.as_str()) {
            return Err(validation_error(
                "hosted_validation_path_mismatch",
                "Hosted validation input must bind the exact target path.",
            ));
        }
        let classified = self.classify_record(target)?;
        let persisted = match input.get("frontmatter") {
            Some(Value::Object(frontmatter)) => frontmatter.clone(),
            Some(_) => serde_json::Map::new(),
            None => classified.frontmatter,
        };
        let persisted = Value::Object(persisted);
        let types = self
            .collection
            .determine_types_for_path(&persisted, Some(&target.path));
        let effective = self.collection.apply_defaults(&persisted, &types);
        let effective = self.collection.coerce_types(&effective, &types);
        let mut requirements = BTreeSet::new();
        for type_name in &types {
            if let Some(value) =
                crate::field_references::get_value(&effective, &self.collection.settings().id_field)
                    .and_then(comparable_value)
            {
                requirements.insert(HostedValidationRequirement {
                    kind: HostedValidationRequirementKind::Identity,
                    type_name: type_name.clone(),
                    field_reference: self.collection.settings().id_field.clone(),
                    comparable_value: value,
                });
            }
            let Some(type_definition) = self.collection.types.get(type_name) else {
                continue;
            };
            for field_reference in
                crate::validation::validator::unique_field_references(type_definition)
            {
                if let Some(value) =
                    crate::field_references::get_value(&effective, &field_reference)
                        .and_then(comparable_value)
                {
                    requirements.insert(HostedValidationRequirement {
                        kind: HostedValidationRequirementKind::UniqueField,
                        type_name: type_name.clone(),
                        field_reference,
                        comparable_value: value,
                    });
                }
            }
        }
        let mut resolution_lookups = self
            .collection
            .validation_resolution_targets(&effective, &types, &target.path)
            .into_iter()
            .flat_map(|target| self.resolution_lookup_alternatives(&target))
            .collect::<Vec<_>>();
        resolution_lookups.sort();
        resolution_lookups.dedup();
        Ok(HostedValidationPlan {
            catalog_revision: self.resource_revision().to_string(),
            target_stable_id,
            target_path: target.path.clone(),
            input: input.clone(),
            requirements: requirements.into_iter().collect(),
            resolution_lookups,
        })
    }

    /// Execute canonical validation against a bounded caller-supplied exact
    /// neighborhood. The host supplies uniqueness and link-resolution
    /// candidates selected from a consistent projection snapshot.
    pub fn execute_hosted_validation_typed(
        &self,
        plan: &HostedValidationPlan,
        records: &[CanonicalRecordInput],
    ) -> Result<super::CanonicalOperationOutcome, CatalogError> {
        if plan.catalog_revision != self.resource_revision() {
            return Err(validation_error(
                "hosted_validation_catalog_mismatch",
                "Hosted validation plan does not bind the compiled catalog.",
            ));
        }
        if records.len() > MAX_HOSTED_VALIDATION_RECORDS {
            return Err(validation_error(
                "hosted_validation_context_budget_exceeded",
                "Hosted validation context exceeds its exact-record budget.",
            ));
        }
        let exact_bytes = records.iter().try_fold(0_usize, |total, record| {
            total.checked_add(record.document.len())
        });
        if exact_bytes.is_none_or(|bytes| bytes > MAX_HOSTED_VALIDATION_EXACT_BYTES) {
            return Err(validation_error(
                "hosted_validation_context_byte_budget_exceeded",
                "Hosted validation context exceeds its exact-byte budget.",
            ));
        }
        let directory = tempfile::tempdir().map_err(validation_stage_error)?;
        let mut stable_ids = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for record in records {
            let stable_id = record.stable_id.as_ref().ok_or_else(|| {
                validation_error(
                    "hosted_validation_identity_required",
                    "Every hosted validation context record requires stable identity.",
                )
            })?;
            let path = self
                .collection
                .validate_record_path(&record.path)
                .map_err(|error| validation_error("invalid_path", error.to_string()))?;
            if !stable_ids.insert(stable_id.clone()) || !paths.insert(path.to_string()) {
                return Err(validation_error(
                    "hosted_validation_context_ambiguous",
                    "Hosted validation context contains duplicate path or stable identity.",
                ));
            }
            let destination = path.under(directory.path());
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(validation_stage_error)?;
            }
            fs::write(destination, &record.document).map_err(validation_stage_error)?;
        }
        if !stable_ids.contains(&plan.target_stable_id) || !paths.contains(&plan.target_path) {
            return Err(validation_error(
                "hosted_validation_target_missing",
                "Hosted validation context omitted its exact target record.",
            ));
        }
        let data_contracts = crate::data_contracts::DataContractRegistry::load_resolved(
            self.contracts.clone(),
            &self.collection.types,
        )
        .map_err(|error| validation_error(error.code, error.message))?;
        let collection = Collection {
            root: directory.path().to_path_buf(),
            spec_profile: SpecProfile::V03,
            settings: self.collection.settings.clone(),
            config_extensions: self.collection.config_extensions.clone(),
            types: self.collection.types.clone(),
            type_plans: self.collection.type_plans.clone(),
            type_warnings: self.collection.type_warnings.clone(),
            data_contracts,
            root_capability: Collection::capability_for_root(directory.path())
                .map_err(validation_stage_error)?,
        };
        let result = collection
            .v03_operations()
            .expect("compiled catalogs always use the canonical profile")
            .validate(&plan.input);
        super::CanonicalOperationOutcome::hosted_wire_edge(super::OperationKind::Validate, result)
            .map_err(|error| validation_error(error.code(), error.to_string()))
    }

    /// Compatibility projection for current Connect callers. Validation's
    /// value remains explicitly wire-only because no typed validation model
    /// exists; diagnostics and envelope state are typed.
    #[deprecated(note = "use execute_hosted_validation_typed")]
    pub fn execute_hosted_validation(
        &self,
        plan: &HostedValidationPlan,
        records: &[CanonicalRecordInput],
    ) -> Result<OperationResult, CatalogError> {
        Ok(self
            .execute_hosted_validation_typed(plan, records)?
            .to_v03())
    }
}

fn comparable_value(value: &Value) -> Option<String> {
    if value.is_null() {
        None
    } else {
        Some(
            value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string()),
        )
    }
}

fn validation_error(code: impl Into<String>, message: impl Into<String>) -> CatalogError {
    CatalogError {
        code: code.into(),
        message: message.into(),
    }
}

fn validation_stage_error(error: std::io::Error) -> CatalogError {
    validation_error(
        "hosted_validation_stage_failed",
        format!("Hosted validation stage could not be written: {error}"),
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
            configuration_document: "spec_version: 0.3.0\nsettings:\n  id_field: id\n".to_string(),
            types: vec![ResolvedTypeResource {
                path: "_types/task.md".to_string(),
                revision: "type-1".to_string(),
                definition: json!({
                    "kind": "mdbase.type",
                    "name": "task",
                    "version": 1,
                    "match": {"path_glob": "tasks/*.md"},
                    "schema": {"dialect": "json-schema-2020-12", "value": {
                        "type": "object",
                        "properties": {
                            "slug": {"type": "string"},
                            "related": {"type": "string"}
                        }
                    }},
                    "collection": {
                        "unique": [{"field": "slug"}],
                        "links": {"related": {"validate_exists": true}}
                    }
                }),
                schema: json!({"type": "object"}),
            }],
            contracts: Vec::new(),
        })
        .unwrap()
    }

    fn record(id: &str, path: &str, document: &str) -> CanonicalRecordInput {
        CanonicalRecordInput {
            stable_id: Some(id.to_string()),
            path: path.to_string(),
            document: document.to_string(),
            file_size: document.len() as u64,
            file_mtime: None,
        }
    }

    #[test]
    fn plans_projection_candidates_and_validates_exact_conflicts() {
        let catalog = catalog();
        let target = record(
            "one",
            "tasks/one.md",
            "---\nid: task-1\nslug: shared\nrelated: '[[task-2]]'\n---\nOne\n",
        );
        let conflict = record(
            "two",
            "tasks/two.md",
            "---\nid: task-2\nslug: shared\n---\nTwo\n",
        );
        let unrelated = record(
            "three",
            "tasks/three.md",
            "---\nid: task-3\nslug: other\n---\nThree\n",
        );
        let plan = catalog
            .plan_hosted_validation(&json!({"path": target.path}), &target)
            .unwrap();
        assert!(plan.resolution_lookups.iter().any(|lookup| {
            lookup.kind == crate::runtime::RecordResolutionKeyKind::Id && lookup.value == "task-2"
        }));
        let conflict_projection = catalog.project_record(&conflict).unwrap();
        let unrelated_projection = catalog.project_record(&unrelated).unwrap();
        assert!(plan.projection_may_conflict(
            &catalog
                .finalize_projection(
                    conflict_projection.clone(),
                    crate::runtime::ResolvedRecordStructure {
                        schema_version: conflict_projection.structure.schema_version.clone(),
                        path: conflict_projection.structure.path.clone(),
                        structural_digest: conflict_projection.structure.structural_digest.clone(),
                        body_tags: conflict_projection.structure.body_tags.clone(),
                        body_links: conflict_projection.structure.body_links.clone(),
                        body_embeds: conflict_projection.structure.body_embeds.clone(),
                        occurrences: Vec::new(),
                    },
                )
                .unwrap()
        ));
        assert!(!plan.projection_may_conflict(
            &catalog
                .finalize_projection(
                    unrelated_projection.clone(),
                    crate::runtime::ResolvedRecordStructure {
                        schema_version: unrelated_projection.structure.schema_version.clone(),
                        path: unrelated_projection.structure.path.clone(),
                        structural_digest: unrelated_projection.structure.structural_digest.clone(),
                        body_tags: unrelated_projection.structure.body_tags.clone(),
                        body_links: unrelated_projection.structure.body_links.clone(),
                        body_embeds: unrelated_projection.structure.body_embeds.clone(),
                        occurrences: Vec::new(),
                    },
                )
                .unwrap()
        ));
        let result = catalog
            .execute_hosted_validation(&plan, &[target, conflict])
            .unwrap();
        assert!(!result.valid);
        assert!(result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "duplicate_value"));
    }
}
