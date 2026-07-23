//! Revision-safe access to complete saved-view source documents.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use super::execute::is_configured_obsidian_source;
use super::model::ObsidianBaseDocument;
use crate::frontmatter::parser::{is_parse_error, parse_document, yaml_mapping_to_json};
use crate::operations::{
    atomic_create, atomic_write, ensure_no_symlink_components, ensure_revision,
    ensure_safe_relative_path,
};
use crate::v03::{self, Diagnostic, OperationResult};
use crate::Collection;

pub(super) fn read(collection: &Collection, input: &Value) -> OperationResult {
    let path = match required_path(input) {
        Ok(path) => path,
        Err(diagnostic) => return failed(diagnostic),
    };
    if let Err(diagnostic) = validate_existing_path(collection, path) {
        return failed(diagnostic);
    }
    source_result(collection, path)
}

pub(super) fn create(collection: &Collection, input: &Value) -> OperationResult {
    let document = match required_document(input) {
        Ok(document) => document,
        Err(diagnostic) => return failed(diagnostic),
    };
    let path = match create_path(collection, input, document) {
        Ok(path) => path,
        Err(diagnostic) => return failed(diagnostic),
    };
    if let Err(diagnostics) = validate_document(collection, &path, document, false) {
        return failed_many(diagnostics);
    }
    let full_path = collection.root.join(&path);
    match atomic_create(&full_path, document.as_bytes()) {
        Ok(()) => source_result(collection, &path),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            failed(Diagnostic::error(
                "path_conflict",
                format!("A saved-view source already exists at '{path}'."),
                Some(path),
            ))
        }
        Err(error) => failed(io_diagnostic(&path, error)),
    }
}

pub(super) fn update(collection: &Collection, input: &Value) -> OperationResult {
    let path = match required_path(input) {
        Ok(path) => path,
        Err(diagnostic) => return failed(diagnostic),
    };
    let document = match required_document(input) {
        Ok(document) => document,
        Err(diagnostic) => return failed(diagnostic),
    };
    if let Err(diagnostic) = validate_existing_path(collection, path) {
        return failed(diagnostic);
    }
    if let Err(diagnostics) = validate_document(collection, path, document, true) {
        return failed_many(diagnostics);
    }
    let full_path = collection.root.join(path);
    if let Err(error) = ensure_revision(
        &full_path,
        path,
        input.get("if_revision").and_then(Value::as_str),
    ) {
        return legacy_failure(error, path);
    }
    match atomic_write(&full_path, document.as_bytes()) {
        Ok(()) => source_result(collection, path),
        Err(error) => failed(io_diagnostic(path, error)),
    }
}

pub(super) fn delete(collection: &Collection, input: &Value) -> OperationResult {
    let path = match required_path(input) {
        Ok(path) => path,
        Err(diagnostic) => return failed(diagnostic),
    };
    if let Err(diagnostic) = validate_existing_path(collection, path) {
        return failed(diagnostic);
    }
    let full_path = collection.root.join(path);
    if let Err(error) = ensure_revision(
        &full_path,
        path,
        input.get("if_revision").and_then(Value::as_str),
    ) {
        return legacy_failure(error, path);
    }
    match fs::remove_file(&full_path) {
        Ok(()) => OperationResult {
            valid: true,
            result: json!({ "path": path, "deleted": true }),
            diagnostics: Vec::new(),
        },
        Err(error) => failed(io_diagnostic(path, error)),
    }
}

fn create_path(
    collection: &Collection,
    input: &Value,
    document: &str,
) -> Result<String, Diagnostic> {
    if let Some(path) = input.get("path").and_then(Value::as_str) {
        validate_safe_path(collection, path)?;
        return Ok(path.replace('\\', "/"));
    }
    let format = input
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or_else(|| default_format(collection));
    let inferred_name = first_view_name(format, document);
    let name = input
        .get("name")
        .and_then(Value::as_str)
        .or(inferred_name.as_deref())
        .unwrap_or("view");
    let stem = slug(name);
    let (folder, extension) = match format {
        "obsidian.base" => (obsidian_create_folder(collection), "base"),
        "mdbase.view" => ("views".to_string(), "md"),
        _ => {
            return Err(Diagnostic::error(
                "unsupported_view_format",
                format!("Saved-view source format '{format}' is not supported."),
                None,
            ))
        }
    };
    let path = format!("{}/{stem}.{extension}", folder.trim_matches('/'));
    validate_safe_path(collection, &path)?;
    Ok(path)
}

fn default_format(collection: &Collection) -> &str {
    if collection
        .config_extensions
        .get("x-obsidian")
        .and_then(|value| value.get("bases"))
        .and_then(|value| value.get("default_for_new_views"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        "obsidian.base"
    } else {
        "mdbase.view"
    }
}

fn obsidian_create_folder(collection: &Collection) -> String {
    collection
        .config_extensions
        .get("x-obsidian")
        .and_then(|value| value.get("bases"))
        .and_then(|value| value.get("create_folder"))
        .and_then(Value::as_str)
        .unwrap_or("TaskNotes/Views")
        .to_string()
}

fn first_view_name(format: &str, document: &str) -> Option<String> {
    if format != "obsidian.base" {
        return None;
    }
    serde_yaml::from_str::<Value>(document)
        .ok()?
        .get("views")?
        .as_array()?
        .first()?
        .get("name")?
        .as_str()
        .map(str::to_string)
}

fn slug(value: &str) -> String {
    let slug = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "view".to_string()
    } else {
        slug
    }
}

