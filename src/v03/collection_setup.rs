use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::type_pack::{plan_type_pack, stage_type_pack_plan};
use super::{
    collection_validation_errors, introduced_validation_errors, revision,
    validation_diagnostic_digest, Diagnostic, OperationResult,
};
use crate::mutation::shadow as mutation_shadow;
use crate::v03::{ContractSetupChoice, TypePackAssessmentOptions, TypePackProvision};
use crate::Collection;

const PROVISION_LOCK_PATH: &str = "mdbase.provisions.yaml";
const MAX_POINTER_BYTES: usize = 1024;
const MAX_POINTER_SEGMENTS: usize = 16;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigurationRequirement {
    pub id: String,
    pub path: String,
    pub predicate: ConfigurationPredicate,
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationPredicate {
    Contains,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigurationProvision {
    pub requirement: String,
    pub operation: ConfigurationOperation,
    pub path: String,
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationOperation {
    SetAdd,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CollectionSetupRequirements {
    #[serde(default)]
    pub configuration: Vec<ConfigurationRequirement>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CollectionSetupProvisions {
    #[serde(default)]
    pub configuration: Vec<ConfigurationProvision>,
    #[serde(default)]
    pub type_packs: Vec<CollectionSetupTypePack>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CollectionSetupTypePackOptions {
    #[serde(default)]
    pub adopt_resources: BTreeMap<String, String>,
    #[serde(default)]
    pub preserve_seed_targets: BTreeSet<String>,
    #[serde(default)]
    pub target_overrides: BTreeMap<String, String>,
    #[serde(default)]
    pub contract_setups: Vec<ContractSetupChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSetupTypePack {
    pub provision: TypePackProvision,
    #[serde(default)]
    pub options: CollectionSetupTypePackOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSetup {
    pub application_id: String,
    pub declaration_digest: String,
    #[serde(default)]
    pub requirements: CollectionSetupRequirements,
    #[serde(default)]
    pub provisions: CollectionSetupProvisions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSetupApplyOptions {
    pub expected_assessment_digest: String,
    pub expected_collection_revision: String,
    pub expected_provision_digest: String,
    #[serde(default)]
    pub allow_type_pack_downgrades: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigurationConflict {
    pub code: String,
    pub path: String,
    pub expected: String,
    pub observed: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigurationSetupAssessment {
    pub requirement: String,
    pub path: String,
    pub value: Value,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<ConfigurationConflict>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionSetupAssessment {
    pub status: String,
    pub applicable: bool,
    pub application_id: String,
    pub declaration_digest: String,
    pub provision_digest: String,
    pub collection_revision: String,
    pub final_collection_revision: String,
    pub configuration: Vec<ConfigurationSetupAssessment>,
    pub type_packs: Vec<Value>,
    pub final_resource_revisions: BTreeMap<String, String>,
    #[serde(default)]
    pub baseline_diagnostic_count: usize,
    #[serde(default)]
    pub final_diagnostic_count: usize,
    #[serde(default)]
    pub resolved_diagnostic_count: usize,
    #[serde(default)]
    pub introduced_diagnostic_count: usize,
    #[serde(default)]
    pub baseline_diagnostic_digest: String,
    pub assessment_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigurationContributionReceipt {
    pub requirement: String,
    pub path: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionSetupReceipt {
    pub application_id: String,
    pub declaration_digest: String,
    pub provision_digest: String,
    pub assessment_digest: String,
    pub collection_revision: String,
    pub configuration: Vec<ConfigurationContributionReceipt>,
    pub type_packs: Vec<Value>,
    pub cleanup_deferred: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ProvisionContributor {
    application_id: String,
    declaration_digest: String,
    provision_digest: String,
    requirement: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ProvisionContribution {
    path: String,
    value: Value,
    contributors: Vec<ProvisionContributor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProvisionLock {
    kind: String,
    lock_version: u32,
    contributions: Vec<ProvisionContribution>,
}

struct CollectionSetupPlan {
    shadow: Option<mutation_shadow::ShadowCollection>,
    assessment: CollectionSetupAssessment,
    receipt_configuration: Vec<ConfigurationContributionReceipt>,
}

impl Collection {
    /// Assess one complete application-declared setup without changing the collection.
    pub fn assess_collection_setup(&self, setup: &CollectionSetup) -> OperationResult {
        match plan_collection_setup(self, setup) {
            Ok(plan) => OperationResult {
                valid: true,
                result: serde_json::to_value(plan.assessment).expect("assessment serializes"),
                diagnostics: Vec::new(),
            },
            Err(diagnostics) => failed(diagnostics),
        }
    }

    /// Apply the exact reviewed setup as one crash-recoverable collection transaction.
    pub fn apply_collection_setup(
        &self,
        setup: &CollectionSetup,
        options: &CollectionSetupApplyOptions,
    ) -> OperationResult {
        let plan = match plan_collection_setup(self, setup) {
            Ok(plan) => plan,
            Err(diagnostics) => return failed(diagnostics),
        };
        if options.expected_collection_revision != plan.assessment.collection_revision
            || options.expected_provision_digest != plan.assessment.provision_digest
            || options.expected_assessment_digest != plan.assessment.assessment_digest
        {
            return setup_error(
                "concurrent_modification",
                "The collection setup review is stale. Assess the complete setup again before applying it.",
            );
        }
        if !plan.assessment.applicable {
            let conflicts = plan
                .assessment
                .configuration
                .iter()
                .filter_map(|entry| entry.conflict.clone())
                .collect::<Vec<_>>();
            return OperationResult {
                valid: false,
                result: json!({"assessment": plan.assessment, "conflicts": conflicts}),
                diagnostics: vec![Diagnostic::error(
                    "collection_setup_conflict",
                    "The application setup has unresolved conflicts.",
                    Some("mdbase.yaml".to_string()),
                )],
            };
        }
        for pack in &plan.assessment.type_packs {
            if pack.get("status").and_then(Value::as_str) != Some("downgrade") {
                continue;
            }
            let Some(id) = pack
                .get("desired")
                .and_then(|desired| desired.get("id"))
                .and_then(Value::as_str)
            else {
                return setup_error(
                    "invalid_collection_setup",
                    "A type-pack downgrade assessment is missing its pack identity.",
                );
            };
            if !options.allow_type_pack_downgrades.contains(id) {
                return setup_error(
                    "type_pack_downgrade",
                    format!("Managed type-pack downgrade '{id}' requires explicit approval."),
                );
            }
        }

        if plan.assessment.status == "current" {
            return applied_setup_result(&plan, false);
        }

        let Some(shadow) = plan.shadow.as_ref() else {
            return setup_error(
                "collection_setup_apply_failed",
                "The collection setup plan has changes but no staged workspace.",
            );
        };

        let desired = match mutation_shadow::collect_collection_files(&shadow.collection) {
            Ok(desired) => desired,
            Err(diagnostic) => return failed(vec![*diagnostic]),
        };
        let commit = match crate::transactions::commit_migration(self, &shadow.baseline, &desired) {
            Ok(commit) => commit,
            Err(error) => return setup_error(error.code(), error.to_string()),
        };
        let reopened = match self.reopen_held(true) {
            Ok(collection) => collection,
            Err(error) => {
                return setup_error(
                    "collection_setup_apply_failed",
                    format!("The committed collection setup could not be reopened: {error:?}"),
                )
            }
        };
        // Reopening verifies structural integrity. Record schema diagnostics are
        // reported by the assessment but do not block application setup.
        let committed = match mutation_shadow::collect_collection_files(&reopened) {
            Ok(files) => baseline_revision(&files),
            Err(diagnostic) => return failed(vec![*diagnostic]),
        };
        applied_setup_result_with_revision(&plan, committed, commit.cleanup_deferred)
    }
}

fn plan_collection_setup(
    collection: &Collection,
    setup: &CollectionSetup,
) -> Result<CollectionSetupPlan, Vec<Diagnostic>> {
    validate_setup(setup).map_err(|diagnostic| vec![*diagnostic])?;
    let baseline_validation_errors = collection_validation_errors(collection);
    let provision_value = serde_json::to_value(&setup.provisions)
        .map_err(|error| invalid_setup(format!("Could not serialize setup provisions: {error}")))?;
    let provision_digest = jcs_digest(&provision_value).map_err(|diagnostic| vec![*diagnostic])?;
    if let Some(plan) = plan_unchanged_collection_setup(
        collection,
        setup,
        &baseline_validation_errors,
        &provision_digest,
    )? {
        return Ok(plan);
    }
    let mut shadow =
        mutation_shadow::shadow_collection(collection).map_err(|diagnostic| vec![*diagnostic])?;
    let collection_revision = baseline_revision(&shadow.baseline);
    let config_path = shadow.directory.path().join("mdbase.yaml");
    let config_bytes = fs::read(&config_path).map_err(|error| {
        vec![Diagnostic::error(
            "invalid_collection_setup",
            format!("Could not read mdbase.yaml for setup assessment: {error}"),
            Some("mdbase.yaml".to_string()),
        )]
    })?;
    let mut config: serde_yaml::Value = serde_yaml::from_slice(&config_bytes).map_err(|error| {
        vec![Diagnostic::error(
            "invalid_collection_setup",
            format!("Could not parse mdbase.yaml for setup assessment: {error}"),
            Some("mdbase.yaml".to_string()),
        )]
    })?;
    let mut configuration = Vec::new();
    let mut receipt_configuration = Vec::new();
    for provision in &setup.provisions.configuration {
        let segments = decode_configuration_pointer(&provision.path)
            .map_err(|diagnostic| vec![*diagnostic])?;
        let assessment = assess_and_stage_configuration(&mut config, provision, &segments);
        if assessment.conflict.is_none() {
            receipt_configuration.push(ConfigurationContributionReceipt {
                requirement: provision.requirement.clone(),
                path: provision.path.clone(),
                value: provision.value.clone(),
            });
        }
        configuration.push(assessment);
    }
    let config_changed = configuration.iter().any(|entry| entry.action == "add");
    if config_changed {
        let bytes = serde_yaml::to_string(&config)
            .map_err(|error| invalid_setup(format!("Could not serialize mdbase.yaml: {error}")))?;
        fs::write(&config_path, bytes).map_err(|error| {
            vec![Diagnostic::error(
                "collection_setup_apply_failed",
                format!("Could not stage mdbase.yaml: {error}"),
                Some("mdbase.yaml".to_string()),
            )]
        })?;
        shadow.collection = Collection::open(shadow.directory.path()).map_err(|error| {
            vec![Diagnostic::error(
                "invalid_collection_setup",
                format!("The staged configuration is not a valid collection: {error:?}"),
                Some("mdbase.yaml".to_string()),
            )]
        })?;
    }

    let mut type_packs = Vec::new();
    for pack in &setup.provisions.type_packs {
        let options = TypePackAssessmentOptions {
            installed_by: setup.application_id.clone(),
            adopt_resources: pack.options.adopt_resources.clone(),
            preserve_seed_targets: pack.options.preserve_seed_targets.clone(),
            target_overrides: pack.options.target_overrides.clone(),
            contract_setups: pack.options.contract_setups.clone(),
        };
        let plan = plan_type_pack(&shadow.collection, &pack.provision, &options)
            .map_err(|diagnostic| vec![*diagnostic])?;
        let applicable = plan.assessment.get("applicable").and_then(Value::as_bool) == Some(true);
        type_packs.push(plan.assessment.clone());
        if applicable {
            stage_type_pack_plan(&mut shadow, &plan).map_err(|diagnostic| vec![*diagnostic])?;
        }
    }

    let provision_lock_path = shadow.directory.path().join(PROVISION_LOCK_PATH);
    let previous_lock_bytes = fs::read(&provision_lock_path).ok();
    let mut lock =
        read_provision_lock(&shadow.collection).map_err(|diagnostic| vec![*diagnostic])?;
    for receipt in &receipt_configuration {
        add_contributor(
            &mut lock,
            receipt,
            &setup.application_id,
            &setup.declaration_digest,
            &provision_digest,
        )
        .map_err(|diagnostic| vec![*diagnostic])?;
    }
    let lock_bytes = serialize_provision_lock(&mut lock).map_err(|diagnostic| vec![*diagnostic])?;
    let lock_changed = previous_lock_bytes.as_deref() != Some(lock_bytes.as_slice());
    if !receipt_configuration.is_empty() || previous_lock_bytes.is_some() {
        fs::write(&provision_lock_path, &lock_bytes).map_err(|error| {
            vec![Diagnostic::error(
                "collection_setup_apply_failed",
                format!("Could not stage {PROVISION_LOCK_PATH}: {error}"),
                Some(PROVISION_LOCK_PATH.to_string()),
            )]
        })?;
    }
    shadow.collection = Collection::open(shadow.directory.path()).map_err(|error| {
        vec![Diagnostic::error(
            "invalid_collection_setup",
            format!("The complete staged setup is not a valid collection: {error:?}"),
            None,
        )]
    })?;
    let final_validation_errors = collection_validation_errors(&shadow.collection);
    let introduced_diagnostics =
        introduced_validation_errors(&baseline_validation_errors, &final_validation_errors);
    let resolved_diagnostic_count =
        introduced_validation_errors(&final_validation_errors, &baseline_validation_errors).len();
    let desired = mutation_shadow::collect_collection_files(&shadow.collection)
        .map_err(|diagnostic| vec![*diagnostic])?;
    let final_collection_revision = baseline_revision(&desired);
    let mut final_resource_revisions = BTreeMap::new();
    for path in ["mdbase.yaml", "mdbase.lock.yaml", PROVISION_LOCK_PATH] {
        if let Some(bytes) = desired.get(path) {
            final_resource_revisions.insert(path.to_string(), revision(bytes));
        }
    }
    let changed = config_changed
        || (!receipt_configuration.is_empty() && lock_changed)
        || type_packs
            .iter()
            .any(|pack| pack.get("status").and_then(Value::as_str) != Some("current"));
    let assessment = complete_assessment(
        setup,
        provision_digest,
        collection_revision,
        final_collection_revision,
        configuration,
        type_packs,
        final_resource_revisions,
        &baseline_validation_errors,
        final_validation_errors.len(),
        resolved_diagnostic_count,
        introduced_diagnostics.len(),
        changed,
    )?;
    Ok(CollectionSetupPlan {
        shadow: Some(shadow),
        assessment,
        receipt_configuration,
    })
}

fn plan_unchanged_collection_setup(
    collection: &Collection,
    setup: &CollectionSetup,
    baseline_validation_errors: &[Diagnostic],
    provision_digest: &str,
) -> Result<Option<CollectionSetupPlan>, Vec<Diagnostic>> {
    let config_bytes = collection
        .held_root()
        .read("mdbase.yaml")
        .map_err(|error| {
            vec![Diagnostic::error(
                "invalid_collection_setup",
                format!("Could not read mdbase.yaml for setup assessment: {error}"),
                Some("mdbase.yaml".to_string()),
            )]
        })?;
    let mut config: serde_yaml::Value = serde_yaml::from_slice(&config_bytes).map_err(|error| {
        vec![Diagnostic::error(
            "invalid_collection_setup",
            format!("Could not parse mdbase.yaml for setup assessment: {error}"),
            Some("mdbase.yaml".to_string()),
        )]
    })?;
    let mut configuration = Vec::new();
    let mut receipt_configuration = Vec::new();
    for provision in &setup.provisions.configuration {
        let segments = decode_configuration_pointer(&provision.path)
            .map_err(|diagnostic| vec![*diagnostic])?;
        let assessment = assess_and_stage_configuration(&mut config, provision, &segments);
        if assessment.conflict.is_none() {
            receipt_configuration.push(ConfigurationContributionReceipt {
                requirement: provision.requirement.clone(),
                path: provision.path.clone(),
                value: provision.value.clone(),
            });
        }
        configuration.push(assessment);
    }
    if configuration.iter().any(|entry| entry.action == "add") {
        return Ok(None);
    }

    let mut type_packs = Vec::new();
    for pack in &setup.provisions.type_packs {
        let options = TypePackAssessmentOptions {
            installed_by: setup.application_id.clone(),
            adopt_resources: pack.options.adopt_resources.clone(),
            preserve_seed_targets: pack.options.preserve_seed_targets.clone(),
            target_overrides: pack.options.target_overrides.clone(),
            contract_setups: pack.options.contract_setups.clone(),
        };
        let plan = plan_type_pack(collection, &pack.provision, &options)
            .map_err(|diagnostic| vec![*diagnostic])?;
        let current = plan.assessment.get("status").and_then(Value::as_str) == Some("current");
        let applicable = plan.assessment.get("applicable").and_then(Value::as_bool) == Some(true);
        type_packs.push(plan.assessment);
        if applicable && !current {
            return Ok(None);
        }
    }

    let previous_lock_bytes = collection.held_root().read(PROVISION_LOCK_PATH).ok();
    let mut lock = read_provision_lock(collection).map_err(|diagnostic| vec![*diagnostic])?;
    for receipt in &receipt_configuration {
        add_contributor(
            &mut lock,
            receipt,
            &setup.application_id,
            &setup.declaration_digest,
            provision_digest,
        )
        .map_err(|diagnostic| vec![*diagnostic])?;
    }
    let lock_bytes = serialize_provision_lock(&mut lock).map_err(|diagnostic| vec![*diagnostic])?;
    if !receipt_configuration.is_empty()
        && previous_lock_bytes.as_deref() != Some(lock_bytes.as_slice())
    {
        return Ok(None);
    }

    let baseline = mutation_shadow::collect_collection_files(collection)
        .map_err(|diagnostic| vec![*diagnostic])?;
    let collection_revision = baseline_revision(&baseline);
    let mut final_resource_revisions = BTreeMap::new();
    for path in ["mdbase.yaml", "mdbase.lock.yaml", PROVISION_LOCK_PATH] {
        if let Some(bytes) = baseline.get(path) {
            final_resource_revisions.insert(path.to_string(), revision(bytes));
        }
    }
    let assessment = complete_assessment(
        setup,
        provision_digest.to_string(),
        collection_revision.clone(),
        collection_revision,
        configuration,
        type_packs,
        final_resource_revisions,
        baseline_validation_errors,
        baseline_validation_errors.len(),
        0,
        0,
        false,
    )?;
    Ok(Some(CollectionSetupPlan {
        shadow: None,
        assessment,
        receipt_configuration,
    }))
}

#[allow(clippy::too_many_arguments)]
fn complete_assessment(
    setup: &CollectionSetup,
    provision_digest: String,
    collection_revision: String,
    final_collection_revision: String,
    configuration: Vec<ConfigurationSetupAssessment>,
    type_packs: Vec<Value>,
    final_resource_revisions: BTreeMap<String, String>,
    baseline_validation_errors: &[Diagnostic],
    final_diagnostic_count: usize,
    resolved_diagnostic_count: usize,
    introduced_diagnostic_count: usize,
    changed: bool,
) -> Result<CollectionSetupAssessment, Vec<Diagnostic>> {
    let has_configuration_conflict = configuration.iter().any(|entry| entry.conflict.is_some());
    let has_pack_conflict = type_packs
        .iter()
        .any(|pack| pack.get("applicable").and_then(Value::as_bool) != Some(true));
    let applicable = !has_configuration_conflict && !has_pack_conflict;
    let status = if !applicable {
        "conflict"
    } else if changed {
        "provision"
    } else {
        "current"
    };
    let mut assessment = CollectionSetupAssessment {
        status: status.to_string(),
        applicable,
        application_id: setup.application_id.clone(),
        declaration_digest: setup.declaration_digest.clone(),
        provision_digest,
        collection_revision,
        final_collection_revision,
        configuration,
        type_packs,
        final_resource_revisions,
        baseline_diagnostic_count: baseline_validation_errors.len(),
        final_diagnostic_count,
        resolved_diagnostic_count,
        introduced_diagnostic_count,
        baseline_diagnostic_digest: validation_diagnostic_digest(baseline_validation_errors),
        assessment_digest: String::new(),
    };
    let identity = serde_json::to_value(&assessment)
        .map_err(|error| invalid_setup(format!("Could not serialize setup assessment: {error}")))?;
    assessment.assessment_digest = jcs_digest(&identity).map_err(|diagnostic| vec![*diagnostic])?;
    Ok(assessment)
}

fn applied_setup_result(plan: &CollectionSetupPlan, cleanup_deferred: bool) -> OperationResult {
    applied_setup_result_with_revision(
        plan,
        plan.assessment.final_collection_revision.clone(),
        cleanup_deferred,
    )
}

fn applied_setup_result_with_revision(
    plan: &CollectionSetupPlan,
    collection_revision: String,
    cleanup_deferred: bool,
) -> OperationResult {
    let type_packs = plan
        .assessment
        .type_packs
        .iter()
        .filter_map(|pack| pack.get("desired").cloned())
        .collect();
    let receipt = CollectionSetupReceipt {
        application_id: plan.assessment.application_id.clone(),
        declaration_digest: plan.assessment.declaration_digest.clone(),
        provision_digest: plan.assessment.provision_digest.clone(),
        assessment_digest: plan.assessment.assessment_digest.clone(),
        collection_revision,
        configuration: plan.receipt_configuration.clone(),
        type_packs,
        cleanup_deferred,
    };
    OperationResult {
        valid: true,
        result: json!({"assessment": &plan.assessment, "receipt": receipt}),
        diagnostics: Vec::new(),
    }
}

fn validate_setup(setup: &CollectionSetup) -> Result<(), Box<Diagnostic>> {
    let app_pattern = Regex::new(r"^[a-z][a-z0-9]*(?:[._-][a-z0-9]+)+$")
        .expect("valid application identity expression");
    if !app_pattern.is_match(&setup.application_id) {
        return Err(invalid_setup_diagnostic(
            "application_id must be a stable namespaced identifier.",
        ));
    }
    if !is_sha256_digest(&setup.declaration_digest) {
        return Err(invalid_setup_diagnostic(
            "declaration_digest must be a lowercase sha256 digest.",
        ));
    }
    let requirement_pattern = Regex::new(r"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$")
        .expect("valid requirement identity expression");
    let mut requirements = BTreeMap::new();
    for requirement in &setup.requirements.configuration {
        if !requirement_pattern.is_match(&requirement.id) {
            return Err(invalid_setup_diagnostic(format!(
                "Configuration requirement '{}' has an invalid identifier.",
                requirement.id
            )));
        }
        validate_scalar(&requirement.value)?;
        decode_configuration_pointer(&requirement.path)?;
        if requirements.insert(&requirement.id, requirement).is_some() {
            return Err(invalid_setup_diagnostic(format!(
                "Configuration requirement '{}' is duplicated.",
                requirement.id
            )));
        }
    }
    let mut linked = BTreeSet::new();
    for provision in &setup.provisions.configuration {
        validate_scalar(&provision.value)?;
        decode_configuration_pointer(&provision.path)?;
        let Some(requirement) = requirements.get(&provision.requirement) else {
            return Err(invalid_setup_diagnostic(format!(
                "Configuration provision references unknown requirement '{}'.",
                provision.requirement
            )));
        };
        if requirement.path != provision.path || requirement.value != provision.value {
            return Err(invalid_setup_diagnostic(format!(
                "Configuration provision '{}' must repeat its requirement path and value exactly.",
                provision.requirement
            )));
        }
        if !linked.insert(&provision.requirement) {
            return Err(invalid_setup_diagnostic(format!(
                "Configuration requirement '{}' has more than one provision.",
                provision.requirement
            )));
        }
    }
    if let Some(missing) = requirements.keys().find(|id| !linked.contains(**id)) {
        return Err(invalid_setup_diagnostic(format!(
            "Configuration requirement '{missing}' has no provision."
        )));
    }
    Ok(())
}

fn validate_scalar(value: &Value) -> Result<(), Box<Diagnostic>> {
    if matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    ) {
        Ok(())
    } else {
        Err(invalid_setup_diagnostic(
            "Configuration contribution values must be JSON scalars.",
        ))
    }
}

fn decode_configuration_pointer(path: &str) -> Result<Vec<String>, Box<Diagnostic>> {
    if path.len() > MAX_POINTER_BYTES || !path.starts_with('/') {
        return Err(invalid_setup_diagnostic(format!(
            "Configuration path '{path}' must be a bounded RFC 6901 JSON pointer."
        )));
    }
    let encoded = path[1..].split('/').collect::<Vec<_>>();
    if encoded.len() < 2 || encoded.len() > MAX_POINTER_SEGMENTS {
        return Err(invalid_setup_diagnostic(format!(
            "Configuration path '{path}' must address a value below an x-* namespace."
        )));
    }
    let mut segments = Vec::with_capacity(encoded.len());
    for segment in encoded {
        let decoded = decode_pointer_segment(segment).ok_or_else(|| {
            invalid_setup_diagnostic(format!(
                "Configuration path '{path}' contains an invalid JSON pointer escape."
            ))
        })?;
        if decoded.is_empty()
            || decoded == "-"
            || decoded.chars().all(|character| character.is_ascii_digit())
            || decoded.chars().any(char::is_control)
        {
            return Err(invalid_setup_diagnostic(format!(
                "Configuration path '{path}' contains a disallowed object-key segment."
            )));
        }
        segments.push(decoded);
    }
    let namespace =
        Regex::new(r"^x-[a-z0-9][a-z0-9-]*$").expect("valid extension namespace expression");
    if !namespace.is_match(&segments[0]) {
        return Err(invalid_setup_diagnostic(format!(
            "Configuration path '{path}' must be inside a top-level x-* extension namespace."
        )));
    }
    Ok(segments)
}

fn decode_pointer_segment(segment: &str) -> Option<String> {
    let mut output = String::with_capacity(segment.len());
    let mut characters = segment.chars();
    while let Some(character) = characters.next() {
        if character != '~' {
            output.push(character);
            continue;
        }
        match characters.next()? {
            '0' => output.push('~'),
            '1' => output.push('/'),
            _ => return None,
        }
    }
    Some(output)
}

fn assess_and_stage_configuration(
    config: &mut serde_yaml::Value,
    provision: &ConfigurationProvision,
    segments: &[String],
) -> ConfigurationSetupAssessment {
    let mut node = config;
    for segment in &segments[..segments.len() - 1] {
        if !node.is_mapping() {
            return configuration_conflict(
                provision,
                "configuration_path_conflict",
                "mapping",
                yaml_shape(node),
                format!(
                    "Configuration path '{}' crosses a value that is not a mapping.",
                    provision.path
                ),
            );
        }
        let mapping = node.as_mapping_mut().expect("mapping shape checked");
        let key = serde_yaml::Value::String(segment.clone());
        if !mapping.contains_key(&key) {
            mapping.insert(key.clone(), serde_yaml::Value::Mapping(Default::default()));
        }
        node = mapping.get_mut(&key).expect("inserted mapping node");
    }
    if !node.is_mapping() {
        return configuration_conflict(
            provision,
            "configuration_path_conflict",
            "mapping",
            yaml_shape(node),
            format!(
                "Configuration path '{}' has a parent that is not a mapping.",
                provision.path
            ),
        );
    }
    let mapping = node.as_mapping_mut().expect("mapping shape checked");
    let target = serde_yaml::Value::String(segments.last().expect("path has target").clone());
    if !mapping.contains_key(&target) {
        mapping.insert(target.clone(), serde_yaml::Value::Sequence(Vec::new()));
    }
    let value = mapping.get_mut(&target).expect("inserted sequence target");
    let Some(sequence) = value.as_sequence_mut() else {
        return configuration_conflict(
            provision,
            "configuration_type_conflict",
            "sequence",
            yaml_shape(value),
            format!(
                "Configuration target '{}' must be a sequence before set_add can be applied.",
                provision.path
            ),
        );
    };
    let desired_key = scalar_key(&provision.value).expect("validated scalar canonicalizes");
    let current = sequence.iter().any(|candidate| {
        let json = crate::frontmatter::parser::yaml_to_json(candidate);
        scalar_key(&json).as_deref() == Ok(desired_key.as_str())
    });
    if !current {
        sequence.push(serde_yaml::to_value(&provision.value).expect("JSON scalar serializes"));
    }
    ConfigurationSetupAssessment {
        requirement: provision.requirement.clone(),
        path: provision.path.clone(),
        value: provision.value.clone(),
        action: if current { "current" } else { "add" }.to_string(),
        conflict: None,
    }
}

fn configuration_conflict(
    provision: &ConfigurationProvision,
    code: &str,
    expected: &str,
    observed: &str,
    message: String,
) -> ConfigurationSetupAssessment {
    ConfigurationSetupAssessment {
        requirement: provision.requirement.clone(),
        path: provision.path.clone(),
        value: provision.value.clone(),
        action: "conflict".to_string(),
        conflict: Some(ConfigurationConflict {
            code: code.to_string(),
            path: provision.path.clone(),
            expected: expected.to_string(),
            observed: observed.to_string(),
            message,
        }),
    }
}

fn yaml_shape(value: &serde_yaml::Value) -> &'static str {
    match value {
        serde_yaml::Value::Null => "null",
        serde_yaml::Value::Bool(_) => "boolean",
        serde_yaml::Value::Number(_) => "number",
        serde_yaml::Value::String(_) => "string",
        serde_yaml::Value::Sequence(_) => "sequence",
        serde_yaml::Value::Mapping(_) => "mapping",
        serde_yaml::Value::Tagged(_) => "tagged",
    }
}

fn read_provision_lock(collection: &Collection) -> Result<ProvisionLock, Box<Diagnostic>> {
    let bytes = match collection.held_root().read(PROVISION_LOCK_PATH) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProvisionLock {
                kind: "mdbase.provision-lock".to_string(),
                lock_version: 1,
                contributions: Vec::new(),
            })
        }
        Err(error) => {
            return Err(Box::new(Diagnostic::error(
                "invalid_collection_setup",
                format!("Could not read {PROVISION_LOCK_PATH}: {error}"),
                Some(PROVISION_LOCK_PATH.to_string()),
            )))
        }
    };
    let lock: ProvisionLock = serde_yaml::from_slice(&bytes).map_err(|error| {
        Diagnostic::error(
            "invalid_collection_setup",
            format!("Could not parse {PROVISION_LOCK_PATH}: {error}"),
            Some(PROVISION_LOCK_PATH.to_string()),
        )
    })?;
    let yaml: serde_yaml::Value = serde_yaml::from_slice(&bytes).map_err(|error| {
        Diagnostic::error(
            "invalid_collection_setup",
            format!("Could not parse {PROVISION_LOCK_PATH}: {error}"),
            Some(PROVISION_LOCK_PATH.to_string()),
        )
    })?;
    let value = crate::frontmatter::parser::yaml_to_json(&yaml);
    if let Some(diagnostic) = super::validate_provision_lock(&value, PROVISION_LOCK_PATH)
        .into_iter()
        .next()
    {
        return Err(Box::new(diagnostic));
    }
    if lock.kind != "mdbase.provision-lock" || lock.lock_version != 1 {
        return Err(Box::new(Diagnostic::error(
            "invalid_collection_setup",
            format!("{PROVISION_LOCK_PATH} has an unsupported kind or version."),
            Some(PROVISION_LOCK_PATH.to_string()),
        )));
    }
    Ok(lock)
}

fn add_contributor(
    lock: &mut ProvisionLock,
    receipt: &ConfigurationContributionReceipt,
    application_id: &str,
    declaration_digest: &str,
    provision_digest: &str,
) -> Result<(), Box<Diagnostic>> {
    let key = scalar_key(&receipt.value)?;
    let index = lock
        .contributions
        .iter()
        .position(|entry| {
            entry.path == receipt.path && scalar_key(&entry.value).as_deref() == Ok(key.as_str())
        })
        .unwrap_or_else(|| {
            lock.contributions.push(ProvisionContribution {
                path: receipt.path.clone(),
                value: receipt.value.clone(),
                contributors: Vec::new(),
            });
            lock.contributions.len() - 1
        });
    let contributor = ProvisionContributor {
        application_id: application_id.to_string(),
        declaration_digest: declaration_digest.to_string(),
        provision_digest: provision_digest.to_string(),
        requirement: receipt.requirement.clone(),
    };
    if !lock.contributions[index]
        .contributors
        .contains(&contributor)
    {
        lock.contributions[index].contributors.push(contributor);
    }
    Ok(())
}

fn serialize_provision_lock(lock: &mut ProvisionLock) -> Result<Vec<u8>, Box<Diagnostic>> {
    for contribution in &mut lock.contributions {
        contribution.contributors.sort_by(|left, right| {
            (
                &left.application_id,
                &left.requirement,
                &left.declaration_digest,
                &left.provision_digest,
            )
                .cmp(&(
                    &right.application_id,
                    &right.requirement,
                    &right.declaration_digest,
                    &right.provision_digest,
                ))
        });
    }
    lock.contributions.sort_by(|left, right| {
        let left_value = scalar_key(&left.value).unwrap_or_default();
        let right_value = scalar_key(&right.value).unwrap_or_default();
        (&left.path, left_value).cmp(&(&right.path, right_value))
    });
    let mut bytes = serde_json::to_vec_pretty(lock).map_err(|error| {
        invalid_setup_diagnostic(format!(
            "Could not serialize configuration provision receipts: {error}"
        ))
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn baseline_revision(files: &crate::transactions::FileBaseline) -> String {
    let mut digest = Sha256::new();
    for (path, bytes) in files {
        digest.update((path.len() as u64).to_be_bytes());
        digest.update(path.as_bytes());
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    format!("sha256:{:x}", digest.finalize())
}

fn scalar_key(value: &Value) -> Result<String, Box<Diagnostic>> {
    serde_jcs::to_string(value).map_err(|error| {
        invalid_setup_diagnostic(format!(
            "Could not canonicalize configuration contribution: {error}"
        ))
    })
}

fn jcs_digest(value: &Value) -> Result<String, Box<Diagnostic>> {
    let bytes = serde_jcs::to_vec(value).map_err(|error| {
        invalid_setup_diagnostic(format!("Could not canonicalize collection setup: {error}"))
    })?;
    Ok(revision(&bytes))
}

fn is_sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn invalid_setup(message: impl Into<String>) -> Vec<Diagnostic> {
    vec![*invalid_setup_diagnostic(message)]
}

fn invalid_setup_diagnostic(message: impl Into<String>) -> Box<Diagnostic> {
    Box::new(Diagnostic::error(
        "invalid_collection_setup",
        message,
        Some("mdbase.yaml".to_string()),
    ))
}

fn setup_error(code: impl Into<String>, message: impl Into<String>) -> OperationResult {
    failed(vec![Diagnostic::error(code, message, None)])
}

fn failed(diagnostics: Vec<Diagnostic>) -> OperationResult {
    OperationResult {
        valid: false,
        result: json!({}),
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECLARATION_DIGEST: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn collection(config: &str) -> (tempfile::TempDir, Collection) {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("mdbase.yaml"), config).unwrap();
        let collection = Collection::open(directory.path()).unwrap();
        (directory, collection)
    }

    fn collection_with_invalid_record() -> (tempfile::TempDir, Collection) {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: error\n",
        )
        .unwrap();
        fs::create_dir_all(directory.path().join("_types")).unwrap();
        fs::write(
            directory.path().join("_types/note.md"),
            "---\nkind: mdbase.type\nname: note\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    required: [title]\n    properties:\n      title: { type: string }\n---\n",
        )
        .unwrap();
        fs::write(directory.path().join("broken.md"), "---\ntype: note\n---\n").unwrap();
        let collection = Collection::open(directory.path()).unwrap();
        (directory, collection)
    }

    fn empty_setup(application_id: &str) -> CollectionSetup {
        CollectionSetup {
            application_id: application_id.to_string(),
            declaration_digest: DECLARATION_DIGEST.to_string(),
            requirements: CollectionSetupRequirements::default(),
            provisions: CollectionSetupProvisions::default(),
        }
    }

    fn setup(application_id: &str) -> CollectionSetup {
        CollectionSetup {
            application_id: application_id.to_string(),
            declaration_digest: DECLARATION_DIGEST.to_string(),
            requirements: CollectionSetupRequirements {
                configuration: vec![ConfigurationRequirement {
                    id: "base-sources".to_string(),
                    path: "/x-obsidian/bases/include".to_string(),
                    predicate: ConfigurationPredicate::Contains,
                    value: json!("views/tasknotes/**/*.base"),
                }],
            },
            provisions: CollectionSetupProvisions {
                configuration: vec![ConfigurationProvision {
                    requirement: "base-sources".to_string(),
                    operation: ConfigurationOperation::SetAdd,
                    path: "/x-obsidian/bases/include".to_string(),
                    value: json!("views/tasknotes/**/*.base"),
                }],
                type_packs: Vec::new(),
            },
        }
    }

    fn apply_options(assessment: &Value) -> CollectionSetupApplyOptions {
        CollectionSetupApplyOptions {
            expected_assessment_digest: assessment["assessment_digest"]
                .as_str()
                .unwrap()
                .to_string(),
            expected_collection_revision: assessment["collection_revision"]
                .as_str()
                .unwrap()
                .to_string(),
            expected_provision_digest: assessment["provision_digest"].as_str().unwrap().to_string(),
            allow_type_pack_downgrades: BTreeSet::new(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn replacement_root_never_receives_setup_reads_or_publication() {
        let (directory, collection) = collection("spec_version: 0.3.0\n");
        let original = directory.path().to_path_buf();
        let held = original.with_extension("setup-held");
        fs::rename(&original, &held).unwrap();
        fs::create_dir(&original).unwrap();
        fs::write(original.join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        fs::write(original.join("sentinel"), "replacement\n").unwrap();

        let declaration = setup("dev.example.editor");
        let assessment = collection.assess_collection_setup(&declaration);
        assert!(assessment.valid, "{:?}", assessment.diagnostics);
        let applied =
            collection.apply_collection_setup(&declaration, &apply_options(&assessment.result));
        assert!(applied.valid, "{:?}", applied.diagnostics);
        assert!(fs::read_to_string(held.join("mdbase.yaml"))
            .unwrap()
            .contains("x-obsidian"));
        assert_eq!(
            fs::read_to_string(original.join("sentinel")).unwrap(),
            "replacement\n"
        );
        assert!(!original.join(PROVISION_LOCK_PATH).exists());
    }

    #[test]
    fn empty_setup_is_current_when_the_collection_has_preexisting_errors() {
        let (directory, collection) = collection_with_invalid_record();
        let declaration = empty_setup("dev.example.editor");

        let assessment = collection.assess_collection_setup(&declaration);
        assert!(assessment.valid, "{:?}", assessment.diagnostics);
        assert_eq!(assessment.result["status"], "current");
        assert_eq!(assessment.result["baseline_diagnostic_count"], 1);
        assert_eq!(assessment.result["final_diagnostic_count"], 1);
        assert_eq!(assessment.result["introduced_diagnostic_count"], 0);

        let applied =
            collection.apply_collection_setup(&declaration, &apply_options(&assessment.result));
        assert!(applied.valid, "{:?}", applied.diagnostics);
        assert!(directory.path().join("broken.md").is_file());
    }

    #[test]
    fn unrelated_setup_preserves_preexisting_errors() {
        let (directory, collection) = collection_with_invalid_record();
        let declaration = setup("dev.example.tasknotes");

        let assessment = collection.assess_collection_setup(&declaration);
        assert!(assessment.valid, "{:?}", assessment.diagnostics);
        assert_eq!(assessment.result["status"], "provision");
        assert_eq!(assessment.result["baseline_diagnostic_count"], 1);
        assert_eq!(assessment.result["final_diagnostic_count"], 1);
        assert_eq!(assessment.result["introduced_diagnostic_count"], 0);

        let applied =
            collection.apply_collection_setup(&declaration, &apply_options(&assessment.result));
        assert!(applied.valid, "{:?}", applied.diagnostics);
        assert!(directory.path().join("broken.md").is_file());
    }

    #[test]
    fn setup_reports_but_permits_validation_errors_introduced_by_a_type_pack() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: error\n",
        )
        .unwrap();
        fs::create_dir_all(directory.path().join("_types")).unwrap();
        let current_type = "---\nkind: mdbase.type\nname: note\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    properties:\n      title: { type: string }\n---\n";
        let desired_type = "---\nkind: mdbase.type\nname: note\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    required: [title]\n    properties:\n      title: { type: string }\n---\n";
        fs::write(directory.path().join("_types/note.md"), current_type).unwrap();
        fs::write(directory.path().join("note.md"), "---\ntype: note\n---\n").unwrap();
        let collection = Collection::open(directory.path()).unwrap();
        assert!(collection_validation_errors(&collection).is_empty());

        let mut declaration = empty_setup("dev.example.editor");
        declaration
            .provisions
            .type_packs
            .push(CollectionSetupTypePack {
                provision: TypePackProvision {
                    manifest: json!({
                        "kind": "mdbase.type-pack",
                        "id": "example.note",
                        "version": "1.0.0",
                        "resources": [{
                            "kind": "type",
                            "mode": "managed",
                            "source": "note.md",
                            "target": "_types/note.md",
                            "digest": revision(desired_type.as_bytes()),
                        }],
                    }),
                    resources: vec![crate::v03::TypePackResource {
                        source: "note.md".to_string(),
                        document: desired_type.to_string(),
                    }],
                },
                options: CollectionSetupTypePackOptions {
                    adopt_resources: BTreeMap::from([(
                        "_types/note.md".to_string(),
                        revision(current_type.as_bytes()),
                    )]),
                    ..CollectionSetupTypePackOptions::default()
                },
            });

        let assessment = collection.assess_collection_setup(&declaration);
        assert!(assessment.valid, "{:?}", assessment.diagnostics);
        assert_eq!(assessment.result["introduced_diagnostic_count"], 1);
        assert_eq!(assessment.result["final_diagnostic_count"], 1);

        let applied =
            collection.apply_collection_setup(&declaration, &apply_options(&assessment.result));
        assert!(applied.valid, "{:?}", applied.diagnostics);
        assert_eq!(
            fs::read_to_string(directory.path().join("_types/note.md")).unwrap(),
            desired_type
        );
        let reopened = Collection::open(directory.path()).unwrap();
        assert_eq!(collection_validation_errors(&reopened).len(), 1);
    }

    #[test]
    fn setup_adds_one_value_atomically_and_is_idempotent() {
        let (directory, collection) = collection(
            "spec_version: 0.3.0\nsettings:\n  validation: warn\nx-unrelated:\n  retained: true\n",
        );
        let declaration = setup("dev.example.tasknotes");
        let assessment = collection.assess_collection_setup(&declaration);
        assert!(assessment.valid, "{:?}", assessment.diagnostics);
        assert_eq!(assessment.result["status"], "provision");
        assert_eq!(assessment.result["configuration"][0]["action"], "add");

        let applied =
            collection.apply_collection_setup(&declaration, &apply_options(&assessment.result));
        assert!(applied.valid, "{:?}", applied.diagnostics);
        let config: serde_yaml::Value =
            serde_yaml::from_slice(&fs::read(directory.path().join("mdbase.yaml")).unwrap())
                .unwrap();
        assert_eq!(
            config["x-obsidian"]["bases"]["include"],
            serde_yaml::to_value(vec!["views/tasknotes/**/*.base"]).unwrap()
        );
        assert_eq!(config["x-unrelated"]["retained"].as_bool(), Some(true));
        assert!(directory.path().join(PROVISION_LOCK_PATH).is_file());

        let reopened = Collection::open(directory.path()).unwrap();
        let repeated = reopened.assess_collection_setup(&declaration);
        assert!(repeated.valid);
        assert_eq!(repeated.result["status"], "current");
        assert_eq!(repeated.result["configuration"][0]["action"], "current");
        let repeated_plan = plan_collection_setup(&reopened, &declaration).unwrap();
        assert!(
            repeated_plan.shadow.is_none(),
            "current setup checks must not duplicate the collection"
        );
        let before = fs::read(directory.path().join("mdbase.yaml")).unwrap();
        let reapplied =
            reopened.apply_collection_setup(&declaration, &apply_options(&repeated.result));
        assert!(reapplied.valid, "{:?}", reapplied.diagnostics);
        assert_eq!(
            fs::read(directory.path().join("mdbase.yaml")).unwrap(),
            before
        );
    }

    #[test]
    fn wrong_target_type_is_a_structured_conflict_and_changes_nothing() {
        let (directory, collection) =
            collection("spec_version: 0.3.0\nx-obsidian:\n  bases:\n    include: user-policy\n");
        let before = fs::read(directory.path().join("mdbase.yaml")).unwrap();
        let declaration = setup("dev.example.tasknotes");
        let assessment = collection.assess_collection_setup(&declaration);
        assert!(assessment.valid);
        assert_eq!(assessment.result["status"], "conflict");
        assert_eq!(
            assessment.result["configuration"][0]["conflict"]["code"],
            "configuration_type_conflict"
        );
        let applied =
            collection.apply_collection_setup(&declaration, &apply_options(&assessment.result));
        assert!(!applied.valid);
        assert_eq!(applied.diagnostics[0].code, "collection_setup_conflict");
        assert_eq!(
            fs::read(directory.path().join("mdbase.yaml")).unwrap(),
            before
        );
        assert!(!directory.path().join(PROVISION_LOCK_PATH).exists());
    }

    #[test]
    fn stale_review_is_rejected_before_any_setup_file_changes() {
        let (directory, collection) = collection("spec_version: 0.3.0\n");
        let declaration = setup("dev.example.tasknotes");
        let assessment = collection.assess_collection_setup(&declaration);
        assert!(assessment.valid);
        fs::write(directory.path().join("concurrent.md"), "Concurrent\n").unwrap();
        let before = fs::read(directory.path().join("mdbase.yaml")).unwrap();
        let applied =
            collection.apply_collection_setup(&declaration, &apply_options(&assessment.result));
        assert!(!applied.valid);
        assert_eq!(applied.diagnostics[0].code, "concurrent_modification");
        assert_eq!(
            fs::read(directory.path().join("mdbase.yaml")).unwrap(),
            before
        );
        assert!(!directory.path().join(PROVISION_LOCK_PATH).exists());
    }

    #[test]
    fn same_value_has_one_yaml_member_and_multiple_sorted_contributors() {
        let (directory, collection) = collection("spec_version: 0.3.0\n");
        let first = setup("dev.example.zeta");
        let assessment = collection.assess_collection_setup(&first);
        assert!(
            collection
                .apply_collection_setup(&first, &apply_options(&assessment.result))
                .valid
        );

        let reopened = Collection::open(directory.path()).unwrap();
        let second = setup("dev.example.alpha");
        let assessment = reopened.assess_collection_setup(&second);
        assert_eq!(assessment.result["configuration"][0]["action"], "current");
        assert_eq!(assessment.result["status"], "provision");
        assert!(
            reopened
                .apply_collection_setup(&second, &apply_options(&assessment.result))
                .valid
        );

        let config: serde_yaml::Value =
            serde_yaml::from_slice(&fs::read(directory.path().join("mdbase.yaml")).unwrap())
                .unwrap();
        assert_eq!(
            config["x-obsidian"]["bases"]["include"]
                .as_sequence()
                .unwrap()
                .len(),
            1
        );
        let lock: ProvisionLock =
            serde_yaml::from_slice(&fs::read(directory.path().join(PROVISION_LOCK_PATH)).unwrap())
                .unwrap();
        assert_eq!(lock.contributions.len(), 1);
        assert_eq!(lock.contributions[0].contributors.len(), 2);
        assert_eq!(
            lock.contributions[0].contributors[0].application_id,
            "dev.example.alpha"
        );
        assert_eq!(
            lock.contributions[0].contributors[1].application_id,
            "dev.example.zeta"
        );
    }

    #[test]
    fn core_or_malformed_pointer_targets_are_rejected() {
        let (_directory, collection) = collection("spec_version: 0.3.0\n");
        for path in [
            "/settings/validation/include",
            "/x-obsidian/0/include",
            "/x-obsidian/~2/include",
        ] {
            let mut declaration = setup("dev.example.tasknotes");
            declaration.requirements.configuration[0].path = path.to_string();
            declaration.provisions.configuration[0].path = path.to_string();
            let result = collection.assess_collection_setup(&declaration);
            assert!(!result.valid, "{path}");
            assert_eq!(result.diagnostics[0].code, "invalid_collection_setup");
        }
    }

    #[test]
    fn type_pack_conflict_keeps_configuration_and_receipts_uncommitted() {
        let (directory, collection) = collection("spec_version: 0.3.0\n");
        fs::create_dir_all(directory.path().join("schemas")).unwrap();
        fs::write(
            directory.path().join("schemas/example.json"),
            "{\"type\":\"string\"}\n",
        )
        .unwrap();
        let desired = "{\"type\":\"object\"}\n";
        let mut declaration = setup("dev.example.tasknotes");
        declaration
            .provisions
            .type_packs
            .push(CollectionSetupTypePack {
                provision: TypePackProvision {
                    manifest: json!({
                        "kind": "mdbase.type-pack",
                        "id": "example.schemas",
                        "version": "1.0.0",
                        "resources": [{
                            "kind": "schema",
                            "mode": "managed",
                            "source": "example.json",
                            "target": "schemas/example.json",
                            "digest": revision(desired.as_bytes()),
                        }],
                    }),
                    resources: vec![crate::v03::TypePackResource {
                        source: "example.json".to_string(),
                        document: desired.to_string(),
                    }],
                },
                options: CollectionSetupTypePackOptions::default(),
            });
        let before_config = fs::read(directory.path().join("mdbase.yaml")).unwrap();
        let before_schema = fs::read(directory.path().join("schemas/example.json")).unwrap();
        let assessment = collection.assess_collection_setup(&declaration);
        assert!(assessment.valid, "{:?}", assessment.diagnostics);
        assert_eq!(assessment.result["status"], "conflict");
        assert_eq!(assessment.result["type_packs"][0]["applicable"], false);
        let applied =
            collection.apply_collection_setup(&declaration, &apply_options(&assessment.result));
        assert!(!applied.valid);
        assert_eq!(
            fs::read(directory.path().join("mdbase.yaml")).unwrap(),
            before_config
        );
        assert_eq!(
            fs::read(directory.path().join("schemas/example.json")).unwrap(),
            before_schema
        );
        assert!(!directory.path().join("mdbase.lock.yaml").exists());
        assert!(!directory.path().join(PROVISION_LOCK_PATH).exists());
    }

    #[test]
    fn provision_receipts_participate_in_provider_snapshot_revisions() {
        let (directory, collection) = collection("spec_version: 0.3.0\n");
        let before = collection.snapshot().unwrap();
        let declaration = setup("dev.example.tasknotes");
        let assessment = collection.assess_collection_setup(&declaration);
        assert!(
            collection
                .apply_collection_setup(&declaration, &apply_options(&assessment.result))
                .valid
        );
        let reopened = Collection::open(directory.path()).unwrap();
        let after = reopened.snapshot().unwrap();
        assert_ne!(before.revision, after.revision);
        assert_ne!(before.resource_revision, after.resource_revision);
        assert!(after.resources.iter().any(|resource| {
            resource.path == PROVISION_LOCK_PATH
                && resource.kind == crate::runtime::CollectionSnapshotResourceKind::Lock
        }));
    }
}
