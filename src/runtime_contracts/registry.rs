use std::collections::{BTreeMap, BTreeSet};

use jsonschema::JSONSchema;
use serde_json::Value;

use super::model::{
    ComposeOptions, ContractDocument, ContractEntry, ContractKind, ContractOrigin, ContractSource,
    PolicySelector, RuntimeDiagnostic, SourceKind,
};
use super::schemas::{CanonicalValidators, EmbeddedSchemas};

#[derive(Debug)]
pub struct RuntimeRegistry {
    pub providers: BTreeMap<String, ContractEntry>,
    pub actions: BTreeMap<String, ContractEntry>,
    pub events: BTreeMap<String, ContractEntry>,
    pub capabilities: BTreeMap<String, ContractEntry>,
    pub policies: BTreeMap<String, ContractEntry>,
    pub workflows: BTreeMap<String, ContractEntry>,
    pub provider_ids: BTreeSet<String>,
    pub capability_ids: BTreeSet<String>,
    pub selected_policy_ids: Vec<String>,
    pub diagnostics: Vec<RuntimeDiagnostic>,
    pub(crate) action_inputs: BTreeMap<String, std::sync::Arc<JSONSchema>>,
    pub(crate) action_outputs: BTreeMap<String, std::sync::Arc<JSONSchema>>,
    pub(crate) event_payloads: BTreeMap<String, std::sync::Arc<JSONSchema>>,
}

impl RuntimeRegistry {
    fn empty() -> Self {
        Self {
            providers: BTreeMap::new(),
            actions: BTreeMap::new(),
            events: BTreeMap::new(),
            capabilities: BTreeMap::new(),
            policies: BTreeMap::new(),
            workflows: BTreeMap::new(),
            provider_ids: BTreeSet::new(),
            capability_ids: BTreeSet::new(),
            selected_policy_ids: Vec::new(),
            diagnostics: Vec::new(),
            action_inputs: BTreeMap::new(),
            action_outputs: BTreeMap::new(),
            event_payloads: BTreeMap::new(),
        }
    }

    pub fn valid(&self) -> bool {
        !self
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == "error")
    }

    pub fn contract(&self, kind: ContractKind, id: &str) -> Option<&ContractEntry> {
        match kind {
            ContractKind::Provider => self.providers.get(id),
            ContractKind::Action => self.actions.get(id),
            ContractKind::Event => self.events.get(id),
            ContractKind::Capability => self.capabilities.get(id),
            ContractKind::Workflow => self.workflows.get(id),
            ContractKind::RuntimePolicy => self.policies.get(id),
            _ => None,
        }
    }

    pub fn selected_policy(&self) -> Option<&ContractEntry> {
        (self.selected_policy_ids.len() == 1)
            .then(|| self.policies.get(&self.selected_policy_ids[0]))
            .flatten()
            .filter(|entry| entry.contract.get("enabled") != Some(&Value::Bool(false)))
    }
}

pub(crate) fn compose(
    validators: &CanonicalValidators,
    mut sources: Vec<ContractSource>,
    options: &ComposeOptions,
) -> RuntimeRegistry {
    for source in &mut sources {
        sort_documents(&mut source.documents);
    }
    let mut sources = sources
        .into_iter()
        .map(|source| {
            let (rank, id) = source.kind.sort_key();
            (rank, id.to_string(), source_signature(&source), source)
        })
        .collect::<Vec<_>>();
    sources.sort_by(|left, right| (&left.0, &left.1, &left.2).cmp(&(&right.0, &right.1, &right.2)));
    let mut registry = RuntimeRegistry::empty();

    for (_, _, _, source) in sources {
        if let SourceKind::Provider { id } = &source.kind {
            let diagnostics = validate_provider_source(validators, id, &source.documents);
            if diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == "error")
            {
                registry.diagnostics.extend(diagnostics);
                continue;
            }
        }
        for document in source.documents {
            add_document(validators, &mut registry, &source.kind, document);
        }
    }
    resolve_selected_policies(&mut registry, &options.selected_policies);
    registry
}

