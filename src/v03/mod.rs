//! mdbase v0.3 schema loading and canonical diagnostics.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use jsonschema::{Draft, JSONSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use walkdir::{DirEntry, WalkDir};

use crate::frontmatter::parser::{is_parse_error, parse_document, yaml_to_json};
use crate::{Collection, SpecProfile};
use schema_diagnostics::validation_diagnostic;

pub(crate) mod batch;
pub(crate) mod cel;
mod collection_setup;
mod lifecycle;
mod operations;
pub(crate) mod query;
mod schema_diagnostics;
mod type_pack;
pub(crate) mod write_membership;

pub use cel::{
    evaluate_runtime_expression, evaluate_runtime_template, validate_runtime_expression,
    WorkflowCelError,
};
pub use collection_setup::{
    CollectionSetup, CollectionSetupApplyOptions, CollectionSetupAssessment,
    CollectionSetupProvisions, CollectionSetupReceipt, CollectionSetupRequirements,
    CollectionSetupTypePack, CollectionSetupTypePackOptions, ConfigurationConflict,
    ConfigurationContributionReceipt, ConfigurationOperation, ConfigurationPredicate,
    ConfigurationProvision, ConfigurationRequirement, ConfigurationSetupAssessment,
};
pub use operations::{OperationResult, Operations};
pub use query::QueryPerformance;
pub use type_pack::{
    ContractIdentity, ContractSetupChoice, ContractSetupMode, ExistingContractImplementation,
    TypePackApplyOptions, TypePackAssessmentOptions, TypePackProvision, TypePackResource,
};

pub const SPEC_VERSION: &str = "0.3.0";
pub const PRERELEASE_SPEC_VERSIONS: &[&str] = &["0.3.0-alpha.1"];
pub const RUNTIME_PROFILE_VERSION: &str = "0.1.0";