fn validate_existing_path(collection: &Collection, path: &str) -> Result<(), Diagnostic> {
    validate_safe_path(collection, path)?;
    if !collection.root.join(path).is_file() {
        return Err(Diagnostic::error(
            "view_not_found",
            format!("Saved-view source '{path}' does not exist."),
            Some(path.to_string()),
        ));
    }
    validate_document(
        collection,
        path,
        &fs::read_to_string(collection.root.join(path))
            .map_err(|error| io_diagnostic(path, error))?,
        true,
    )
    .map_err(|diagnostics| {
        diagnostics.into_iter().next().unwrap_or_else(|| {
            Diagnostic::error(
                "invalid_view",
                "Saved-view source is invalid.",
                Some(path.into()),
            )
        })
    })
}

fn validate_safe_path(collection: &Collection, path: &str) -> Result<(), Diagnostic> {
    ensure_safe_relative_path(path, collection.spec_profile)
        .map_err(|error| legacy_diagnostic(error, path))?;
    ensure_no_symlink_components(&collection.root, path, collection.spec_profile)
        .map_err(|error| legacy_diagnostic(error, path))?;
    match Path::new(path).extension().and_then(|value| value.to_str()) {
        Some("base") if is_configured_obsidian_source(collection, path) => Ok(()),
        Some("base") => Err(Diagnostic::error(
            "invalid_view_path",
            "Obsidian Base sources must match x-obsidian.bases.include.",
            Some(path.to_string()),
        )),
        Some("md") => Ok(()),
        _ => Err(Diagnostic::error(
            "invalid_view_path",
            "Saved-view sources must use a configured .base path or a .md path.",
            Some(path.to_string()),
        )),
    }
}

fn validate_document(
    collection: &Collection,
    path: &str,
    document: &str,
    require_existing_kind: bool,
) -> Result<(), Vec<Diagnostic>> {
    match Path::new(path).extension().and_then(|value| value.to_str()) {
        Some("base") => serde_yaml::from_str::<ObsidianBaseDocument>(document)
            .map(|_| ())
            .map_err(|error| {
                vec![Diagnostic::error(
                    "invalid_view",
                    format!("Could not parse Obsidian Base: {error}"),
                    Some(path.to_string()),
                )]
            }),
        Some("md") => {
            if require_existing_kind {
                let current = fs::read_to_string(collection.root.join(path))
                    .map_err(|error| vec![io_diagnostic(path, error)])?;
                validate_canonical_document(path, &current)?;
            }
            validate_canonical_document(path, document)
        }
        _ => unreachable!("path validation rejects other source formats"),
    }
}

fn validate_canonical_document(path: &str, document: &str) -> Result<(), Vec<Diagnostic>> {
    let parsed = parse_document(document);
    let frontmatter = match parsed.frontmatter {
        Some(serde_yaml::Value::Mapping(mapping)) => yaml_mapping_to_json(&mapping),
        Some(value) if is_parse_error(&value) => {
            return Err(vec![Diagnostic::error(
                "invalid_view",
                "Saved-view frontmatter is not valid YAML.",
                Some(path.to_string()),
            )])
        }
        _ => {
            return Err(vec![Diagnostic::error(
                "invalid_view",
                "Canonical saved-view sources require mapping frontmatter.",
                Some(path.to_string()),
            )])
        }
    };
    let diagnostics = v03::validate_view(&frontmatter, path);
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == "error")
    {
        Err(diagnostics)
    } else {
        Ok(())
    }
}

fn source_result(collection: &Collection, path: &str) -> OperationResult {
    let bytes = match fs::read(collection.root.join(path)) {
        Ok(bytes) => bytes,
        Err(error) => return failed(io_diagnostic(path, error)),
    };
    let document = match String::from_utf8(bytes.clone()) {
        Ok(document) => document,
        Err(_) => {
            return failed(Diagnostic::error(
                "invalid_view",
                "Saved-view source is not valid UTF-8.",
                Some(path.to_string()),
            ))
        }
    };
    OperationResult {
        valid: true,
        result: json!({
            "path": path,
            "format": if path.ends_with(".base") { "obsidian.base" } else { "mdbase.view" },
            "revision": v03::revision(&bytes),
            "document": document,
        }),
        diagnostics: Vec::new(),
    }
}

fn required_path(input: &Value) -> Result<&str, Diagnostic> {
    input
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| Diagnostic::error("invalid_request", "path must be a string.", None))
}

fn required_document(input: &Value) -> Result<&str, Diagnostic> {
    input
        .get("document")
        .and_then(Value::as_str)
        .ok_or_else(|| Diagnostic::error("invalid_request", "document must be a string.", None))
}

fn io_diagnostic(path: &str, error: std::io::Error) -> Diagnostic {
    Diagnostic::error(
        "view_write_failed",
        format!("Saved-view source could not be read or written: {error}"),
        Some(path.to_string()),
    )
}

fn legacy_diagnostic(error: Value, path: &str) -> Diagnostic {
    Diagnostic::error(
        error
            .pointer("/error/code")
            .and_then(Value::as_str)
            .unwrap_or("invalid_view_path"),
        error
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("Saved-view source path is invalid."),
        Some(path.to_string()),
    )
}

fn legacy_failure(error: Value, path: &str) -> OperationResult {
    failed(legacy_diagnostic(error, path))
}

fn failed(diagnostic: Diagnostic) -> OperationResult {
    failed_many(vec![diagnostic])
}

fn failed_many(diagnostics: Vec<Diagnostic>) -> OperationResult {
    OperationResult {
        valid: false,
        result: json!({}),
        diagnostics,
    }
}
