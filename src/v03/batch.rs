use std::fs;
use std::path::Path;

use serde_json::{json, Value};
use walkdir::WalkDir;

use super::{Diagnostic, OperationResult, Operations};
use crate::mutation::{
    collect_collection_files, collect_collection_files_context, shadow_collection,
    shadow_collection_context, ShadowCollection,
};
use crate::runtime::{
    CanonicalOperationOutcome, CollectionSnapshot, OperationContext, OperationKind, ProviderError,
};
use crate::Collection;

#[allow(clippy::large_enum_variant)]
pub(crate) enum RuntimeSinglePreparation {
    NoMutation(CanonicalOperationOutcome),
    Prepared(Box<RuntimeMutationPlan>),
}

pub(crate) struct RuntimeMutationPlan {
    pub(crate) operation: CanonicalOperationOutcome,
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
    #[cfg(test)]
    if matches!(operation, "create" | "update" | "delete" | "rename") {
        crate::mutation::probe_runtime_decode();
    }
    if matches!(operation, "create" | "update" | "delete") {
        return prepare_sparse_runtime(collection, operation, input, context);
    }

    let before = collection.snapshot_with_context(context)?;
    context.check()?;
    let shadow = shadow_collection_context(collection, context)?;
    let typed_operation = operation.parse::<OperationKind>()?;
    let shadow_input = match adapt_mtime_precondition(collection, &shadow.collection, input) {
        Ok(input) => input,
        Err(diagnostic) => {
            return Ok(RuntimeSinglePreparation::NoMutation(
                CanonicalOperationOutcome::invalid(typed_operation, vec![(*diagnostic).into()]),
            ))
        }
    };
    context.check()?;
    let outcome = if operation == "rename" {
        prepare_runtime_rename_typed(collection, &shadow.collection, &shadow_input)?
    } else {
        let shadow_operations = Operations::new(&shadow.collection)
            .map_err(|diagnostic| ProviderError::CollectionOpen(diagnostic.message.clone()))?;
        CanonicalOperationOutcome::wire_only(
            typed_operation,
            execute_non_record_runtime_operation(&shadow_operations, operation, &shadow_input),
        )
    };
    context.check()?;
    if !outcome.valid {
        return Ok(RuntimeSinglePreparation::NoMutation(outcome));
    }
    let desired = collect_collection_files_context(&shadow.collection, context)?;
    if desired == shadow.baseline {
        return Ok(RuntimeSinglePreparation::NoMutation(outcome));
    }
    let after = shadow.collection.snapshot_with_context(context)?;
    context.check()?;
    Ok(RuntimeSinglePreparation::Prepared(Box::new(
        RuntimeMutationPlan {
            operation: outcome,
            baseline: shadow.baseline,
            desired,
            before,
            after,
        },
    )))
}

fn prepare_runtime_rename_typed(
    _authority: &Collection,
    working_set: &Collection,
    input: &Value,
) -> Result<CanonicalOperationOutcome, ProviderError> {
    if input.get("simulate_before_ref_update").is_some()
        || input.get("last_known_ref_mtimes").is_some()
    {
        return Ok(CanonicalOperationOutcome::invalid(
            OperationKind::Rename,
            vec![crate::api::Diagnostic {
                severity: crate::api::Severity::Error,
                code: crate::api::DiagnosticCode::new("invalid_request"),
                message: "Internal concurrency simulation fields are not accepted by canonical operations.".to_string(),
                path: None,
                field: None,
                type_name: None,
                schema_location: None,
                details: None,
            }],
        ));
    }
    let (request, options, last_known_mtime) =
        match super::mutation_adapter::decode_rename(working_set, input) {
            Ok(decoded) => decoded,
            Err(diagnostics) => {
                return Ok(CanonicalOperationOutcome::invalid(
                    OperationKind::Rename,
                    diagnostics.into_iter().map(Into::into).collect(),
                ))
            }
        };
    let planned =
        match crate::mutation::plan_rename(working_set, request, options, last_known_mtime) {
            Ok(planned) => planned,
            Err(error) => {
                return Ok(CanonicalOperationOutcome::failure(
                    OperationKind::Rename,
                    error,
                ))
            }
        };
    let (valid, result, mut diagnostics) = crate::mutation::rename_result(working_set, planned)
        .map_err(|error| ProviderError::Transaction {
            code: "typed_outcome_projection_failed",
            message: error.to_string(),
        })?;
    for diagnostic in &mut diagnostics {
        if diagnostic.severity == crate::api::Severity::Error {
            diagnostic.code =
                crate::api::DiagnosticCode::new(crate::errors::RENAME_REF_UPDATE_FAILED);
        }
    }
    Ok(CanonicalOperationOutcome::record_mutation(
        OperationKind::Rename,
        valid,
        result,
        diagnostics,
    ))
}

