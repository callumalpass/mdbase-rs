use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::{json, Value};
use walkdir::WalkDir;

use super::{Diagnostic, OperationResult, Operations};
use crate::runtime::{CollectionSnapshot, OperationContext, ProviderError};
use crate::Collection;

pub(crate) enum RuntimeSinglePreparation {
    NoMutation(OperationResult),
    Prepared(Box<RuntimeMutationPlan>),
}

pub(crate) struct RuntimeMutationPlan {
    pub(crate) result: OperationResult,
    pub(crate) baseline: crate::transactions::FileBaseline,
    pub(crate) desired: crate::transactions::FileBaseline,
    pub(crate) before: CollectionSnapshot,
    pub(crate) after: CollectionSnapshot,
}

pub(crate) fn prepare_single_runtime(
    collection: &Collection,
    operation: &str,
    input: &Value,
    context: &OperationContext,
) -> Result<RuntimeSinglePreparation, ProviderError> {
    context.check()?;
    if matches!(operation, "create" | "update" | "delete") {
        return prepare_sparse_runtime(collection, operation, input, context);
    }

    let before = collection.snapshot()?;
    context.check()?;
    let shadow = shadow_collection_context(collection, context)?;
    let shadow_input = match adapt_mtime_precondition(collection, &shadow.collection, input) {
        Ok(input) => input,
        Err(diagnostic) => {
            return Ok(RuntimeSinglePreparation::NoMutation(failed(vec![
                *diagnostic,
            ])))
        }
    };
    context.check()?;
    let shadow_operations = shadow
        .collection
        .v03_operations()
        .map_err(|diagnostic| ProviderError::CollectionOpen(diagnostic.message.clone()))?;
    let result = execute_staged_operation(&shadow_operations, operation, &shadow_input);
    context.check()?;
    if !result.valid {
        return Ok(RuntimeSinglePreparation::NoMutation(result));
    }
    let desired = collect_collection_files_context(&shadow.collection, context)?;
    if desired == shadow.baseline {
        return Ok(RuntimeSinglePreparation::NoMutation(result));
    }
    let after = shadow.collection.snapshot()?;
    context.check()?;
    Ok(RuntimeSinglePreparation::Prepared(Box::new(
        RuntimeMutationPlan {
            result,
            baseline: shadow.baseline,
            desired,
            before,
            after,
        },
    )))
}

fn execute_staged_operation(
    operations: &Operations<'_>,
    operation: &str,
    input: &Value,
) -> OperationResult {
    match operation {
        "create" | "update" | "delete" | "rename" => {
            operations.execute_mutation_direct(operation, input)
        }
        "create_view_source" => operations.create_view_source(input),
        "update_view_source" => operations.update_view_source(input),
        "delete_view_source" => operations.delete_view_source(input),
        _ => failed(vec![Diagnostic::error(
            "invalid_request",
            format!("Unsupported mutation operation '{operation}'."),
            None,
        )]),
    }
}