fn add_document(
    validators: &CanonicalValidators,
    registry: &mut RuntimeRegistry,
    source: &SourceKind,
    document: ContractDocument,
) {
    let Some(kind) = document.kind().filter(|kind| kind.is_registry_contract()) else {
        return;
    };
    let validation = validators.validate_contract(&document);
    if !validation.valid {
        registry.diagnostics.extend(validation.diagnostics);
        return;
    }
    let Some(id) = document.id().map(str::to_string) else {
        return;
    };
    let origin = ContractOrigin {
        source: source.label(),
        location: document.path.clone(),
    };
    if let Some(existing) = map_ref(registry, kind).get(&id) {
        if canonical_json(&existing.contract) == canonical_json(&document.frontmatter) {
            let existing = map_mut(registry, kind)
                .get_mut(&id)
                .expect("existing contract must remain present");
            existing.origins.push(origin);
            existing.origins.sort_by(|left, right| {
                (&left.source, &left.location).cmp(&(&right.source, &right.location))
            });
            return;
        }
        let existing_origins = existing.origins.clone();
        let existing_version = existing.contract.get("version").cloned();
        registry.diagnostics.push(
            RuntimeDiagnostic::error(
                "contract_conflict",
                format!("Conflicting {} contract {id}.", kind.as_str()),
            )
            .for_id(&id)
            .at_path(&document.path)
            .with_details(serde_json::json!({
                "existing_origins": existing_origins,
                "new_origin": origin,
                "existing_version": existing_version,
                "new_version": document.frontmatter.get("version"),
            })),
        );
        return;
    }

    let (embedded, diagnostics) = validators.compile_embedded(&document);
    if !diagnostics.is_empty() {
        registry.diagnostics.extend(diagnostics);
        return;
    }
    install_compiled_schemas(registry, &id, embedded);
    add_effective_capabilities(registry, &document.frontmatter);
    map_mut(registry, kind).insert(
        id,
        ContractEntry {
            contract: document.frontmatter,
            origins: vec![origin],
        },
    );
}

fn install_compiled_schemas(registry: &mut RuntimeRegistry, id: &str, schemas: EmbeddedSchemas) {
    if let Some(schema) = schemas.action_input {
        registry.action_inputs.insert(id.to_string(), schema);
    }
    if let Some(schema) = schemas.action_output {
        registry.action_outputs.insert(id.to_string(), schema);
    }
    if let Some(schema) = schemas.event_payload {
        registry.event_payloads.insert(id.to_string(), schema);
    }
}

fn add_effective_capabilities(registry: &mut RuntimeRegistry, contract: &Value) {
    if let Some(provider) = contract.get("provider").and_then(Value::as_str) {
        registry.provider_ids.insert(provider.to_string());
    }
    match contract.get("type").and_then(Value::as_str) {
        Some("provider") => {
            if let Some(id) = contract.get("id").and_then(Value::as_str) {
                registry.provider_ids.insert(id.to_string());
            }
            add_string_array(
                &mut registry.capability_ids,
                contract.pointer("/contracts/capabilities"),
            );
        }
        Some("capability") => {
            if let Some(id) = contract.get("id").and_then(Value::as_str) {
                registry.capability_ids.insert(id.to_string());
            }
        }
        Some("action") => {
            add_string_array(&mut registry.capability_ids, contract.get("effects"));
            add_string_array(
                &mut registry.capability_ids,
                contract.pointer("/requires/capabilities"),
            );
        }
        Some("runtime_policy") => {
            if let Some(capabilities) = contract.get("capabilities").and_then(Value::as_object) {
                registry.capability_ids.extend(capabilities.keys().cloned());
            }
        }
        _ => {}
    }
}

fn add_string_array(target: &mut BTreeSet<String>, value: Option<&Value>) {
    target.extend(
        value
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string),
    );
}

fn resolve_selected_policies(registry: &mut RuntimeRegistry, selectors: &[PolicySelector]) {
    let mut resolved = Vec::new();
    for selector in selectors {
        let selected = match selector {
            PolicySelector::Id(id) => registry.policies.contains_key(id).then(|| id.clone()),
            PolicySelector::Path(path) => {
                let matches = registry
                    .policies
                    .iter()
                    .filter(|(_, entry)| {
                        entry.origins.iter().any(|origin| origin.location == *path)
                    })
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                (matches.len() == 1).then(|| matches[0].clone())
            }
        };
        match selected {
            Some(id) => resolved.push(id),
            None => {
                let value = match selector {
                    PolicySelector::Id(id) | PolicySelector::Path(id) => id,
                };
                registry.diagnostics.push(
                    RuntimeDiagnostic::error(
                        "policy_not_selected",
                        format!("Selected runtime policy {value} was not found."),
                    )
                    .for_id(value),
                );
            }
        }
    }
    resolved.sort();
    resolved.dedup();
    registry.selected_policy_ids = resolved;
}

