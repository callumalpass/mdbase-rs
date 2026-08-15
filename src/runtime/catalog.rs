use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::operations::read::RecordFileFacts;
use crate::types::{inheritance, loader};
use crate::v03::{Diagnostic, OperationResult, TypeFile};
use crate::{Collection, Settings, SpecProfile};

use super::record_structure::RecordStructure;
use super::CollectionSnapshotRecord;

/// Provider-neutral inputs for compiling the record semantics needed by
/// point and incremental execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogInput {
    pub resource_revision: String,
    pub configuration_document: String,
    pub types: Vec<ResolvedTypeResource>,
    #[serde(default)]
    pub contracts: Vec<crate::data_contracts::ResolvedRecordContract>,
}

/// One validated type resource with its schema reference already resolved from
/// the same authority snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedTypeResource {
    pub path: String,
    pub revision: String,
    pub definition: Value,
    pub schema: Value,
}

/// Exact input for evaluating one canonical record. The provider may include a
/// stable identity for correlation, but record semantics never interpret it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalRecordInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
    pub path: String,
    pub document: String,
    #[serde(default)]
    pub file_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_mtime: Option<String>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{code}: {message}")]
pub struct CatalogError {
    pub code: String,
    pub message: String,
}

/// Immutable, provider-neutral compiled resource catalog.
///
/// It contains no records, filesystem authority, credentials, or durable host
/// identity. Evicting it changes performance only.
pub struct CompiledCatalog {
    resource_revision: String,
    collection: Collection,
}

impl CompiledCatalog {
    pub fn compile(input: CatalogInput) -> Result<Self, CatalogError> {
        let config = crate::config::load_config_document_for_open(&input.configuration_document);
        if config.get("valid") != Some(&Value::Bool(true)) {
            return Err(catalog_error_from_value(&config, "invalid_config"));
        }
        let config_value = config.get("config").ok_or_else(|| CatalogError {
            code: "invalid_config".to_string(),
            message: "Parsed configuration has no config object.".to_string(),
        })?;
        if config_value.get("spec_profile").and_then(Value::as_str) != Some("v0.3") {
            return Err(CatalogError {
                code: "unsupported_profile".to_string(),
                message: "Incremental record catalogs require the canonical v0.3 profile."
                    .to_string(),
            });
        }
        let settings = settings_from_config(config_value)?;
        let config_extensions = config_value
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, _)| key.starts_with("x-"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Map<_, _>>();

        let mut type_files = Vec::with_capacity(input.types.len());
        for resource in input.types {
            let definition = resource
                .definition
                .as_object()
                .ok_or_else(|| CatalogError {
                    code: "invalid_type_definition".to_string(),
                    message: format!("Type resource '{}' is not an object.", resource.path),
                })?;
            let diagnostics = crate::v03::validate_type_file(&resource.definition, &resource.path);
            if let Some(diagnostic) = diagnostics
                .iter()
                .find(|diagnostic| diagnostic.severity == "error")
            {
                return Err(catalog_error_from_diagnostic(diagnostic));
            }
            let name = definition
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if !resource.schema.is_object() {
                return Err(CatalogError {
                    code: "invalid_embedded_schema".to_string(),
                    message: format!(
                        "Type resource '{}' did not resolve to a JSON Schema object.",
                        resource.path
                    ),
                });
            }
            type_files.push(TypeFile {
                path: resource.path,
                name,
                version: definition.get("version").and_then(Value::as_u64),
                frontmatter: resource.definition,
                schema: resource.schema,
            });
        }
        let loaded =
            loader::load_resolved_type_files(type_files).map_err(|message| CatalogError {
                code: "invalid_type_definition".to_string(),
                message,
            })?;
        let mut types = loaded.types;
        inheritance::resolve_inheritance(&mut types).map_err(|message| CatalogError {
            code: if message.contains("Circular") {
                "circular_inheritance"
            } else if message.contains("Unknown type") || message.contains("extends unknown") {
                "missing_parent_type"
            } else {
                "invalid_type_definition"
            }
            .to_string(),
            message,
        })?;
        let type_plans =
            crate::types::compiled::compile_registry(&types).map_err(|error| CatalogError {
                code: error.code.to_string(),
                message: error.message,
            })?;

        let data_contracts =
            crate::data_contracts::DataContractRegistry::load_resolved(input.contracts, &types)
                .map_err(|error| CatalogError {
                    code: error.code,
                    message: error.message,
                })?;

        Ok(Self {
            resource_revision: input.resource_revision,
            collection: Collection {
                root: PathBuf::new(),
                spec_profile: SpecProfile::V03,
                settings,
                config_extensions,
                types,
                type_plans,
                type_warnings: loaded.warnings,
                data_contracts,
            },
        })
    }