fn prepare_sparse_runtime(
    collection: &Collection,
    operation: &str,
    input: &Value,
    context: &OperationContext,
) -> Result<RuntimeSinglePreparation, ProviderError> {
    let input_path = input.get("path").and_then(Value::as_str);
    let shadow = sparse_shadow_collection(collection, input_path, context)?;
    let shadow_input = match adapt_mtime_precondition(collection, &shadow.collection, input) {
        Ok(input) => input,
        Err(diagnostic) => {
            return Ok(RuntimeSinglePreparation::NoMutation(failed(vec![
                *diagnostic,
            ])))
        }
    };
    context.check()?;
    let shadow_operations = shadow
        .collection
        .v03_operations()
        .map_err(|diagnostic| ProviderError::CollectionOpen(diagnostic.message.clone()))?;
    let mut result = shadow_operations.execute_mutation_direct(operation, &shadow_input);
    context.check()?;
    if !result.valid {
        return Ok(RuntimeSinglePreparation::NoMutation(result));
    }
    if input
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(RuntimeSinglePreparation::NoMutation(result));
    }

    let path = result
        .result
        .get("path")
        .or_else(|| input.get("path"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ProviderError::CollectionOpen(
                "sparse mutation result did not identify its record path".to_string(),
            )
        })?
        .to_string();
    let mut baseline = crate::transactions::FileBaseline::new();
    if let Ok(bytes) = fs::read(collection.root.join(&path)) {
        baseline.insert(path.clone(), bytes);
    }
    if operation == "create" && baseline.contains_key(&path) && input_path != Some(path.as_str()) {
        return Ok(RuntimeSinglePreparation::NoMutation(failed(vec![
            Diagnostic::error(
                "path_conflict",
                format!("File already exists: {path}"),
                Some(path),
            ),
        ])));
    }
    let mut desired = crate::transactions::FileBaseline::new();
    if let Ok(bytes) = fs::read(shadow.collection.root.join(&path)) {
        desired.insert(path.clone(), bytes);
    }
    if baseline == desired {
        return Ok(RuntimeSinglePreparation::NoMutation(result));
    }

    if operation == "delete"
        && input
            .get("check_backlinks")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        context.check()?;
        let mut preview_input = input.as_object().cloned().unwrap_or_default();
        preview_input.insert("dry_run".to_string(), Value::Bool(true));
        let preview = collection
            .v03_operations()
            .map_err(|diagnostic| ProviderError::CollectionOpen(diagnostic.message.clone()))?
            .execute_mutation_direct("delete", &Value::Object(preview_input));
        context.check()?;
        if !preview.valid {
            return Ok(RuntimeSinglePreparation::NoMutation(preview));
        }
        if let Some(broken_links) = preview.result.get("broken_links") {
            result.result["broken_links"] = broken_links.clone();
        }
    }

    if matches!(operation, "create" | "update") && collection.settings.default_validation == "error"
    {
        let frontmatter = result
            .result
            .get("frontmatter")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let type_names = collection.determine_types_for_path(&frontmatter, Some(&path));
        let issues = collection
            .check_uniqueness_indexed(&frontmatter, &type_names, &path)
            .map_err(|error| ProviderError::Transaction {
                code: "cache_maintenance_failed",
                message: error.to_string(),
            })?;
        context.check()?;
        if !issues.is_empty() {
            let diagnostics = issues
                .into_iter()
                .map(|issue| {
                    let mut diagnostic = Diagnostic::error(&issue.code, issue.message, issue.path);
                    diagnostic.field = issue.field;
                    diagnostic
                })
                .collect();
            return Ok(RuntimeSinglePreparation::NoMutation(failed(diagnostics)));
        }
    }

    let before = targeted_snapshot(collection, &path)?;
    let after = targeted_snapshot(&shadow.collection, &path)?;
    context.check()?;
    Ok(RuntimeSinglePreparation::Prepared(Box::new(
        RuntimeMutationPlan {
            result,
            baseline,
            desired,
            before,
            after,
        },
    )))
}

fn targeted_snapshot(
    collection: &Collection,
    path: &str,
) -> Result<CollectionSnapshot, ProviderError> {
    let records = match collection.snapshot_record(path) {
        Ok(record) => vec![record],
        Err(ProviderError::CollectionOpen(_)) => Vec::new(),
        Err(error) => return Err(error),
    };
    Ok(CollectionSnapshot {
        revision: String::new(),
        resource_revision: String::new(),
        spec_version: super::SPEC_VERSION.to_string(),
        resources: Vec::new(),
        records,
    })
}

fn sparse_shadow_collection(
    collection: &Collection,
    target: Option<&str>,
    context: &OperationContext,
) -> Result<ShadowCollection, ProviderError> {
    context.check()?;
    let directory =
        tempfile::tempdir().map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
    copy_sparse_controls(collection, directory.path(), context)?;
    let mut baseline = crate::transactions::FileBaseline::new();
    if let Some(target) = target {
        let source = collection.root.join(target);
        if source.is_file() {
            let bytes = fs::read(&source)
                .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
            let destination = directory.path().join(target);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
            }
            fs::write(&destination, &bytes)
                .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
            baseline.insert(target.replace('\\', "/"), bytes);
        }
    }
    context.check()?;
    let shadow = Collection::open(directory.path())
        .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
    Ok(ShadowCollection {
        directory,
        collection: shadow,
        baseline,
    })
}

fn copy_sparse_controls(
    collection: &Collection,
    destination: &Path,
    context: &OperationContext,
) -> Result<(), ProviderError> {
    for relative in ["mdbase.yaml", "mdbase.lock.yaml"] {
        let source = collection.root.join(relative);
        if source.is_file() {
            fs::copy(&source, destination.join(relative))
                .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
        }
    }
    for folder in [
        collection.settings.types_folder.as_str(),
        collection.settings.contracts_folder.as_str(),
        "_schemas",
    ] {
        let source = collection.root.join(folder);
        if !source.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&source).follow_links(false) {
            context.check()?;
            let entry = entry.map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
            let relative = entry
                .path()
                .strip_prefix(&collection.root)
                .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
            let target = destination.join(relative);
            if entry.file_type().is_dir() {
                fs::create_dir_all(&target)
                    .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
            } else if entry.file_type().is_file() {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
                }
                fs::copy(entry.path(), &target)
                    .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
            }
        }
    }
    Ok(())
}

