use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{batch, validate_type_pack, Diagnostic, OperationResult};
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
pub struct TypePackInstall {
    pub manifest: Value,
    pub resources: Vec<TypePackResource>,
    pub provides: Vec<ContractIdentity>,
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

#[derive(Debug, Deserialize)]
struct ManifestResource {
    kind: String,
    source: String,
    target: String,
    digest: String,
}

impl Collection {
    /// Transactionally install one complete, self-contained mdbase type pack.
    pub fn install_type_pack(
        &self,
        manifest: &Value,
        resources: &[TypePackResource],
        replace: bool,
    ) -> OperationResult {
        self.install_type_pack_with_preconditions(manifest, resources, replace, &BTreeMap::new())
    }

    /// Transactionally install a type pack only if exact existing targets still
    /// match their reviewed revisions.
    pub fn install_type_pack_with_preconditions(
        &self,
        manifest: &Value,
        resources: &[TypePackResource],
        replace: bool,
        expected_revisions: &BTreeMap<String, String>,
    ) -> OperationResult {
        let diagnostics = validate_type_pack(manifest, "mdbase-pack.yaml");
        if !diagnostics.is_empty() {
            return failed(diagnostics);
        }
        let manifest_resources = match serde_json::from_value::<Vec<ManifestResource>>(
            manifest.get("resources").cloned().unwrap_or(Value::Null),
        ) {
            Ok(resources) => resources,
            Err(error) => {
                return failed(vec![Diagnostic::error(
                    "invalid_type_pack",
                    format!("Could not read type pack resources: {error}"),
                    Some("mdbase-pack.yaml".to_string()),
                )])
            }
        };
        let sources = resources
            .iter()
            .map(|resource| (resource.source.as_str(), resource.document.as_bytes()))
            .collect::<BTreeMap<_, _>>();
        if sources.len() != resources.len() {
            return pack_error("A type pack resource source may appear only once.");
        }
        let mut targets = BTreeSet::new();
        let mut planned = Vec::new();
        for resource in manifest_resources {
            if !matches!(resource.kind.as_str(), "contract" | "type" | "schema") {
                return pack_error("A type pack resource has an unsupported kind.");
            }
            if !targets.insert(resource.target.clone()) {
                return pack_error("A type pack target may appear only once.");
            }
            if let Err(error) = CollectionPath::new(&resource.source) {
                return pack_error(&format!("Unsafe type pack source: {error}"));
            }
            let Some(bytes) = sources.get(resource.source.as_str()) else {
                return pack_error(&format!(
                    "Type pack source '{}' is missing.",
                    resource.source
                ));
            };
            let actual = format!("sha256:{:x}", Sha256::digest(bytes));
            if actual != resource.digest {
                return pack_error(&format!(
                    "Type pack source '{}' has digest {}, expected {}.",
                    resource.source, actual, resource.digest
                ));
            }
            let target =
                match validate_resource_target(self, &resource.kind, &resource.target, bytes) {
                    Ok(path) => path,
                    Err(message) => return pack_error(&message),
                };
            if let Err(error) = crate::operations::ensure_no_symlink_components(
                &self.root,
                target.as_str(),
                SpecProfile::V03,
            ) {
                return pack_error(&format!("Unsafe type pack target: {error}"));
            }
            let live_path = target.under(&self.root);
            let action = if live_path.exists() {
                match fs::read(&live_path) {
                    Ok(existing) if existing == *bytes => "unchanged",
                    Ok(_) => "replace",
                    Err(error) => {
                        return pack_error(&format!(
                            "Could not inspect existing type pack target '{}': {error}",
                            target.as_str()
                        ))
                    }
                }
            } else {
                "create"
            };
            planned.push((
                target.to_string(),
                (*bytes).to_vec(),
                action,
                resource.digest,
                resource.kind,
            ));
        }
        if planned.len() != sources.len() {
            return pack_error("The type pack contains undeclared source resources.");
        }

        let shadow = match batch::shadow_collection(self) {
            Ok(shadow) => shadow,
            Err(diagnostic) => return failed(vec![*diagnostic]),
        };
        for (target, expected) in expected_revisions {
            if !planned
                .iter()
                .any(|(planned, _, _, _, _)| planned == target)
            {
                return pack_error(&format!(
                    "Type pack precondition target '{target}' is not a declared resource."
                ));
            }
            let Some(bytes) = shadow.baseline.get(target) else {
                return failed(vec![Diagnostic::error(
                    "concurrent_modification",
                    format!("Type pack target '{target}' no longer exists."),
                    Some(target.clone()),
                )]);
            };
            let actual = format!("sha256:{:x}", Sha256::digest(bytes));
            if &actual != expected {
                return failed(vec![Diagnostic::error(
                    "concurrent_modification",
                    format!("Type pack target '{target}' changed after it was reviewed."),
                    Some(target.clone()),
                )]);
            }
        }
        for (target, bytes, action, _, _) in &planned {
            let path = shadow.directory.path().join(target);
            if *action == "replace" && !replace && !expected_revisions.contains_key(target) {
                return failed(vec![Diagnostic::error(
                    "type_pack_conflict",
                    format!("Type pack target '{target}' already has different content."),
                    Some(target.clone()),
                )]);
            }
            if let Some(parent) = path.parent() {
                if let Err(error) = fs::create_dir_all(parent) {
                    return pack_error(&format!(
                        "Could not stage type pack target '{target}': {error}"
                    ));
                }
            }
            if let Err(error) = fs::write(&path, bytes) {
                return pack_error(&format!(
                    "Could not stage type pack target '{target}': {error}"
                ));
            }
        }
        let staged = match Collection::open(shadow.directory.path()) {
            Ok(staged) => staged,
            Err(error) => {
                return failed(vec![Diagnostic::error(
                    "invalid_type_pack",
                    format!("The staged type pack does not produce a valid collection: {error:?}"),
                    None,
                )])
            }
        };
        let validation = staged.validate_op(&json!({}));
        if validation.get("valid").and_then(Value::as_bool) != Some(true) {
            let diagnostics = validation
                .get("issues")
                .cloned()
                .and_then(|issues| serde_json::from_value(issues).ok())
                .unwrap_or_else(|| {
                    vec![Diagnostic::error(
                        "invalid_type_pack",
                        "Existing records do not conform after staging the type pack.",
                        None,
                    )]
                });
            return failed(diagnostics);
        }
        let desired = match batch::collect_collection_files(&staged) {
            Ok(desired) => desired,
            Err(diagnostic) => return failed(vec![*diagnostic]),
        };
        let commit = match crate::transactions::commit_migration(self, &shadow.baseline, &desired) {
            Ok(commit) => commit,
            Err(error) => {
                return failed(vec![Diagnostic::error(
                    "type_pack_apply_failed",
                    error.to_string(),
                    None,
                )])
            }
        };
        let committed = match Collection::open(&self.root) {
            Ok(committed) => committed,
            Err(error) => {
                return failed(vec![Diagnostic::error(
                    "type_pack_apply_failed",
                    format!("The committed type pack could not be reopened: {error:?}"),
                    None,
                )])
            }
        };
        let committed_validation = committed.validate_op(&json!({}));
        if committed_validation.get("valid").and_then(Value::as_bool) != Some(true) {
            return failed(vec![Diagnostic::error(
                "type_pack_apply_failed",
                "The committed type pack did not pass collection validation.",
                None,
            )]);
        }
        let resource_diff = planned
            .iter()
            .map(|(target, _, action, digest, kind)| {
                json!({
                    "target": target,
                    "action": action,
                    "digest": digest,
                    "kind": kind,
                })
            })
            .collect::<Vec<_>>();
        OperationResult {
            valid: true,
            result: json!({
                "id": manifest["id"],
                "version": manifest["version"],
                "resources": resource_diff,
                "cleanup_deferred": commit.cleanup_deferred,
            }),
            diagnostics: Vec::new(),
        }
    }

