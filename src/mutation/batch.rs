//! Version-neutral typed batch coordination.
//!
//! A non-partial batch (and every dry run) is evaluated in one caller-owned
//! full shadow.  Each staged mutation writes that working set directly, so a
//! later item observes the exact bytes planned by earlier items.  Publication
//! is one recoverable `commit_shadow`.  Partial filesystem batches deliberately
//! use one independent shadow and commit per attempted item; this is why the
//! runtime adapter rejects partial batches, whose single host claim cannot
//! durably describe several independently committed transactions.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::api::{
    BatchDeletePreflightResult, BatchItemResult, BatchOperation, BatchOperationResult,
    BatchRenamePartialUpdates, BatchRenamePreflightResult, BatchRenameResult, BatchRequest,
    BatchResult, Diagnostic, DiagnosticCode, MdbaseError, OperationOutcome, Severity,
};
use crate::diagnostic::Diagnostic as CanonicalDiagnostic;
use crate::runtime::{CollectionSnapshot, OperationContext, ProviderError};
use crate::transactions::FileBaseline;
use crate::{Collection, SpecProfile};

use super::{PlannedDelete, PlannedRecord, PlannedRename, PreparationOptions};

/// Complete canonical result, including failures that prevented publication.
pub(crate) struct BatchExecution {
    pub(crate) result: BatchResult,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

/// One atomic batch plan retained for the filesystem runtime transaction.
pub(crate) struct RuntimeBatchPlan {
    pub(crate) execution: BatchExecution,
    pub(crate) baseline: FileBaseline,
    pub(crate) desired: FileBaseline,
    pub(crate) before: CollectionSnapshot,
    pub(crate) after: CollectionSnapshot,
}

pub(crate) enum RuntimeBatchPreparation {
    NoMutation(BatchExecution),
    Prepared(Box<RuntimeBatchPlan>),
}

/// Wire-only details which have no typed `BatchRequest` representation.
#[derive(Clone, Debug, Default)]
pub(crate) struct BatchWireOptions {
    pub(crate) create_documents: Vec<Option<String>>,
}

impl BatchWireOptions {
    fn typed(count: usize) -> Self {
        Self {
            create_documents: vec![None; count],
        }
    }
}

pub(crate) fn batch(
    collection: &Collection,
    request: BatchRequest,
) -> Result<OperationOutcome<BatchResult>, MdbaseError> {
    ensure_canonical(collection)?;
    validate_request(&request)?;
    if request.allow_partial && !request.dry_run {
        let options = BatchWireOptions::typed(request.operations.len());
        let execution = execute_partial(collection, request.operations, options);
        if execution.result.failed != 0 {
            return Err(MdbaseError::PartialBatch {
                result: execution.result,
                diagnostics: execution.diagnostics,
            });
        }
        return Ok(OperationOutcome {
            value: execution.result,
            diagnostics: execution.diagnostics,
        });
    }

    let options = BatchWireOptions::typed(request.operations.len());
    let execution = execute_atomic(
        collection,
        request.operations,
        options,
        request.dry_run,
        None,
    )?;
    if execution.result.failed != 0 {
        return Err(MdbaseError::Operation {
            diagnostics: execution.diagnostics,
        });
    }
    Ok(OperationOutcome {
        value: execution.result,
        diagnostics: execution.diagnostics,
    })
}

/// Coordinate a batch for a wire adapter while retaining the complete failed
/// aggregate instead of applying the typed API's error projection.
pub(crate) fn batch_wire(
    collection: &Collection,
    request: BatchRequest,
    options: BatchWireOptions,
) -> Result<BatchExecution, MdbaseError> {
    ensure_canonical(collection)?;
    validate_request(&request)?;
    if request.allow_partial && !request.dry_run {
        Ok(execute_partial(collection, request.operations, options))
    } else {
        execute_atomic(
            collection,
            request.operations,
            options,
            request.dry_run,
            None,
        )
    }
}

pub(crate) fn prepare_runtime_batch(
    collection: &Collection,
    request: BatchRequest,
    options: BatchWireOptions,
    context: &OperationContext,
) -> Result<RuntimeBatchPreparation, ProviderError> {
    context.check()?;
    if request.allow_partial {
        return Ok(RuntimeBatchPreparation::NoMutation(BatchExecution {
            result: BatchResult {
                operations: Vec::new(),
                succeeded: 0,
                failed: 0,
                preflight: true,
                dry_run: request.dry_run,
            },
            diagnostics: vec![diagnostic(
                "invalid_request",
                "allow_partial batches are not supported by the runtime: one host claim can settle only one atomic transaction",
                None,
            )],
        }));
    }
    if let Err(error) = validate_request(&request) {
        return Ok(RuntimeBatchPreparation::NoMutation(error_execution(
            error,
            request.dry_run,
        )));
    }
    let before = collection.snapshot_with_context(context)?;
    context.check()?;
    let shadow = super::shadow_collection_context(collection, context)?;
    let mut execution = execute_items(
        &shadow.collection,
        request.operations,
        options,
        request.dry_run,
        context,
    );
    if request.dry_run || execution.result.failed != 0 {
        execution.result.preflight = true;
        return Ok(RuntimeBatchPreparation::NoMutation(execution));
    }
    context.check()?;
    let desired = super::collect_collection_files_context(&shadow.collection, context)?;
    let after = shadow.collection.snapshot_with_context(context)?;
    execution.result.preflight = false;
    context.check()?;
    Ok(RuntimeBatchPreparation::Prepared(Box::new(
        RuntimeBatchPlan {
            execution,
            baseline: shadow.baseline,
            desired,
            before,
            after,
        },
    )))
}

fn execute_atomic(
    collection: &Collection,
    operations: Vec<BatchOperation>,
    options: BatchWireOptions,
    dry_run: bool,
    context: Option<&OperationContext>,
) -> Result<BatchExecution, MdbaseError> {
    let shadow = match context {
        Some(context) => super::shadow_collection_context(collection, context)
            .map_err(provider_error_as_mdbase)?,
        None => {
            super::shadow_collection(collection).map_err(|item| operation_error(vec![*item]))?
        }
    };
    let mut execution = match context {
        Some(context) => execute_items(&shadow.collection, operations, options, dry_run, context),
        None => execute_items_direct_context(&shadow.collection, operations, options, dry_run),
    };
    if dry_run || execution.result.failed != 0 {
        execution.result.preflight = true;
        return Ok(execution);
    }
    let desired = match context {
        Some(context) => super::collect_collection_files_context(&shadow.collection, context)
            .map_err(provider_error_as_mdbase)?,
        None => super::collect_collection_files(&shadow.collection)
            .map_err(|item| operation_error(vec![*item]))?,
    };
    let commit = crate::transactions::commit_shadow(collection, &shadow.baseline, &desired)
        .map_err(|error| {
            operation_error(vec![CanonicalDiagnostic::error(
                error.code(),
                error.to_string(),
                None,
            )])
        })?;
    attach_facts(&mut execution.result, &commit.file_facts);
    execution.result.preflight = false;
    if commit.cleanup_deferred {
        execution.diagnostics.push(warning(
            "transaction_cleanup_deferred",
            "The batch committed, but transaction cleanup was deferred.",
            None,
        ));
    }
    Ok(execution)
}

fn execute_partial(
    collection: &Collection,
    operations: Vec<BatchOperation>,
    options: BatchWireOptions,
) -> BatchExecution {
    let mut items = Vec::with_capacity(operations.len());
    let mut diagnostics = Vec::new();
    for (index, (operation, create_document)) in operations
        .into_iter()
        .zip(options.create_documents)
        .enumerate()
    {
        let (item, mut item_aggregate) =
            execute_partial_item(collection, index, operation, create_document);
        diagnostics.append(&mut item_aggregate);
        items.push(item);
    }
    aggregate_partial(items, diagnostics)
}

/// Execute and independently commit one typed partial-batch item.
///
/// Wire adapters use this same primitive after lazily decoding one raw item,
/// so malformed wire items cannot duplicate canonical mutation sequencing.
pub(crate) fn execute_partial_item(
    collection: &Collection,
    index: usize,
    operation: BatchOperation,
    create_document: Option<String>,
) -> (BatchItemResult, Vec<Diagnostic>) {
    let kind = kind(&operation).to_string();
    match execute_atomic(
        collection,
        vec![operation],
        BatchWireOptions {
            create_documents: vec![create_document],
        },
        false,
        None,
    ) {
        Ok(mut execution) => {
            let mut item = execution.result.operations.remove(0);
            item.index = index;
            (item, execution.diagnostics)
        }
        Err(error) => {
            let diagnostics = diagnostics_from_error(&error);
            (
                BatchItemResult {
                    index,
                    kind,
                    valid: false,
                    result: BatchOperationResult::default(),
                    diagnostics: diagnostics.clone(),
                },
                diagnostics,
            )
        }
    }
}

/// Aggregate already ordered partial item outcomes without executing mutations.
pub(crate) fn aggregate_partial(
    items: Vec<BatchItemResult>,
    diagnostics: Vec<Diagnostic>,
) -> BatchExecution {
    aggregate(items, diagnostics, false, false)
}

fn execute_items_direct_context(
    collection: &Collection,
    operations: Vec<BatchOperation>,
    options: BatchWireOptions,
    dry_run: bool,
) -> BatchExecution {
    execute_items(
        collection,
        operations,
        options,
        dry_run,
        &OperationContext::internal(),
    )
}

fn execute_items(
    collection: &Collection,
    operations: Vec<BatchOperation>,
    options: BatchWireOptions,
    dry_run: bool,
    context: &OperationContext,
) -> BatchExecution {
    let mut items = Vec::with_capacity(operations.len());
    let mut diagnostics = Vec::new();
    for (index, (operation, create_document)) in operations
        .into_iter()
        .zip(options.create_documents)
        .enumerate()
    {
        if let Err(error) = context.check() {
            let item_diagnostics = vec![diagnostic(error.code(), error.to_string(), None)];
            diagnostics.extend(item_diagnostics.iter().cloned());
            items.push(BatchItemResult {
                index,
                kind: kind(&operation).to_string(),
                valid: false,
                result: BatchOperationResult::default(),
                diagnostics: item_diagnostics,
            });
            break;
        }
        let kind_name = kind(&operation).to_string();
        let evaluated = evaluate_item(collection, operation, create_document, dry_run);
        let item = match evaluated {
            Ok((valid, result, item_diagnostics)) => {
                diagnostics.extend(item_diagnostics.iter().cloned());
                BatchItemResult {
                    index,
                    kind: kind_name,
                    valid,
                    result,
                    diagnostics: item_diagnostics,
                }
            }
            Err(error) => {
                let item_diagnostics = diagnostics_from_error(&error);
                diagnostics.extend(item_diagnostics.iter().cloned());
                BatchItemResult {
                    index,
                    kind: kind_name,
                    valid: false,
                    result: BatchOperationResult::default(),
                    diagnostics: item_diagnostics,
                }
            }
        };
        let failed = !item.valid;
        items.push(item);
        if failed {
            break;
        }
    }
    aggregate(items, diagnostics, true, dry_run)
}

fn evaluate_item(
    collection: &Collection,
    operation: BatchOperation,
    create_document: Option<String>,
    dry_run: bool,
) -> Result<(bool, BatchOperationResult, Vec<Diagnostic>), MdbaseError> {
    let options = PreparationOptions {
        create_document,
        dry_run,
    };
    match operation {
        BatchOperation::Create(request) => record_result(
            collection,
            super::staged_create(collection, request, options)?,
        ),
        BatchOperation::Update(request) => record_result(
            collection,
            super::staged_update(collection, request, options)?,
        ),
        BatchOperation::Delete(request) => {
            delete_result(super::staged_delete(collection, request, options)?)
        }
        BatchOperation::Rename(request) => rename_result(
            collection,
            super::staged_rename(collection, request, options)?,
        ),
    }
}

pub(crate) fn record_result(
    collection: &Collection,
    planned: PlannedRecord,
) -> Result<(bool, BatchOperationResult, Vec<Diagnostic>), MdbaseError> {
    let metadata = std::fs::metadata(planned.path.under(&collection.root)).map_err(|error| {
        MdbaseError::InvalidResult {
            message: error.to_string(),
        }
    })?;
    let outcome = super::project_record(collection, planned, metadata)?;
    Ok((
        true,
        BatchOperationResult::Record(outcome.value),
        outcome.diagnostics,
    ))
}

pub(crate) fn delete_result(
    planned: PlannedDelete,
) -> Result<(bool, BatchOperationResult, Vec<Diagnostic>), MdbaseError> {
    let result = if planned.deleted {
        BatchOperationResult::Delete(planned.result())
    } else {
        BatchOperationResult::DeletePreflight(BatchDeletePreflightResult {
            path: planned.path,
            deleted: false,
            dry_run: true,
            would_delete: true,
            broken_links: crate::api::reference_evidence(planned.broken_links),
        })
    };
    Ok((true, result, Vec::new()))
}

pub(crate) fn rename_result(
    collection: &Collection,
    planned: PlannedRename,
) -> Result<(bool, BatchOperationResult, Vec<Diagnostic>), MdbaseError> {
    let mut diagnostics = planned
        .warnings
        .iter()
        .map(|value| value_diagnostic(value, Severity::Warning, "rename_warning"))
        .collect::<Vec<_>>();
    diagnostics.extend(planned.reference_failures.iter().map(|value| {
        value_diagnostic(
            value,
            Severity::Error,
            crate::errors::RENAME_REF_UPDATE_FAILED,
        )
    }));
    let valid = !diagnostics
        .iter()
        .any(|item| item.severity == Severity::Error);
    if planned.dry_run {
        let partial_updates =
            (!planned.reference_failures.is_empty()).then_some(BatchRenamePartialUpdates {
                failed: crate::api::reference_evidence(planned.reference_failures),
            });
        return Ok((
            valid,
            BatchOperationResult::RenamePreflight(BatchRenamePreflightResult {
                from: planned.from,
                to: planned.to,
                dry_run: true,
                would_rename: true,
                references_affected: crate::api::reference_evidence(planned.references_affected),
                partial_updates,
            }),
            diagnostics,
        ));
    }
    let outcome = super::service::project_planned_record(collection, planned.destination.clone())?;
    let partial_updates =
        (!planned.reference_failures.is_empty()).then_some(BatchRenamePartialUpdates {
            failed: crate::api::reference_evidence(planned.reference_failures.clone()),
        });
    let result = planned.result(outcome.value);
    Ok((
        valid,
        BatchOperationResult::Rename(BatchRenameResult {
            result,
            partial_updates,
        }),
        diagnostics,
    ))
}

fn aggregate(
    operations: Vec<BatchItemResult>,
    diagnostics: Vec<Diagnostic>,
    preflight: bool,
    dry_run: bool,
) -> BatchExecution {
    let succeeded = operations.iter().filter(|item| item.valid).count();
    let failed = operations.len() - succeeded;
    BatchExecution {
        result: BatchResult {
            operations,
            succeeded,
            failed,
            preflight,
            dry_run,
        },
        diagnostics,
    }
}

fn validate_request(request: &BatchRequest) -> Result<(), MdbaseError> {
    if request.operations.is_empty() {
        return Err(MdbaseError::InvalidRequest {
            message: "batch operations must not be empty".to_string(),
        });
    }
    let mut paths = BTreeSet::new();
    for operation in &request.operations {
        let candidates: Vec<&crate::api::CollectionPath> = match operation {
            BatchOperation::Create(request) => request.path.iter().collect(),
            BatchOperation::Update(request) => vec![&request.path],
            BatchOperation::Delete(request) => vec![&request.path],
            BatchOperation::Rename(request) => vec![&request.from, &request.to],
        };
        for path in candidates {
            if !paths.insert(path.as_str()) {
                return Err(MdbaseError::Operation {
                    diagnostics: vec![diagnostic(
                        "duplicate_batch_path",
                        format!("Batch path '{}' is used more than once.", path.as_str()),
                        Some(path.to_string()),
                    )],
                });
            }
        }
    }
    Ok(())
}

fn attach_facts(
    result: &mut BatchResult,
    facts: &std::collections::BTreeMap<String, crate::transactions::CommittedFileFacts>,
) {
    for item in &mut result.operations {
        let record = match &mut item.result {
            BatchOperationResult::Record(record) => Some(record),
            BatchOperationResult::Rename(rename) => Some(&mut rename.result.document),
            _ => None,
        };
        if let Some(record) = record {
            if let Some(committed) = facts.get(record.path.as_str()) {
                committed.attach_record_file(&mut record.file);
            }
        }
    }
}

fn ensure_canonical(collection: &Collection) -> Result<(), MdbaseError> {
    if collection.spec_profile == SpecProfile::V03 {
        Ok(())
    } else {
        Err(MdbaseError::MigrationRequired { operation: "batch" })
    }
}

fn error_execution(error: MdbaseError, dry_run: bool) -> BatchExecution {
    BatchExecution {
        result: BatchResult {
            operations: Vec::new(),
            succeeded: 0,
            failed: 0,
            preflight: true,
            dry_run,
        },
        diagnostics: diagnostics_from_error(&error),
    }
}

fn diagnostics_from_error(error: &MdbaseError) -> Vec<Diagnostic> {
    if !error.diagnostics().is_empty() {
        return error.diagnostics().to_vec();
    }
    vec![diagnostic("invalid_request", error.to_string(), None)]
}

fn diagnostic(
    code: impl Into<String>,
    message: impl Into<String>,
    path: Option<String>,
) -> Diagnostic {
    Diagnostic {
        severity: Severity::Error,
        code: DiagnosticCode::new(code),
        message: message.into(),
        path,
        field: None,
        type_name: None,
        schema_location: None,
        details: None,
    }
}

fn warning(
    code: impl Into<String>,
    message: impl Into<String>,
    path: Option<String>,
) -> Diagnostic {
    Diagnostic {
        severity: Severity::Warning,
        code: DiagnosticCode::new(code),
        message: message.into(),
        path,
        field: None,
        type_name: None,
        schema_location: None,
        details: None,
    }
}

fn value_diagnostic(value: &Value, severity: Severity, fallback: &str) -> Diagnostic {
    Diagnostic {
        severity,
        code: DiagnosticCode::new(
            value
                .get("code")
                .or_else(|| value.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or(fallback),
        ),
        message: value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(if severity == Severity::Error {
                "Some reference updates failed"
            } else {
                "Rename warning"
            })
            .to_string(),
        path: value
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string),
        field: value
            .get("field")
            .and_then(Value::as_str)
            .map(str::to_string),
        type_name: None,
        schema_location: None,
        details: Some(value.clone()),
    }
}

fn kind(operation: &BatchOperation) -> &'static str {
    match operation {
        BatchOperation::Create(_) => "create",
        BatchOperation::Update(_) => "update",
        BatchOperation::Delete(_) => "delete",
        BatchOperation::Rename(_) => "rename",
    }
}