pub(crate) fn execute_single(
    collection: &Collection,
    operation: &str,
    input: &Value,
) -> OperationResult {
    if input
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let operations = match collection.v03_operations() {
            Ok(operations) => operations,
            Err(diagnostic) => return failed(vec![*diagnostic]),
        };
        return operations.execute_mutation_direct(operation, input);
    }

    let shadow = match shadow_collection(collection) {
        Ok(shadow) => shadow,
        Err(diagnostic) => return failed(vec![*diagnostic]),
    };
    let shadow_input = match adapt_mtime_precondition(collection, &shadow.collection, input) {
        Ok(input) => input,
        Err(diagnostic) => return failed(vec![*diagnostic]),
    };
    let shadow_operations = match shadow.collection.v03_operations() {
        Ok(operations) => operations,
        Err(diagnostic) => return failed(vec![*diagnostic]),
    };
    let mut result = shadow_operations.execute_mutation_direct(operation, &shadow_input);
    if !result.valid {
        return result;
    }
    let desired = match collect_collection_files(&shadow.collection) {
        Ok(desired) => desired,
        Err(diagnostic) => return failed(vec![*diagnostic]),
    };
    let commit = match crate::transactions::commit_shadow(collection, &shadow.baseline, &desired) {
        Ok(commit) => commit,
        Err(error) => {
            return failed(vec![Diagnostic::error(
                error.code(),
                error.to_string(),
                None,
            )])
        }
    };
    if commit.cleanup_deferred {
        result.diagnostics.push(Diagnostic {
            severity: "warning".to_string(),
            code: "transaction_cleanup_deferred".to_string(),
            message: "The mutation committed, but transaction cleanup was deferred.".to_string(),
            path: None,
            field: None,
            type_name: None,
            schema_location: None,
            details: None,
        });
    }
    result
}

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
        let preview = execute_items(&shadow_operations, items, false, false);
        if dry_run || !preview.valid {
            return batch_result(preview, true, dry_run);
        }
        let desired = match collect_collection_files(&shadow.collection) {
            Ok(desired) => desired,
            Err(diagnostic) => return failed(vec![*diagnostic]),
        };
        let commit =
            match crate::transactions::commit_shadow(collection, &shadow.baseline, &desired) {
                Ok(commit) => commit,
                Err(error) => {
                    return failed(vec![Diagnostic::error(
                        error.code(),
                        error.to_string(),
                        None,
                    )])
                }
            };
        let mut result = batch_result(preview, false, false);
        if commit.cleanup_deferred {
            result.diagnostics.push(Diagnostic {
                severity: "warning".to_string(),
                code: "transaction_cleanup_deferred".to_string(),
                message: "The batch committed, but transaction cleanup was deferred.".to_string(),
                path: None,
                field: None,
                type_name: None,
                schema_location: None,
                details: None,
            });
        }
        return result;
    }

    let operations = match collection.v03_operations() {
        Ok(operations) => operations,
        Err(diagnostic) => return failed(vec![*diagnostic]),
    };
    batch_result(
        execute_items(&operations, items, allow_partial, true),
        false,
        false,
    )
}

fn adapt_mtime_precondition(
    collection: &Collection,
    shadow: &Collection,
    input: &Value,
) -> Result<Value, Box<Diagnostic>> {
    let Some(expected) = input.get("last_known_mtime").and_then(Value::as_u64) else {
        return Ok(input.clone());
    };
    let path = input
        .get("path")
        .or_else(|| input.get("from"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Box::new(Diagnostic::error(
                "invalid_request",
                "A mutation mtime precondition requires a record path.",
                None,
            ))
        })?;
    let current = modified_millis(&collection.root.join(path)).ok_or_else(|| {
        Box::new(Diagnostic::error(
            "concurrent_modification",
            format!("File '{path}' no longer matches the requested modification time."),
            Some(path.to_string()),
        ))
    })?;
    if current != expected {
        return Err(Box::new(Diagnostic::error(
            "concurrent_modification",
            format!("File '{path}' was modified externally."),
            Some(path.to_string()),
        )));
    }
    let shadow_mtime = modified_millis(&shadow.root.join(path)).ok_or_else(|| {
        Box::new(Diagnostic::error(
            "batch_preflight_failed",
            format!("Preflight record '{path}' is unavailable."),
            Some(path.to_string()),
        ))
    })?;
    let mut adapted = input.as_object().cloned().unwrap_or_default();
    adapted.insert(
        "last_known_mtime".to_string(),
        Value::Number(shadow_mtime.into()),
    );
    Ok(Value::Object(adapted))
}