fn execute_non_record_runtime_operation(
    operations: &Operations<'_>,
    operation: &str,
    input: &Value,
) -> OperationResult {
    match operation {
        "create_view_source" => operations.create_view_source(input),
        "update_view_source" => operations.update_view_source(input),
        "delete_view_source" => operations.delete_view_source(input),
        "create_type" => operations.create_type(input),
        "update_type" => operations.update_type(input),
        "apply_type_pack" => execute_type_pack(operations.collection(), input),
        "apply_collection_setup" => execute_collection_setup(operations.collection(), input),
        _ => failed(vec![Diagnostic::error(
            "invalid_request",
            format!("Unsupported mutation operation '{operation}'."),
            None,
        )]),
    }
}

fn execute_type_pack(collection: &Collection, input: &Value) -> OperationResult {
    let provision = input
        .get("provision")
        .cloned()
        .and_then(|value| serde_json::from_value::<super::TypePackProvision>(value).ok());
    let options = input
        .get("options")
        .cloned()
        .and_then(|value| serde_json::from_value::<super::TypePackApplyOptions>(value).ok());
    match (provision, options) {
        (Some(provision), Some(options)) => collection.apply_type_pack(&provision, &options),
        _ => invalid_request("Type-pack apply input requires valid provision and options."),
    }
}

fn execute_collection_setup(collection: &Collection, input: &Value) -> OperationResult {
    let setup = input
        .get("setup")
        .cloned()
        .and_then(|value| serde_json::from_value::<super::CollectionSetup>(value).ok());
    let options = input
        .get("options")
        .cloned()
        .and_then(|value| serde_json::from_value::<super::CollectionSetupApplyOptions>(value).ok());
    match (setup, options) {
        (Some(setup), Some(options)) => collection.apply_collection_setup(&setup, &options),
        _ => invalid_request("Collection setup apply input requires valid setup and options."),
    }
}

fn prepare_sparse_runtime(
    collection: &Collection,
    operation: &str,
    input: &Value,
    context: &OperationContext,
) -> Result<RuntimeSinglePreparation, ProviderError> {
    let kind = operation.parse::<OperationKind>()?;
    let input_path = input.get("path").and_then(Value::as_str);
    let shadow = sparse_shadow_collection(collection, input_path, context)?;
    let shadow_input = match adapt_mtime_precondition(collection, &shadow.collection, input) {
        Ok(input) => input,
        Err(diagnostic) => {
            return Ok(RuntimeSinglePreparation::NoMutation(
                CanonicalOperationOutcome::invalid(kind, vec![(*diagnostic).into()]),
            ))
        }
    };
    context.check()?;
    let (outcome, delete_plan) =
        prepare_sparse_typed(collection, &shadow.collection, kind, &shadow_input)?;
    context.check()?;
    if !outcome.valid
        || input
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return Ok(RuntimeSinglePreparation::NoMutation(outcome));
    }

    let path = delete_plan
        .as_ref()
        .map(|planned| planned.path.as_str())
        .or_else(|| outcome.record().map(|record| record.path.as_str()))
        .ok_or_else(|| {
            ProviderError::CollectionOpen(
                "sparse mutation outcome did not identify its record path".to_string(),
            )
        })?
        .to_string();
    let mut baseline = if kind == OperationKind::Delete {
        shadow.baseline.clone()
    } else {
        crate::transactions::FileBaseline::new()
    };
    if kind != OperationKind::Delete && collection.held_root().exists_file(Path::new(&path)) {
        let bytes = read_held_bounded(collection, Path::new(&path), context)?;
        baseline.insert(path.clone(), bytes);
    }
    if kind == OperationKind::Create
        && baseline.contains_key(&path)
        && input_path != Some(path.as_str())
    {
        return Ok(RuntimeSinglePreparation::NoMutation(
            CanonicalOperationOutcome::invalid(
                kind,
                vec![crate::api::Diagnostic {
                    severity: crate::api::Severity::Error,
                    code: crate::api::DiagnosticCode::new("path_conflict"),
                    message: format!("File already exists: {path}"),
                    path: Some(path),
                    field: None,
                    type_name: None,
                    schema_location: None,
                    details: None,
                }],
            ),
        ));
    }
    let mut desired = crate::transactions::FileBaseline::new();
    if shadow.collection.held_root().exists_file(Path::new(&path)) {
        let bytes = read_held_bounded(&shadow.collection, Path::new(&path), context)?;
        desired.insert(path.clone(), bytes);
    }
    if baseline == desired {
        return Ok(RuntimeSinglePreparation::NoMutation(outcome));
    }

    if matches!(kind, OperationKind::Create | OperationKind::Update)
        && collection.settings.default_validation == "error"
    {
        let frontmatter = outcome
            .record()
            .map(|record| &record.frontmatter)
            .ok_or_else(|| {
                ProviderError::CollectionOpen(
                    "record mutation outcome omitted frontmatter".to_string(),
                )
            })?;
        let type_names = collection.determine_types_for_path(frontmatter, Some(&path));
        let issues = collection
            .check_uniqueness_indexed(frontmatter, &type_names, &path)
            .map_err(|error| ProviderError::Transaction {
                code: "cache_maintenance_failed",
                message: error.to_string(),
            })?;
        context.check()?;
        if !issues.is_empty() {
            let diagnostics = issues
                .into_iter()
                .map(|issue| crate::api::Diagnostic {
                    severity: crate::api::Severity::Error,
                    code: crate::api::DiagnosticCode::new(issue.code),
                    message: issue.message,
                    path: issue.path,
                    field: issue.field,
                    type_name: None,
                    schema_location: None,
                    details: None,
                })
                .collect();
            return Ok(RuntimeSinglePreparation::NoMutation(
                CanonicalOperationOutcome::invalid(kind, diagnostics),
            ));
        }
    }

    let before = delete_plan
        .as_ref()
        .map(planned_delete_snapshot)
        .unwrap_or_else(|| targeted_snapshot(collection, &path, context))?;
    let after = targeted_snapshot(&shadow.collection, &path, context)?;
    context.check()?;
    Ok(RuntimeSinglePreparation::Prepared(Box::new(
        RuntimeMutationPlan {
            operation: outcome,
            baseline,
            desired,
            before,
            after,
        },
    )))
}