fn validate_provider_source(
    validators: &CanonicalValidators,
    provider_id: &str,
    documents: &[ContractDocument],
) -> Vec<RuntimeDiagnostic> {
    let mut diagnostics = documents
        .iter()
        .flat_map(|document| validators.validate_contract(document).diagnostics)
        .collect::<Vec<_>>();
    let descriptors = documents
        .iter()
        .filter(|document| document.kind() == Some(ContractKind::Provider))
        .collect::<Vec<_>>();
    if descriptors.len() != 1 || descriptors[0].id() != Some(provider_id) {
        diagnostics.push(
            RuntimeDiagnostic::error(
                "provider_contract_mismatch",
                format!(
                    "Provider source {provider_id} must supply exactly one matching descriptor."
                ),
            )
            .for_id(provider_id),
        );
        return diagnostics;
    }
    let descriptor = &descriptors[0].frontmatter;
    for (list, kind) in [
        ("events", ContractKind::Event),
        ("actions", ContractKind::Action),
        ("capabilities", ContractKind::Capability),
        ("workflows", ContractKind::Workflow),
    ] {
        let advertised = string_set(descriptor.pointer(&format!("/contracts/{list}")));
        let supplied = documents
            .iter()
            .filter(|document| document.kind() == Some(kind))
            .filter_map(ContractDocument::id)
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        if advertised != supplied {
            diagnostics.push(
                RuntimeDiagnostic::error(
                    "provider_contract_mismatch",
                    format!(
                        "Provider {provider_id} advertised {list} do not match supplied contracts."
                    ),
                )
                .for_id(provider_id)
                .with_details(serde_json::json!({
                    "advertised": advertised,
                    "supplied": supplied,
                })),
            );
        }
    }
    for document in documents.iter().filter(|document| {
        matches!(
            document.kind(),
            Some(ContractKind::Action | ContractKind::Event | ContractKind::Capability)
        )
    }) {
        if document
            .frontmatter
            .get("provider")
            .and_then(Value::as_str)
            .is_some_and(|owner| owner != provider_id)
        {
            diagnostics.push(
                RuntimeDiagnostic::error(
                    "provider_contract_mismatch",
                    format!(
                        "Contract {} is not owned by provider {provider_id}.",
                        document.id().unwrap_or("<unknown>")
                    ),
                )
                .for_id(document.id().unwrap_or(provider_id))
                .at_path(&document.path),
            );
        }
    }
    diagnostics
}

fn string_set(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn map_mut(
    registry: &mut RuntimeRegistry,
    kind: ContractKind,
) -> &mut BTreeMap<String, ContractEntry> {
    match kind {
        ContractKind::Provider => &mut registry.providers,
        ContractKind::Action => &mut registry.actions,
        ContractKind::Event => &mut registry.events,
        ContractKind::Capability => &mut registry.capabilities,
        ContractKind::Workflow => &mut registry.workflows,
        ContractKind::RuntimePolicy => &mut registry.policies,
        _ => unreachable!("state records are not registry contracts"),
    }
}

fn map_ref(registry: &RuntimeRegistry, kind: ContractKind) -> &BTreeMap<String, ContractEntry> {
    match kind {
        ContractKind::Provider => &registry.providers,
        ContractKind::Action => &registry.actions,
        ContractKind::Event => &registry.events,
        ContractKind::Capability => &registry.capabilities,
        ContractKind::Workflow => &registry.workflows,
        ContractKind::RuntimePolicy => &registry.policies,
        _ => unreachable!("state records are not registry contracts"),
    }
}

fn sort_documents(documents: &mut [ContractDocument]) {
    documents.sort_by(|left, right| {
        let left_key = (
            left.kind().map(ContractKind::as_str).unwrap_or(""),
            left.id().unwrap_or(""),
            left.path.as_str(),
            canonical_json(&left.frontmatter),
        );
        let right_key = (
            right.kind().map(ContractKind::as_str).unwrap_or(""),
            right.id().unwrap_or(""),
            right.path.as_str(),
            canonical_json(&right.frontmatter),
        );
        left_key.cmp(&right_key)
    });
}

fn source_signature(source: &ContractSource) -> String {
    source
        .documents
        .iter()
        .map(|document| {
            format!(
                "{}\u{0}{}\u{0}{}",
                document.path,
                document.id().unwrap_or(""),
                canonical_json(&document.frontmatter)
            )
        })
        .collect::<Vec<_>>()
        .join("\u{1}")
}

pub(crate) fn canonical_json(value: &Value) -> String {
    match value {
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap(),
                        canonical_json(&object[key])
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        _ => serde_json::to_string(value).expect("JSON value must serialize"),
    }
}