    pub fn resource_revision(&self) -> &str {
        &self.resource_revision
    }

    pub fn read_record(&self, input: &Value, record: &CanonicalRecordInput) -> OperationResult {
        self.collection
            .v03_operations()
            .expect("compiled catalogs always use the canonical profile")
            .read_record(
                input,
                &record.path,
                &record.document,
                &RecordFileFacts {
                    size: record.file_size,
                    mtime: record.file_mtime.clone(),
                },
            )
    }

    pub fn read_record_not_found(&self, input: &Value) -> OperationResult {
        self.collection
            .v03_operations()
            .expect("compiled catalogs always use the canonical profile")
            .read_record_not_found(input)
    }

    /// Parse and classify one exact Markdown document for a provider-owned
    /// record without reading or mutating any collection filesystem.
    ///
    /// This deliberately preserves malformed or non-object frontmatter as an
    /// opaque document in the same way as filesystem snapshots used by exact
    /// synchronization. Typed reads remain strict through [`Self::read_record`].
    pub fn classify_record(
        &self,
        record: &CanonicalRecordInput,
    ) -> Result<CollectionSnapshotRecord, CatalogError> {
        let path = self
            .collection
            .validate_record_path(&record.path)
            .map_err(|error| CatalogError {
                code: "invalid_path".to_string(),
                message: format!("The record path is invalid: {error}"),
            })?;
        Ok(super::snapshot::materialize_snapshot_record(
            &self.collection,
            path.as_str(),
            record.document.clone(),
        ))
    }

    pub fn determine_types_for_record(&self, path: &str, frontmatter: &Value) -> Vec<String> {
        self.collection
            .determine_types_for_path(frontmatter, Some(path))
    }

    pub fn type_warnings(&self) -> &[String] {
        self.collection.type_warnings()
    }

    pub(crate) fn id_field(&self) -> &str {
        &self.collection.settings().id_field
    }

    pub(crate) fn record_extensions(&self) -> &[String] {
        &self.collection.settings().extensions
    }

    /// Extract provider-neutral structural facts from one exact record.
    ///
    /// This operation validates the canonical relative path but does not
    /// enumerate records or resolve occurrences against a catalogue. A host
    /// may perform that second step against its own consistent snapshot.
    pub fn parse_record_structure(
        &self,
        record: &CanonicalRecordInput,
    ) -> Result<RecordStructure, CatalogError> {
        let path = self
            .collection
            .validate_record_path(&record.path)
            .map_err(|error| CatalogError {
                code: "invalid_path".to_string(),
                message: format!("The record path is invalid: {error}"),
            })?;
        Ok(super::record_structure::parse_record_structure(
            path.as_str(),
            &record.document,
        ))
    }

    /// Alias emphasizing that this is a projection rather than a resolver.
    pub fn project_record_structure(
        &self,
        record: &CanonicalRecordInput,
    ) -> Result<RecordStructure, CatalogError> {
        self.parse_record_structure(record)
    }
}