pub(crate) fn revision(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub fn is_supported_spec_version(version: &str) -> bool {
    version == SPEC_VERSION || PRERELEASE_SPEC_VERSIONS.contains(&version)
}

const CONFIG_SCHEMA_ID: &str = "https://mdbase.dev/schemas/v0.3/config.schema.json";
const DATA_CONTRACT_SCHEMA_ID: &str = "https://mdbase.dev/schemas/v0.3/data-contract.schema.json";
const DIAGNOSTIC_SCHEMA_ID: &str = "https://mdbase.dev/schemas/v0.3/diagnostic.schema.json";
const TYPE_FILE_SCHEMA_ID: &str = "https://mdbase.dev/schemas/v0.3/type-file.schema.json";

const CONFIG_SCHEMA: &str = include_str!("../../schemas/v0.3/config.schema.json");
const DATA_CONTRACT_SCHEMA: &str = include_str!("../../schemas/v0.3/data-contract.schema.json");
const DIAGNOSTIC_SCHEMA: &str = include_str!("../../schemas/v0.3/diagnostic.schema.json");
const OPERATION_RESULT_SCHEMA: &str =
    include_str!("../../schemas/v0.3/operation-result.schema.json");
const PROVISION_LOCK_SCHEMA: &str = include_str!("../../schemas/v0.3/provision-lock.schema.json");
const QUERY_SCHEMA: &str = include_str!("../../schemas/v0.3/query.schema.json");
const QUERY_RESULT_SCHEMA: &str = include_str!("../../schemas/v0.3/query-result.schema.json");
const TYPE_FILE_SCHEMA: &str = include_str!("../../schemas/v0.3/type-file.schema.json");
const TYPE_PACK_SCHEMA: &str = include_str!("../../schemas/v0.3/type-pack.schema.json");
const TYPE_PACK_LOCK_SCHEMA: &str = include_str!("../../schemas/v0.3/type-pack-lock.schema.json");
const VIEW_SCHEMA: &str = include_str!("../../schemas/v0.3/view.schema.json");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: String,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl Diagnostic {
    pub fn error(
        code: impl Into<String>,
        message: impl Into<String>,
        path: Option<String>,
    ) -> Self {
        Self {
            severity: "error".to_string(),
            code: code.into(),
            message: message.into(),
            path,
            field: None,
            type_name: None,
            schema_location: None,
            details: None,
        }
    }
}

pub(super) fn collection_validation_errors(collection: &Collection) -> Vec<Diagnostic> {
    let validation = collection.validate_op(&serde_json::json!({}));
    let valid = validation.get("valid").and_then(Value::as_bool) == Some(true);
    let mut diagnostics = validation
        .get("issues")
        .cloned()
        .and_then(|issues| serde_json::from_value::<Vec<Diagnostic>>(issues).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|diagnostic| diagnostic.severity == "error")
        .collect::<Vec<_>>();
    if !valid && diagnostics.is_empty() {
        diagnostics.push(Diagnostic::error(
            "collection_validation_failed",
            "Collection validation failed without a structured error diagnostic.",
            None,
        ));
    }
    diagnostics
}

pub(super) fn introduced_validation_errors(
    baseline: &[Diagnostic],
    candidate: &[Diagnostic],
) -> Vec<Diagnostic> {
    // Compare multisets so setup may coexist with legacy errors while still
    // failing closed if it adds another instance of an existing diagnostic.
    let mut remaining = diagnostic_counts(baseline);
    let mut introduced = Vec::new();
    for diagnostic in candidate {
        let key = diagnostic_key(diagnostic);
        match remaining.get_mut(&key) {
            Some(count) if *count > 0 => *count -= 1,
            _ => introduced.push(diagnostic.clone()),
        }
    }
    introduced
}

pub(super) fn validation_diagnostic_digest(diagnostics: &[Diagnostic]) -> String {
    let counts = diagnostic_counts(diagnostics);
    let bytes = serde_jcs::to_vec(&counts).expect("diagnostic multiset canonicalizes");
    revision(&bytes)
}

fn diagnostic_counts(diagnostics: &[Diagnostic]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for diagnostic in diagnostics {
        *counts.entry(diagnostic_key(diagnostic)).or_default() += 1;
    }
    counts
}

fn diagnostic_key(diagnostic: &Diagnostic) -> String {
    serde_jcs::to_string(diagnostic).expect("diagnostic canonicalizes")
}

#[derive(Debug, Clone)]
pub struct TypeFile {
    pub path: String,
    pub name: String,
    pub version: Option<u64>,
    /// Digest of the exact Markdown source parsed into this definition.
    pub revision: String,
    pub frontmatter: Value,
    pub schema: Value,
}

#[derive(Debug, Clone)]
pub struct CollectionReport {
    pub valid: bool,
    pub config: Option<Value>,
    pub types: Vec<TypeFile>,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn validate_canonical_schemas() -> Result<(), String> {
    for (name, source) in [
        ("config", CONFIG_SCHEMA),
        ("data-contract", DATA_CONTRACT_SCHEMA),
        ("diagnostic", DIAGNOSTIC_SCHEMA),
        ("operation-result", OPERATION_RESULT_SCHEMA),
        ("provision-lock", PROVISION_LOCK_SCHEMA),
        ("query", QUERY_SCHEMA),
        ("query-result", QUERY_RESULT_SCHEMA),
        ("type-file", TYPE_FILE_SCHEMA),
        ("type-pack", TYPE_PACK_SCHEMA),
        ("type-pack-lock", TYPE_PACK_LOCK_SCHEMA),
        ("view", VIEW_SCHEMA),
    ] {
        let schema: Value = serde_json::from_str(source)
            .map_err(|error| format!("{name} schema is not valid JSON: {error}"))?;
        compile_canonical_schema(&schema)
            .map_err(|error| format!("{name} schema is not valid JSON Schema 2020-12: {error}"))?;
    }
    Ok(())
}

pub fn validate_config(value: &Value, path: &str) -> Vec<Diagnostic> {
    let mut canonical = value.clone();
    if canonical.get("spec_version").and_then(Value::as_str) != Some(SPEC_VERSION)
        && canonical
            .get("spec_version")
            .and_then(Value::as_str)
            .is_some_and(is_supported_spec_version)
    {
        canonical["spec_version"] = Value::String(SPEC_VERSION.to_string());
    }
    validate_canonical_value(CONFIG_SCHEMA, &canonical, path, CONFIG_SCHEMA_ID, None)
}

pub fn validate_type_file(value: &Value, path: &str) -> Vec<Diagnostic> {
    let type_name = value.get("name").and_then(Value::as_str);
    validate_canonical_value(
        TYPE_FILE_SCHEMA,
        value,
        path,
        TYPE_FILE_SCHEMA_ID,
        type_name,
    )
}

pub fn validate_data_contract(value: &Value, path: &str) -> Vec<Diagnostic> {
    validate_canonical_value(
        DATA_CONTRACT_SCHEMA,
        value,
        path,
        DATA_CONTRACT_SCHEMA_ID,
        None,
    )
}

pub fn validate_type_pack(value: &Value, path: &str) -> Vec<Diagnostic> {
    validate_canonical_value(
        TYPE_PACK_SCHEMA,
        value,
        path,
        "https://mdbase.dev/schemas/v0.3/type-pack.schema.json",
        None,
    )
}

pub(crate) fn validate_provision_lock(value: &Value, path: &str) -> Vec<Diagnostic> {
    validate_canonical_value(
        PROVISION_LOCK_SCHEMA,
        value,
        path,
        "https://mdbase.dev/schemas/v0.3/provision-lock.schema.json",
        None,
    )
}

pub fn validate_type_pack_lock(value: &Value, path: &str) -> Vec<Diagnostic> {
    validate_canonical_value(
        TYPE_PACK_LOCK_SCHEMA,
        value,
        path,
        "https://mdbase.dev/schemas/v0.3/type-pack-lock.schema.json",
        None,
    )
}

pub fn validate_query(value: &Value) -> Vec<Diagnostic> {
    validate_canonical_value(
        QUERY_SCHEMA,
        value,
        "query",
        "https://mdbase.dev/schemas/v0.3/query.schema.json",
        None,
    )
}

pub fn validate_query_result(value: &Value) -> Vec<Diagnostic> {
    validate_canonical_value(
        QUERY_RESULT_SCHEMA,
        value,
        "query-result",
        "https://mdbase.dev/schemas/v0.3/query-result.schema.json",
        None,
    )
}

/// Validate an ordinary `type: view` record against the canonical schema.
pub fn validate_view(value: &Value, path: &str) -> Vec<Diagnostic> {
    validate_canonical_value(
        VIEW_SCHEMA,
        value,
        path,
        "https://mdbase.dev/schemas/v0.3/view.schema.json",
        None,
    )
    .into_iter()
    .map(|mut diagnostic| {
        if diagnostic.code == "schema_validation_failed" {
            diagnostic.code = "invalid_view".to_string();
        }
        diagnostic
    })
    .collect()
}

pub fn validate_record(type_file: &TypeFile, value: &Value, path: &str) -> Vec<Diagnostic> {
    validate_value(
        &type_file.schema,
        value,
        path,
        "embedded://type/schema",
        Some(&type_file.name),
    )
}

pub fn validate_schema_instance(
    schema: &Value,
    value: &Value,
    path: &str,
    type_name: Option<&str>,
) -> Vec<Diagnostic> {
    validate_value(schema, value, path, "embedded://type/schema", type_name)
}

/// Read and validate only a collection's canonical configuration resource.
///
/// This does not inspect type definitions or ordinary records.
pub fn inspect_configuration(root: &Path) -> Result<Value, Vec<Diagnostic>> {
    let path = root.join("mdbase.yaml");
    let value = read_yaml_document(&path, "mdbase.yaml").map_err(|diagnostic| vec![*diagnostic])?;
    let diagnostics = validate_config(&value, "mdbase.yaml");
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == "error")
    {
        Err(diagnostics)
    } else {
        Ok(value)
    }
}

pub fn inspect_collection(root: &Path) -> CollectionReport {
    let mut diagnostics = Vec::new();
    let mut config = None;
    let mut types = Vec::new();
    let config_path = root.join("mdbase.yaml");
    let config_label = "mdbase.yaml";

    match read_yaml_document(&config_path, config_label) {
        Ok(value) => {
            diagnostics.extend(validate_config(&value, config_label));
            config = Some(value);
        }
        Err(diagnostic) => diagnostics.push(*diagnostic),
    }

    let types_folder = config
        .as_ref()
        .and_then(|value| value.pointer("/settings/types_folder"))
        .and_then(Value::as_str)
        .unwrap_or("_types");
    let types_root = root.join(types_folder);
    let mut names = HashSet::new();

    if types_root.exists() {
        let walker = WalkDir::new(&types_root)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(should_descend);
        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    diagnostics.push(Diagnostic::error(
                        "invalid_type_definition",
                        error.to_string(),
                        None,
                    ));
                    continue;
                }
            };
            if !entry.file_type().is_file()
                || entry.path().extension().and_then(|value| value.to_str()) != Some("md")
            {
                continue;
            }
            let relative = relative_path(root, entry.path());
            match fs::read_to_string(entry.path()) {
                Ok(text) => match parse_type_file(&text, entry.path(), root, &relative) {
                    Ok(type_file) => {
                        if !names.insert(type_file.name.to_ascii_lowercase()) {
                            diagnostics.push(Diagnostic::error(
                                "duplicate_type",
                                format!("Type '{}' is defined more than once.", type_file.name),
                                Some(relative),
                            ));
                        } else {
                            types.push(type_file);
                        }
                    }
                    Err(mut type_diagnostics) => diagnostics.append(&mut type_diagnostics),
                },
                Err(error) => diagnostics.push(Diagnostic::error(
                    "invalid_type_definition",
                    format!("Failed to read type file: {error}"),
                    Some(relative),
                )),
            }
        }
    }

    diagnostics.sort_by(|left, right| {
        (
            left.path.as_deref().unwrap_or(""),
            left.code.as_str(),
            left.field.as_deref().unwrap_or(""),
            left.message.as_str(),
        )
            .cmp(&(
                right.path.as_deref().unwrap_or(""),
                right.code.as_str(),
                right.field.as_deref().unwrap_or(""),
                right.message.as_str(),
            ))
    });
    let valid = !diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == "error");

    CollectionReport {
        valid,
        config,
        types,
        diagnostics,
    }
}