fn prepare_sparse_typed(
    authority: &Collection,
    working_set: &Collection,
    kind: OperationKind,
    input: &Value,
) -> Result<
    (
        CanonicalOperationOutcome,
        Option<crate::mutation::PlannedDelete>,
    ),
    ProviderError,
> {
    let invalid = |diagnostics: Vec<Diagnostic>| {
        CanonicalOperationOutcome::invalid(kind, diagnostics.into_iter().map(Into::into).collect())
    };
    let projected = match kind {
        OperationKind::Create => {
            let (request, options) = match super::mutation_adapter::decode_create(input) {
                Ok(decoded) => decoded,
                Err(diagnostics) => return Ok((invalid(diagnostics), None)),
            };
            let planned = match crate::mutation::staged_create(working_set, request, options) {
                Ok(planned) => planned,
                Err(error) => return Ok((CanonicalOperationOutcome::failure(kind, error), None)),
            };
            crate::mutation::record_result(working_set, planned)
        }
        OperationKind::Update => {
            let (request, options) = match super::mutation_adapter::decode_update(input) {
                Ok(decoded) => decoded,
                Err(diagnostics) => return Ok((invalid(diagnostics), None)),
            };
            let planned = match crate::mutation::staged_update(working_set, request, options) {
                Ok(planned) => planned,
                Err(error) => return Ok((CanonicalOperationOutcome::failure(kind, error), None)),
            };
            crate::mutation::record_result(working_set, planned)
        }
        OperationKind::Delete => {
            let (request, options) = match super::mutation_adapter::decode_delete(input) {
                Ok(decoded) => decoded,
                Err(diagnostics) => return Ok((invalid(diagnostics), None)),
            };
            let planned =
                match crate::mutation::plan_delete(authority, working_set, request, options) {
                    Ok(planned) => planned,
                    Err(error) => {
                        return Ok((CanonicalOperationOutcome::failure(kind, error), None))
                    }
                };
            let projected = crate::mutation::delete_result(planned.clone());
            let (valid, result, diagnostics) = projected.map_err(typed_projection_error)?;
            return Ok((
                CanonicalOperationOutcome::record_mutation(kind, valid, result, diagnostics),
                Some(planned),
            ));
        }
        _ => unreachable!("only sparse record mutations are decoded here"),
    };
    let (valid, result, diagnostics) = projected.map_err(typed_projection_error)?;
    Ok((
        CanonicalOperationOutcome::record_mutation(kind, valid, result, diagnostics),
        None,
    ))
}

fn typed_projection_error(error: crate::api::MdbaseError) -> ProviderError {
    ProviderError::Transaction {
        code: "typed_outcome_projection_failed",
        message: error.to_string(),
    }
}

fn planned_delete_snapshot(
    planned: &crate::mutation::PlannedDelete,
) -> Result<CollectionSnapshot, ProviderError> {
    Ok(CollectionSnapshot {
        revision: String::new(),
        resource_revision: String::new(),
        spec_version: super::SPEC_VERSION.to_string(),
        resources: Vec::new(),
        records: vec![crate::runtime::CollectionSnapshotRecord {
            path: planned.path.to_string(),
            revision: planned.before_revision.clone(),
            frontmatter: planned.before_frontmatter.clone(),
            body: planned.before_body.clone(),
            types: planned.types.clone(),
            document: String::new(),
            frontmatter_error: None,
        }],
    })
}