fn modified_millis(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

pub(crate) struct ShadowCollection {
    pub(crate) directory: tempfile::TempDir,
    pub(crate) collection: Collection,
    pub(crate) baseline: crate::transactions::FileBaseline,
}

pub(crate) fn shadow_collection(
    collection: &Collection,
) -> Result<ShadowCollection, Box<Diagnostic>> {
    shadow_collection_inner(collection, None).map_err(|error| match error {
        RuntimeBatchError::Diagnostic(diagnostic) => diagnostic,
        RuntimeBatchError::Provider(error) => {
            Box::new(Diagnostic::error(error.code(), error.to_string(), None))
        }
    })
}

fn shadow_collection_context(
    collection: &Collection,
    context: &OperationContext,
) -> Result<ShadowCollection, ProviderError> {
    shadow_collection_inner(collection, Some(context)).map_err(|error| match error {
        RuntimeBatchError::Diagnostic(diagnostic) => {
            ProviderError::CollectionOpen(diagnostic.message.clone())
        }
        RuntimeBatchError::Provider(error) => error,
    })
}

enum RuntimeBatchError {
    Diagnostic(Box<Diagnostic>),
    Provider(ProviderError),
}

fn shadow_collection_inner(
    collection: &Collection,
    context: Option<&OperationContext>,
) -> Result<ShadowCollection, RuntimeBatchError> {
    if let Some(context) = context {
        context.check().map_err(RuntimeBatchError::Provider)?;
    }
    let directory = tempfile::tempdir().map_err(|error| {
        RuntimeBatchError::Diagnostic(Box::new(Diagnostic::error(
            "batch_preflight_failed",
            format!("Could not create batch preflight workspace: {error}"),
            None,
        )))
    })?;
    let baseline = copy_collection(collection, directory.path(), context)?;
    let shadow = Collection::open(directory.path()).map_err(|error| {
        RuntimeBatchError::Diagnostic(Box::new(Diagnostic::error(
            "batch_preflight_failed",
            format!("Could not open batch preflight collection: {error:?}"),
            None,
        )))
    })?;
    Ok(ShadowCollection {
        directory,
        collection: shadow,
        baseline,
    })
}

fn copy_collection(
    collection: &Collection,
    destination: &Path,
    context: Option<&OperationContext>,
) -> Result<crate::transactions::FileBaseline, RuntimeBatchError> {
    let source = &collection.root;
    let mut baseline = BTreeMap::new();
    for entry in WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| should_descend(collection, entry.path()))
    {
        if let Some(context) = context {
            context.check().map_err(RuntimeBatchError::Provider)?;
        }
        let entry = entry.map_err(|error| {
            RuntimeBatchError::Diagnostic(Box::new(Diagnostic::error(
                "batch_preflight_failed",
                format!("Could not inspect collection for batch preflight: {error}"),
                None,
            )))
        })?;
        let relative = entry.path().strip_prefix(source).map_err(|error| {
            RuntimeBatchError::Diagnostic(Box::new(Diagnostic::error(
                "batch_preflight_failed",
                error.to_string(),
                None,
            )))
        })?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).map_err(|error| {
                RuntimeBatchError::Diagnostic(Box::new(copy_error(relative, error)))
            })?;
        } else if entry.file_type().is_file() && should_copy_file(collection, relative) {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    RuntimeBatchError::Diagnostic(Box::new(copy_error(relative, error)))
                })?;
            }
            let bytes = fs::read(entry.path()).map_err(|error| {
                RuntimeBatchError::Diagnostic(Box::new(copy_error(relative, error)))
            })?;
            fs::write(&target, &bytes).map_err(|error| {
                RuntimeBatchError::Diagnostic(Box::new(copy_error(relative, error)))
            })?;
            baseline.insert(portable_path(relative), bytes);
        }
    }
    Ok(baseline)
}