pub fn parse_type_file(
    text: &str,
    absolute_path: &Path,
    collection_root: &Path,
    relative_path: &str,
) -> Result<TypeFile, Vec<Diagnostic>> {
    let document = parse_document(text);
    let yaml = match document.frontmatter {
        Some(value) if is_parse_error(&value) => {
            return Err(vec![Diagnostic::error(
                "invalid_frontmatter",
                "Failed to parse YAML frontmatter.",
                Some(relative_path.to_string()),
            )]);
        }
        Some(value) => value,
        None => {
            return Err(vec![Diagnostic::error(
                "invalid_type_definition",
                "Type file has no frontmatter.",
                Some(relative_path.to_string()),
            )]);
        }
    };
    if !yaml.is_mapping() {
        return Err(vec![Diagnostic::error(
            "invalid_frontmatter",
            "Type file frontmatter must be a mapping.",
            Some(relative_path.to_string()),
        )]);
    }
    let frontmatter = yaml_to_json(&yaml);
    let mut diagnostics = validate_type_file(&frontmatter, relative_path);
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == "error")
    {
        return Err(diagnostics);
    }

    let name = frontmatter
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let version = frontmatter.get("version").and_then(Value::as_u64);
    let schema_wrapper = frontmatter.get("schema").and_then(Value::as_object);
    let schema = if let Some(value) = schema_wrapper.and_then(|wrapper| wrapper.get("value")) {
        value.clone()
    } else if let Some(reference) = schema_wrapper
        .and_then(|wrapper| wrapper.get("ref"))
        .and_then(Value::as_str)
    {
        match resolve_schema_ref(reference, absolute_path, collection_root) {
            Ok(value) => value,
            Err(diagnostic) => {
                diagnostics.push(*diagnostic);
                return Err(diagnostics);
            }
        }
    } else {
        return Err(vec![Diagnostic::error(
            "invalid_type_definition",
            "Type file must define schema.value or schema.ref.",
            Some(relative_path.to_string()),
        )]);
    };

    if let Some((code, reference)) = unsupported_schema_reference(&schema) {
        diagnostics.push(Diagnostic::error(
            code,
            if code == "schema_ref_forbidden" {
                format!("Type '{name}' contains forbidden JSON Schema reference '{reference}'.")
            } else {
                format!(
                    "Type '{name}' requires the optional external_schema_refs feature for '{reference}'."
                )
            },
            Some(relative_path.to_string()),
        ));
        return Err(diagnostics);
    }

    if let Err(error) = compile_schema(&schema) {
        diagnostics.push(Diagnostic {
            severity: "error".to_string(),
            code: "invalid_embedded_schema".to_string(),
            message: error,
            path: Some(relative_path.to_string()),
            field: Some("schema".to_string()),
            type_name: Some(name.clone()),
            schema_location: None,
            details: None,
        });
        return Err(diagnostics);
    }

    Ok(TypeFile {
        path: relative_path.to_string(),
        name,
        version,
        revision: format!("sha256:{:x}", Sha256::digest(text.as_bytes())),
        frontmatter,
        schema,
    })
}