fn settings_from_config(config: &Value) -> Result<Settings, CatalogError> {
    let settings = config
        .get("settings")
        .and_then(Value::as_object)
        .ok_or_else(|| CatalogError {
            code: "invalid_config".to_string(),
            message: "Parsed configuration settings are missing.".to_string(),
        })?;
    let strings = |key: &str, fallback: Vec<String>| {
        settings
            .get(key)
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or(fallback)
    };
    let string = |key: &str, fallback: &str| {
        settings
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or(fallback)
            .to_string()
    };
    let mut parsed = Settings {
        extensions: strings("extensions", Vec::new()),
        exclude: strings(
            "exclude",
            vec![".git".into(), "node_modules".into(), ".mdbase".into()],
        ),
        include_subfolders: settings
            .get("include_subfolders")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        types_folder: string("types_folder", "_types"),
        contracts_folder: string("contracts_folder", "_contracts"),
        migrations_folder: string("migrations_folder", "_types/_migrations"),
        explicit_type_keys: strings("explicit_type_keys", vec!["type".into(), "types".into()]),
        write_defaults: false,
        default_validation: string("default_validation", "warn"),
        default_strict: settings
            .get("default_strict")
            .cloned()
            .unwrap_or(Value::Bool(false)),
        timezone: settings
            .get("timezone")
            .and_then(Value::as_str)
            .map(str::to_string),
        id_field: string("id_field", "id"),
        id_field_explicit: settings
            .get("id_field_explicit")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        write_nulls: "explicit".to_string(),
        write_empty_lists: settings
            .get("write_empty_lists")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        rename_update_refs: settings
            .get("rename_update_refs")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        cache_folder: string("cache_folder", ".mdbase"),
    };
    for (label, folder, allow_hidden) in [
        ("types_folder", &mut parsed.types_folder, false),
        ("contracts_folder", &mut parsed.contracts_folder, false),
        ("migrations_folder", &mut parsed.migrations_folder, false),
        ("cache_folder", &mut parsed.cache_folder, true),
    ] {
        let normalized =
            crate::api::CollectionPath::new(folder.as_str()).map_err(|error| CatalogError {
                code: "invalid_config".to_string(),
                message: format!("settings.{label} is not a portable collection path: {error}"),
            })?;
        if !allow_hidden
            && normalized
                .as_str()
                .split('/')
                .any(|component| component.starts_with('.'))
        {
            return Err(CatalogError {
                code: "invalid_config".to_string(),
                message: format!("settings.{label} must not use a hidden filesystem namespace"),
            });
        }
        *folder = normalized.to_string();
    }
    Ok(parsed)
}

fn catalog_error_from_value(value: &Value, fallback_code: &str) -> CatalogError {
    let error = value.get("error").unwrap_or(value);
    CatalogError {
        code: error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or(fallback_code)
            .to_string(),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Catalog compilation failed.")
            .to_string(),
    }
}

