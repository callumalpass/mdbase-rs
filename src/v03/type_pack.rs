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
            ));
        }
        if planned.len() != sources.len() {
            return pack_error("The type pack contains undeclared source resources.");
        }

        let shadow = match batch::shadow_collection(self) {
            Ok(shadow) => shadow,
            Err(diagnostic) => return failed(vec![*diagnostic]),
        };
        for (target, bytes, action, _) in &planned {
            let path = shadow.directory.path().join(target);
            if *action == "replace" && !replace {
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
            .map(|(target, _, action, digest)| {
                json!({
                    "target": target,
                    "action": action,
                    "digest": digest,
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
}