/// Parse and validate a standalone `mdbase.contract` Markdown document.
///
/// This is intentionally usable without opening the collection so editors can
/// diagnose an in-memory contract while it is being changed.
pub fn parse_data_contract_file(
    text: &str,
    absolute_path: &Path,
    collection_root: &Path,
    relative_path: &str,
) -> Result<Value, Vec<Diagnostic>> {
    let document = parse_document(text);
    let yaml = match document.frontmatter {
        Some(value) if is_parse_error(&value) => {
            return Err(vec![Diagnostic::error(
                "invalid_frontmatter",
                "Failed to parse YAML frontmatter.",
                Some(relative_path.to_string()),
            )]);
        }
        Some(value) => value,
        None => {
            return Err(vec![Diagnostic::error(
                "invalid_data_contract",
                "Data contract file has no frontmatter.",
                Some(relative_path.to_string()),
            )]);
        }
    };
    if !yaml.is_mapping() {
        return Err(vec![Diagnostic::error(
            "invalid_frontmatter",
            "Data contract frontmatter must be a mapping.",
            Some(relative_path.to_string()),
        )]);
    }

    let frontmatter = yaml_to_json(&yaml);
    let mut diagnostics = validate_data_contract(&frontmatter, relative_path);
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == "error")
    {
        return Err(diagnostics);
    }

    for field in [
        "record_schema",
        "binding_schema",
        "data_schema",
        "source_schema",
        "input_schema",
        "output_schema",
        "error_schema",
        "provider_schema",
    ] {
        let Some(wrapper) = frontmatter.get(field) else {
            continue;
        };
        let schema = if let Some(value) = wrapper.get("value") {
            value.clone()
        } else if let Some(reference) = wrapper.get("ref").and_then(Value::as_str) {
            match resolve_schema_ref(reference, absolute_path, collection_root) {
                Ok(value) => value,
                Err(diagnostic) => {
                    diagnostics.push(*diagnostic);
                    continue;
                }
            }
        } else {
            continue;
        };
        if let Some((code, reference)) = unsupported_schema_reference(&schema) {
            diagnostics.push(Diagnostic {
                severity: "error".to_string(),
                code: code.to_string(),
                message: format!(
                    "Data contract {field} contains unsupported JSON Schema reference '{reference}'."
                ),
                path: Some(relative_path.to_string()),
                field: Some(field.to_string()),
                type_name: None,
                schema_location: None,
                details: None,
            });
            continue;
        }
        if let Err(error) = compile_schema(&schema) {
            diagnostics.push(Diagnostic {
                severity: "error".to_string(),
                code: "invalid_embedded_schema".to_string(),
                message: error,
                path: Some(relative_path.to_string()),
                field: Some(field.to_string()),
                type_name: None,
                schema_location: None,
                details: None,
            });
        }
    }

    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == "error")
    {
        Err(diagnostics)
    } else {
        Ok(frontmatter)
    }
}