fn targeted_snapshot(
    collection: &Collection,
    path: &str,
    context: &OperationContext,
) -> Result<CollectionSnapshot, ProviderError> {
    let records = match collection.snapshot_record_with_context(path, context) {
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
    #[cfg(test)]
    crate::mutation::probe_sparse_shadow();
    context.check()?;
    let directory =
        tempfile::tempdir().map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
    let mut captured_entries = 0_u64;
    let mut resource_entries = 0_u64;
    copy_sparse_controls(
        collection,
        directory.path(),
        context,
        &mut captured_entries,
        &mut resource_entries,
    )?;
    let mut baseline = crate::transactions::FileBaseline::new();
    if let Some(target) = target {
        if collection.held_root().exists_file(Path::new(target)) {
            captured_entries = checked_capture_increment(captured_entries)?;
            context.check_entries(captured_entries)?;
            let bytes = read_held_bounded(collection, Path::new(target), context)?;
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
    captured_entries: &mut u64,
    resource_entries: &mut u64,
) -> Result<(), ProviderError> {
    for relative in ["mdbase.yaml", "mdbase.lock.yaml"] {
        if collection.held_root().exists_file(Path::new(relative)) {
            *captured_entries = checked_capture_increment(*captured_entries)?;
            *resource_entries = checked_capture_increment(*resource_entries)?;
            context.check_entries(*captured_entries)?;
            context.check_resource_entries(*resource_entries)?;
            let bytes = read_held_bounded(collection, Path::new(relative), context)?;
            fs::write(destination.join(relative), bytes)
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
                *captured_entries = checked_capture_increment(*captured_entries)?;
                *resource_entries = checked_capture_increment(*resource_entries)?;
                context.check_entries(*captured_entries)?;
                context.check_resource_entries(*resource_entries)?;
                context.check_depth(relative.components().count().saturating_sub(1) as u64)?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
                }
                let bytes = read_held_bounded(collection, relative, context)?;
                fs::write(&target, bytes)
                    .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
            }
        }
    }
    Ok(())
}

fn checked_capture_increment(value: u64) -> Result<u64, ProviderError> {
    value.checked_add(1).ok_or({
        ProviderError::CaptureLimitExceeded(crate::runtime::CaptureLimitExceeded {
            kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
            limit: u64::MAX,
            attempted: u64::MAX,
        })
    })
}

fn read_held_bounded(
    collection: &Collection,
    relative: &Path,
    context: &OperationContext,
) -> Result<Vec<u8>, ProviderError> {
    use std::io::Read;
    context.check()?;
    let mut file = collection
        .held_root()
        .open_file(relative)
        .map_err(|error| {
            ProviderError::CollectionOpen(format!(
                "failed to open '{}': {error}",
                relative.display()
            ))
        })?;
    let size = file
        .metadata()
        .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?
        .len();
    context.check_file_bytes(size)?;
    let capacity = usize::try_from(size).map_err(|_| crate::runtime::CaptureLimitExceeded {
        kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
        limit: usize::MAX as u64,
        attempted: size,
    })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| crate::runtime::CaptureLimitExceeded {
            kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
            limit: usize::MAX as u64,
            attempted: size,
        })?;
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        context.check()?;
        let read = file
            .read(&mut chunk)
            .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
        if read == 0 {
            break;
        }
        let attempted = (bytes.len() as u64).checked_add(read as u64).ok_or(
            crate::runtime::CaptureLimitExceeded {
                kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
                limit: u64::MAX,
                attempted: u64::MAX,
            },
        )?;
        context.check_file_bytes(attempted)?;
        context.charge_read(read as u64)?;
        context.charge_retained(read as u64)?;
        bytes.extend_from_slice(&chunk[..read]);
        context.check()?;
    }
    Ok(bytes)
}

