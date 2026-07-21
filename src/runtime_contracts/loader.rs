use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use walkdir::WalkDir;

use super::model::{
    ContractDocument, LoadOptions, PolicySelector, RuntimeDiagnostic, RuntimePackage,
};
use super::schemas::CanonicalValidators;
use crate::frontmatter::parser::{is_parse_error, parse_document, yaml_to_json};
use crate::Collection;

pub(crate) fn load_collection(
    validators: &CanonicalValidators,
    collection: &Collection,
) -> RuntimePackage {
    let mut package = RuntimePackage::default();
    load_type_files(collection, &mut package);

    let mut files = collection.scan_collection_files();
    files.sort();
    for absolute_path in files {
        let path = relative_path(&collection.root, &absolute_path);
        let Some(document) = read_document(&absolute_path, &path, &mut package.diagnostics) else {
            continue;
        };
        if document.kind().is_none() {
            continue;
        }
        package
            .diagnostics
            .extend(validators.validate_contract(&document).diagnostics);
        package.push(document);
    }
    package
}

pub(crate) fn selected_policies(
    collection: &Collection,
    options: &LoadOptions,
) -> (Vec<PolicySelector>, Vec<RuntimeDiagnostic>) {
    if !options.selected_policies.is_empty() {
        return (options.selected_policies.clone(), Vec::new());
    }
    let config = crate::config::load_config(&collection.root);
    let mut diagnostics = Vec::new();
    let profile = config.pointer("/config/runtime/profile_version");
    if let Some(profile) = profile.and_then(Value::as_str) {
        if profile != super::model::RUNTIME_PROFILE_VERSION {
            diagnostics.push(
                RuntimeDiagnostic::error(
                    "runtime_profile_version_mismatch",
                    format!(
                        "Runtime profile {profile} is not supported; expected {}.",
                        super::model::RUNTIME_PROFILE_VERSION
                    ),
                )
                .at_path("mdbase.yaml"),
            );
        }
    }
    let selected = config
        .pointer("/config/runtime/policy")
        .and_then(Value::as_str)
        .map(|path| vec![PolicySelector::Path(normalize_path(path))])
        .unwrap_or_default();
    (selected, diagnostics)
}

fn load_type_files(collection: &Collection, package: &mut RuntimePackage) {
    let root = collection.root.join(&collection.settings.types_folder);
    if !root.exists() {
        return;
    }
    for entry in WalkDir::new(&root).sort_by_file_name() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                package.diagnostics.push(RuntimeDiagnostic::error(
                    "invalid_type_definition",
                    error.to_string(),
                ));
                continue;
            }
        };
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("md")
        {
            continue;
        }
        let path = relative_path(&collection.root, entry.path());
        let Some(document) = read_document(entry.path(), &path, &mut package.diagnostics) else {
            continue;
        };
        package.diagnostics.extend(
            crate::v03::validate_type_file(&document.frontmatter, &path)
                .into_iter()
                .map(|diagnostic| RuntimeDiagnostic {
                    severity: diagnostic.severity,
                    code: diagnostic.code,
                    message: diagnostic.message,
                    path: diagnostic.path,
                    id: None,
                    field: diagnostic.field,
                    details: diagnostic.details,
                }),
        );
        package.type_files.push(document);
    }
}

fn read_document(
    absolute_path: &Path,
    path: &str,
    diagnostics: &mut Vec<RuntimeDiagnostic>,
) -> Option<ContractDocument> {
    let content = match fs::read_to_string(absolute_path) {
        Ok(content) => content,
        Err(error) => {
            diagnostics.push(
                RuntimeDiagnostic::error("invalid_frontmatter", error.to_string()).at_path(path),
            );
            return None;
        }
    };
    let parsed = parse_document(&content);
    let frontmatter = match parsed.frontmatter {
        None | Some(serde_yaml::Value::Null) => serde_json::json!({}),
        Some(value) if is_parse_error(&value) => {
            diagnostics.push(
                RuntimeDiagnostic::error(
                    "invalid_frontmatter",
                    "Failed to parse YAML frontmatter.",
                )
                .at_path(path),
            );
            return None;
        }
        Some(value @ serde_yaml::Value::Mapping(_)) => yaml_to_json(&value),
        Some(_) => {
            diagnostics.push(
                RuntimeDiagnostic::error(
                    "invalid_frontmatter",
                    "Frontmatter must parse to an object.",
                )
                .at_path(path),
            );
            return None;
        }
    };
    Some(ContractDocument {
        path: path.to_string(),
        frontmatter,
        body: parsed.body,
    })
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn normalize_path(path: &str) -> String {
    PathBuf::from(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
        .trim_start_matches("./")
        .to_string()
}