fn read_yaml_document(path: &Path, relative_path: &str) -> Result<Value, Box<Diagnostic>> {
    let text = fs::read_to_string(path).map_err(|error| {
        Box::new(Diagnostic::error(
            if error.kind() == std::io::ErrorKind::NotFound {
                "missing_config"
            } else {
                "invalid_config"
            },
            format!("Failed to read {relative_path}: {error}"),
            Some(relative_path.to_string()),
        ))
    })?;
    let yaml: serde_yaml::Value = serde_yaml::from_str(&text).map_err(|error| {
        Box::new(Diagnostic::error(
            "invalid_config",
            format!("Failed to parse {relative_path}: {error}"),
            Some(relative_path.to_string()),
        ))
    })?;
    let value = yaml_to_json(&yaml);
    if !value.is_object() {
        return Err(Box::new(Diagnostic::error(
            "invalid_config",
            "Configuration must be a mapping.",
            Some(relative_path.to_string()),
        )));
    }
    Ok(value)
}

fn validate_canonical_value(
    schema_source: &str,
    value: &Value,
    path: &str,
    schema_id: &str,
    type_name: Option<&str>,
) -> Vec<Diagnostic> {
    let schema: Value = match serde_json::from_str(schema_source) {
        Ok(schema) => schema,
        Err(error) => {
            return vec![Diagnostic::error(
                "invalid_schema",
                error.to_string(),
                Some(path.to_string()),
            )]
        }
    };
    let compiled = match compile_canonical_schema(&schema) {
        Ok(compiled) => compiled,
        Err(error) => {
            return vec![Diagnostic::error(
                "invalid_schema",
                error,
                Some(path.to_string()),
            )]
        }
    };
    let diagnostics = match compiled.validate(value) {
        Ok(()) => Vec::new(),
        Err(errors) => errors
            .map(|error| validation_diagnostic(error, path, schema_id, type_name))
            .collect(),
    };
    diagnostics
}

