//! First-class, collection-local mdbase data contracts.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use jsonschema::{Draft, JSONSchema};
use semver::Version;
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::frontmatter::parser::{is_parse_error, parse_document, yaml_to_json};
use crate::types::schema::{DataContractImplementation, TypeDef};
use crate::{Collection, Settings};

#[derive(Debug, Clone)]
pub struct DataContractLoadError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DataContractDefinition {
    pub kind: String,
    pub id: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding_schema: Option<Value>,
    pub source_paths: Vec<String>,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DataContractImplementationDescriptor {
    pub contract: String,
    pub version: String,
    pub contract_digest: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub type_version: u64,
    pub implementation_digest: String,
    pub fields: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContractViewDiagnostic {
    pub code: String,
    pub message: String,
    pub severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContractViewResult {
    pub valid: bool,
    pub contract: String,
    pub version: String,
    pub contract_digest: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub implementation_digest: String,
    pub view: Value,
    pub diagnostics: Vec<ContractViewDiagnostic>,
}

struct RegisteredContract {
    definition: DataContractDefinition,
    record_schema: Value,
    record_validator: JSONSchema,
    binding_validator: Option<JSONSchema>,
}

pub struct DataContractRegistry {
    contracts: HashMap<(String, String), RegisteredContract>,
    implementations: HashMap<(String, String), Vec<DataContractImplementationDescriptor>>,
}

impl DataContractRegistry {
    pub fn empty() -> Self {
        Self {
            contracts: HashMap::new(),
            implementations: HashMap::new(),
        }
    }

    pub fn load(
        collection_root: &Path,
        settings: &Settings,
        types: &HashMap<String, TypeDef>,
    ) -> Result<Self, DataContractLoadError> {
        let contracts_root = collection_root.join(&settings.contracts_folder);
        let mut registry = Self::empty();
        if contracts_root.exists() {
            for entry in WalkDir::new(&contracts_root)
                .follow_links(false)
                .sort_by_file_name()
                .into_iter()
            {
                let entry = entry.map_err(|error| {
                    load_error(
                        "invalid_data_contract",
                        format!("Could not inspect contracts folder: {error}"),
                    )
                })?;
                if !entry.file_type().is_file()
                    || entry.path().extension().and_then(|value| value.to_str()) != Some("md")
                {
                    continue;
                }
                registry.load_contract_file(collection_root, entry.path())?;
            }
        }

        let mut type_names = types.keys().cloned().collect::<Vec<_>>();
        type_names.sort();
        for type_name in type_names {
            let type_definition = &types[&type_name];
            for implementation in &type_definition.implementations {
                registry.register_implementation(type_definition, implementation)?;
            }
        }
        for descriptors in registry.implementations.values_mut() {
            descriptors.sort_by(|left, right| left.type_name.cmp(&right.type_name));
        }
        Ok(registry)
    }

    pub fn contracts(&self) -> Vec<DataContractDefinition> {
        let mut definitions = self
            .contracts
            .values()
            .map(|registered| registered.definition.clone())
            .collect::<Vec<_>>();
        definitions.sort_by(|left, right| {
            left.id.cmp(&right.id).then_with(|| {
                match (
                    Version::parse(&left.version),
                    Version::parse(&right.version),
                ) {
                    (Ok(left), Ok(right)) => left.cmp(&right),
                    _ => left.version.cmp(&right.version),
                }
            })
        });
        definitions
    }

    pub fn implementations(
        &self,
        contract: &str,
        version: &str,
    ) -> Vec<DataContractImplementationDescriptor> {
        self.implementations
            .get(&(contract.to_string(), version.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    pub fn project(
        &self,
        type_name: &str,
        contract: &str,
        version: &str,
        effective_frontmatter: &Value,
    ) -> ContractViewResult {
        let identity = (contract.to_string(), version.to_string());
        let registered = self.contracts.get(&identity);
        let implementation = self.implementations.get(&identity).and_then(|descriptors| {
            descriptors
                .iter()
                .find(|entry| entry.type_name == type_name)
        });
        let (Some(registered), Some(implementation)) = (registered, implementation) else {
            let code = if registered.is_some() {
                "data_contract_implementation_not_found"
            } else {
                "data_contract_not_found"
            };
            return ContractViewResult {
                valid: false,
                contract: contract.to_string(),
                version: version.to_string(),
                contract_digest: registered
                    .map(|value| value.definition.digest.clone())
                    .unwrap_or_default(),
                type_name: type_name.to_string(),
                implementation_digest: implementation
                    .map(|value| value.implementation_digest.clone())
                    .unwrap_or_default(),
                view: json!({}),
                diagnostics: vec![view_error(
                    code,
                    format!(
                        "Type '{type_name}' does not implement data contract '{contract}' {version}"
                    ),
                )],
            };
        };

        let mut view = Value::Object(Map::new());
        for (contract_field, record_field) in &implementation.fields {
            if let Some(value) = get_field_path(effective_frontmatter, record_field) {
                set_field_path(&mut view, contract_field, value.clone());
            }
        }
        let diagnostics = registered
            .record_validator
            .validate(&view)
            .err()
            .into_iter()
            .flatten()
            .map(|error| ContractViewDiagnostic {
                code: "data_contract_record_invalid".to_string(),
                message: format!(
                    "record projected through '{type_name}' does not satisfy '{contract}' {version}: {error}"
                ),
                severity: "error".to_string(),
                field: json_pointer_to_field_path(&error.instance_path.to_string()),
                path: None,
            })
            .collect::<Vec<_>>();
        ContractViewResult {
            valid: diagnostics.is_empty(),
            contract: contract.to_string(),
            version: version.to_string(),
            contract_digest: registered.definition.digest.clone(),
            type_name: type_name.to_string(),
            implementation_digest: implementation.implementation_digest.clone(),
            view,
            diagnostics,
        }
    }

    fn load_contract_file(
        &mut self,
        collection_root: &Path,
        path: &Path,
    ) -> Result<(), DataContractLoadError> {
        let relative = relative_path(collection_root, path);
        let content = std::fs::read_to_string(path).map_err(|error| {
            load_error(
                "invalid_data_contract",
                format!("Could not read data contract '{relative}': {error}"),
            )
        })?;
        let document = parse_document(&content);
        let yaml = match document.frontmatter {
            Some(value) if is_parse_error(&value) => {
                return Err(load_error(
                    "invalid_data_contract",
                    format!("Could not parse data contract '{relative}'"),
                ))
            }
            Some(value) => value,
            None => return Ok(()),
        };
        let frontmatter = yaml_to_json(&yaml);
        if frontmatter.get("kind").and_then(Value::as_str) != Some("mdbase.contract") {
            return Ok(());
        }
        if let Some(diagnostic) = crate::v03::validate_data_contract(&frontmatter, &relative)
            .into_iter()
            .find(|diagnostic| diagnostic.severity == "error")
        {
            return Err(load_error(
                "invalid_data_contract",
                format!(
                    "Data contract '{relative}' is invalid: {}",
                    diagnostic.message
                ),
            ));
        }
        let id = frontmatter["id"].as_str().unwrap_or_default().to_string();
        let version = frontmatter["version"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let schema_wrapper = frontmatter["schema"].clone();
        let record_schema = resolve_schema_wrapper(
            &schema_wrapper,
            path,
            collection_root,
            &format!("{id} {version} schema"),
        )?;
        let binding_wrapper = frontmatter.get("binding_schema").cloned();
        let binding_schema = binding_wrapper
            .as_ref()
            .map(|wrapper| {
                resolve_schema_wrapper(
                    wrapper,
                    path,
                    collection_root,
                    &format!("{id} {version} binding_schema"),
                )
            })
            .transpose()?;
        let record_validator = compile_schema(&record_schema, &format!("{id} {version} schema"))?;
        let binding_validator = binding_schema
            .as_ref()
            .map(|schema| compile_schema(schema, &format!("{id} {version} binding_schema")))
            .transpose()?;
        let portable = json!({
            "kind": "mdbase.contract",
            "id": id,
            "version": version,
            "schema": schema_wrapper,
        });
        let mut portable = portable.as_object().cloned().unwrap_or_default();
        if let Some(binding_wrapper) = &binding_wrapper {
            portable.insert("binding_schema".to_string(), binding_wrapper.clone());
        }
        let digest = digest_value(&Value::Object(portable));
        let identity = (id.clone(), version.clone());
        if let Some(existing) = self.contracts.get_mut(&identity) {
            if existing.definition.digest != digest {
                return Err(load_error(
                    "data_contract_conflict",
                    format!(
                        "data contract conflict for '{id}' {version}: {} and {relative} have different digests",
                        existing.definition.source_paths[0]
                    ),
                ));
            }
            existing.definition.source_paths.push(relative);
            existing.definition.source_paths.sort();
            return Ok(());
        }
        self.contracts.insert(
            identity,
            RegisteredContract {
                definition: DataContractDefinition {
                    kind: "mdbase.contract".to_string(),
                    id,
                    version,
                    name: frontmatter
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    description: frontmatter
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    schema: schema_wrapper,
                    binding_schema: binding_wrapper,
                    source_paths: vec![relative],
                    digest,
                },
                record_schema,
                record_validator,
                binding_validator,
            },
        );
        Ok(())
    }

    fn register_implementation(
        &mut self,
        type_definition: &TypeDef,
        implementation: &DataContractImplementation,
    ) -> Result<(), DataContractLoadError> {
        let identity = (
            implementation.contract.clone(),
            implementation.version.clone(),
        );
        let Some(contract) = self.contracts.get(&identity) else {
            return Err(load_error(
                "data_contract_not_found",
                format!(
                    "Type '{}' implements missing exact data contract '{}' {}",
                    type_definition.name, implementation.contract, implementation.version
                ),
            ));
        };
        for required in contract
            .record_schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !implementation.fields.contains_key(required) {
                return Err(load_error(
                    "data_contract_field_invalid",
                    format!(
                        "Type '{}' does not map required contract field '{}'",
                        type_definition.name, required
                    ),
                ));
            }
        }
        for (contract_field, record_field) in &implementation.fields {
            if !schema_declares_field(&contract.record_schema, contract_field) {
                return Err(load_error(
                    "data_contract_field_invalid",
                    format!(
                        "Type '{}' maps contract field '{}', but that contract field is not declared",
                        type_definition.name, contract_field
                    ),
                ));
            }
            if !type_definition
                .json_schema
                .as_ref()
                .is_some_and(|schema| schema_declares_field(schema, record_field))
            {
                return Err(load_error(
                    "data_contract_field_invalid",
                    format!(
                        "Type '{}' maps '{}' to '{}', but the record field is not declared",
                        type_definition.name, contract_field, record_field
                    ),
                ));
            }
        }
        let binding = implementation.binding.clone().unwrap_or_else(|| json!({}));
        if let Some(validator) = &contract.binding_validator {
            if let Err(mut errors) = validator.validate(&binding) {
                let message = errors
                    .next()
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "binding is invalid".to_string());
                return Err(load_error(
                    "data_contract_binding_invalid",
                    format!(
                        "Type '{}' has invalid binding for '{}' {}: {message}",
                        type_definition.name, implementation.contract, implementation.version
                    ),
                ));
            }
        } else if binding.as_object().is_some_and(|object| !object.is_empty()) {
            return Err(load_error(
                "data_contract_binding_invalid",
                format!(
                    "Type '{}' supplies a binding, but '{}' {} has no binding_schema",
                    type_definition.name, implementation.contract, implementation.version
                ),
            ));
        }

        let descriptor = DataContractImplementationDescriptor {
            contract: implementation.contract.clone(),
            version: implementation.version.clone(),
            contract_digest: contract.definition.digest.clone(),
            type_name: type_definition.name.clone(),
            type_version: type_definition.version.unwrap_or(1),
            implementation_digest: implementation_digest(
                &contract.definition.digest,
                type_definition,
                implementation,
            ),
            fields: implementation.fields.clone(),
            binding: implementation.binding.clone(),
            source_path: type_definition.source_path.clone(),
        };
        self.implementations
            .entry(identity)
            .or_default()
            .push(descriptor);
        Ok(())
    }
}

impl Collection {
    pub fn list_data_contracts(&self) -> Vec<DataContractDefinition> {
        self.data_contracts.contracts()
    }

    pub fn get_data_contract_implementations(
        &self,
        contract: &str,
        version: &str,
    ) -> Vec<DataContractImplementationDescriptor> {
        self.data_contracts.implementations(contract, version)
    }

    pub fn project_contract_type(
        &self,
        type_name: &str,
        contract: &str,
        version: &str,
        effective_frontmatter: &Value,
    ) -> ContractViewResult {
        self.data_contracts
            .project(type_name, contract, version, effective_frontmatter)
    }

    pub fn get_contract_view(
        &self,
        path: &str,
        contract: &str,
        version: &str,
        selected_type: Option<&str>,
    ) -> ContractViewResult {
        let read = self.read(&json!({"path": path}));
        if let Some(error) = read.get("error") {
            return failed_view(
                contract,
                version,
                selected_type.unwrap_or_default(),
                error["code"].as_str().unwrap_or("operation_failed"),
                error["message"]
                    .as_str()
                    .unwrap_or("Record could not be read"),
                Some(path),
            );
        }
        let implementations = self.data_contracts.implementations(contract, version);
        let candidates = read["types"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|type_name| {
                implementations
                    .iter()
                    .any(|implementation| implementation.type_name == *type_name)
            })
            .collect::<Vec<_>>();
        let selected = selected_type.or_else(|| {
            if candidates.len() == 1 {
                candidates.first().copied()
            } else {
                None
            }
        });
        let Some(selected) = selected else {
            return failed_view(
                contract,
                version,
                "",
                if candidates.is_empty() {
                    "data_contract_implementation_not_found"
                } else {
                    "data_contract_implementation_ambiguous"
                },
                if candidates.is_empty() {
                    format!("Record '{path}' has no type implementing '{contract}' {version}")
                } else {
                    format!(
                        "Record '{path}' matches multiple implementations of '{contract}' {version}; select one type explicitly: {}",
                        candidates.join(", ")
                    )
                },
                Some(path),
            );
        };
        if !candidates.contains(&selected) {
            return failed_view(
                contract,
                version,
                selected,
                "data_contract_implementation_not_found",
                format!("Record '{path}' does not match implementing type '{selected}'"),
                Some(path),
            );
        }
        let effective = read
            .get("effective_frontmatter")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let mut projected = self
            .data_contracts
            .project(selected, contract, version, &effective);
        for diagnostic in &mut projected.diagnostics {
            diagnostic.path = Some(path.to_string());
        }
        projected
    }

    pub(crate) fn data_contract_issues(
        &self,
        type_names: &[String],
        effective_frontmatter: &Value,
        path: &str,
    ) -> Vec<crate::errors::Issue> {
        type_names
            .iter()
            .filter_map(|type_name| self.types.get(type_name))
            .flat_map(|type_definition| {
                type_definition
                    .implementations
                    .iter()
                    .flat_map(|implementation| {
                        self.data_contracts
                            .project(
                                &type_definition.name,
                                &implementation.contract,
                                &implementation.version,
                                effective_frontmatter,
                            )
                            .diagnostics
                    })
            })
            .map(|diagnostic| crate::errors::Issue {
                code: diagnostic.code,
                message: diagnostic.message,
                path: Some(path.to_string()),
                field: diagnostic.field,
                severity: crate::errors::Severity::Error,
                expected: None,
                actual: None,
                type_name: None,
                line: None,
                column: None,
            })
            .collect()
    }
}

fn resolve_schema_wrapper(
    wrapper: &Value,
    source_path: &Path,
    collection_root: &Path,
    label: &str,
) -> Result<Value, DataContractLoadError> {
    let value = if let Some(value) = wrapper.get("value") {
        value.clone()
    } else if let Some(reference) = wrapper.get("ref").and_then(Value::as_str) {
        crate::v03::resolve_schema_ref(reference, source_path, collection_root).map_err(
            |diagnostic| load_error(diagnostic.code, format!("{label}: {}", diagnostic.message)),
        )?
    } else {
        return Err(load_error(
            "invalid_data_contract",
            format!("{label} must define schema.value or schema.ref"),
        ));
    };
    if let Some((code, reference)) = crate::v03::unsupported_schema_reference(&value) {
        return Err(load_error(
            code,
            format!("{label} contains unsupported embedded reference '{reference}'"),
        ));
    }
    Ok(value)
}

fn compile_schema(schema: &Value, label: &str) -> Result<JSONSchema, DataContractLoadError> {
    JSONSchema::options()
        .with_draft(Draft::Draft202012)
        .compile(schema)
        .map_err(|error| {
            load_error(
                "invalid_data_contract",
                format!("{label} is not valid JSON Schema: {error}"),
            )
        })
}

fn schema_declares_field(schema: &Value, field_path: &str) -> bool {
    let mut current = schema;
    for raw_segment in field_path.split('.') {
        let (segment, array) = raw_segment
            .strip_suffix("[]")
            .map(|segment| (segment, true))
            .unwrap_or((raw_segment, false));
        let Some(next) = current
            .get("properties")
            .and_then(Value::as_object)
            .and_then(|properties| properties.get(segment))
        else {
            return false;
        };
        current = next;
        if array {
            let Some(items) = current.get("items") else {
                return false;
            };
            current = items;
        }
    }
    true
}

fn implementation_digest(
    contract_digest: &str,
    type_definition: &TypeDef,
    implementation: &DataContractImplementation,
) -> String {
    let frontmatter = type_definition
        .v03_frontmatter
        .as_ref()
        .and_then(Value::as_object);
    let mut type_semantics = Map::new();
    for key in [
        "name",
        "version",
        "match",
        "schema",
        "collection",
        "lifecycle",
    ] {
        if let Some(value) = frontmatter.and_then(|value| value.get(key)) {
            type_semantics.insert(key.to_string(), value.clone());
        }
    }
    digest_value(&json!({
        "contract_digest": contract_digest,
        "type": type_semantics,
        "implementation": implementation,
    }))
}

pub fn data_contract_digest(frontmatter: &Value) -> String {
    let mut portable = Map::new();
    for key in ["kind", "id", "version", "schema", "binding_schema"] {
        if let Some(value) = frontmatter.get(key) {
            portable.insert(key.to_string(), value.clone());
        }
    }
    digest_value(&Value::Object(portable))
}

fn digest_value(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).expect("JSON values always serialize");
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn get_field_path<'a>(source: &'a Value, field_path: &str) -> Option<&'a Value> {
    let mut value = source;
    for raw_segment in field_path.split('.') {
        let (segment, array) = raw_segment
            .strip_suffix("[]")
            .map(|segment| (segment, true))
            .unwrap_or((raw_segment, false));
        value = value.get(segment)?;
        if array && !value.is_array() {
            return None;
        }
    }
    Some(value)
}

fn set_field_path(target: &mut Value, field_path: &str, value: Value) {
    let segments = field_path
        .split('.')
        .map(|segment| segment.strip_suffix("[]").unwrap_or(segment))
        .collect::<Vec<_>>();
    let mut current = target;
    for segment in &segments[..segments.len().saturating_sub(1)] {
        if current.get(*segment).and_then(Value::as_object).is_none() {
            current[*segment] = json!({});
        }
        current = &mut current[*segment];
    }
    if let Some(last) = segments.last() {
        current[*last] = value;
    }
}

fn json_pointer_to_field_path(pointer: &str) -> Option<String> {
    let value = pointer
        .trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.replace("~1", "/").replace("~0", "~"))
        .collect::<Vec<_>>()
        .join(".");
    (!value.is_empty()).then_some(value)
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn load_error(code: impl Into<String>, message: impl Into<String>) -> DataContractLoadError {
    DataContractLoadError {
        code: code.into(),
        message: message.into(),
    }
}

fn view_error(code: impl Into<String>, message: impl Into<String>) -> ContractViewDiagnostic {
    ContractViewDiagnostic {
        code: code.into(),
        message: message.into(),
        severity: "error".to_string(),
        field: None,
        path: None,
    }
}

fn failed_view(
    contract: &str,
    version: &str,
    type_name: &str,
    code: &str,
    message: impl Into<String>,
    path: Option<&str>,
) -> ContractViewResult {
    let mut diagnostic = view_error(code, message);
    diagnostic.path = path.map(str::to_string);
    ContractViewResult {
        valid: false,
        contract: contract.to_string(),
        version: version.to_string(),
        contract_digest: String::new(),
        type_name: type_name.to_string(),
        implementation_digest: String::new(),
        view: json!({}),
        diagnostics: vec![diagnostic],
    }
}