pub(crate) fn execute_wire_mutation(
    collection: &Collection,
    operation: &str,
    input: &Value,
) -> OperationResult {
    if input
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let shadow = match shadow_collection(collection) {
            Ok(shadow) => shadow,
            Err(diagnostic) => return failed(vec![*diagnostic]),
        };
        let operations = match Operations::new(&shadow.collection) {
            Ok(operations) => operations,
            Err(diagnostic) => return failed(vec![*diagnostic]),
        };
        return operations.execute_staged_mutation(operation, input);
    }

    let shadow = match shadow_collection(collection) {
        Ok(shadow) => shadow,
        Err(diagnostic) => return failed(vec![*diagnostic]),
    };
    let shadow_input = match adapt_mtime_precondition(collection, &shadow.collection, input) {
        Ok(input) => input,
        Err(diagnostic) => return failed(vec![*diagnostic]),
    };
    let shadow_operations = match Operations::new(&shadow.collection) {
        Ok(operations) => operations,
        Err(diagnostic) => return failed(vec![*diagnostic]),
    };
    let mut result = shadow_operations.execute_staged_mutation(operation, &shadow_input);
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
    crate::transactions::attach_committed_file_facts(&mut result.result, &commit.file_facts);
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
    let items = match validate_batch_envelope(input) {
        Ok(items) => items,
        Err(diagnostics) => return failed(diagnostics),
    };
    if let Some(diagnostic) = duplicate_batch_path(items) {
        return failed(vec![diagnostic]);
    }
    let allow_partial = input
        .get("allow_partial")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let dry_run = input
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if allow_partial && !dry_run {
        return execute_partial_wire(collection, items);
    }
    let (request, options) = match decode_batch_request(collection, input) {
        Ok(decoded) => decoded,
        Err(diagnostics) => return failed(diagnostics),
    };
    match crate::mutation::batch_wire(collection, request, options) {
        Ok(execution) => batch_operation_result(execution),
        Err(error) => super::operations::typed_error_result(error),
    }
}

pub(crate) fn decode_batch_request(
    collection: &Collection,
    input: &Value,
) -> Result<(crate::api::BatchRequest, crate::mutation::BatchWireOptions), Vec<Diagnostic>> {
    let items = validate_batch_envelope(input)?;
    if let Some(diagnostic) = duplicate_batch_path(items) {
        return Err(vec![diagnostic]);
    }
    let mut operations = Vec::with_capacity(items.len());
    let mut create_documents = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let (operation, create_document) = decode_batch_item(collection, index, item)?;
        operations.push(operation);
        create_documents.push(create_document);
    }
    Ok((
        crate::api::BatchRequest {
            operations,
            allow_partial: input
                .get("allow_partial")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            dry_run: input
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        crate::mutation::BatchWireOptions { create_documents },
    ))
}

fn validate_batch_envelope(input: &Value) -> Result<&[Value], Vec<Diagnostic>> {
    let items = input
        .get("operations")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            vec![Diagnostic::error(
                "invalid_request",
                "Batch input requires an operations array.",
                None,
            )]
        })?;
    if items.is_empty() {
        return Err(vec![Diagnostic::error(
            "invalid_request",
            "Batch operations must not be empty.",
            None,
        )]);
    }
    for name in ["allow_partial", "dry_run"] {
        if input.get(name).is_some_and(|value| !value.is_boolean()) {
            return Err(vec![Diagnostic::error(
                "invalid_request",
                format!("{name} must be a boolean."),
                None,
            )]);
        }
    }
    Ok(items)
}

fn decode_batch_item(
    collection: &Collection,
    index: usize,
    item: &Value,
) -> Result<(crate::api::BatchOperation, Option<String>), Vec<Diagnostic>> {
    let kind = item.get("kind").and_then(Value::as_str).ok_or_else(|| {
        vec![Diagnostic::error(
            "invalid_request",
            format!("Batch operation {index} requires kind."),
            None,
        )]
    })?;
    let operation_input = item.get("input").cloned().unwrap_or_else(|| json!({}));
    if !operation_input.is_object() {
        return Err(vec![Diagnostic::error(
            "invalid_request",
            format!("Batch operation {index} input must be an object."),
            None,
        )]);
    }
    validate_authoritative_mtime(collection, &operation_input)?;
    match kind {
        "create" => {
            let (request, options) = super::mutation_adapter::decode_create(&operation_input)?;
            Ok((
                crate::api::BatchOperation::Create(request),
                options.create_document,
            ))
        }
        "update" => {
            let (request, _) = super::mutation_adapter::decode_update(&operation_input)?;
            Ok((crate::api::BatchOperation::Update(request), None))
        }
        "delete" => {
            let (request, _) = super::mutation_adapter::decode_delete(&operation_input)?;
            Ok((crate::api::BatchOperation::Delete(request), None))
        }
        "rename" => {
            let (request, _, _) =
                super::mutation_adapter::decode_rename(collection, &operation_input)?;
            Ok((crate::api::BatchOperation::Rename(request), None))
        }
        _ => Err(vec![Diagnostic::error(
            "invalid_request",
            format!("Unsupported batch operation kind '{kind}'."),
            None,
        )]),
    }
}