pub(crate) fn collect_collection_files(
    collection: &Collection,
) -> Result<crate::transactions::FileBaseline, Box<Diagnostic>> {
    collect_collection_files_inner(collection, None).map_err(|error| match error {
        RuntimeBatchError::Diagnostic(diagnostic) => diagnostic,
        RuntimeBatchError::Provider(error) => {
            Box::new(Diagnostic::error(error.code(), error.to_string(), None))
        }
    })
}

fn collect_collection_files_context(
    collection: &Collection,
    context: &OperationContext,
) -> Result<crate::transactions::FileBaseline, ProviderError> {
    collect_collection_files_inner(collection, Some(context)).map_err(|error| match error {
        RuntimeBatchError::Diagnostic(diagnostic) => {
            ProviderError::CollectionOpen(diagnostic.message.clone())
        }
        RuntimeBatchError::Provider(error) => error,
    })
}

fn collect_collection_files_inner(
    collection: &Collection,
    context: Option<&OperationContext>,
) -> Result<crate::transactions::FileBaseline, RuntimeBatchError> {
    let mut files = BTreeMap::new();
    let source = &collection.root;
    for entry in WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| should_descend(collection, entry.path()))
    {
        if let Some(context) = context {
            context.check().map_err(RuntimeBatchError::Provider)?;
        }
        let entry = entry.map_err(|error| {
            RuntimeBatchError::Diagnostic(Box::new(Diagnostic::error(
                "batch_preflight_failed",
                format!("Could not inspect preflight result: {error}"),
                None,
            )))
        })?;
        let relative = entry.path().strip_prefix(source).map_err(|error| {
            RuntimeBatchError::Diagnostic(Box::new(Diagnostic::error(
                "batch_preflight_failed",
                error.to_string(),
                None,
            )))
        })?;
        if entry.file_type().is_file() && should_copy_file(collection, relative) {
            let bytes = fs::read(entry.path()).map_err(|error| {
                RuntimeBatchError::Diagnostic(Box::new(copy_error(relative, error)))
            })?;
            files.insert(portable_path(relative), bytes);
        }
    }
    Ok(files)
}

fn should_descend(collection: &Collection, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(&collection.root) else {
        return false;
    };
    if relative.as_os_str().is_empty() || is_system_definition_path(collection, relative) {
        return true;
    }
    if !path.is_dir() {
        return true;
    }
    let relative = portable_path(relative);
    if collection.is_excluded(&relative) {
        return false;
    }
    // A directory containing its own config is a nested collection boundary.
    // Avoid copying any of it into the preflight workspace.
    !path.join("mdbase.yaml").is_file()
}

fn should_copy_file(collection: &Collection, relative: &Path) -> bool {
    if relative == Path::new("mdbase.yaml") {
        return true;
    }
    if relative == Path::new("mdbase.lock.yaml") {
        return true;
    }
    if relative == Path::new("mdbase.provisions.yaml") {
        return true;
    }
    let extension = relative.extension().and_then(|value| value.to_str());
    if relative.starts_with(Path::new(&collection.settings.migrations_folder)) {
        return matches!(extension, Some("md" | "json"));
    }
    if relative.starts_with(Path::new(&collection.settings.types_folder))
        || relative.starts_with(Path::new(&collection.settings.contracts_folder))
    {
        return extension == Some("md");
    }
    if extension == Some("base") {
        return crate::views::compatibility_source_paths(collection)
            .iter()
            .any(|path| path == &collection.root.join(relative));
    }
    let relative = portable_path(relative);
    !collection.is_excluded(&relative)
        && (collection.is_valid_extension(&relative)
            || Path::new(&relative)
                .extension()
                .and_then(|value| value.to_str())
                == Some("json"))
}

fn is_system_definition_path(collection: &Collection, relative: &Path) -> bool {
    relative.starts_with(Path::new(&collection.settings.types_folder))
        || relative.starts_with(Path::new(&collection.settings.contracts_folder))
        || relative.starts_with(Path::new(&collection.settings.migrations_folder))
}

