//! Provider-neutral bounded point-validation planning for hosted authorities.

use std::collections::BTreeSet;
use std::fs;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::v03::OperationResult;
use crate::validation::cross_record::unique_comparable_value;
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

/// Records a hosted write's canonical validation compared its written records
/// against. A plan holds only when its context held every other record that
/// shares one of these uniqueness keys or answers one of these link lookups.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedWriteContext {
    pub uniqueness_keys: Vec<super::UniquenessKey>,
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
            .and_then(unique_comparable_value)
            .as_deref()
                == Some(requirement.comparable_value.as_str())
        })
    }
}

impl CompiledCatalog {
    /// What canonical write validation compared a hosted write's records
    /// against. Empty unless the write is validated (`validation: error`).
    pub(crate) fn hosted_write_context<'a>(
        &self,
        operation: &str,
        written: impl IntoIterator<Item = &'a crate::api::RecordDocument>,
    ) -> HostedWriteContext {
        let mut context = HostedWriteContext::default();
        if self.collection.settings.default_validation != "error"
            || !matches!(operation, "create" | "update" | "batch")
        {
            return context;
        }
        for record in written {
            let types = record
                .types
                .iter()
                .map(|name| name.to_lowercase())
                .collect::<Vec<_>>();
            context
                .uniqueness_keys
                .extend(self.uniqueness_requirements(&record.effective_frontmatter, &types));
            context.resolution_lookups.extend(
                self.collection
                    .validation_resolution_targets(
                        &record.effective_frontmatter,
                        &types,
                        record.path.as_str(),
                    )
                    .iter()
                    .flat_map(|target| self.resolution_lookup_alternatives(target)),
            );
        }
        context.uniqueness_keys.sort();
        context.uniqueness_keys.dedup();
        context.resolution_lookups.sort();
        context.resolution_lookups.dedup();
        context
    }

    /// The context of a rejected hosted write: a batch rejected for its links
    /// still reports each item's resulting record.
    pub(crate) fn rejected_write_context(
        &self,
        operation: &str,
        outcome: &super::CanonicalOperationOutcome,
    ) -> HostedWriteContext {
        let super::CanonicalOperationValue::Batch(Some(batch)) = &outcome.value else {
            return HostedWriteContext::default();
        };
        self.hosted_write_context(
            operation,
            batch
                .operations
                .iter()
                .filter_map(|item| match &item.result {
                    crate::api::BatchOperationResult::Record(record) => Some(record),
                    _ => None,
                }),
        )
    }

    /// The staged write's context, and its rejection when a written record's
    /// required links do not resolve among the staged records. Staged create
    /// and update check uniqueness but not links, which only the complete
    /// context can answer.
    pub(crate) fn hosted_write_verdict(
        &self,
        staged: &Collection,
        operation: &str,
        changes: &[super::HostedRecordChange],
    ) -> Result<(HostedWriteContext, Option<super::CanonicalOperationOutcome>), CatalogError> {
        let written = changes
            .iter()
            .filter_map(|change| change.after.as_ref())
            .collect::<Vec<_>>();
        let context = self.hosted_write_context(operation, written.iter().copied());
        if context.resolution_lookups.is_empty() {
            return Ok((context, None));
        }
        let issues = staged
            .written_link_issues(&written)
            .map_err(|error| validation_error("hosted_mutation_stage_failed", error.to_string()))?;
        if issues.is_empty() {
            return Ok((context, None));
        }
        let kind = operation
            .parse::<super::OperationKind>()
            .map_err(|error| validation_error("unsupported_hosted_mutation", error.to_string()))?;
        let diagnostics = issues
            .iter()
            .map(|issue| crate::mutation::diagnostic_from_issue(issue).into())
            .collect();
        Ok((
            context,
            Some(super::CanonicalOperationOutcome::invalid(kind, diagnostics)),
        ))
    }

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
                    .and_then(unique_comparable_value)
            {
                requirements.insert(HostedValidationRequirement {
                    kind: HostedValidationRequirementKind::Identity,
                    type_name: type_name.clone(),
                    field_reference: self.collection.settings().id_field.clone(),
                    comparable_value: value,
                });
            }
        }
        for (key, type_name) in self.collection.unique_requirements(&effective, &types) {
            requirements.insert(HostedValidationRequirement {
                kind: HostedValidationRequirementKind::UniqueField,
                type_name,
                field_reference: key.field,
                comparable_value: key.value,
            });
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
            sequence_floor: std::collections::HashMap::new(),
            authority: crate::collection_root::CollectionRoot::acquire(directory.path())
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

    fn unique_slug_catalog(validation: &str) -> CompiledCatalog {
        CompiledCatalog::compile(CatalogInput {
            resource_revision: "catalog-unique".to_string(),
            configuration_document: format!(
                "spec_version: 0.3.0\nsettings:\n  validation: {validation}\n"
            ),
            types: vec![ResolvedTypeResource {
                path: "_types/note.md".to_string(),
                revision: "type-1".to_string(),
                definition: json!({
                    "kind": "mdbase.type",
                    "name": "note",
                    "version": 1,
                    "match": {"path_glob": "notes/*.md"},
                    "schema": {"dialect": "json-schema-2020-12", "value": {
                        "type": "object",
                        "properties": {"slug": {"type": "string"}}
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

    fn create_note(
        path: &str,
        slug: &str,
        records: Vec<CanonicalRecordInput>,
    ) -> crate::runtime::HostedMutationRequest {
        crate::runtime::HostedMutationRequest {
            operation: "create".to_string(),
            primary_stable_id: format!("id-{path}"),
            input: json!({"path": path, "type": "note", "frontmatter": {"slug": slug}}),
            records,
        }
    }

    fn slug_key(value: &str) -> crate::runtime::UniquenessKey {
        crate::runtime::UniquenessKey {
            set: "type:note".to_string(),
            field_reference: "slug".to_string(),
            comparable_value: value.to_string(),
        }
    }

    #[test]
    fn plans_report_the_uniqueness_keys_their_writes_were_validated_against() {
        let catalog = unique_slug_catalog("error");
        let existing = record(
            "existing",
            "notes/a.md",
            "---\ntype: note\nslug: same\n---\n",
        );

        let without_context = catalog
            .plan_hosted_mutation_typed(&create_note("notes/b.md", "same", Vec::new()))
            .unwrap();
        assert!(without_context.operation.valid);
        assert_eq!(
            without_context.context_requirements.uniqueness_keys,
            vec![slug_key("same")]
        );

        let with_conflict = catalog
            .plan_hosted_mutation_typed(&create_note("notes/b.md", "same", vec![existing.clone()]))
            .unwrap();
        assert!(!with_conflict.operation.valid);
        assert!(with_conflict.changes.is_empty());
        assert!(with_conflict
            .context_requirements
            .uniqueness_keys
            .is_empty());

        let update = catalog
            .plan_hosted_mutation_typed(&crate::runtime::HostedMutationRequest {
                operation: "update".to_string(),
                primary_stable_id: "existing".to_string(),
                input: json!({"patch": {"slug": "renamed"}}),
                records: vec![existing.clone()],
            })
            .unwrap();
        assert!(update.operation.valid);
        assert_eq!(
            update.context_requirements.uniqueness_keys,
            vec![slug_key("renamed")]
        );

        let projection = catalog.project_record(&existing).unwrap();
        assert_eq!(projection.facts.uniqueness_keys, vec![slug_key("same")]);

        let delete = catalog
            .plan_hosted_mutation_typed(&crate::runtime::HostedMutationRequest {
                operation: "delete".to_string(),
                primary_stable_id: "existing".to_string(),
                input: json!({}),
                records: vec![existing],
            })
            .unwrap();
        assert!(delete.context_requirements.uniqueness_keys.is_empty());
    }

    #[test]
    fn plans_without_error_validation_report_no_uniqueness_requirements() {
        let plan = unique_slug_catalog("warn")
            .plan_hosted_mutation_typed(&create_note("notes/b.md", "same", Vec::new()))
            .unwrap();
        assert!(plan.operation.valid);
        assert!(plan.context_requirements.uniqueness_keys.is_empty());
    }

    #[test]
    fn plans_report_link_lookups_and_resolve_them_against_staged_targets() {
        let catalog = unique_slug_catalog("error");
        let target = record("target", "notes/target.md", "---\ntype: note\n---\n");
        let linking = |records| crate::runtime::HostedMutationRequest {
            operation: "create".to_string(),
            primary_stable_id: "source".to_string(),
            input: json!({
                "path": "notes/source.md",
                "type": "note",
                "frontmatter": {"related": "[[notes/target]]"}
            }),
            records,
        };

        let unstaged = catalog
            .plan_hosted_mutation_typed(&linking(Vec::new()))
            .unwrap();
        assert!(!unstaged.operation.valid);
        assert!(unstaged.changes.is_empty());
        assert!(!unstaged.context_requirements.resolution_lookups.is_empty());

        let staged = catalog
            .plan_hosted_mutation_typed(&linking(vec![target.clone()]))
            .unwrap();
        assert!(staged.operation.valid, "{:?}", staged.operation);
        assert_eq!(staged.changes.len(), 1);
        assert_eq!(
            staged.context_requirements.resolution_lookups,
            unstaged.context_requirements.resolution_lookups
        );

        let mut dangling = linking(vec![target]);
        dangling.input["frontmatter"]["related"] = json!("[[notes/missing]]");
        let dangling = catalog.plan_hosted_mutation_typed(&dangling).unwrap();
        assert!(!dangling.operation.valid);
        assert!(dangling
            .operation
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_str() == "link_not_found"));
    }
}