    /// Atomically install every type pack needed by one authorization decision.
    ///
    /// Contract choices are evaluated against one collection snapshot. Starter
    /// implementations are retained per contract, reviewed existing types are
    /// edited once even when several packs target them, and the combined result
    /// is validated and committed as one transaction.
    pub fn install_type_packs_with_contract_setups(
        &self,
        packs: &[TypePackInstall],
        setups: &[ContractSetupChoice],
    ) -> OperationResult {
        let prepared = match prepare_contract_setup_pack(self, packs, setups) {
            Ok(prepared) => prepared,
            Err(diagnostic) => return failed(vec![*diagnostic]),
        };
        self.install_type_pack_with_preconditions(
            &prepared.manifest,
            &prepared.resources,
            false,
            &prepared.expected_revisions,
        )
    }
}

struct PreparedContractSetupPack {
    manifest: Value,
    resources: Vec<TypePackResource>,
    expected_revisions: BTreeMap<String, String>,
}

#[derive(Debug)]
struct PlannedResource {
    kind: String,
    document: String,
}

type ContractSetupResult<T> = Result<T, Box<Diagnostic>>;

fn prepare_contract_setup_pack(
    collection: &Collection,
    packs: &[TypePackInstall],
    setups: &[ContractSetupChoice],
) -> ContractSetupResult<PreparedContractSetupPack> {
    if packs.is_empty() {
        return Err(contract_setup_diagnostic(
            "Contract setup requires at least one type pack.",
        ));
    }
    if setups.is_empty() {
        return Err(contract_setup_diagnostic(
            "Contract setup requires an explicit choice for at least one contract.",
        ));
    }
    let setup_by_contract = setups
        .iter()
        .map(|setup| (setup.contract.clone(), setup))
        .collect::<BTreeMap<_, _>>();
    if setup_by_contract.len() != setups.len() {
        return Err(contract_setup_diagnostic(
            "Each contract must have exactly one setup choice.",
        ));
    }
    let provided = packs
        .iter()
        .flat_map(|pack| pack.provides.iter().cloned())
        .collect::<BTreeSet<_>>();
    if setups
        .iter()
        .any(|setup| !provided.contains(&setup.contract))
    {
        return Err(contract_setup_diagnostic(
            "A setup choice refers to a contract not provided by these type packs.",
        ));
    }

    let mut planned = BTreeMap::<String, PlannedResource>::new();
    for pack in packs {
        let diagnostics = validate_type_pack(&pack.manifest, "mdbase-pack.yaml");
        if let Some(diagnostic) = diagnostics.into_iter().next() {
            return Err(Box::new(diagnostic));
        }
        let manifest_resources = serde_json::from_value::<Vec<ManifestResource>>(
            pack.manifest
                .get("resources")
                .cloned()
                .unwrap_or(Value::Null),
        )
        .map_err(|error| {
            contract_setup_diagnostic(format!("Could not read type pack resources: {error}"))
        })?;
        let sources = pack
            .resources
            .iter()
            .map(|resource| (resource.source.as_str(), resource.document.as_str()))
            .collect::<BTreeMap<_, _>>();
        if sources.len() != pack.resources.len() || sources.len() != manifest_resources.len() {
            return Err(contract_setup_diagnostic(
                "Each type pack source must be declared exactly once.",
            ));
        }
        let pack_contracts = pack.provides.iter().cloned().collect::<BTreeSet<_>>();
        let starter_contracts = pack_contracts
            .iter()
            .filter(|contract| {
                setup_by_contract
                    .get(*contract)
                    .is_some_and(|setup| matches!(setup.mode, ContractSetupMode::Starter))
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        let pack_has_starter = !starter_contracts.is_empty();

        for manifest_resource in manifest_resources {
            CollectionPath::new(&manifest_resource.source).map_err(|error| {
                contract_setup_diagnostic(format!("Unsafe type pack source: {error}"))
            })?;
            let Some(original) = sources.get(manifest_resource.source.as_str()) else {
                return Err(contract_setup_diagnostic(format!(
                    "Type pack source '{}' is missing.",
                    manifest_resource.source
                )));
            };
            let actual = format!("sha256:{:x}", Sha256::digest(original.as_bytes()));
            if actual != manifest_resource.digest {
                return Err(contract_setup_diagnostic(format!(
                    "Type pack source '{}' has digest {}, expected {}.",
                    manifest_resource.source, actual, manifest_resource.digest
                )));
            }
            let document = if manifest_resource.kind == "type" {
                filter_starter_type_document(
                    original,
                    &pack_contracts,
                    &starter_contracts,
                    pack_has_starter,
                )?
            } else {
                Some((*original).to_string())
            };
            let Some(document) = document else {
                continue;
            };
            insert_planned_resource(
                &mut planned,
                manifest_resource.target,
                PlannedResource {
                    kind: manifest_resource.kind,
                    document,
                },
            )?;
        }
    }

    let mut existing_documents = BTreeMap::<String, (String, String)>::new();
    let mut expected_revisions = BTreeMap::new();
    for setup in setups {
        let ContractSetupMode::Existing(existing) = &setup.mode else {
            continue;
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
        let entry = existing_documents
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

    for (target, (_, document)) in existing_documents {
        if !expected_revisions.contains_key(&target) {
            continue;
        }
        insert_planned_resource(
            &mut planned,
            target,
            PlannedResource {
                kind: "type".to_string(),
                document,
            },
        )?;
    }
    if planned.is_empty() {
        return Err(contract_setup_diagnostic(
            "Contract setup produced no collection resources.",
        ));
    }

    let mut manifest_resources = Vec::with_capacity(planned.len());
    let mut resources = Vec::with_capacity(planned.len());
    for (index, (target, resource)) in planned.into_iter().enumerate() {
        let extension = std::path::Path::new(&target)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("bin");
        let source = format!("contract-setup/resource-{index}.{extension}");
        manifest_resources.push(json!({
            "kind": resource.kind,
            "source": source,
            "target": target,
            "digest": format!("sha256:{:x}", Sha256::digest(resource.document.as_bytes())),
        }));
        resources.push(TypePackResource {
            source,
            document: resource.document,
        });
    }
    Ok(PreparedContractSetupPack {
        manifest: json!({
            "kind": "mdbase.type-pack",
            "id": "dev.mdbase.authorization-contract-setup",
            "version": "1.0.0",
            "resources": manifest_resources,
        }),
        resources,
        expected_revisions,
    })
}

fn insert_planned_resource(
    planned: &mut BTreeMap<String, PlannedResource>,
    target: String,
    resource: PlannedResource,
) -> ContractSetupResult<()> {
    if let Some(current) = planned.get(&target) {
        if current.kind == resource.kind && current.document == resource.document {
            return Ok(());
        }
        return Err(contract_setup_diagnostic(format!(
            "Contract setup contains conflicting resources for '{target}'."
        )));
    }
    planned.insert(target, resource);
    Ok(())
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

fn filter_starter_type_document(
    document: &str,
    provided: &BTreeSet<ContractIdentity>,
    starter: &BTreeSet<ContractIdentity>,
    pack_has_starter: bool,
) -> ContractSetupResult<Option<String>> {
    let (yaml_start, yaml_end) = frontmatter_bounds(document)?;
    let yaml = &document[yaml_start..yaml_end];
    let mut parsed = serde_yaml::from_str::<serde_yaml::Value>(yaml)
        .map_err(|_| contract_setup_diagnostic("A starter type has invalid YAML frontmatter."))?;
    let mapping = parsed
        .as_mapping_mut()
        .ok_or_else(|| contract_setup_diagnostic("A starter type has invalid YAML frontmatter."))?;
    let key = serde_yaml::Value::String("implements".to_string());
    let Some(value) = mapping.get_mut(&key) else {
        return Ok(pack_has_starter.then(|| document.to_string()));
    };
    let sequence = value.as_sequence_mut().ok_or_else(|| {
        contract_setup_diagnostic("A starter type has an unsupported implements declaration.")
    })?;
    let before = sequence.len();
    sequence.retain(|implementation| {
        implementation_identity(implementation)
            .is_none_or(|identity| !provided.contains(&identity) || starter.contains(&identity))
    });
    if sequence.is_empty() {
        return Ok(None);
    }
    if sequence.len() == before {
        return Ok(Some(document.to_string()));
    }
    replace_frontmatter(document, &parsed).map(Some)
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

fn replace_frontmatter(
    document: &str,
    frontmatter: &serde_yaml::Value,
) -> ContractSetupResult<String> {
    let (yaml_start, yaml_end) = frontmatter_bounds(document)?;
    let serialized = serde_yaml::to_string(frontmatter)
        .map_err(|_| contract_setup_diagnostic("A starter type could not be serialized."))?;
    let serialized = serialized.strip_prefix("---\n").unwrap_or(&serialized);
    let serialized = if document.contains("\r\n") {
        serialized.replace('\n', "\r\n")
    } else {
        serialized.to_string()
    };
    Ok(format!(
        "{}{}{}",
        &document[..yaml_start],
        serialized,
        &document[yaml_end..]
    ))
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

fn pack_error(message: &str) -> OperationResult {
    failed(vec![Diagnostic::error(
        "invalid_type_pack",
        message,
        Some("mdbase-pack.yaml".to_string()),
    )])
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
                "source": source,
                "target": target,
                "digest": format!("sha256:{:x}", Sha256::digest(document.as_bytes())),
            })).collect::<Vec<_>>(),
        })
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

    fn contract_pack(contract_id: &str, slug: &str) -> TypePackInstall {
        let schema = r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","required":["title"],"additionalProperties":false,"properties":{"title":{"type":"string"}}}"#.to_string();
        let contract = format!(
            "---\nkind: mdbase.contract\ncontract_type: record\nid: {contract_id}\nversion: 1.0.0\nrecord_schema:\n  dialect: json-schema-2020-12\n  ref: ../schemas/{slug}.schema.json\n---\n"
        );
        let starter = format!(
            "---\nkind: mdbase.type\nname: {slug}\nversion: 1\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    required: [title]\n    additionalProperties: true\n    properties:\n      title: {{ type: string }}\nimplements:\n  - contract: {contract_id}\n    version: 1.0.0\n    fields:\n      title: title\n---\n"
        );
        let definitions = [
            (
                "schema",
                format!("{slug}.schema.json"),
                format!("schemas/{slug}.schema.json"),
                schema,
            ),
            (
                "contract",
                format!("{slug}.contract.md"),
                format!("_contracts/{contract_id}.md"),
                contract,
            ),
            (
                "type",
                format!("{slug}.type.md"),
                format!("_types/{slug}.md"),
                starter,
            ),
        ];
        TypePackInstall {
            manifest: json!({
                "kind": "mdbase.type-pack",
                "id": format!("example.{slug}"),
                "version": "1.0.0",
                "resources": definitions.iter().map(|(kind, source, target, document)| json!({
                    "kind": kind,
                    "source": source,
                    "target": target,
                    "digest": format!("sha256:{:x}", Sha256::digest(document.as_bytes())),
                })).collect::<Vec<_>>(),
            }),
            resources: definitions
                .into_iter()
                .map(|(_, source, _, document)| TypePackResource { source, document })
                .collect(),
            provides: vec![ContractIdentity {
                id: contract_id.to_string(),
                version: "1.0.0".to_string(),
            }],
        }
    }

    fn combined_pack(mut left: TypePackInstall, right: TypePackInstall) -> TypePackInstall {
        left.manifest["resources"].as_array_mut().unwrap().extend(
            right.manifest["resources"]
                .as_array()
                .unwrap()
                .iter()
                .cloned(),
        );
        left.resources.extend(right.resources);
        left.provides.extend(right.provides);
        left
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

        let installed = collection.install_type_pack(&manifest, &resources, false);
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

        let repeated = reopened.install_type_pack(&manifest, &resources, false);
        assert!(repeated.valid, "{:?}", repeated.diagnostics);
        assert!(repeated.result["resources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|resource| resource["action"] == "unchanged"));
    }

    #[test]
    fn conflicts_leave_every_live_resource_unchanged() {
        let (root, collection) = collection();
        let definitions = task_resources();
        let resources = definitions
            .iter()
            .map(|(_, source, _, document)| resource(source, document))
            .collect::<Vec<_>>();
        assert!(
            collection
                .install_type_pack(&manifest(&definitions), &resources, false)
                .valid
        );
        let contract_path = root.path().join("_contracts/example.task.md");
        let before = fs::read(&contract_path).unwrap();

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
        let rejected = collection.install_type_pack(&manifest(&changed), &changed_resources, false);
        assert!(!rejected.valid);
        assert_eq!(rejected.diagnostics[0].code, "type_pack_conflict");
        assert_eq!(fs::read(contract_path).unwrap(), before);
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
        assert!(
            collection
                .install_type_pack(&manifest, &resources, false)
                .valid
        );
        let target = "_types/task.md".to_string();
        let original = fs::read(root.path().join(&target)).unwrap();
        let reviewed = format!("sha256:{:x}", Sha256::digest(&original));
        let externally_changed = String::from_utf8(original)
            .unwrap()
            .replace("required: [title]", "required: []");
        fs::write(root.path().join(&target), &externally_changed).unwrap();
        let reopened = Collection::open(root.path()).unwrap();

        let rejected = reopened.install_type_pack_with_preconditions(
            &manifest,
            &resources,
            true,
            &[(target.clone(), reviewed)].into_iter().collect(),
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
        let rejected = collection.install_type_pack(&invalid_digest, &resources, false);
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
        let rejected =
            collection.install_type_pack(&manifest(&invalid_registry), &invalid_resources, false);
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
            let rejected = collection.install_type_pack(
                &manifest(&[(kind, source, target, document)]),
                &[resource(source, document)],
                false,
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
        let rejected = collection.install_type_pack(
            &manifest(&[("type", "task.md", "_types/task.md", contract_document)]),
            &[resource("task.md", contract_document)],
            false,
        );

        assert!(!rejected.valid);
        assert_eq!(rejected.diagnostics[0].code, "invalid_type_pack");
        assert!(!root.path().join("_types/task.md").exists());
    }

    #[test]
    fn mixed_starter_and_existing_choices_in_one_pack_are_applied_per_contract() {
        let (root, _) = collection();
        write(&root.path().join("_types/note.md"), EXISTING_TYPE);
        let collection = Collection::open(root.path()).unwrap();
        let revision = format!("sha256:{:x}", Sha256::digest(EXISTING_TYPE.as_bytes()));
        let pack = combined_pack(
            contract_pack("example.alpha", "alpha"),
            contract_pack("example.beta", "beta"),
        );
        let result = collection.install_type_packs_with_contract_setups(
            &[pack],
            &[
                existing_setup("example.alpha", &revision),
                ContractSetupChoice {
                    contract: ContractIdentity {
                        id: "example.beta".to_string(),
                        version: "1.0.0".to_string(),
                    },
                    mode: ContractSetupMode::Starter,
                },
            ],
        );

        assert!(result.valid, "{:?}", result.diagnostics);
        assert!(!root.path().join("_types/alpha.md").exists());
        assert!(root.path().join("_types/beta.md").is_file());
        let note = fs::read_to_string(root.path().join("_types/note.md")).unwrap();
        assert!(note.contains("contract: example.alpha"));
        assert!(!note.contains("contract: example.beta"));
    }

    #[test]
    fn separate_packs_can_map_to_one_existing_type_and_retry_idempotently() {
        let (root, _) = collection();
        write(&root.path().join("_types/note.md"), EXISTING_TYPE);
        let collection = Collection::open(root.path()).unwrap();
        let revision = format!("sha256:{:x}", Sha256::digest(EXISTING_TYPE.as_bytes()));
        let packs = [
            contract_pack("example.alpha", "alpha"),
            contract_pack("example.beta", "beta"),
        ];
        let setups = [
            existing_setup("example.alpha", &revision),
            existing_setup("example.beta", &revision),
        ];

        let installed = collection.install_type_packs_with_contract_setups(&packs, &setups);
        assert!(installed.valid, "{:?}", installed.diagnostics);
        let once = fs::read_to_string(root.path().join("_types/note.md")).unwrap();
        assert!(once.contains("contract: example.alpha"));
        assert!(once.contains("contract: example.beta"));

        let reopened = Collection::open(root.path()).unwrap();
        let retried = reopened.install_type_packs_with_contract_setups(&packs, &setups);
        assert!(retried.valid, "{:?}", retried.diagnostics);
        assert_eq!(
            fs::read_to_string(root.path().join("_types/note.md")).unwrap(),
            once
        );
        assert!(retried.result["resources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|resource| resource["action"] == "unchanged"));
    }

    #[test]
    fn one_stale_existing_choice_rolls_back_the_complete_multi_pack_plan() {
        let (root, _) = collection();
        write(&root.path().join("_types/note.md"), EXISTING_TYPE);
        let collection = Collection::open(root.path()).unwrap();
        let revision = format!("sha256:{:x}", Sha256::digest(EXISTING_TYPE.as_bytes()));
        let rejected = collection.install_type_packs_with_contract_setups(
            &[
                contract_pack("example.alpha", "alpha"),
                contract_pack("example.beta", "beta"),
            ],
            &[
                existing_setup("example.alpha", &revision),
                existing_setup("example.beta", &format!("sha256:{}", "0".repeat(64))),
            ],
        );

        assert!(!rejected.valid);
        assert_eq!(rejected.diagnostics[0].code, "concurrent_modification");
        assert_eq!(
            fs::read_to_string(root.path().join("_types/note.md")).unwrap(),
            EXISTING_TYPE
        );
        assert!(!root.path().join("_contracts/example.alpha.md").exists());
        assert!(!root.path().join("_contracts/example.beta.md").exists());
    }
}