fn operation_error(diagnostics: Vec<CanonicalDiagnostic>) -> MdbaseError {
    MdbaseError::Operation {
        diagnostics: diagnostics.into_iter().map(Into::into).collect(),
    }
}

fn provider_error_as_mdbase(error: ProviderError) -> MdbaseError {
    MdbaseError::Operation {
        diagnostics: vec![diagnostic(error.code(), error.to_string(), None)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{CollectionPath, CreateRequest, DeleteRequest, RenameRequest, UpdateRequest};

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        root
    }

    fn create(path: &str, body: &str) -> BatchOperation {
        BatchOperation::Create(
            CreateRequest::new(CollectionPath::new(path).unwrap()).with_body(body),
        )
    }

    #[test]
    fn typed_atomic_and_dry_run_use_one_working_shadow() {
        for dry_run in [false, true] {
            let root = fixture();
            let collection = Collection::open(root.path()).unwrap();
            crate::mutation::reset_mutation_path_probes();
            let request = BatchRequest {
                operations: vec![create("one.md", "one"), create("two.md", "two")],
                allow_partial: false,
                dry_run,
            };
            let outcome = batch(&collection, request).unwrap();
            assert_eq!(outcome.value.succeeded, 2);
            assert_eq!(outcome.value.preflight, dry_run);
            assert_eq!(outcome.value.dry_run, dry_run);
            assert_eq!(
                crate::mutation::mutation_path_probes(),
                crate::mutation::MutationPathProbes {
                    full_shadows: 1,
                    ..Default::default()
                }
            );
            assert_eq!(root.path().join("one.md").exists(), !dry_run);
        }
    }

    #[test]
    fn typed_partial_error_retains_committed_successes_and_attempts_one_shadow_per_item() {
        let root = fixture();
        let collection = Collection::open(root.path()).unwrap();
        crate::mutation::reset_mutation_path_probes();
        let request = BatchRequest {
            operations: vec![
                create("one.md", "one"),
                BatchOperation::Update(UpdateRequest::new(
                    CollectionPath::new("missing.md").unwrap(),
                    serde_json::json!({"title": "missing"}),
                )),
                create("three.md", "three"),
            ],
            allow_partial: true,
            dry_run: false,
        };
        let error = batch(&collection, request).unwrap_err();
        let MdbaseError::PartialBatch {
            result,
            diagnostics,
        } = error
        else {
            panic!("expected inspectable partial batch error")
        };
        assert_eq!((result.succeeded, result.failed), (2, 1));
        assert_eq!(result.operations.len(), 3);
        assert!(result.operations[0].valid);
        assert!(!result.operations[1].valid);
        assert!(result.operations[2].valid);
        assert!(!diagnostics.is_empty());
        assert!(root.path().join("one.md").is_file());
        assert!(root.path().join("three.md").is_file());
        assert_eq!(
            crate::mutation::mutation_path_probes(),
            crate::mutation::MutationPathProbes {
                full_shadows: 3,
                ..Default::default()
            }
        );
    }

    #[test]
    fn all_mutation_kinds_obey_atomic_dry_run_and_partial_failure_boundaries() {
        fn prepared_fixture() -> tempfile::TempDir {
            let root = fixture();
            std::fs::write(root.path().join("update.md"), "---\ntitle: Before\n---\n").unwrap();
            std::fs::write(root.path().join("rename.md"), "rename\n").unwrap();
            std::fs::write(root.path().join("delete.md"), "delete\n").unwrap();
            root
        }

        fn operations() -> Vec<BatchOperation> {
            vec![
                create("create.md", "created"),
                BatchOperation::Update(UpdateRequest::new(
                    CollectionPath::new("update.md").unwrap(),
                    serde_json::json!({"title": "After"}),
                )),
                BatchOperation::Rename(RenameRequest::new(
                    CollectionPath::new("rename.md").unwrap(),
                    CollectionPath::new("renamed.md").unwrap(),
                )),
                BatchOperation::Delete(DeleteRequest::new(
                    CollectionPath::new("missing.md").unwrap(),
                )),
                BatchOperation::Delete(DeleteRequest::new(
                    CollectionPath::new("delete.md").unwrap(),
                )),
            ]
        }

        for dry_run in [false, true] {
            let root = prepared_fixture();
            let collection = Collection::open(root.path()).unwrap();
            let error = batch(
                &collection,
                BatchRequest {
                    operations: operations(),
                    allow_partial: false,
                    dry_run,
                },
            )
            .unwrap_err();
            assert!(matches!(error, MdbaseError::Operation { .. }));
            assert!(!root.path().join("create.md").exists());
            assert!(root.path().join("rename.md").is_file());
            assert!(!root.path().join("renamed.md").exists());
            assert!(root.path().join("delete.md").is_file());
            assert!(std::fs::read_to_string(root.path().join("update.md"))
                .unwrap()
                .contains("Before"));
        }

        let root = prepared_fixture();
        let collection = Collection::open(root.path()).unwrap();
        let error = batch(
            &collection,
            BatchRequest {
                operations: operations(),
                allow_partial: true,
                dry_run: false,
            },
        )
        .unwrap_err();
        let MdbaseError::PartialBatch { result, .. } = error else {
            panic!("expected partial aggregate")
        };
        assert_eq!((result.succeeded, result.failed), (4, 1));
        assert_eq!(result.operations.len(), 5);
        assert!(root.path().join("create.md").is_file());
        assert!(!root.path().join("rename.md").exists());
        assert!(root.path().join("renamed.md").is_file());
        assert!(!root.path().join("delete.md").exists());
        assert!(std::fs::read_to_string(root.path().join("update.md"))
            .unwrap()
            .contains("After"));
    }

    #[test]
    fn atomic_batch_generated_sequences_evolve_across_the_single_working_set() {
        let root = fixture();
        std::fs::create_dir(root.path().join("_types")).unwrap();
        std::fs::write(
            root.path().join("_types/item.md"),
            "---\nname: item\nfields:\n  sequence: { type: integer, generated: sequence }\n---\n",
        )
        .unwrap();
        let collection = Collection::open(root.path()).unwrap();
        let request = BatchRequest::new(vec![
            BatchOperation::Create(
                CreateRequest::new(CollectionPath::new("one.md").unwrap())
                    .with_frontmatter(serde_json::json!({"type": "item"})),
            ),
            BatchOperation::Create(
                CreateRequest::new(CollectionPath::new("two.md").unwrap())
                    .with_frontmatter(serde_json::json!({"type": "item"})),
            ),
        ])
        .unwrap();
        let result = batch(&collection, request).unwrap();
        let BatchOperationResult::Record(one) = &result.value.operations[0].result else {
            panic!("create must return a typed record")
        };
        let BatchOperationResult::Record(two) = &result.value.operations[1].result else {
            panic!("create must return a typed record")
        };
        assert_eq!(one.frontmatter["sequence"], 1);
        assert_eq!(two.frontmatter["sequence"], 2);
    }

    #[test]
    fn runtime_atomic_uses_one_shadow_and_partial_is_rejected_without_one() {
        let root = fixture();
        let collection = Collection::open(root.path()).unwrap();
        let context = OperationContext::legacy();

        crate::mutation::reset_mutation_path_probes();
        let partial = BatchRequest {
            operations: vec![create("partial.md", "no")],
            allow_partial: true,
            dry_run: false,
        };
        let RuntimeBatchPreparation::NoMutation(rejected) =
            prepare_runtime_batch(&collection, partial, BatchWireOptions::typed(1), &context)
                .unwrap()
        else {
            panic!("runtime partial batch must be rejected")
        };
        assert_eq!(rejected.diagnostics[0].code.as_str(), "invalid_request");
        assert_eq!(
            crate::mutation::mutation_path_probes(),
            crate::mutation::MutationPathProbes::default()
        );
        assert!(!root.path().join("partial.md").exists());

        crate::mutation::reset_mutation_path_probes();
        let atomic = BatchRequest {
            operations: vec![create("one.md", "one"), create("two.md", "two")],
            allow_partial: false,
            dry_run: false,
        };
        let RuntimeBatchPreparation::Prepared(plan) =
            prepare_runtime_batch(&collection, atomic, BatchWireOptions::typed(2), &context)
                .unwrap()
        else {
            panic!("runtime atomic batch must prepare one transaction")
        };
        assert_eq!(plan.execution.result.succeeded, 2);
        assert!(!plan.execution.result.preflight);
        assert_eq!(
            crate::mutation::mutation_path_probes(),
            crate::mutation::MutationPathProbes {
                full_shadows: 1,
                ..Default::default()
            }
        );
        assert!(!root.path().join("one.md").exists());
    }
}
