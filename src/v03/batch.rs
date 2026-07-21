use std::fs;
use std::path::Path;

use serde_json::{json, Value};
use walkdir::WalkDir;

use super::{Diagnostic, OperationResult, Operations};
use crate::Collection;

pub(crate) fn execute(collection: &Collection, input: &Value) -> OperationResult {
    let Some(items) = input.get("operations").and_then(Value::as_array) else {
        return invalid_request("Batch input requires an operations array.");
    };
    if items.is_empty() {
        return invalid_request("Batch operations must not be empty.");
    }
    let dry_run = input
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let allow_partial = input
        .get("allow_partial")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if dry_run || !allow_partial {
        let shadow = match shadow_collection(collection) {
            Ok(shadow) => shadow,
            Err(diagnostic) => return failed(vec![*diagnostic]),
        };
        let shadow_operations = match shadow.collection.v03_operations() {
            Ok(operations) => operations,
            Err(diagnostic) => return failed(vec![*diagnostic]),
        };
        let preview = execute_items(&shadow_operations, items, false);
        if dry_run || !preview.valid {
            return batch_result(preview, true, dry_run);
        }
    }

    let operations = match collection.v03_operations() {
        Ok(operations) => operations,
        Err(diagnostic) => return failed(vec![*diagnostic]),
    };
    batch_result(
        execute_items(&operations, items, allow_partial),
        false,
        false,
    )
}

struct ShadowCollection {
    _directory: tempfile::TempDir,
    collection: Collection,
}

fn shadow_collection(collection: &Collection) -> Result<ShadowCollection, Box<Diagnostic>> {
    let directory = tempfile::tempdir().map_err(|error| {
        Box::new(Diagnostic::error(
            "batch_preflight_failed",
            format!("Could not create batch preflight workspace: {error}"),
            None,
        ))
    })?;
    copy_collection(
        &collection.root,
        directory.path(),
        &collection.settings.cache_folder,
    )?;
    let shadow = Collection::open(directory.path()).map_err(|error| {
        Box::new(Diagnostic::error(
            "batch_preflight_failed",
            format!("Could not open batch preflight collection: {error:?}"),
            None,
        ))
    })?;
    Ok(ShadowCollection {
        _directory: directory,
        collection: shadow,
    })
}

fn copy_collection(
    source: &Path,
    destination: &Path,
    cache_folder: &str,
) -> Result<(), Box<Diagnostic>> {
    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry.map_err(|error| {
            Box::new(Diagnostic::error(
                "batch_preflight_failed",
                format!("Could not inspect collection for batch preflight: {error}"),
                None,
            ))
        })?;
        let relative = entry.path().strip_prefix(source).map_err(|error| {
            Box::new(Diagnostic::error(
                "batch_preflight_failed",
                error.to_string(),
                None,
            ))
        })?;
        if relative.as_os_str().is_empty() || is_cache_path(relative, cache_folder) {
            continue;
        }
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).map_err(|error| Box::new(copy_error(relative, error)))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| Box::new(copy_error(relative, error)))?;
            }
            fs::copy(entry.path(), &target)
                .map_err(|error| Box::new(copy_error(relative, error)))?;
        }
    }
    Ok(())
}

fn is_cache_path(path: &Path, cache_folder: &str) -> bool {
    path.components()
        .next()
        .is_some_and(|component| component.as_os_str() == cache_folder)
}

fn copy_error(path: &Path, error: std::io::Error) -> Diagnostic {
    Diagnostic::error(
        "batch_preflight_failed",
        format!(
            "Could not copy '{}' for batch preflight: {error}",
            path.to_string_lossy()
        ),
        None,
    )
}

struct BatchExecution {
    valid: bool,
    results: Vec<Value>,
    diagnostics: Vec<Diagnostic>,
}

fn execute_items(
    operations: &Operations<'_>,
    items: &[Value],
    allow_partial: bool,
) -> BatchExecution {
    let mut results = Vec::new();
    let mut diagnostics = Vec::new();
    let mut valid = true;
    for (index, item) in items.iter().enumerate() {
        let Some(kind) = item.get("kind").and_then(Value::as_str) else {
            let diagnostic = Diagnostic::error(
                "invalid_request",
                format!("Batch operation {index} requires kind."),
                None,
            );
            diagnostics.push(diagnostic.clone());
            results.push(json!({
                "index": index,
                "valid": false,
                "result": {},
                "diagnostics": [diagnostic],
            }));
            valid = false;
            if !allow_partial {
                break;
            }
            continue;
        };
        let operation_input = item.get("input").cloned().unwrap_or_else(|| json!({}));
        let operation = match kind {
            "create" => operations.create(&operation_input),
            "update" => operations.update(&operation_input),
            "delete" => operations.delete(&operation_input),
            "rename" => operations.rename(&operation_input),
            _ => OperationResult {
                valid: false,
                result: json!({}),
                diagnostics: vec![Diagnostic::error(
                    "invalid_request",
                    format!("Unsupported batch operation kind '{kind}'."),
                    None,
                )],
            },
        };
        if !operation.valid {
            valid = false;
        }
        diagnostics.extend(operation.diagnostics.iter().cloned());
        results.push(json!({
            "index": index,
            "kind": kind,
            "valid": operation.valid,
            "result": operation.result,
            "diagnostics": operation.diagnostics,
        }));
        if !valid && !allow_partial {
            break;
        }
    }
    BatchExecution {
        valid,
        results,
        diagnostics,
    }
}

fn batch_result(execution: BatchExecution, preflight: bool, dry_run: bool) -> OperationResult {
    let succeeded = execution
        .results
        .iter()
        .filter(|result| result.get("valid") == Some(&Value::Bool(true)))
        .count();
    let failed = execution.results.len() - succeeded;
    OperationResult {
        valid: execution.valid,
        result: json!({
            "operations": execution.results,
            "succeeded": succeeded,
            "failed": failed,
            "preflight": preflight,
            "dry_run": dry_run,
        }),
        diagnostics: execution.diagnostics,
    }
}

fn invalid_request(message: &str) -> OperationResult {
    failed(vec![Diagnostic::error("invalid_request", message, None)])
}

fn failed(diagnostics: Vec<Diagnostic>) -> OperationResult {
    OperationResult {
        valid: false,
        result: json!({}),
        diagnostics,
    }
}
