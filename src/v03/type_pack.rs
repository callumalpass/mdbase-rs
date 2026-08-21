use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{
    batch, revision, validate_type_pack, validate_type_pack_lock, Diagnostic, OperationResult,
};
use crate::api::CollectionPath;
use crate::frontmatter::parser::{is_parse_error, parse_document};
use crate::{Collection, SpecProfile};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TypePackResource {
    pub source: String,
    pub document: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub struct ContractIdentity {
    pub id: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TypePackProvision {
    pub manifest: Value,
    pub resources: Vec<TypePackResource>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TypePackAssessmentOptions {
    pub installed_by: String,
    #[serde(default)]
    pub adopt_resources: BTreeMap<String, String>,
    #[serde(default)]
    pub preserve_seed_targets: BTreeSet<String>,
    #[serde(default)]
    pub target_overrides: BTreeMap<String, String>,
    #[serde(default)]
    pub contract_setups: Vec<ContractSetupChoice>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TypePackApplyOptions {
    pub installed_by: String,
    pub expected_assessment_digest: String,
    #[serde(default)]
    pub allow_downgrade: bool,
    #[serde(default)]
    pub adopt_resources: BTreeMap<String, String>,
    #[serde(default)]
    pub preserve_seed_targets: BTreeSet<String>,
    #[serde(default)]
    pub target_overrides: BTreeMap<String, String>,
    #[serde(default)]
    pub contract_setups: Vec<ContractSetupChoice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ExistingContractImplementation {
    pub type_name: String,
    pub type_revision: String,
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ContractSetupMode {
    Starter,
    Existing(ExistingContractImplementation),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ContractSetupChoice {
    pub contract: ContractIdentity,
    #[serde(flatten)]
    pub mode: ContractSetupMode,
}

#[derive(Debug, Clone, Deserialize)]
struct ManifestResource {
    kind: String,
    mode: String,
    source: String,
    target: String,
    digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct TypePackReceiptResource {
    kind: String,
    mode: String,
    source: String,
    target: String,
    digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct TypePackReceipt {
    id: String,
    version: String,
    digest: String,
    installed_by: String,
    resources: Vec<TypePackReceiptResource>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TypePackLock {
    kind: String,
    lock_version: u32,
    packs: Vec<TypePackReceipt>,
}

#[derive(Debug)]
pub(crate) struct PlannedPackResource {
    kind: String,
    mode: String,
    source: String,
    target: String,
    pub(crate) action: String,
    digest: String,
    current_digest: Option<String>,
    installed_digest: Option<String>,
    adopted_from_digest: Option<String>,
    pub(crate) reason: Option<String>,
    bytes: Option<Vec<u8>>,
}

#[derive(Debug)]
pub(crate) struct TypePackPlan {
    pub(crate) assessment: Value,
    pub(crate) assessment_digest: String,
    next_lock: TypePackLock,
    next_lock_bytes: Vec<u8>,
    resources: Vec<PlannedPackResource>,
    contract_setup: Option<PreparedContractSetupPack>,
}

const TYPE_PACK_LOCK_PATH: &str = "mdbase.lock.yaml";

impl Collection {
    /// Inspect one exact managed type pack without changing collection files.
    pub fn assess_type_pack(
        &self,
        provision: &TypePackProvision,
        options: &TypePackAssessmentOptions,
    ) -> OperationResult {
        match plan_type_pack(self, provision, options) {
            Ok(plan) => OperationResult {
                valid: true,
                result: plan.assessment,
                diagnostics: Vec::new(),
            },
            Err(diagnostic) => failed(vec![*diagnostic]),
        }
    }

    /// Apply one reviewed managed type-pack assessment transactionally.
    pub fn apply_type_pack(
        &self,
        provision: &TypePackProvision,
        options: &TypePackApplyOptions,
    ) -> OperationResult {
        let assessment_options = TypePackAssessmentOptions {
            installed_by: options.installed_by.clone(),
            adopt_resources: options.adopt_resources.clone(),
            preserve_seed_targets: options.preserve_seed_targets.clone(),
            target_overrides: options.target_overrides.clone(),
            contract_setups: options.contract_setups.clone(),
        };
        let plan = match plan_type_pack(self, provision, &assessment_options) {
            Ok(plan) => plan,
            Err(diagnostic) => return failed(vec![*diagnostic]),
        };
        if plan.assessment_digest != options.expected_assessment_digest {
            return pack_diagnostic(
                "concurrent_modification",
                "The managed type-pack assessment is stale. Assess the collection again before applying it.",
            );
        }
        if plan.assessment["applicable"].as_bool() != Some(true) {
            let reason = plan
                .resources
                .iter()
                .find(|resource| resource.action == "conflict")
                .and_then(|resource| resource.reason.as_deref())
                .unwrap_or("The managed type pack has unresolved conflicts.");
            return pack_diagnostic("type_pack_conflict", reason);
        }
        if plan.assessment["status"].as_str() == Some("downgrade") && !options.allow_downgrade {
            return pack_diagnostic(
                "type_pack_downgrade",
                "A managed type-pack downgrade requires explicit approval.",
            );
        }
        apply_type_pack_plan(self, plan)
    }
}

pub(crate) fn plan_type_pack(
    collection: &Collection,
    provision: &TypePackProvision,
    options: &TypePackAssessmentOptions,
) -> Result<TypePackPlan, Box<Diagnostic>> {
    let installer_pattern = regex::Regex::new(r"^[a-z][a-z0-9]*(?:[._-][a-z0-9]+)+$")
        .expect("valid installer identity expression");
    if !installer_pattern.is_match(&options.installed_by) {
        return Err(Box::new(Diagnostic::error(
            "invalid_type_pack",
            "installed_by must be a stable namespaced identifier.",
            Some("mdbase-pack.yaml".to_string()),
        )));
    }
    let diagnostics = validate_type_pack(&provision.manifest, "mdbase-pack.yaml");
    if let Some(diagnostic) = diagnostics.into_iter().next() {
        return Err(Box::new(diagnostic));
    }
    let manifest_resources = serde_json::from_value::<Vec<ManifestResource>>(
        provision
            .manifest
            .get("resources")
            .cloned()
            .unwrap_or(Value::Null),
    )
    .map_err(|error| {
        Box::new(Diagnostic::error(
            "invalid_type_pack",
            format!("Could not read type pack resources: {error}"),
            Some("mdbase-pack.yaml".to_string()),
        ))
    })?;
    for target in options.target_overrides.keys() {
        if !manifest_resources
            .iter()
            .any(|resource| resource.target == *target)
        {
            return Err(pack_plan_error(format!(
                "Target override '{target}' is not a resource in the desired pack."
            )));
        }
    }
    let resolved_resources = manifest_resources
        .iter()
        .cloned()
        .map(|mut resource| {
            if let Some(target) = options.target_overrides.get(&resource.target) {
                resource.target = target.clone();
            }
            resource
        })
        .collect::<Vec<_>>();
    if resolved_resources
        .iter()
        .map(|resource| resource.target.as_str())
        .collect::<BTreeSet<_>>()
        .len()
        != resolved_resources.len()
    {
        return Err(pack_plan_error(
            "Target overrides resolve more than one resource to the same path.",
        ));
    }
    for target in options.adopt_resources.keys() {
        if !resolved_resources
            .iter()
            .any(|resource| resource.target == *target && resource.mode == "managed")
        {
            return Err(pack_plan_error(format!(
                "Adoption target '{target}' is not a managed resource in the desired pack."
            )));
        }
    }
    for target in &options.preserve_seed_targets {
        if !resolved_resources
            .iter()
            .any(|resource| resource.target == *target && resource.mode == "seed")
        {
            return Err(pack_plan_error(format!(
                "Preserved seed target '{target}' is not a seed resource in the desired pack."
            )));
        }
    }
    let sources = provision
        .resources
        .iter()
        .map(|resource| (resource.source.as_str(), resource.document.as_bytes()))
        .collect::<BTreeMap<_, _>>();
    if sources.len() != provision.resources.len() || sources.len() != manifest_resources.len() {
        return Err(pack_plan_error(
            "Each type-pack source must be declared exactly once.",
        ));
    }

    let (lock, lock_bytes) = read_type_pack_lock(collection)?;
    let pack_id = provision
        .manifest
        .get("id")
        .and_then(Value::as_str)
        .expect("validated pack ID")
        .to_string();
    let pack_version = provision
        .manifest
        .get("version")
        .and_then(Value::as_str)
        .expect("validated pack version")
        .to_string();
    let current = lock
        .packs
        .iter()
        .find(|receipt| receipt.id == pack_id)
        .cloned();
    let desired = TypePackReceipt {
        id: pack_id.clone(),
        version: pack_version.clone(),
        digest: jcs_digest(&provision.manifest)?,
        installed_by: current
            .as_ref()
            .map(|receipt| receipt.installed_by.clone())
            .unwrap_or_else(|| options.installed_by.clone()),
        resources: resolved_resources
            .iter()
            .map(|resource| TypePackReceiptResource {
                kind: resource.kind.clone(),
                mode: resource.mode.clone(),
                source: resource.source.clone(),
                target: resource.target.clone(),
                digest: resource.digest.clone(),
            })
            .collect(),
    };
    let current_resources = current
        .as_ref()
        .map(|receipt| {
            receipt
                .resources
                .iter()
                .map(|resource| (resource.source.as_str(), resource))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let other_owners = lock
        .packs
        .iter()
        .filter(|receipt| receipt.id != pack_id)
        .flat_map(|receipt| {
            receipt
                .resources
                .iter()
                .filter(|resource| resource.mode == "managed")
                .map(|resource| (resource.target.as_str(), receipt.id.as_str()))
        })
        .collect::<BTreeMap<_, _>>();

    let mut targets = BTreeSet::new();
    let mut planned = Vec::new();
    for resource in &resolved_resources {
        if !targets.insert(resource.target.clone()) {
            return Err(pack_plan_error("A type-pack target may appear only once."));
        }
        CollectionPath::new(&resource.source)
            .map_err(|error| pack_plan_error(format!("Unsafe type-pack source: {error}")))?;
        let bytes = sources.get(resource.source.as_str()).ok_or_else(|| {
            pack_plan_error(format!(
                "Type-pack source '{}' is missing.",
                resource.source
            ))
        })?;
        let actual = revision(bytes);
        if actual != resource.digest {
            return Err(pack_plan_error(format!(
                "Type-pack source '{}' has digest {}, expected {}.",
                resource.source, actual, resource.digest
            )));
        }
        let target = validate_resource_target(collection, &resource.kind, &resource.target, bytes)
            .map_err(pack_plan_error)?;
        crate::operations::ensure_no_symlink_components(
            &collection.root,
            target.as_str(),
            SpecProfile::V03,
        )
        .map_err(|error| pack_plan_error(format!("Unsafe type-pack target: {error}")))?;
        let before = read_optional(&target.under(&collection.root)).map_err(|error| {
            pack_plan_error(format!(
                "Could not inspect type-pack target '{}': {error}",
                resource.target
            ))
        })?;
        let current_digest = before.as_deref().map(revision);
        let installed = current_resources
            .get(resource.source.as_str())
            .copied()
            .filter(|installed| installed.target == resource.target);
        let owner = other_owners.get(resource.target.as_str()).copied();
        let mut adopted_from_digest = None;
        let (action, reason) = if let Some(owner) = owner {
            (
                "conflict",
                Some(format!("{} is managed by {}.", resource.target, owner)),
            )
        } else if resource.mode == "seed" {
            (
                if before.is_none()
                    && installed.is_none()
                    && !options.preserve_seed_targets.contains(&resource.target)
                {
                    "create"
                } else {
                    "preserve"
                },
                None,
            )
        } else if installed.is_none() {
            if before.is_none() {
                ("create", None)
            } else if current_digest.as_deref() == Some(resource.digest.as_str()) {
                adopted_from_digest = current_digest.clone();
                ("adopt", None)
            } else if options
                .adopt_resources
                .get(resource.target.as_str())
                .is_some_and(|expected| Some(expected.as_str()) == current_digest.as_deref())
            {
                adopted_from_digest = current_digest.clone();
                ("update", None)
            } else {
                (
                    "conflict",
                    Some(format!(
                        "{} exists but is not managed by {}.",
                        resource.target, pack_id
                    )),
                )
            }
        } else if installed.is_some_and(|installed| installed.mode != "managed") {
            (
                "conflict",
                Some(format!(
                    "{} was installed as a seed and cannot be claimed as managed implicitly.",
                    resource.target
                )),
            )
        } else if current_digest.as_deref() != installed.map(|installed| installed.digest.as_str())
        {
            (
                "conflict",
                Some(format!(
                    "{} changed since {} {} was applied.",
                    resource.target,
                    pack_id,
                    current
                        .as_ref()
                        .map(|receipt| receipt.version.as_str())
                        .unwrap_or("unknown")
                )),
            )
        } else if installed.is_some_and(|installed| installed.digest == resource.digest) {
            ("unchanged", None)
        } else {
            ("update", None)
        };
        planned.push(PlannedPackResource {
            kind: resource.kind.clone(),
            mode: resource.mode.clone(),
            source: resource.source.clone(),
            target: resource.target.clone(),
            action: action.to_string(),
            digest: resource.digest.clone(),
            current_digest,
            installed_digest: installed.map(|installed| installed.digest.clone()),
            adopted_from_digest,
            reason,
            bytes: Some((*bytes).to_vec()),
        });
    }

    if let Some(current) = &current {
        for resource in &current.resources {
            // A publisher may rename a source while retaining its installed
            // target. The desired resource now owns that path, so the old
            // receipt must not schedule a second, destructive retirement.
            if resolved_resources
                .iter()
                .any(|desired| desired.target == resource.target)
            {
                continue;
            }
            let target = CollectionPath::new(&resource.target).map_err(|error| {
                pack_plan_error(format!("Unsafe installed type-pack target: {error}"))
            })?;
            let before = read_optional(&target.under(&collection.root)).map_err(|error| {
                pack_plan_error(format!(
                    "Could not inspect installed type-pack target '{}': {error}",
                    resource.target
                ))
            })?;
            let current_digest = before.as_deref().map(revision);
            let (action, reason) = if resource.mode != "managed" {
                ("preserve", None)
            } else if current_digest.as_deref() == Some(resource.digest.as_str()) {
                ("delete", None)
            } else {
                (
                    "conflict",
                    Some(format!(
                        "{} changed and cannot be retired safely.",
                        resource.target
                    )),
                )
            };
            planned.push(PlannedPackResource {
                kind: resource.kind.clone(),
                mode: resource.mode.clone(),
                source: resource.source.clone(),
                target: resource.target.clone(),
                action: action.to_string(),
                digest: resource.digest.clone(),
                current_digest,
                installed_digest: Some(resource.digest.clone()),
                adopted_from_digest: None,
                reason,
                bytes: None,
            });
        }
    }

    let mut status = if planned.iter().any(|resource| resource.action == "conflict") {
        "conflict"
    } else if current.is_none() {
        "install"
    } else if current.as_ref().is_some_and(|receipt| {
        receipt.version == desired.version && receipt.digest == desired.digest
    }) {
        if current
            .as_ref()
            .is_some_and(|receipt| receipt.resources != desired.resources)
            || planned.iter().any(|resource| {
                !matches!(resource.action.as_str(), "unchanged" | "preserve" | "adopt")
            })
        {
            "reconfigure"
        } else {
            "current"
        }
    } else if current
        .as_ref()
        .is_some_and(|receipt| receipt.version == desired.version)
    {
        "conflict"
    } else {
        let current_version = Version::parse(&current.as_ref().expect("current receipt").version)
            .map_err(|error| {
            pack_plan_error(format!("Invalid installed pack version: {error}"))
        })?;
        let desired_version = Version::parse(&desired.version)
            .map_err(|error| pack_plan_error(format!("Invalid desired pack version: {error}")))?;
        if desired_version > current_version {
            "upgrade"
        } else {
            "downgrade"
        }
    };
    if status == "conflict"
        && current.as_ref().is_some_and(|receipt| {
            receipt.version == desired.version && receipt.digest != desired.digest
        })
        && !planned.iter().any(|resource| resource.action == "conflict")
    {
        if let Some(first) = planned.first_mut() {
            first.action = "conflict".to_string();
            first.reason = Some(format!(
                "{} {} has a different immutable pack digest. Publish a new version.",
                desired.id, desired.version
            ));
        }
        status = "conflict";
    }

    let mut packs = lock
        .packs
        .iter()
        .filter(|receipt| receipt.id != desired.id)
        .cloned()
        .collect::<Vec<_>>();
    packs.push(desired.clone());
    packs.sort_by(|left, right| left.id.cmp(&right.id));
    let next_lock = TypePackLock {
        kind: "mdbase.type-pack-lock".to_string(),
        lock_version: 1,
        packs,
    };
    let next_lock_bytes = serialize_type_pack_lock(&next_lock)?;
    let lock_action = match lock_bytes.as_deref() {
        None => "create",
        Some(current) if current == next_lock_bytes => "unchanged",
        Some(_) => "update",
    };
    let resource_values = planned
        .iter()
        .map(planned_resource_value)
        .collect::<Vec<_>>();
    let contract_setup = if options.contract_setups.is_empty() {
        None
    } else {
        Some(prepare_existing_contract_setups(
            collection,
            &options.contract_setups,
        )?)
    };
    let contract_setup_resources = contract_setup
        .as_ref()
        .map(prepared_contract_setup_resources)
        .transpose()?
        .unwrap_or_default();
    if status == "current" && !contract_setup_resources.is_empty() {
        status = "reconfigure";
    }
    let mut assessment = json!({
        "status": status,
        "applicable": status != "conflict",
        "desired": desired,
        "resources": resource_values,
        "lock": {
            "target": TYPE_PACK_LOCK_PATH,
            "action": lock_action,
            "digest": revision(&next_lock_bytes),
        },
        "contract_setups": {
            "choices": options.contract_setups,
            "resources": contract_setup_resources,
        },
    });
    if let Some(current) = current {
        assessment["current"] = serde_json::to_value(current).expect("receipt serializes");
    }
    let mut assessment_identity = assessment.clone();
    assessment_identity["lock_digest"] = lock_bytes
        .as_deref()
        .map(revision)
        .map(Value::String)
        .unwrap_or(Value::Null);
    let assessment_digest = jcs_digest(&assessment_identity)?;
    assessment["assessment_digest"] = Value::String(assessment_digest.clone());
    Ok(TypePackPlan {
        assessment,
        assessment_digest,
        next_lock,
        next_lock_bytes,
        resources: planned,
        contract_setup,
    })
}

fn apply_type_pack_plan(collection: &Collection, plan: TypePackPlan) -> OperationResult {
    let mut shadow = match batch::shadow_collection(collection) {
        Ok(shadow) => shadow,
        Err(diagnostic) => return failed(vec![*diagnostic]),
    };
    if let Err(diagnostic) = stage_type_pack_plan(&mut shadow, &plan) {
        return failed(vec![*diagnostic]);
    }
    let desired = match batch::collect_collection_files(&shadow.collection) {
        Ok(desired) => desired,
        Err(diagnostic) => return failed(vec![*diagnostic]),
    };
    let commit = match crate::transactions::commit_migration(collection, &shadow.baseline, &desired)
    {
        Ok(commit) => commit,
        Err(error) => return pack_diagnostic(error.code(), error.to_string()),
    };
    let _reopened = match Collection::open(&collection.root) {
        Ok(reopened) => reopened,
        Err(error) => {
            return pack_diagnostic(
                "type_pack_apply_failed",
                format!("The committed type pack could not be reopened: {error:?}"),
            )
        }
    };
    // Reopening verifies structural integrity. Record schema diagnostics are
    // data quality signals and do not make a type-pack installation invalid.
    let mut result = plan.assessment;
    result["receipt"] = serde_json::to_value(
        plan.next_lock
            .packs
            .iter()
            .find(|receipt| receipt.id == result["desired"]["id"])
            .expect("desired receipt retained"),
    )
    .expect("receipt serializes");
    result["cleanup_deferred"] = Value::Bool(commit.cleanup_deferred);
    OperationResult {
        valid: true,
        result,
        diagnostics: Vec::new(),
    }
}

pub(crate) fn stage_type_pack_plan(
    shadow: &mut batch::ShadowCollection,
    plan: &TypePackPlan,
) -> Result<(), Box<Diagnostic>> {
    for resource in &plan.resources {
        let path = shadow.directory.path().join(&resource.target);
        match resource.action.as_str() {
            "create" | "update" => {
                if let Some(parent) = path.parent() {
                    if let Err(error) = fs::create_dir_all(parent) {
                        return Err(pack_plan_error(format!(
                            "Could not stage '{}': {error}",
                            resource.target
                        )));
                    }
                }
                if let Err(error) = fs::write(&path, resource.bytes.as_deref().unwrap_or_default())
                {
                    return Err(pack_plan_error(format!(
                        "Could not stage '{}': {error}",
                        resource.target
                    )));
                }
            }
            "delete" => {
                if let Err(error) = fs::remove_file(&path) {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        return Err(pack_plan_error(format!(
                            "Could not retire '{}': {error}",
                            resource.target
                        )));
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(prepared) = &plan.contract_setup {
        stage_prepared_contract_setup(shadow, prepared, &plan.resources)?;
    }
    if let Err(error) = fs::write(
        shadow.directory.path().join(TYPE_PACK_LOCK_PATH),
        &plan.next_lock_bytes,
    ) {
        return Err(pack_plan_error(format!(
            "Could not stage {TYPE_PACK_LOCK_PATH}: {error}"
        )));
    }
    let staged = match Collection::open(shadow.directory.path()) {
        Ok(staged) => staged,
        Err(error) => {
            return Err(pack_plan_error(format!(
                "The staged type pack does not produce a valid collection: {error:?}"
            )))
        }
    };
    shadow.collection = staged;
    Ok(())
}

fn read_type_pack_lock(
    collection: &Collection,
) -> Result<(TypePackLock, Option<Vec<u8>>), Box<Diagnostic>> {
    let path = collection.root.join(TYPE_PACK_LOCK_PATH);
    let bytes = match fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(pack_plan_error(format!(
                "Could not read {TYPE_PACK_LOCK_PATH}: {error}"
            )))
        }
    };
    let Some(bytes) = bytes else {
        return Ok((
            TypePackLock {
                kind: "mdbase.type-pack-lock".to_string(),
                lock_version: 1,
                packs: Vec::new(),
            },
            None,
        ));
    };
    let yaml: serde_yaml::Value = serde_yaml::from_slice(&bytes).map_err(|error| {
        pack_plan_error(format!("Could not parse {TYPE_PACK_LOCK_PATH}: {error}"))
    })?;
    let value = crate::frontmatter::parser::yaml_to_json(&yaml);
    if let Some(diagnostic) = validate_type_pack_lock(&value, TYPE_PACK_LOCK_PATH)
        .into_iter()
        .next()
    {
        return Err(Box::new(diagnostic));
    }
    let lock: TypePackLock = serde_json::from_value(value).map_err(|error| {
        pack_plan_error(format!("Could not load {TYPE_PACK_LOCK_PATH}: {error}"))
    })?;
    let mut ids = BTreeSet::new();
    let mut targets = BTreeMap::new();
    for receipt in &lock.packs {
        if !ids.insert(receipt.id.as_str()) {
            return Err(pack_plan_error(format!(
                "{TYPE_PACK_LOCK_PATH} contains duplicate pack {}.",
                receipt.id
            )));
        }
        for resource in &receipt.resources {
            if resource.mode != "managed" {
                continue;
            }
            if let Some(owner) = targets.insert(resource.target.as_str(), receipt.id.as_str()) {
                return Err(pack_plan_error(format!(
                    "{TYPE_PACK_LOCK_PATH} assigns {} to both {} and {}.",
                    resource.target, owner, receipt.id
                )));
            }
        }
    }
    Ok((lock, Some(bytes)))
}

fn serialize_type_pack_lock(lock: &TypePackLock) -> Result<Vec<u8>, Box<Diagnostic>> {
    let mut bytes = serde_json::to_vec_pretty(lock).map_err(|error| {
        pack_plan_error(format!(
            "Could not serialize managed type-pack provenance: {error}"
        ))
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn read_optional(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn planned_resource_value(resource: &PlannedPackResource) -> Value {
    let mut value = json!({
        "kind": resource.kind,
        "mode": resource.mode,
        "source": resource.source,
        "target": resource.target,
        "action": resource.action,
        "digest": resource.digest,
    });
    if let Some(digest) = &resource.current_digest {
        value["current_digest"] = Value::String(digest.clone());
    }
    if let Some(digest) = &resource.installed_digest {
        value["installed_digest"] = Value::String(digest.clone());
    }
    if let Some(digest) = &resource.adopted_from_digest {
        value["adopted_from_digest"] = Value::String(digest.clone());
    }
    if let Some(reason) = &resource.reason {
        value["reason"] = Value::String(reason.clone());
    }
    value
}

fn jcs_digest(value: &Value) -> Result<String, Box<Diagnostic>> {
    let bytes = serde_jcs::to_vec(value).map_err(|error| {
        pack_plan_error(format!(
            "Could not canonicalize type-pack identity: {error}"
        ))
    })?;
    Ok(revision(&bytes))
}

fn pack_plan_error(message: impl Into<String>) -> Box<Diagnostic> {
    Box::new(Diagnostic::error(
        "invalid_type_pack",
        message,
        Some("mdbase-pack.yaml".to_string()),
    ))
}

fn pack_diagnostic(code: impl Into<String>, message: impl Into<String>) -> OperationResult {
    failed(vec![Diagnostic::error(
        code,
        message,
        Some("mdbase-pack.yaml".to_string()),
    )])
}

#[derive(Debug)]
struct PreparedContractSetupPack {
    manifest: Value,
    resources: Vec<TypePackResource>,
    expected_revisions: BTreeMap<String, String>,
}

fn prepared_contract_setup_resources(
    prepared: &PreparedContractSetupPack,
) -> Result<Vec<Value>, Box<Diagnostic>> {
    let manifest_resources = serde_json::from_value::<Vec<ManifestResource>>(
        prepared
            .manifest
            .get("resources")
            .cloned()
            .unwrap_or(Value::Null),
    )
    .map_err(|error| {
        contract_setup_diagnostic(format!(
            "Could not inspect prepared contract setup resources: {error}"
        ))
    })?;
    Ok(manifest_resources
        .into_iter()
        .map(|resource| {
            let current_digest = prepared.expected_revisions.get(&resource.target).cloned();
            json!({
                "kind": resource.kind,
                "source": resource.source,
                "target": resource.target,
                "action": "update",
                "digest": resource.digest,
                "current_digest": current_digest,
            })
        })
        .collect())
}

fn stage_prepared_contract_setup(
    shadow: &batch::ShadowCollection,
    prepared: &PreparedContractSetupPack,
    pack_resources: &[PlannedPackResource],
) -> Result<(), Box<Diagnostic>> {
    let manifest_resources = serde_json::from_value::<Vec<ManifestResource>>(
        prepared
            .manifest
            .get("resources")
            .cloned()
            .unwrap_or(Value::Null),
    )
    .map_err(|error| {
        contract_setup_diagnostic(format!(
            "Could not stage prepared contract setup resources: {error}"
        ))
    })?;
    let sources = prepared
        .resources
        .iter()
        .map(|resource| (resource.source.as_str(), resource.document.as_bytes()))
        .collect::<BTreeMap<_, _>>();
    for resource in manifest_resources {
        let bytes = sources.get(resource.source.as_str()).ok_or_else(|| {
            contract_setup_diagnostic(format!(
                "Prepared contract setup source '{}' is missing.",
                resource.source
            ))
        })?;
        let target =
            validate_resource_target(&shadow.collection, &resource.kind, &resource.target, bytes)
                .map_err(contract_setup_diagnostic)?;
        if pack_resources.iter().any(|pack_resource| {
            pack_resource.target == resource.target
                && matches!(
                    pack_resource.action.as_str(),
                    "create" | "update" | "delete"
                )
        }) {
            return Err(contract_setup_diagnostic(format!(
                "Contract setup target '{}' is also changed by the managed type pack.",
                resource.target
            )));
        }
        let expected = prepared
            .expected_revisions
            .get(target.as_str())
            .ok_or_else(|| {
                contract_setup_diagnostic(format!(
                    "Contract setup target '{}' has no reviewed revision.",
                    target.as_str()
                ))
            })?;
        let current = shadow.baseline.get(target.as_str()).ok_or_else(|| {
            Box::new(Diagnostic::error(
                "concurrent_modification",
                format!("Type pack target '{}' no longer exists.", target.as_str()),
                Some(target.to_string()),
            ))
        })?;
        if revision(current) != *expected {
            return Err(Box::new(Diagnostic::error(
                "concurrent_modification",
                format!(
                    "Type pack target '{}' changed after it was reviewed.",
                    target.as_str()
                ),
                Some(target.to_string()),
            )));
        }
        let path = shadow.directory.path().join(target.as_str());
        fs::write(&path, bytes).map_err(|error| {
            contract_setup_diagnostic(format!(
                "Could not stage contract setup target '{}': {error}",
                target.as_str()
            ))
        })?;
    }
    Ok(())
}

type ContractSetupResult<T> = Result<T, Box<Diagnostic>>;

fn prepare_existing_contract_setups(
    collection: &Collection,
    setups: &[ContractSetupChoice],
) -> ContractSetupResult<PreparedContractSetupPack> {
    if setups.is_empty() {
        return Err(contract_setup_diagnostic(
            "Existing-type setup requires at least one reviewed contract mapping.",
        ));
    }
    if setups
        .iter()
        .any(|setup| matches!(setup.mode, ContractSetupMode::Starter))
    {
        return Err(contract_setup_diagnostic(
            "Starter setup is controlled by seed resources in the managed type pack.",
        ));
    }
    let identities = setups
        .iter()
        .map(|setup| setup.contract.clone())
        .collect::<BTreeSet<_>>();
    if identities.len() != setups.len() {
        return Err(contract_setup_diagnostic(
            "Each contract must have exactly one existing-type setup choice.",
        ));
    }

    let mut documents = BTreeMap::<String, (String, String)>::new();
    let mut expected_revisions = BTreeMap::new();
    for setup in setups {
        let ContractSetupMode::Existing(existing) = &setup.mode else {
            unreachable!("starter setup rejected above")
        };
        validate_existing_setup(existing)?;
        let read = collection.read_type_file(&json!({ "name": existing.type_name }));
        if !read.valid {
            return Err(contract_setup_diagnostic(format!(
                "Type '{}' is no longer available.",
                existing.type_name
            )));
        }
        let target = read.result["path"].as_str().unwrap_or_default().to_string();
        let revision = read.result["revision"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let document = read.result["document"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if target.is_empty() || revision.is_empty() || document.is_empty() {
            return Err(contract_setup_diagnostic(format!(
                "Type '{}' is no longer available.",
                existing.type_name
            )));
        }
        let entry = documents
            .entry(target.clone())
            .or_insert((revision.clone(), document));
        match implementation_state(&entry.1, &setup.contract, existing)? {
            ImplementationState::Exact => continue,
            ImplementationState::Conflicting => {
                return Err(contract_setup_diagnostic(format!(
                    "Type '{}' already implements {} {} differently.",
                    existing.type_name, setup.contract.id, setup.contract.version
                )))
            }
            ImplementationState::Missing => {}
        }
        if entry.0 != existing.type_revision {
            return Err(Box::new(Diagnostic::error(
                "concurrent_modification",
                format!(
                    "Type '{}' changed after it was reviewed.",
                    existing.type_name
                ),
                Some(target),
            )));
        }
        entry.1 = add_contract_implementation(&entry.1, &setup.contract, existing)?;
        expected_revisions.insert(target, entry.0.clone());
    }

    let changed = documents
        .into_iter()
        .filter(|(target, _)| expected_revisions.contains_key(target))
        .collect::<BTreeMap<_, _>>();
    if changed.is_empty() {
        return Ok(PreparedContractSetupPack {
            manifest: json!({
                "kind": "mdbase.type-pack",
                "id": "dev.mdbase.existing-contract-setup",
                "version": "1.0.0",
                "resources": [],
            }),
            resources: Vec::new(),
            expected_revisions,
        });
    }

    let mut manifest_resources = Vec::with_capacity(changed.len());
    let mut resources = Vec::with_capacity(changed.len());
    for (index, (target, (_, document))) in changed.into_iter().enumerate() {
        let extension = Path::new(&target)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("md");
        let source = format!("contract-setup/resource-{index}.{extension}");
        manifest_resources.push(json!({
            "kind": "type",
            "mode": "managed",
            "source": source,
            "target": target,
            "digest": revision(document.as_bytes()),
        }));
        resources.push(TypePackResource { source, document });
    }
    Ok(PreparedContractSetupPack {
        manifest: json!({
            "kind": "mdbase.type-pack",
            "id": "dev.mdbase.existing-contract-setup",
            "version": "1.0.0",
            "resources": manifest_resources,
        }),
        resources,
        expected_revisions,
    })
}

fn validate_existing_setup(setup: &ExistingContractImplementation) -> ContractSetupResult<()> {
    if setup.type_name.trim().is_empty()
        || setup.type_name.len() > 100
        || setup.type_revision.len() > 100
        || setup.fields.len() > 100
        || setup.fields.iter().any(|(contract_field, type_field)| {
            contract_field.is_empty()
                || contract_field.len() > 500
                || type_field.is_empty()
                || type_field.len() > 500
        })
    {
        return Err(contract_setup_diagnostic(
            "The selected contract implementation is invalid.",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImplementationState {
    Missing,
    Exact,
    Conflicting,
}

fn implementation_state(
    document: &str,
    contract: &ContractIdentity,
    setup: &ExistingContractImplementation,
) -> ContractSetupResult<ImplementationState> {
    let (yaml_start, yaml_end) = frontmatter_bounds(document)?;
    let parsed = serde_yaml::from_str::<serde_yaml::Value>(&document[yaml_start..yaml_end])
        .map_err(|_| {
            contract_setup_diagnostic("The selected type has invalid YAML frontmatter.")
        })?;
    let Some(sequence) = parsed
        .as_mapping()
        .and_then(|mapping| mapping.get(serde_yaml::Value::String("implements".to_string())))
        .and_then(serde_yaml::Value::as_sequence)
    else {
        return Ok(ImplementationState::Missing);
    };
    let Some(existing) = sequence
        .iter()
        .find(|implementation| implementation_identity(implementation).as_ref() == Some(contract))
    else {
        return Ok(ImplementationState::Missing);
    };
    let mapping = existing.as_mapping().ok_or_else(|| {
        contract_setup_diagnostic("The selected type has invalid implements entries.")
    })?;
    let fields = mapping
        .get(serde_yaml::Value::String("fields".to_string()))
        .cloned()
        .unwrap_or_else(|| serde_yaml::Value::Mapping(Default::default()));
    let fields = serde_yaml::from_value::<BTreeMap<String, String>>(fields)
        .map_err(|_| contract_setup_diagnostic("The selected type has invalid field mappings."))?;
    let binding = mapping
        .get(serde_yaml::Value::String("binding".to_string()))
        .cloned()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| contract_setup_diagnostic("The selected type has an invalid binding."))?;
    let binding_matches =
        normalized_binding(binding.as_ref()) == normalized_binding(setup.binding.as_ref());
    Ok(if fields == setup.fields && binding_matches {
        ImplementationState::Exact
    } else {
        ImplementationState::Conflicting
    })
}

fn normalized_binding(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.as_object().is_some_and(serde_json::Map::is_empty))
}

fn implementation_identity(value: &serde_yaml::Value) -> Option<ContractIdentity> {
    let mapping = value.as_mapping()?;
    Some(ContractIdentity {
        id: mapping
            .get(serde_yaml::Value::String("contract".to_string()))?
            .as_str()?
            .to_string(),
        version: mapping
            .get(serde_yaml::Value::String("version".to_string()))?
            .as_str()?
            .to_string(),
    })
}

fn add_contract_implementation(
    document: &str,
    contract: &ContractIdentity,
    setup: &ExistingContractImplementation,
) -> ContractSetupResult<String> {
    let (yaml_start, yaml_end) = frontmatter_bounds(document)?;
    let yaml = &document[yaml_start..yaml_end];
    let parsed = serde_yaml::from_str::<serde_yaml::Value>(yaml).map_err(|_| {
        contract_setup_diagnostic("The selected type has invalid YAML frontmatter.")
    })?;
    let mapping = parsed.as_mapping().ok_or_else(|| {
        contract_setup_diagnostic("The selected type has invalid YAML frontmatter.")
    })?;
    let key = serde_yaml::Value::String("implements".to_string());
    let mut implementations = match mapping.get(&key) {
        None | Some(serde_yaml::Value::Null) => Vec::new(),
        Some(serde_yaml::Value::Sequence(values)) => values.clone(),
        Some(_) => {
            return Err(contract_setup_diagnostic(
                "The selected type has an unsupported implements declaration.",
            ))
        }
    };
    let mut implementation = serde_yaml::Mapping::new();
    implementation.insert(
        serde_yaml::Value::String("contract".to_string()),
        serde_yaml::Value::String(contract.id.clone()),
    );
    implementation.insert(
        serde_yaml::Value::String("version".to_string()),
        serde_yaml::Value::String(contract.version.clone()),
    );
    implementation.insert(
        serde_yaml::Value::String("fields".to_string()),
        serde_yaml::to_value(&setup.fields)
            .map_err(|_| contract_setup_diagnostic("The field mapping is invalid."))?,
    );
    if let Some(binding) = &setup.binding {
        implementation.insert(
            serde_yaml::Value::String("binding".to_string()),
            serde_yaml::to_value(binding)
                .map_err(|_| contract_setup_diagnostic("The binding is invalid."))?,
        );
    }
    implementations.push(serde_yaml::Value::Mapping(implementation));
    replace_yaml_node(
        document,
        yaml_start,
        yaml_end,
        "implements",
        &implementations,
    )
}

fn replace_yaml_node<T: Serialize>(
    document: &str,
    yaml_start: usize,
    yaml_end: usize,
    key: &str,
    value: &T,
) -> ContractSetupResult<String> {
    let yaml = &document[yaml_start..yaml_end];
    let serialized = serde_yaml::to_string(value)
        .map_err(|_| contract_setup_diagnostic("The contract setup could not be serialized."))?;
    let serialized = serialized.strip_prefix("---\n").unwrap_or(&serialized);
    let newline = if document.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let block = format!(
        "{key}:{newline}{}",
        serialized
            .trim_end_matches('\n')
            .lines()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join(newline)
    );
    let (node_start, node_end) = yaml_node_range(yaml, key).unwrap_or((yaml.len(), yaml.len()));
    let mut next_yaml = String::new();
    next_yaml.push_str(&yaml[..node_start]);
    if node_start == yaml.len() && !next_yaml.is_empty() && !next_yaml.ends_with(['\n', '\r']) {
        next_yaml.push_str(newline);
    }
    next_yaml.push_str(&block);
    next_yaml.push_str(newline);
    next_yaml.push_str(&yaml[node_end..]);
    let mut result = String::with_capacity(document.len() + block.len());
    result.push_str(&document[..yaml_start]);
    result.push_str(&next_yaml);
    result.push_str(&document[yaml_end..]);
    Ok(result)
}

fn frontmatter_bounds(document: &str) -> ContractSetupResult<(usize, usize)> {
    let mut lines = document.split_inclusive('\n');
    let first = lines
        .next()
        .ok_or_else(|| contract_setup_diagnostic("The selected type has no YAML frontmatter."))?;
    if first.trim_end_matches(['\n', '\r']).trim() != "---" {
        return Err(contract_setup_diagnostic(
            "The selected type has no YAML frontmatter.",
        ));
    }
    let yaml_start = first.len();
    let mut cursor = yaml_start;
    for line in lines {
        if line.trim_end_matches(['\n', '\r']).trim() == "---" {
            return Ok((yaml_start, cursor));
        }
        cursor += line.len();
    }
    Err(contract_setup_diagnostic(
        "The selected type has unterminated YAML frontmatter.",
    ))
}

fn yaml_node_range(yaml: &str, key: &str) -> Option<(usize, usize)> {
    let mut offsets = Vec::new();
    let mut cursor = 0;
    for line in yaml.split_inclusive('\n') {
        offsets.push((cursor, line));
        cursor += line.len();
    }
    if cursor < yaml.len() || yaml.is_empty() {
        offsets.push((cursor, &yaml[cursor..]));
    }
    let start_index = offsets
        .iter()
        .position(|(_, line)| top_level_yaml_key(line).as_deref() == Some(key))?;
    let start = offsets[start_index].0;
    let mut pending_trivia = None;
    for (offset, line) in offsets.iter().skip(start_index + 1) {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() || trimmed.starts_with('#') {
            pending_trivia.get_or_insert(*offset);
            continue;
        }
        if top_level_yaml_key(line).is_some() {
            return Some((start, pending_trivia.unwrap_or(*offset)));
        }
        pending_trivia = None;
    }
    Some((start, pending_trivia.unwrap_or(yaml.len())))
}

fn top_level_yaml_key(line: &str) -> Option<String> {
    let line = line.trim_end_matches(['\n', '\r']);
    if line.is_empty() || line.starts_with(char::is_whitespace) || line.starts_with('#') {
        return None;
    }
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if double => escaped = true,
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            ':' if !single && !double => {
                return serde_yaml::from_str::<String>(line[..index].trim()).ok()
            }
            _ => {}
        }
    }
    None
}

fn contract_setup_diagnostic(message: impl Into<String>) -> Box<Diagnostic> {
    Box::new(Diagnostic::error("invalid_contract_setup", message, None))
}

fn validate_resource_target(
    collection: &Collection,
    kind: &str,
    target: &str,
    bytes: &[u8],
) -> Result<CollectionPath, String> {
    let path =
        CollectionPath::new(target).map_err(|error| format!("Unsafe type pack target: {error}"))?;
    if path
        .as_str()
        .split('/')
        .any(|component| component.starts_with('.'))
    {
        return Err(format!(
            "Type pack {kind} target '{}' uses a hidden filesystem namespace.",
            path.as_str()
        ));
    }
    let platform = path.to_path_buf();
    let extension = platform.extension().and_then(|value| value.to_str());
    match kind {
        "type"
            if platform.starts_with(&collection.settings.types_folder)
                && !platform.starts_with(&collection.settings.migrations_folder)
                && extension == Some("md") =>
        {
            validate_markdown_kind(bytes, "mdbase.type", path.as_str())?;
        }
        "contract"
            if platform.starts_with(&collection.settings.contracts_folder)
                && extension == Some("md") =>
        {
            validate_markdown_kind(bytes, "mdbase.contract", path.as_str())?;
        }
        "schema"
            if extension == Some("json")
                && !platform.starts_with(&collection.settings.types_folder)
                && !platform.starts_with(&collection.settings.contracts_folder)
                && !platform.starts_with(&collection.settings.migrations_folder)
                && !platform.starts_with(&collection.settings.cache_folder)
                && platform.parent().is_some_and(|parent| {
                    parent.components().any(|component| {
                        matches!(component.as_os_str().to_str(), Some("schemas" | "_schemas"))
                    })
                }) =>
        {
            let schema: Value = serde_json::from_slice(bytes).map_err(|error| {
                format!(
                    "Type pack schema target '{}' is not valid JSON: {error}",
                    path.as_str()
                )
            })?;
            if !schema.is_object() {
                return Err(format!(
                    "Type pack schema target '{}' must contain a JSON Schema object.",
                    path.as_str()
                ));
            }
        }
        "type" => {
            return Err(format!(
                "Type pack type target '{}' must be a Markdown file below '{}'.",
                path.as_str(),
                collection.settings.types_folder
            ))
        }
        "contract" => {
            return Err(format!(
                "Type pack contract target '{}' must be a Markdown file below '{}'.",
                path.as_str(),
                collection.settings.contracts_folder
            ))
        }
        "schema" => {
            return Err(format!(
                "Type pack schema target '{}' must be a JSON file below a schemas directory.",
                path.as_str()
            ))
        }
        _ => return Err(format!("Unsupported type pack resource kind '{kind}'.")),
    }
    Ok(path)
}

fn validate_markdown_kind(bytes: &[u8], expected: &str, target: &str) -> Result<(), String> {
    let document = std::str::from_utf8(bytes)
        .map_err(|_| format!("Type pack target '{target}' is not valid UTF-8."))?;
    let parsed = parse_document(document);
    let frontmatter = match parsed.frontmatter {
        Some(serde_yaml::Value::Mapping(mapping)) => mapping,
        Some(value) if is_parse_error(&value) => {
            return Err(format!(
                "Type pack target '{target}' has invalid YAML frontmatter."
            ))
        }
        _ => {
            return Err(format!(
                "Type pack target '{target}' requires object frontmatter."
            ))
        }
    };
    let actual = frontmatter
        .get(serde_yaml::Value::String("kind".to_string()))
        .and_then(serde_yaml::Value::as_str);
    if actual != Some(expected) {
        return Err(format!(
            "Type pack target '{target}' must declare kind '{expected}'."
        ));
    }
    Ok(())
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
    use sha2::{Digest, Sha256};

    fn write(path: &std::path::Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn resource(source: &str, document: &str) -> TypePackResource {
        TypePackResource {
            source: source.to_string(),
            document: document.to_string(),
        }
    }

    fn manifest(resources: &[(&str, &str, &str, &str)]) -> Value {
        json!({
            "kind": "mdbase.type-pack",
            "id": "example.tasks",
            "version": "1.0.0",
            "resources": resources.iter().map(|(kind, source, target, document)| json!({
                "kind": kind,
                "mode": "managed",
                "source": source,
                "target": target,
                "digest": format!("sha256:{:x}", Sha256::digest(document.as_bytes())),
            })).collect::<Vec<_>>(),
        })
    }

    fn provision(manifest: Value, resources: Vec<TypePackResource>) -> TypePackProvision {
        TypePackProvision {
            manifest,
            resources,
        }
    }

    fn apply_pack(collection: &Collection, provision: &TypePackProvision) -> OperationResult {
        let assessment = collection.assess_type_pack(provision, &assessment_options());
        if !assessment.valid {
            return assessment;
        }
        collection.apply_type_pack(
            provision,
            &TypePackApplyOptions {
                installed_by: "dev.mdbase.tests".to_string(),
                expected_assessment_digest: assessment.result["assessment_digest"]
                    .as_str()
                    .expect("assessment digest")
                    .to_string(),
                allow_downgrade: false,
                adopt_resources: BTreeMap::new(),
                preserve_seed_targets: BTreeSet::new(),
                target_overrides: BTreeMap::new(),
                contract_setups: Vec::new(),
            },
        )
    }

    fn apply_pack_with_setups(
        collection: &Collection,
        provision: &TypePackProvision,
        contract_setups: Vec<ContractSetupChoice>,
    ) -> OperationResult {
        let assessment = collection.assess_type_pack(
            provision,
            &TypePackAssessmentOptions {
                contract_setups: contract_setups.clone(),
                ..assessment_options()
            },
        );
        if !assessment.valid {
            return assessment;
        }
        collection.apply_type_pack(
            provision,
            &TypePackApplyOptions {
                installed_by: "dev.mdbase.tests".to_string(),
                expected_assessment_digest: assessment.result["assessment_digest"]
                    .as_str()
                    .expect("assessment digest")
                    .to_string(),
                allow_downgrade: false,
                adopt_resources: BTreeMap::new(),
                preserve_seed_targets: BTreeSet::new(),
                target_overrides: BTreeMap::new(),
                contract_setups,
            },
        )
    }

    fn assessment_options() -> TypePackAssessmentOptions {
        TypePackAssessmentOptions {
            installed_by: "dev.mdbase.tests".to_string(),
            adopt_resources: BTreeMap::new(),
            preserve_seed_targets: BTreeSet::new(),
            target_overrides: BTreeMap::new(),
            contract_setups: Vec::new(),
        }
    }

    fn collection() -> (tempfile::TempDir, Collection) {
        let root = tempfile::tempdir().unwrap();
        write(
            &root.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: error\n",
        );
        let collection = Collection::open(root.path()).unwrap();
        (root, collection)
    }

    fn existing_setup(contract_id: &str, revision: &str) -> ContractSetupChoice {
        ContractSetupChoice {
            contract: ContractIdentity {
                id: contract_id.to_string(),
                version: "1.0.0".to_string(),
            },
            mode: ContractSetupMode::Existing(ExistingContractImplementation {
                type_name: "note".to_string(),
                type_revision: revision.to_string(),
                fields: [("title".to_string(), "title".to_string())]
                    .into_iter()
                    .collect(),
                binding: None,
            }),
        }
    }

    const EXISTING_TYPE: &str = r#"---
kind: mdbase.type
name: note
version: 1
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    additionalProperties: true
    properties:
      title: { type: string }
---
Existing documentation.
"#;

    fn task_resources() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
        vec![
            (
                "schema",
                "task-contract.schema.json",
                "schemas/task-contract.schema.json",
                r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","required":["title"],"additionalProperties":false,"properties":{"title":{"type":"string"}}}"#,
            ),
            (
                "contract",
                "contract.md",
                "_contracts/example.task.md",
                r#"---
kind: mdbase.contract
contract_type: record
id: example.task
version: 1.0.0
record_schema:
  dialect: json-schema-2020-12
  ref: ../schemas/task-contract.schema.json
---
"#,
            ),
            (
                "type",
                "task.md",
                "_types/task.md",
                r#"---
kind: mdbase.type
name: task
version: 1
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    required: [title]
    additionalProperties: true
    properties:
      title: { type: string }
implements:
  - contract: example.task
    version: 1.0.0
    fields:
      title: title
---
"#,
            ),
        ]
    }

    #[test]
    fn installs_a_complete_pack_atomically_and_reports_an_exact_diff() {
        let (root, collection) = collection();
        let definitions = task_resources();
        let resources = definitions
            .iter()
            .map(|(_, source, _, document)| resource(source, document))
            .collect::<Vec<_>>();
        let manifest = manifest(&definitions);

        let provision = provision(manifest, resources);
        let installed = apply_pack(&collection, &provision);
        assert!(installed.valid, "{:?}", installed.diagnostics);
        assert_eq!(
            installed.result["resources"]
                .as_array()
                .unwrap()
                .iter()
                .map(|resource| resource["action"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["create", "create", "create"]
        );
        let reopened = Collection::open(root.path()).unwrap();
        assert_eq!(reopened.list_data_contracts().len(), 1);
        assert_eq!(
            reopened
                .get_data_contract_implementations("example.task", "1.0.0")
                .len(),
            1
        );

        let repeated = apply_pack(&reopened, &provision);
        assert!(repeated.valid, "{:?}", repeated.diagnostics);
        assert!(repeated.result["resources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|resource| resource["action"] == "unchanged"));
    }

    #[test]
    fn changing_a_resource_source_does_not_retire_its_desired_target() {
        let (root, collection) = collection();
        let definitions = task_resources();
        let resources = definitions
            .iter()
            .map(|(_, source, _, document)| resource(source, document))
            .collect::<Vec<_>>();
        let mut initial_manifest = manifest(&definitions);
        initial_manifest["resources"][2]["mode"] = Value::String("seed".to_string());
        let initial = provision(initial_manifest, resources);
        assert!(apply_pack(&collection, &initial).valid);

        let mut renamed = task_resources();
        renamed[0].1 = "schemas/example.task/1.0.0.schema.json";
        renamed[1].1 = "contracts/example.task/1.0.0.md";
        renamed[2].1 = "types/example-task/1.md";
        let renamed_resources = renamed
            .iter()
            .map(|(_, source, _, document)| resource(source, document))
            .collect::<Vec<_>>();
        let mut renamed_manifest = manifest(&renamed);
        renamed_manifest["version"] = Value::String("1.1.0".to_string());
        renamed_manifest["resources"][2]["mode"] = Value::String("seed".to_string());
        let renamed = provision(renamed_manifest, renamed_resources);

        let upgraded = apply_pack(&collection, &renamed);
        assert!(upgraded.valid, "{:?}", upgraded.diagnostics);
        assert_eq!(
            upgraded.result["resources"]
                .as_array()
                .unwrap()
                .iter()
                .map(|resource| resource["action"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["adopt", "adopt", "preserve"]
        );

        let reopened = Collection::open(root.path()).unwrap();
        assert_eq!(reopened.list_data_contracts().len(), 1);
        assert_eq!(
            reopened
                .get_data_contract_implementations("example.task", "1.0.0")
                .len(),
            1
        );
    }

    #[test]
    fn conflicts_leave_every_live_resource_unchanged() {
        let (root, collection) = collection();
        let definitions = task_resources();
        let resources = definitions
            .iter()
            .map(|(_, source, _, document)| resource(source, document))
            .collect::<Vec<_>>();
        let initial = provision(manifest(&definitions), resources);
        assert!(apply_pack(&collection, &initial).valid);
        let contract_path = root.path().join("_contracts/example.task.md");
        let before = fs::read(&contract_path).unwrap();
        fs::write(
            &contract_path,
            [before.as_slice(), b"\nUser change.\n"].concat(),
        )
        .unwrap();

        let mut changed = task_resources();
        changed[1].3 = r#"---
kind: mdbase.contract
contract_type: record
id: example.task
version: 2.0.0
record_schema:
  dialect: json-schema-2020-12
  ref: ../schemas/task-contract.schema.json
---
"#;
        let changed_resources = changed
            .iter()
            .map(|(_, source, _, document)| resource(source, document))
            .collect::<Vec<_>>();
        let changed = provision(manifest(&changed), changed_resources);
        let rejected = collection.assess_type_pack(&changed, &assessment_options());
        assert!(rejected.valid);
        assert_eq!(rejected.result["status"], "conflict");
        assert_eq!(
            fs::read(contract_path).unwrap(),
            [before, b"\nUser change.\n".to_vec()].concat()
        );
    }

    #[test]
    fn reviewed_target_revisions_are_checked_against_the_transaction_baseline() {
        let (root, collection) = collection();
        let definitions = task_resources();
        let resources = definitions
            .iter()
            .map(|(_, source, _, document)| resource(source, document))
            .collect::<Vec<_>>();
        let manifest = manifest(&definitions);
        let provision = provision(manifest, resources);
        assert!(apply_pack(&collection, &provision).valid);
        let target = "_types/task.md".to_string();
        let original = fs::read(root.path().join(&target)).unwrap();
        let assessment = collection.assess_type_pack(&provision, &assessment_options());
        let externally_changed = String::from_utf8(original)
            .unwrap()
            .replace("required: [title]", "required: []");
        fs::write(root.path().join(&target), &externally_changed).unwrap();
        let reopened = Collection::open(root.path()).unwrap();

        let rejected = reopened.apply_type_pack(
            &provision,
            &TypePackApplyOptions {
                installed_by: "dev.mdbase.tests".to_string(),
                expected_assessment_digest: assessment.result["assessment_digest"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                allow_downgrade: false,
                adopt_resources: BTreeMap::new(),
                preserve_seed_targets: BTreeSet::new(),
                target_overrides: BTreeMap::new(),
                contract_setups: Vec::new(),
            },
        );

        assert!(!rejected.valid);
        assert_eq!(rejected.diagnostics[0].code, "concurrent_modification");
        assert_eq!(
            fs::read_to_string(root.path().join(target)).unwrap(),
            externally_changed
        );
    }

    #[test]
    fn digest_or_registry_errors_create_no_partial_files() {
        let (root, collection) = collection();
        let definitions = task_resources();
        let resources = definitions
            .iter()
            .map(|(_, source, _, document)| resource(source, document))
            .collect::<Vec<_>>();
        let mut invalid_digest = manifest(&definitions);
        invalid_digest["resources"][1]["digest"] =
            Value::String(format!("sha256:{}", "0".repeat(64)));
        let rejected = apply_pack(&collection, &provision(invalid_digest, resources));
        assert!(!rejected.valid);
        assert!(!root.path().join("_contracts/example.task.md").exists());
        assert!(!root.path().join("_types/task.md").exists());
        assert!(!root
            .path()
            .join("schemas/task-contract.schema.json")
            .exists());

        let mut invalid_registry = task_resources();
        invalid_registry[2].3 = r#"---
kind: mdbase.type
name: task
version: 1
schema:
  dialect: json-schema-2020-12
  value: { type: object }
implements:
  - contract: missing.task
    version: 1.0.0
    fields: {}
---
"#;
        let invalid_resources = invalid_registry
            .iter()
            .map(|(_, source, _, document)| resource(source, document))
            .collect::<Vec<_>>();
        let rejected = apply_pack(
            &collection,
            &provision(manifest(&invalid_registry), invalid_resources),
        );
        assert!(!rejected.valid);
        assert!(!root.path().join("_contracts/example.task.md").exists());
        assert!(!root.path().join("_types/task.md").exists());
        assert!(!root
            .path()
            .join("schemas/task-contract.schema.json")
            .exists());
    }

    #[test]
    fn resource_kinds_cannot_write_outside_their_typed_namespaces() {
        let (root, collection) = collection();
        let type_document = r#"---
kind: mdbase.type
name: task
schema:
  dialect: json-schema-2020-12
  value: { type: object }
---
"#;
        let cases = [
            ("type", "payload.md", "payload.md", type_document),
            ("type", "payload.md", "_types/payload.exe", type_document),
            (
                "contract",
                "contract.md",
                "_types/contract.md",
                "---\nkind: mdbase.contract\ncontract_type: record\nid: example\nversion: 1.0.0\nrecord_schema:\n  dialect: json-schema-2020-12\n  value: { type: object }\n---\n",
            ),
            (
                "schema",
                "schema.json",
                ".git/hooks/schema.json",
                r#"{"type":"object"}"#,
            ),
            (
                "schema",
                "schema.json",
                "notes/schema.json",
                r#"{"type":"object"}"#,
            ),
        ];

        for (kind, source, target, document) in cases {
            let rejected = apply_pack(
                &collection,
                &provision(
                    manifest(&[(kind, source, target, document)]),
                    vec![resource(source, document)],
                ),
            );
            assert!(!rejected.valid, "{kind} target {target} was accepted");
            assert_eq!(rejected.diagnostics[0].code, "invalid_type_pack");
            assert!(
                !root.path().join(target).exists(),
                "rejected target {target} was written"
            );
        }
    }

    #[test]
    fn markdown_resource_kind_must_match_its_manifest_kind() {
        let (root, collection) = collection();
        let contract_document = "---\nkind: mdbase.contract\n---\n";
        let rejected = apply_pack(
            &collection,
            &provision(
                manifest(&[("type", "task.md", "_types/task.md", contract_document)]),
                vec![resource("task.md", contract_document)],
            ),
        );

        assert!(!rejected.valid);
        assert_eq!(rejected.diagnostics[0].code, "invalid_type_pack");
        assert!(!root.path().join("_types/task.md").exists());
    }

    #[test]
    fn reviewed_existing_type_mappings_apply_together_and_retry_idempotently() {
        let (root, _) = collection();
        write(&root.path().join("_types/note.md"), EXISTING_TYPE);
        let collection = Collection::open(root.path()).unwrap();
        let revision = revision(EXISTING_TYPE.as_bytes());
        let setups = [
            existing_setup("example.alpha", &revision),
            existing_setup("example.beta", &revision),
        ];
        let alpha = contract_document("example.alpha");
        let beta = contract_document("example.beta");
        let definitions = [
            (
                "contract",
                "alpha.md",
                "_contracts/example.alpha.md",
                alpha.as_str(),
            ),
            (
                "contract",
                "beta.md",
                "_contracts/example.beta.md",
                beta.as_str(),
            ),
        ];
        let provision = provision(
            manifest(&definitions),
            vec![resource("alpha.md", &alpha), resource("beta.md", &beta)],
        );

        let applied = apply_pack_with_setups(&collection, &provision, setups.to_vec());
        assert!(applied.valid, "{:?}", applied.diagnostics);
        let once = fs::read_to_string(root.path().join("_types/note.md")).unwrap();
        assert!(once.contains("contract: example.alpha"));
        assert!(once.contains("contract: example.beta"));

        let reopened = Collection::open(root.path()).unwrap();
        let retried = apply_pack_with_setups(&reopened, &provision, setups.to_vec());
        assert!(retried.valid, "{:?}", retried.diagnostics);
        assert!(retried.result["contract_setups"]["resources"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(
            fs::read_to_string(root.path().join("_types/note.md")).unwrap(),
            once
        );
    }

    #[test]
    fn stale_existing_type_review_writes_nothing() {
        let (root, _) = collection();
        write(&root.path().join("_types/note.md"), EXISTING_TYPE);
        let collection = Collection::open(root.path()).unwrap();
        let contract = contract_document("example.alpha");
        let provision = provision(
            manifest(&[(
                "contract",
                "alpha.md",
                "_contracts/example.alpha.md",
                contract.as_str(),
            )]),
            vec![resource("alpha.md", &contract)],
        );
        let rejected = apply_pack_with_setups(
            &collection,
            &provision,
            vec![existing_setup(
                "example.alpha",
                &format!("sha256:{}", "0".repeat(64)),
            )],
        );

        assert!(!rejected.valid);
        assert_eq!(rejected.diagnostics[0].code, "concurrent_modification");
        assert!(!root.path().join("_contracts/example.alpha.md").exists());
        assert_eq!(
            fs::read_to_string(root.path().join("_types/note.md")).unwrap(),
            EXISTING_TYPE
        );
    }

    fn contract_document(id: &str) -> String {
        format!(
            "---\nkind: mdbase.contract\ncontract_type: record\nid: {id}\nversion: 1.0.0\nrecord_schema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    required: [title]\n    properties:\n      title: {{ type: string }}\n---\n"
        )
    }
}