fn portable_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
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
    atomic: bool,
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
        let operation = match (kind, atomic) {
            ("create", true) => operations.create(&operation_input),
            ("update", true) => operations.update(&operation_input),
            ("delete", true) => operations.delete(&operation_input),
            ("rename", true) => operations.rename(&operation_input),
            ("create" | "update" | "delete" | "rename", false) => {
                operations.execute_mutation_direct(kind, &operation_input)
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn preflight_shadow_copies_only_collection_scope_and_required_assets() {
        let source = tempfile::tempdir().unwrap();
        write(
            &source.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        );
        write(
            &source.path().join("_types/note.md"),
            "---\nkind: mdbase.type\nname: note\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n",
        );
        write(&source.path().join("visible.md"), "---\ntype: note\n---\n");
        write(
            &source.path().join("schema.json"),
            "{\"type\":\"object\"}\n",
        );
        write(&source.path().join(".git/large.md"), "must not copy");
        write(
            &source.path().join("nested/mdbase.yaml"),
            "spec_version: 0.3.0\n",
        );
        write(&source.path().join("nested/hidden.md"), "must not copy");

        let collection = Collection::open(source.path()).unwrap();
        let shadow = shadow_collection(&collection).unwrap();
        let root = &shadow.collection.root;
        assert!(root.join("mdbase.yaml").is_file());
        assert!(root.join("_types/note.md").is_file());
        assert!(root.join("visible.md").is_file());
        assert!(root.join("schema.json").is_file());
        assert!(!root.join(".git").exists());
        assert!(!root.join("nested").exists());
    }

    #[test]
    fn staged_mutations_write_the_caller_owned_working_set_directly() {
        let stage = tempfile::tempdir().unwrap();
        write(
            &stage.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        );
        write(
            &stage.path().join("_types/note.md"),
            "---\nkind: mdbase.type\nname: note\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n",
        );

        let collection = Collection::open(stage.path()).unwrap();
        let operations = collection.v03_operations().unwrap();
        let result = operations.execute_staged_mutation(
            "create",
            &json!({
                "path": "notes/staged.md",
                "frontmatter": {"type": "note", "title": "Staged"},
                "body": "Caller-owned body"
            }),
        );

        assert!(result.valid, "{:?}", result.diagnostics);
        assert!(stage.path().join("notes/staged.md").is_file());
        assert_eq!(result.result["path"], json!("notes/staged.md"));
    }

    #[test]
    fn runtime_single_update_stages_only_the_affected_record() {
        let source = tempfile::tempdir().unwrap();
        write(
            &source.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  default_validation: error\n",
        );
        write(
            &source.path().join("target.md"),
            "---\nid: target\ntitle: Before\n---\nBody\n",
        );
        write(
            &source.path().join("unrelated.md"),
            "---\nid: unrelated\ntitle: Untouched\n---\n",
        );
        let collection = Collection::open(source.path()).unwrap();
        crate::cache::runtime::rebuild(
            &collection,
            &crate::runtime::CollectionGeneration::initial(),
        )
        .unwrap();

        let prepared = prepare_single_runtime(
            &collection,
            "update",
            &json!({"path": "target.md", "fields": {"title": "After"}}),
            &crate::runtime::OperationContext::legacy(),
        )
        .unwrap();
        let RuntimeSinglePreparation::Prepared(plan) = prepared else {
            panic!("expected a staged mutation")
        };
        assert_eq!(plan.baseline.keys().collect::<Vec<_>>(), ["target.md"]);
        assert_eq!(plan.desired.keys().collect::<Vec<_>>(), ["target.md"]);
        assert!(!plan.baseline.contains_key("unrelated.md"));
        assert_eq!(
            fs::read_to_string(source.path().join("target.md")).unwrap(),
            "---\nid: target\ntitle: Before\n---\nBody\n"
        );
    }

    #[test]
    fn runtime_single_dry_run_never_mutates_the_live_record() {
        let source = tempfile::tempdir().unwrap();
        write(
            &source.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  default_validation: error\n",
        );
        let original = "---\nid: target\ntitle: Before\n---\nBody\n";
        write(&source.path().join("target.md"), original);
        let collection = Collection::open(source.path()).unwrap();
        crate::cache::runtime::rebuild(
            &collection,
            &crate::runtime::CollectionGeneration::initial(),
        )
        .unwrap();

        let prepared = prepare_single_runtime(
            &collection,
            "update",
            &json!({
                "path": "target.md",
                "fields": {"title": "After"},
                "dry_run": true
            }),
            &crate::runtime::OperationContext::legacy(),
        )
        .unwrap();
        assert!(matches!(prepared, RuntimeSinglePreparation::NoMutation(_)));
        assert_eq!(
            fs::read_to_string(source.path().join("target.md")).unwrap(),
            original
        );
    }
}