fn execute_partial_wire(collection: &Collection, items: &[Value]) -> OperationResult {
    let mut results = Vec::with_capacity(items.len());
    let mut diagnostics = Vec::new();
    for (index, item) in items.iter().enumerate() {
        match decode_batch_item(collection, index, item) {
            Ok((operation, create_document)) => {
                let (result, mut aggregate) = crate::mutation::execute_partial_item(
                    collection,
                    index,
                    operation,
                    create_document,
                );
                results.push(result);
                diagnostics.append(&mut aggregate);
            }
            Err(item_diagnostics) => {
                let typed = item_diagnostics
                    .iter()
                    .cloned()
                    .map(crate::api::Diagnostic::from)
                    .collect::<Vec<_>>();
                diagnostics.extend(typed.iter().cloned());
                results.push(crate::api::BatchItemResult {
                    index,
                    kind: item
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    valid: false,
                    result: crate::api::BatchOperationResult::default(),
                    diagnostics: typed,
                });
            }
        }
    }
    batch_operation_result(crate::mutation::aggregate_partial(results, diagnostics))
}

fn duplicate_batch_path(items: &[Value]) -> Option<Diagnostic> {
    let mut seen = std::collections::BTreeSet::new();
    for item in items {
        let kind = item.get("kind").and_then(Value::as_str);
        let input = item.get("input").and_then(Value::as_object);
        let keys: &[&str] = if kind == Some("rename") {
            &["from", "to"]
        } else {
            &["path"]
        };
        for key in keys {
            let Some(path) = input
                .and_then(|value| value.get(*key))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let canonical = path.replace('\\', "/");
            if !seen.insert(canonical.clone()) {
                return Some(Diagnostic::error(
                    "duplicate_batch_path",
                    format!("Batch path '{canonical}' is used more than once."),
                    Some(canonical),
                ));
            }
        }
    }
    None
}

fn validate_authoritative_mtime(
    collection: &Collection,
    input: &Value,
) -> Result<(), Vec<Diagnostic>> {
    let Some(expected) = input.get("last_known_mtime").and_then(Value::as_u64) else {
        return Ok(());
    };
    let path = input
        .get("path")
        .or_else(|| input.get("from"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            vec![Diagnostic::error(
                "invalid_request",
                "A mutation mtime precondition requires a record path.",
                None,
            )]
        })?;
    let current = modified_millis(&collection.root.join(path)).ok_or_else(|| {
        vec![Diagnostic::error(
            "concurrent_modification",
            format!("File '{path}' no longer matches the requested modification time."),
            Some(path.to_string()),
        )]
    })?;
    if current != expected {
        return Err(vec![Diagnostic::error(
            "concurrent_modification",
            format!("File '{path}' was modified externally."),
            Some(path.to_string()),
        )]);
    }
    Ok(())
}

pub(crate) fn batch_operation_result(
    execution: crate::mutation::BatchExecution,
) -> OperationResult {
    OperationResult {
        valid: execution.result.failed == 0
            && execution
                .diagnostics
                .iter()
                .all(|item| item.severity != crate::api::Severity::Error),
        result: serde_json::to_value(execution.result).expect("batch result serializes"),
        diagnostics: execution
            .diagnostics
            .into_iter()
            .map(typed_diagnostic)
            .collect(),
    }
}