fn validate_value(
    schema: &Value,
    value: &Value,
    path: &str,
    schema_id: &str,
    type_name: Option<&str>,
) -> Vec<Diagnostic> {
    if let Some((code, reference)) = unsupported_schema_reference(schema) {
        return vec![Diagnostic::error(
            code,
            if code == "schema_ref_forbidden" {
                format!("Forbidden JSON Schema reference: {reference}")
            } else {
                format!("The optional external_schema_refs feature is required for: {reference}")
            },
            Some(path.to_string()),
        )];
    }
    let compiled = match compile_schema(schema) {
        Ok(compiled) => compiled,
        Err(error) => {
            return vec![Diagnostic::error(
                "invalid_schema",
                error,
                Some(path.to_string()),
            )]
        }
    };
    let diagnostics = match compiled.validate(value) {
        Ok(()) => Vec::new(),
        Err(errors) => errors
            .map(|error| validation_diagnostic(error, path, schema_id, type_name))
            .collect(),
    };
    diagnostics
}

fn compile_schema(schema: &Value) -> Result<JSONSchema, String> {
    JSONSchema::options()
        .with_draft(Draft::Draft202012)
        .should_validate_formats(true)
        .compile(schema)
        .map_err(|error| error.to_string())
}

fn compile_canonical_schema(schema: &Value) -> Result<JSONSchema, String> {
    let diagnostic_schema = serde_json::from_str(DIAGNOSTIC_SCHEMA)
        .map_err(|error| format!("diagnostic schema is not valid JSON: {error}"))?;
    JSONSchema::options()
        .with_draft(Draft::Draft202012)
        .should_validate_formats(true)
        .with_document(DIAGNOSTIC_SCHEMA_ID.to_string(), diagnostic_schema)
        .compile(schema)
        .map_err(|error| error.to_string())
}