fn catalog_error_from_diagnostic(diagnostic: &Diagnostic) -> CatalogError {
    CatalogError {
        code: diagnostic.code.clone(),
        message: diagnostic.message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog_input() -> CatalogInput {
        CatalogInput {
            resource_revision: "resources-1".to_string(),
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
                    "schema": {"dialect": "json-schema-2020-12", "value": {
                        "type": "object",
                        "properties": {
                            "status": {"type": "string", "default": "open"},
                            "score": {"type": "integer"}
                        }
                    }},
                    "collection": {"read_defaults": {"status": "open"}}
                }),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "status": {"type": "string", "default": "open"},
                        "score": {"type": "integer"}
                    }
                }),
            }],
            contracts: Vec::new(),
        }
    }

    fn catalog() -> CompiledCatalog {
        CompiledCatalog::compile(catalog_input()).unwrap()
    }

    #[test]
    fn evaluates_one_exact_record_without_a_collection_root() {
        let catalog = catalog();
        let result = catalog.read_record(
            &json!({"path": "tasks/one.md", "include_document": true}),
            &CanonicalRecordInput {
                stable_id: Some("record-1".to_string()),
                path: "tasks/one.md".to_string(),
                document: "---\nscore: 3\n---\nBody\n".to_string(),
                file_size: 24,
                file_mtime: None,
            },
        );
        assert!(result.valid, "{result:?}");
        assert_eq!(result.result["types"], json!(["task"]));
        assert_eq!(result.result["effective_frontmatter"]["status"], "open");
        assert_eq!(result.result["body"], "Body\n");
        assert_eq!(result.result["file"]["size"], 24);
        assert_eq!(result.result["document"], "---\nscore: 3\n---\nBody\n");
    }

    #[test]
    fn rejects_a_record_for_another_requested_path() {
        let result = catalog().read_record(
            &json!({"path": "tasks/two.md"}),
            &CanonicalRecordInput {
                stable_id: None,
                path: "tasks/one.md".to_string(),
                document: "Body\n".to_string(),
                file_size: 5,
                file_mtime: None,
            },
        );
        assert!(!result.valid);
        assert_eq!(result.diagnostics[0].code, "record_identity_mismatch");
    }

    #[test]
    fn classifies_exact_records_without_a_collection_root() {
        let classified = catalog()
            .classify_record(&CanonicalRecordInput {
                stable_id: Some("record-1".to_string()),
                path: "tasks/one.md".to_string(),
                document: "---\nscore: 3\n---\nBody\n".to_string(),
                file_size: 24,
                file_mtime: None,
            })
            .unwrap();
        assert_eq!(classified.path, "tasks/one.md");
        assert_eq!(
            classified.revision,
            crate::v03::revision(classified.document.as_bytes())
        );
        assert_eq!(classified.frontmatter["score"], 3);
        assert_eq!(classified.body, "Body\n");
        assert_eq!(classified.types, ["task"]);
        assert_eq!(classified.frontmatter_error, None);
    }

    #[test]
    fn exact_classification_matches_filesystem_snapshot_including_opaque_markdown() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("_types")).unwrap();
        std::fs::create_dir_all(root.path().join("tasks")).unwrap();
        std::fs::write(
            root.path().join("mdbase.yaml"),
            &catalog_input().configuration_document,
        )
        .unwrap();
        std::fs::write(
            root.path().join("_types/task.md"),
            "---\nkind: mdbase.type\nname: task\nversion: 1\nmatch:\n  path_glob: tasks/*.md\nschema:\n  dialect: json-schema-2020-12\n  value: {type: object}\n---\n",
        )
        .unwrap();
        let document = "---\ntitle: [unterminated\n---\nOpaque body";
        std::fs::write(root.path().join("tasks/opaque.md"), document).unwrap();
        let filesystem = Collection::open(root.path())
            .unwrap()
            .snapshot_record("tasks/opaque.md")
            .unwrap();
        let provider = catalog()
            .classify_record(&CanonicalRecordInput {
                stable_id: None,
                path: "tasks/opaque.md".to_string(),
                document: document.to_string(),
                file_size: document.len() as u64,
                file_mtime: None,
            })
            .unwrap();
        assert_eq!(provider, filesystem);
        assert_eq!(
            provider.frontmatter_error.as_deref(),
            Some("Failed to parse YAML frontmatter")
        );
        assert_eq!(provider.body, document);
    }

    #[test]
    fn exact_classification_rejects_non_record_paths() {
        for path in ["../escape.md", "_types/task.md", "payload.bin"] {
            let error = catalog()
                .classify_record(&CanonicalRecordInput {
                    stable_id: None,
                    path: path.to_string(),
                    document: "Body\n".to_string(),
                    file_size: 5,
                    file_mtime: None,
                })
                .unwrap_err();
            assert_eq!(error.code, "invalid_path");
        }
    }

    #[test]
    fn filesystem_and_provider_inputs_share_the_same_read_semantics() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("_types")).unwrap();
        std::fs::create_dir_all(root.path().join("tasks")).unwrap();
        std::fs::write(
            root.path().join("mdbase.yaml"),
            &catalog_input().configuration_document,
        )
        .unwrap();
        std::fs::write(
            root.path().join("_types/task.md"),
            r#"---
kind: mdbase.type
name: task
version: 1
match:
  path_glob: tasks/*.md
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    properties:
      status: { type: string, default: open }
      score: { type: integer }
collection:
  read_defaults:
    status: open
---
"#,
        )
        .unwrap();
        let document = "---\nscore: 3\n---\nBody\n";
        let record_path = root.path().join("tasks/one.md");
        std::fs::write(&record_path, document).unwrap();
        let metadata = std::fs::metadata(&record_path).unwrap();
        let mtime = metadata.modified().ok().map(|time| {
            let datetime: chrono::DateTime<chrono::Utc> = time.into();
            datetime.format("%Y-%m-%dT%H:%M:%SZ").to_string()
        });
        let input = json!({"path": "tasks/one.md", "include_document": true});
        let collection = Collection::open(root.path()).unwrap();
        let filesystem = collection.v03_operations().unwrap().read(&input);
        let provider = catalog().read_record(
            &input,
            &CanonicalRecordInput {
                stable_id: Some("record-1".to_string()),
                path: "tasks/one.md".to_string(),
                document: document.to_string(),
                file_size: metadata.len(),
                file_mtime: mtime,
            },
        );
        assert_eq!(provider, filesystem);
    }
}