fn typed_diagnostic(value: crate::api::Diagnostic) -> Diagnostic {
    Diagnostic {
        severity: match value.severity {
            crate::api::Severity::Error => "error",
            crate::api::Severity::Warning => "warning",
            crate::api::Severity::Info => "info",
        }
        .to_string(),
        code: value.code.to_string(),
        message: value.message,
        path: value.path,
        field: value.field,
        type_name: value.type_name,
        schema_location: value.schema_location,
        details: value.details,
    }
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
    fn wire_partial_lazily_rejects_late_malformed_unsupported_and_stale_items() {
        let source = tempfile::tempdir().unwrap();
        write(
            &source.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        );
        write(&source.path().join("stale.md"), "stale\n");
        let collection = Collection::open(source.path()).unwrap();
        crate::mutation::reset_mutation_path_probes();

        let result = execute(
            &collection,
            &json!({
                "allow_partial": true,
                "operations": [
                    {"kind": "create", "input": {"path": "first.md", "body": "first"}},
                    {"input": {"path": "missing-kind.md"}},
                    {"kind": "future", "input": {"path": "unsupported.md"}},
                    {"kind": "update", "input": {
                        "path": "stale.md", "body": "changed", "last_known_mtime": 0
                    }},
                    {"kind": "create", "input": {"path": "last.md", "body": "last"}}
                ]
            }),
        );

        assert!(!result.valid);
        assert_eq!(result.result["succeeded"], 2);
        assert_eq!(result.result["failed"], 3);
        assert_eq!(result.result["operations"].as_array().unwrap().len(), 5);
        assert!(result.result["operations"][1].get("kind").is_none());
        assert_eq!(result.result["operations"][2]["kind"], "future");
        assert_eq!(
            result
                .diagnostics
                .iter()
                .map(|item| item.code.as_str())
                .collect::<Vec<_>>(),
            [
                "invalid_request",
                "invalid_request",
                "concurrent_modification"
            ]
        );
        assert!(source.path().join("first.md").is_file());
        assert!(source.path().join("last.md").is_file());
        assert_eq!(
            fs::read_to_string(source.path().join("stale.md")).unwrap(),
            "stale\n"
        );
        assert_eq!(
            crate::mutation::mutation_path_probes().full_shadows,
            2,
            "only successfully decoded items execute"
        );
    }

    #[test]
    fn typed_and_wire_mixed_batch_items_and_diagnostics_have_exact_json_parity() {
        fn prepared_fixture() -> tempfile::TempDir {
            let root = tempfile::tempdir().unwrap();
            write(&root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n");
            write(&root.path().join("update.md"), "---\ntitle: before\n---\n");
            write(&root.path().join("rename.md"), "rename\n");
            write(&root.path().join("delete.md"), "delete\n");
            root
        }
        for dry_run in [false, true] {
            let typed_root = prepared_fixture();
            let wire_root = prepared_fixture();
            let typed_collection = Collection::open(typed_root.path()).unwrap();
            let wire_collection = Collection::open(wire_root.path()).unwrap();
            let request = crate::api::BatchRequest {
                operations: vec![
                    crate::api::BatchOperation::Update(crate::api::UpdateRequest::new(
                        crate::api::CollectionPath::new("update.md").unwrap(),
                        json!({"title": "after"}),
                    )),
                    crate::api::BatchOperation::Rename(crate::api::RenameRequest::new(
                        crate::api::CollectionPath::new("rename.md").unwrap(),
                        crate::api::CollectionPath::new("renamed.md").unwrap(),
                    )),
                    crate::api::BatchOperation::Create(
                        crate::api::CreateRequest::new(
                            crate::api::CollectionPath::new("created.md").unwrap(),
                        )
                        .with_body("created"),
                    ),
                    crate::api::BatchOperation::Delete(crate::api::DeleteRequest::new(
                        crate::api::CollectionPath::new("delete.md").unwrap(),
                    )),
                ],
                allow_partial: false,
                dry_run,
            };
            let wire_input = request.clone().to_wire();
            let typed = typed_collection.typed().unwrap().batch(request).unwrap();
            let wire = execute(&wire_collection, &wire_input);
            assert!(wire.valid, "{wire:#?}");
            assert_eq!(
                serde_json::to_value(&typed.value).unwrap(),
                wire.result,
                "dry_run={dry_run}"
            );
            assert_eq!(
                typed.diagnostics,
                wire.diagnostics
                    .into_iter()
                    .map(crate::api::Diagnostic::from)
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn committed_cleanup_deferred_is_a_nonfatal_warning_for_typed_and_wire() {
        let typed_root = tempfile::tempdir().unwrap();
        write(
            &typed_root.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\n",
        );
        let typed_collection = Collection::open(typed_root.path()).unwrap();
        crate::transactions::inject_cleanup_deferred(typed_root.path());
        let typed = typed_collection
            .typed()
            .unwrap()
            .batch(
                crate::api::BatchRequest::new(vec![crate::api::BatchOperation::Create(
                    crate::api::CreateRequest::new(
                        crate::api::CollectionPath::new("typed.md").unwrap(),
                    ),
                )])
                .unwrap(),
            )
            .unwrap();
        assert_eq!(typed.diagnostics.len(), 1);
        assert_eq!(typed.diagnostics[0].severity, crate::api::Severity::Warning);
        assert_eq!(
            typed.diagnostics[0].code.as_str(),
            "transaction_cleanup_deferred"
        );
        assert!(typed_root.path().join("typed.md").is_file());

        let wire_root = tempfile::tempdir().unwrap();
        write(
            &wire_root.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\n",
        );
        let wire_collection = Collection::open(wire_root.path()).unwrap();
        crate::transactions::inject_cleanup_deferred(wire_root.path());
        let wire = execute(
            &wire_collection,
            &json!({"operations": [{"kind": "create", "input": {"path": "wire.md"}}]}),
        );
        assert!(wire.valid, "{wire:#?}");
        assert_eq!(wire.diagnostics.len(), 1);
        assert_eq!(wire.diagnostics[0].severity, "warning");
        assert_eq!(wire.diagnostics[0].code, "transaction_cleanup_deferred");
        assert!(wire_root.path().join("wire.md").is_file());
    }

    #[test]
    fn duplicate_batch_paths_are_rejected_before_any_mode_can_write_or_reserve() {
        for adjacent in [true, false] {
            for allow_partial in [true, false] {
                for dry_run in [true, false] {
                    let source = tempfile::tempdir().unwrap();
                    write(
                        &source.path().join("mdbase.yaml"),
                        "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
                    );
                    write(
                        &source.path().join("_types/item.md"),
                        "---\nname: item\nfields:\n  sequence: { type: integer, generated: sequence }\n---\n",
                    );
                    let original = "---\ntype: item\ntitle: Before\n---\n";
                    write(&source.path().join("same.md"), original);
                    write(&source.path().join("other.md"), "---\ntitle: Other\n---\n");
                    let collection = Collection::open(source.path()).unwrap();
                    let duplicate = json!({
                        "kind": "update",
                        "input": {"path": "same.md", "fields": {"title": "Would reserve"}}
                    });
                    let middle = json!({
                        "kind": "update",
                        "input": {"path": "other.md", "fields": {"title": "Changed"}}
                    });
                    let operations = if adjacent {
                        vec![duplicate.clone(), duplicate]
                    } else {
                        vec![duplicate.clone(), middle, duplicate]
                    };
                    let result = execute(
                        &collection,
                        &json!({
                            "operations": operations,
                            "allow_partial": allow_partial,
                            "dry_run": dry_run
                        }),
                    );
                    assert!(!result.valid);
                    assert_eq!(result.diagnostics[0].code, "duplicate_batch_path");
                    assert_eq!(result.diagnostics[0].path.as_deref(), Some("same.md"));
                    assert_eq!(
                        fs::read_to_string(source.path().join("same.md")).unwrap(),
                        original
                    );
                    assert!(fs::read_to_string(source.path().join("other.md"))
                        .unwrap()
                        .contains("Other"));
                }
            }
        }
    }

    #[test]
    fn preflight_shadow_copies_only_collection_scope_and_required_assets() {
        let source = tempfile::tempdir().unwrap();
        write(
            &source.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n  exclude: [.git/**, private/**]\nx-obsidian:\n  bases:\n    include: [views/*.base, private/*.base]\n",
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
        write(
            &source.path().join("views/configured.base"),
            "filters: []\n",
        );
        write(&source.path().join("unconfigured.base"), "filters: []\n");
        write(
            &source.path().join("private/excluded.base"),
            "filters: []\n",
        );

        let collection = Collection::open(source.path()).unwrap();
        let shadow = shadow_collection(&collection).unwrap();
        let root = &shadow.collection.root;
        assert!(root.join("mdbase.yaml").is_file());
        assert!(root.join("_types/note.md").is_file());
        assert!(root.join("visible.md").is_file());
        assert!(root.join("schema.json").is_file());
        assert!(root.join("views/configured.base").is_file());
        assert!(!root.join("unconfigured.base").exists());
        assert!(!root.join("private/excluded.base").exists());
        assert!(!root.join(".git").exists());
        assert!(!root.join("nested").exists());
    }

    #[test]
    fn wire_create_update_keep_committed_facts_after_postcommit_path_changes() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n");
        let collection = Collection::open(root.path()).unwrap();
        crate::mutation::reset_mutation_path_probes();
        crate::transactions::inject_post_commit_replacement(
            root.path(),
            "wire.md",
            Some(b"external replacement".to_vec()),
        );
        let created = Operations::new(&collection).unwrap().create(&json!({
            "path": "wire.md", "body": "planned", "include_document": true
        }));
        assert!(created.valid, "{:?}", created.diagnostics);
        assert_eq!(created.result["document"], "planned");
        assert_eq!(
            created.result["revision"],
            super::super::revision(b"planned")
        );
        assert_eq!(created.result["file"]["size"], 7);
        assert_ne!(created.result["file"]["mtime"], "");
        assert_eq!(
            fs::read(root.path().join("wire.md")).unwrap(),
            b"external replacement"
        );
        assert_eq!(
            crate::mutation::mutation_path_probes(),
            crate::mutation::service::MutationPathProbes {
                wire_request_decodes: 1,
                full_shadows: 1,
                ..Default::default()
            }
        );

        write(&root.path().join("wire.md"), "before");
        crate::transactions::inject_post_commit_replacement(root.path(), "wire.md", None);
        let updated = Operations::new(&collection).unwrap().update(&json!({
            "path": "wire.md", "document": "after"
        }));
        assert!(updated.valid, "{:?}", updated.diagnostics);
        assert_eq!(updated.result["document"], "after");
        assert_eq!(updated.result["revision"], super::super::revision(b"after"));
        assert_eq!(updated.result["file"]["size"], 5);
        assert_ne!(updated.result["file"]["mtime"], "");
        assert!(!root.path().join("wire.md").exists());
        assert_eq!(
            crate::mutation::mutation_path_probes(),
            crate::mutation::service::MutationPathProbes {
                wire_request_decodes: 2,
                full_shadows: 2,
                ..Default::default()
            }
        );
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
        let operations = Operations::new(&collection).unwrap();
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