pub(crate) fn resolve_schema_ref(
    reference: &str,
    type_file_path: &Path,
    collection_root: &Path,
) -> Result<Value, Box<Diagnostic>> {
    if is_forbidden_reference(reference) {
        return Err(Box::new(Diagnostic::error(
            "schema_ref_forbidden",
            format!("Only local schema refs are supported: {reference}"),
            Some(relative_path(collection_root, type_file_path)),
        )));
    }
    let (path_reference, fragment) = reference
        .split_once('#')
        .map_or((reference, None), |(path, fragment)| (path, Some(fragment)));
    if path_reference.is_empty() {
        return Err(Box::new(Diagnostic::error(
            "schema_ref_unresolved",
            "Type wrapper schema.ref must name a local JSON file.",
            Some(relative_path(collection_root, type_file_path)),
        )));
    }
    let Some(parent) = type_file_path.parent() else {
        return Err(Box::new(Diagnostic::error(
            "schema_ref_unresolved",
            format!("Cannot resolve schema ref: {reference}"),
            Some(relative_path(collection_root, type_file_path)),
        )));
    };
    let candidate = parent.join(path_reference);
    let canonical = candidate.canonicalize().map_err(|error| {
        Box::new(Diagnostic::error(
            "schema_ref_unresolved",
            format!("Cannot resolve schema ref {reference}: {error}"),
            Some(relative_path(collection_root, type_file_path)),
        ))
    })?;
    if !is_allowed_schema_path(&canonical, collection_root) {
        return Err(Box::new(Diagnostic::error(
            "schema_ref_forbidden",
            format!(
                "Schema ref escapes the collection and allowed package schema roots: {reference}"
            ),
            Some(relative_path(collection_root, type_file_path)),
        )));
    }
    let text = fs::read_to_string(&canonical).map_err(|error| {
        Box::new(Diagnostic::error(
            "schema_ref_unresolved",
            format!("Cannot read schema ref {reference}: {error}"),
            Some(relative_path(collection_root, type_file_path)),
        ))
    })?;
    let document: Value = serde_json::from_str(&text).map_err(|error| {
        Box::new(Diagnostic::error(
            "invalid_embedded_schema",
            format!("Referenced schema is not valid JSON: {error}"),
            Some(relative_path(collection_root, type_file_path)),
        ))
    })?;
    let selected = match fragment {
        None | Some("") => &document,
        Some(pointer) if pointer.starts_with('/') => {
            document.pointer(pointer).ok_or_else(|| {
                Box::new(Diagnostic::error(
                    "schema_ref_unresolved",
                    format!("Schema ref points to a missing JSON Pointer target: {reference}"),
                    Some(relative_path(collection_root, type_file_path)),
                ))
            })?
        }
        Some(_) => {
            return Err(Box::new(Diagnostic::error(
                "schema_ref_unresolved",
                format!("Schema ref fragment must be a JSON Pointer: {reference}"),
                Some(relative_path(collection_root, type_file_path)),
            )))
        }
    };
    if !selected.is_object() {
        return Err(Box::new(Diagnostic::error(
            "invalid_embedded_schema",
            format!("Schema ref must resolve to a JSON Schema object: {reference}"),
            Some(relative_path(collection_root, type_file_path)),
        )));
    }
    Ok(selected.clone())
}

pub(crate) fn unsupported_schema_reference(schema: &Value) -> Option<(&'static str, &str)> {
    match schema {
        Value::Array(values) => values.iter().find_map(unsupported_schema_reference),
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                if !reference.starts_with('#') {
                    let code = if is_forbidden_reference(reference) {
                        "schema_ref_forbidden"
                    } else {
                        "unsupported_profile"
                    };
                    return Some((code, reference));
                }
            }
            object.values().find_map(unsupported_schema_reference)
        }
        _ => None,
    }
}

fn is_forbidden_reference(reference: &str) -> bool {
    let has_scheme = reference.split_once(':').is_some_and(|(scheme, _)| {
        !scheme.is_empty()
            && scheme.chars().enumerate().all(|(index, character)| {
                if index == 0 {
                    character.is_ascii_alphabetic()
                } else {
                    character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
                }
            })
    });
    has_scheme || Path::new(reference).is_absolute() || reference.starts_with("//")
}

fn is_allowed_schema_path(candidate: &Path, collection_root: &Path) -> bool {
    if let Ok(root) = collection_root.canonicalize() {
        if candidate.starts_with(root) {
            return true;
        }
    }
    let mut current = Some(collection_root);
    while let Some(directory) = current {
        let schema_root = directory.join("schemas/v0.3");
        if let Ok(schema_root) = schema_root.canonicalize() {
            if candidate.starts_with(schema_root) {
                return true;
            }
        }
        current = directory.parent();
    }
    false
}

fn should_descend(entry: &DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return true;
    }
    !matches!(
        entry.file_name().to_str(),
        Some(".git" | ".mdbase" | "node_modules")
    )
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub fn schema_path(name: &str) -> Option<PathBuf> {
    match name {
        "config" => Some(PathBuf::from("schemas/v0.3/config.schema.json")),
        "data-contract" => Some(PathBuf::from("schemas/v0.3/data-contract.schema.json")),
        "diagnostic" => Some(PathBuf::from("schemas/v0.3/diagnostic.schema.json")),
        "operation-result" => Some(PathBuf::from("schemas/v0.3/operation-result.schema.json")),
        "query" => Some(PathBuf::from("schemas/v0.3/query.schema.json")),
        "query-result" => Some(PathBuf::from("schemas/v0.3/query-result.schema.json")),
        "type-file" => Some(PathBuf::from("schemas/v0.3/type-file.schema.json")),
        "type-pack" => Some(PathBuf::from("schemas/v0.3/type-pack.schema.json")),
        "type-pack-lock" => Some(PathBuf::from("schemas/v0.3/type-pack-lock.schema.json")),
        "view" => Some(PathBuf::from("schemas/v0.3/view.schema.json")),
        _ => None,
    }
}

impl Collection {
    pub fn v03_operations(&self) -> Result<Operations<'_>, Box<Diagnostic>> {
        Operations::new(self)
    }

    pub fn validate_v03_frontmatter(
        &self,
        frontmatter: &Value,
        path: &str,
    ) -> Option<Vec<Diagnostic>> {
        if self.spec_profile != SpecProfile::V03 {
            return None;
        }
        let type_names = self.determine_types_for_path(frontmatter, Some(path));
        let mut diagnostics = Vec::new();
        for type_name in type_names {
            let Some(type_definition) = self.types.get(&type_name) else {
                diagnostics.push(Diagnostic {
                    severity: "error".to_string(),
                    code: "unknown_type".to_string(),
                    message: format!("Unknown type '{type_name}'."),
                    path: Some(path.to_string()),
                    field: None,
                    type_name: Some(type_name),
                    schema_location: None,
                    details: None,
                });
                continue;
            };
            let Some(schema) = &type_definition.json_schema else {
                continue;
            };
            diagnostics.extend(validate_schema_instance(
                schema,
                frontmatter,
                path,
                Some(&type_name),
            ));
        }
        Some(diagnostics)
    }
}
