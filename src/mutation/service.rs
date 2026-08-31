use crate::api::{
    CreateRequest, DeletePreflightResult, DeleteRequest, DeleteResult, MdbaseError,
    OperationOutcome, RecordDocument, RenamePreflightResult, RenameRequest, RenameResult,
    UpdateRequest,
};
use crate::diagnostic::Diagnostic as CanonicalDiagnostic;
use crate::{Collection, SpecProfile};

use super::{PlannedDelete, PlannedRecord, PlannedRename, PreparationOptions};

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MutationPathProbes {
    pub request_value_constructions: usize,
    pub legacy_request_parses: usize,
    pub wire_request_decodes: usize,
    pub runtime_request_decodes: usize,
    pub full_shadows: usize,
    pub sparse_shadows: usize,
}

#[cfg(test)]
thread_local! { static PROBES: std::cell::Cell<MutationPathProbes> = const { std::cell::Cell::new(MutationPathProbes { request_value_constructions: 0, legacy_request_parses: 0, wire_request_decodes: 0, runtime_request_decodes: 0, full_shadows: 0, sparse_shadows: 0 }) }; }

#[cfg(test)]
pub(crate) fn reset_mutation_path_probes() {
    PROBES.with(|value| value.set(MutationPathProbes::default()));
}
#[cfg(test)]
pub(crate) fn mutation_path_probes() -> MutationPathProbes {
    PROBES.with(std::cell::Cell::get)
}
#[cfg(test)]
fn increment(update: impl FnOnce(&mut MutationPathProbes)) {
    PROBES.with(|cell| {
        let mut value = cell.get();
        update(&mut value);
        cell.set(value);
    });
}

#[cfg(test)]
pub(crate) fn probe_request_value() {
    increment(|value| value.request_value_constructions += 1);
}
#[cfg(all(test, feature = "legacy-collection-mutation"))]
pub(crate) fn probe_legacy_parse() {
    increment(|value| value.legacy_request_parses += 1);
}
#[cfg(test)]
pub(crate) fn probe_wire_decode() {
    increment(|value| value.wire_request_decodes += 1);
}
#[cfg(test)]
pub(crate) fn probe_runtime_decode() {
    increment(|value| value.runtime_request_decodes += 1);
}
#[cfg(test)]
pub(crate) fn probe_full_shadow() {
    increment(|value| value.full_shadows += 1);
}
#[cfg(test)]
pub(crate) fn probe_sparse_shadow() {
    increment(|value| value.sparse_shadows += 1);
}

pub(crate) fn create(
    collection: &Collection,
    request: CreateRequest,
) -> Result<OperationOutcome<RecordDocument>, MdbaseError> {
    execute_shadow(collection, false, |shadow| {
        staged_create(shadow, request, PreparationOptions::default())
    })
}

pub(crate) fn update(
    collection: &Collection,
    request: UpdateRequest,
) -> Result<OperationOutcome<RecordDocument>, MdbaseError> {
    validate_update_shape(&request)?;
    execute_shadow(collection, false, |shadow| {
        staged_update(shadow, request, PreparationOptions::default())
    })
}

pub(crate) fn delete(
    collection: &Collection,
    request: DeleteRequest,
) -> Result<OperationOutcome<DeleteResult>, MdbaseError> {
    let planned = execute_delete_shadow(collection, request, false)?;
    Ok(OperationOutcome {
        value: planned.result(),
        diagnostics: Vec::new(),
    })
}

pub(crate) fn preflight_delete(
    collection: &Collection,
    request: DeleteRequest,
) -> Result<OperationOutcome<DeletePreflightResult>, MdbaseError> {
    let planned = execute_delete_shadow(collection, request, true)?;
    Ok(OperationOutcome {
        value: planned.preflight_result(),
        diagnostics: Vec::new(),
    })
}

pub(crate) fn rename(
    collection: &Collection,
    request: RenameRequest,
) -> Result<OperationOutcome<RenameResult>, MdbaseError> {
    let (planned, mut document) = execute_rename_shadow(collection, request, false)?;
    let diagnostics = rename_diagnostics(&planned);
    if diagnostics.iter().any(|item| item.severity == "error") {
        return Err(canonical_error(diagnostics));
    }
    document
        .diagnostics
        .extend(diagnostics.into_iter().map(Into::into));
    Ok(OperationOutcome {
        value: planned.result(document.value),
        diagnostics: document.diagnostics,
    })
}

pub(crate) fn preflight_rename(
    collection: &Collection,
    request: RenameRequest,
) -> Result<OperationOutcome<RenamePreflightResult>, MdbaseError> {
    let (planned, _) = execute_rename_shadow(collection, request, true)?;
    let diagnostics = rename_diagnostics(&planned);
    if diagnostics.iter().any(|item| item.severity == "error") {
        return Err(canonical_error(diagnostics));
    }
    Ok(OperationOutcome {
        value: planned.preflight_result(),
        diagnostics: diagnostics.into_iter().map(Into::into).collect(),
    })
}

pub(crate) fn staged_create(
    collection: &Collection,
    request: CreateRequest,
    options: PreparationOptions,
) -> Result<PlannedRecord, MdbaseError> {
    let _dry_run = options.dry_run;
    let prepared = super::prepare_create(collection, request, options).map_err(canonical_error)?;
    collection
        .create_planned(prepared)
        .map_err(|failure| canonical_error(failure.diagnostics))
}

pub(crate) fn staged_update(
    collection: &Collection,
    request: UpdateRequest,
    options: PreparationOptions,
) -> Result<PlannedRecord, MdbaseError> {
    validate_update_shape(&request)?;
    let _dry_run = options.dry_run;
    let prepared = super::prepare_update(collection, request, options).map_err(canonical_error)?;
    collection
        .update_planned(prepared)
        .map_err(|failure| canonical_error(failure.diagnostics))
}

pub(crate) fn staged_delete(
    collection: &Collection,
    request: DeleteRequest,
    options: PreparationOptions,
) -> Result<PlannedDelete, MdbaseError> {
    plan_delete(collection, collection, request, options)
}

pub(crate) fn staged_rename(
    collection: &Collection,
    request: RenameRequest,
    options: PreparationOptions,
) -> Result<PlannedRename, MdbaseError> {
    plan_rename(collection, request, options, None)
}

pub(crate) fn plan_rename(
    collection: &Collection,
    request: RenameRequest,
    options: PreparationOptions,
    last_known_mtime: Option<u64>,
) -> Result<PlannedRename, MdbaseError> {
    let prepared = super::prepare_rename(
        collection,
        request,
        options,
        last_known_mtime,
        std::collections::HashMap::new(),
        Vec::new(),
    )
    .map_err(canonical_error)?;
    collection
        .rename_planned(prepared)
        .map_err(|failure| canonical_error(failure.diagnostics))
}

/// Capture deletion evidence from the authority and apply that exact plan to a
/// caller-owned working set. A sparse working set therefore cannot substitute
/// stale bytes or incomplete backlink/type evidence.
pub(crate) fn plan_delete(
    authority: &Collection,
    working_set: &Collection,
    request: DeleteRequest,
    options: PreparationOptions,
) -> Result<PlannedDelete, MdbaseError> {
    let prepared =
        super::prepare_delete(authority, request, options, None).map_err(canonical_error)?;
    working_set
        .delete_planned(prepared)
        .map_err(|failure| canonical_error(failure.diagnostics))
}

fn execute_delete_shadow(
    collection: &Collection,
    request: DeleteRequest,
    dry_run: bool,
) -> Result<PlannedDelete, MdbaseError> {
    if collection.spec_profile != SpecProfile::V03 {
        return Err(MdbaseError::MigrationRequired {
            operation: "mutation",
        });
    }
    let shadow = super::shadow_collection(collection)
        .map_err(|diagnostic| canonical_error(vec![*diagnostic]))?;
    let planned = staged_delete(
        &shadow.collection,
        request,
        PreparationOptions {
            create_document: None,
            dry_run,
        },
    )?;
    if dry_run {
        return Ok(planned);
    }
    let desired = super::collect_collection_files(&shadow.collection)
        .map_err(|diagnostic| canonical_error(vec![*diagnostic]))?;
    crate::transactions::commit_shadow(collection, &shadow.baseline, &desired).map_err(
        |error| {
            canonical_error(vec![CanonicalDiagnostic::error(
                error.code(),
                error.to_string(),
                Some(planned.path.to_string()),
            )])
        },
    )?;
    Ok(planned)
}

fn execute_rename_shadow(
    collection: &Collection,
    request: RenameRequest,
    dry_run: bool,
) -> Result<(PlannedRename, OperationOutcome<RecordDocument>), MdbaseError> {
    if collection.spec_profile != SpecProfile::V03 {
        return Err(MdbaseError::MigrationRequired {
            operation: "rename",
        });
    }
    let shadow = super::shadow_collection(collection)
        .map_err(|diagnostic| canonical_error(vec![*diagnostic]))?;
    let planned = staged_rename(
        &shadow.collection,
        request,
        PreparationOptions {
            create_document: None,
            dry_run,
        },
    )?;
    let destination = planned.destination.clone();
    let mut projected = project_record_facts(
        &shadow.collection,
        destination,
        crate::transactions::CommittedFileFacts {
            size: planned.destination.bytes.len() as u64,
            mtime: None,
        },
    )?;
    if dry_run {
        return Ok((planned, projected));
    }
    let desired = super::collect_collection_files(&shadow.collection)
        .map_err(|diagnostic| canonical_error(vec![*diagnostic]))?;
    let commit = crate::transactions::commit_shadow(collection, &shadow.baseline, &desired)
        .map_err(|error| {
            canonical_error(vec![CanonicalDiagnostic::error(
                error.code(),
                error.to_string(),
                Some(planned.to.to_string()),
            )])
        })?;
    let facts = commit
        .file_facts
        .get(planned.to.as_str())
        .expect("a committed rename destination has locked file facts");
    facts.attach_record_file(&mut projected.value.file);
    Ok((planned, projected))
}

pub(crate) fn project_planned_record(
    collection: &Collection,
    record: PlannedRecord,
) -> Result<OperationOutcome<RecordDocument>, MdbaseError> {
    let size = record.bytes.len() as u64;
    project_record_facts(
        collection,
        record,
        crate::transactions::CommittedFileFacts { size, mtime: None },
    )
}

fn rename_diagnostics(planned: &PlannedRename) -> Vec<CanonicalDiagnostic> {
    let mut diagnostics = planned
        .warnings
        .iter()
        .map(|warning| value_diagnostic(warning, "warning", "rename_warning"))
        .collect::<Vec<_>>();
    diagnostics.extend(planned.reference_failures.iter().map(|failure| {
        let mut diagnostic =
            value_diagnostic(failure, "error", crate::errors::RENAME_REF_UPDATE_FAILED);
        diagnostic.code = crate::errors::RENAME_REF_UPDATE_FAILED.to_string();
        diagnostic
    }));
    diagnostics
}

fn value_diagnostic(
    value: &serde_json::Value,
    severity: &str,
    fallback_code: &str,
) -> CanonicalDiagnostic {
    CanonicalDiagnostic {
        severity: severity.to_string(),
        code: value
            .get("code")
            .or_else(|| value.get("reason"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(fallback_code)
            .to_string(),
        message: value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(if severity == "error" {
                "Some reference updates failed"
            } else {
                "Rename warning"
            })
            .to_string(),
        path: value
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        field: value
            .get("field")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        type_name: None,
        schema_location: None,
        details: Some(value.clone()),
    }
}

fn execute_shadow(
    collection: &Collection,
    dry_run: bool,
    operation: impl FnOnce(&Collection) -> Result<PlannedRecord, MdbaseError>,
) -> Result<OperationOutcome<RecordDocument>, MdbaseError> {
    if collection.spec_profile != SpecProfile::V03 {
        return Err(MdbaseError::MigrationRequired {
            operation: "mutation",
        });
    }
    let shadow = super::shadow_collection(collection)
        .map_err(|diagnostic| canonical_error(vec![*diagnostic]))?;
    let planned = operation(&shadow.collection)?;
    let _before_revision = planned.before_revision.as_deref();
    let planned_path = planned.path.clone();
    let shadow_metadata = shadow
        .collection
        .held_root()
        .metadata(&planned_path.to_path_buf())
        .map_err(invalid_result)?;
    let mut outcome = project_record(&shadow.collection, planned, shadow_metadata)?;
    if dry_run {
        return Ok(outcome);
    }
    let desired = super::collect_collection_files(&shadow.collection)
        .map_err(|diagnostic| canonical_error(vec![*diagnostic]))?;
    let commit = crate::transactions::commit_shadow(collection, &shadow.baseline, &desired)
        .map_err(|error| {
            canonical_error(vec![CanonicalDiagnostic::error(
                error.code(),
                error.to_string(),
                Some(planned_path.to_string()),
            )])
        })?;
    // Facts are bound to the committed entry while the transaction lock is held.
    let facts = commit
        .file_facts
        .get(planned_path.as_str())
        .expect("a committed planned record always has locked file facts");
    facts.attach_record_file(&mut outcome.value.file);
    Ok(outcome)
}

fn validate_update_shape(request: &UpdateRequest) -> Result<(), MdbaseError> {
    if request.document.is_some()
        && (request.body.is_some()
            || request
                .patch
                .as_object()
                .is_none_or(|patch| !patch.is_empty()))
    {
        return Err(MdbaseError::InvalidRequest {
            message: "document cannot be combined with patch or body".to_string(),
        });
    }
    Ok(())
}

pub(crate) fn project_record(
    collection: &Collection,
    planned: PlannedRecord,
    metadata: crate::record_load::FileMetadata,
) -> Result<OperationOutcome<RecordDocument>, MdbaseError> {
    let mtime = metadata.modified().ok().map(|time| {
        let value: chrono::DateTime<chrono::Utc> = time.into();
        value.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    });
    project_record_facts(
        collection,
        planned,
        crate::transactions::CommittedFileFacts {
            size: metadata.len(),
            mtime,
        },
    )
}

fn project_record_facts(
    collection: &Collection,
    planned: PlannedRecord,
    facts: crate::transactions::CommittedFileFacts,
) -> Result<OperationOutcome<RecordDocument>, MdbaseError> {
    let source = String::from_utf8(planned.bytes.clone()).map_err(invalid_result)?;
    let request = crate::api::ReadRequest {
        path: planned.path.clone(),
        include_document: planned.include_document,
    };
    let read_facts = crate::operations::read::RecordFileFacts {
        size: facts.size,
        mtime: facts.mtime,
    };
    let evaluation = crate::operations::read::evaluate_typed_read(
        collection,
        &request,
        crate::operations::read::TypedReadSource::Exact {
            canonical_path: planned.path.as_str(),
            document: &source,
            file_facts: &read_facts,
        },
    );
    match evaluation.into_outcome() {
        Ok(mut outcome) => {
            outcome
                .diagnostics
                .extend(planned.diagnostics.into_iter().map(Into::into));
            Ok(outcome)
        }
        Err(error)
            if error
                .diagnostics()
                .iter()
                .any(|item| item.code.as_str() == crate::errors::INVALID_FRONTMATTER) =>
        {
            opaque_projection(planned, source, read_facts)
        }
        Err(error) => Err(error),
    }
}

fn opaque_projection(
    planned: PlannedRecord,
    source: String,
    facts: crate::operations::read::RecordFileFacts,
) -> Result<OperationOutcome<RecordDocument>, MdbaseError> {
    let name = std::path::Path::new(planned.path.as_str())
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string();
    let folder = std::path::Path::new(planned.path.as_str())
        .parent()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string();
    let revision = crate::api::Revision::parse(planned.after_revision())?;
    Ok(OperationOutcome {
        value: RecordDocument {
            path: planned.path,
            revision,
            types: planned.types,
            frontmatter: planned.frontmatter,
            effective_frontmatter: planned.effective_frontmatter,
            body: planned.body,
            document: planned.include_document.then_some(source),
            file: crate::api::RecordFile {
                name,
                folder,
                size: facts.size,
                mtime: facts.mtime.unwrap_or_default(),
            },
        },
        diagnostics: planned.diagnostics.into_iter().map(Into::into).collect(),
    })
}

fn canonical_error(diagnostics: Vec<CanonicalDiagnostic>) -> MdbaseError {
    MdbaseError::Operation {
        diagnostics: diagnostics.into_iter().map(Into::into).collect(),
    }
}
fn invalid_result(error: impl std::fmt::Display) -> MdbaseError {
    MdbaseError::InvalidResult {
        message: error.to_string(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn revision(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn real_typed_create_update_use_one_shadow_and_no_bridges_each() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  default_validation: warn\n",
        )
        .unwrap();
        let collection = Collection::open(root.path()).unwrap();
        reset_mutation_path_probes();
        let create =
            crate::api::CreateRequest::new(crate::api::CollectionPath::new("one.md").unwrap())
                .with_body("Body");
        collection.typed().unwrap().create(create).unwrap();
        assert_eq!(
            mutation_path_probes(),
            MutationPathProbes {
                full_shadows: 1,
                ..MutationPathProbes::default()
            }
        );
        let update = crate::api::UpdateRequest::new(
            crate::api::CollectionPath::new("one.md").unwrap(),
            serde_json::json!({"title": "One"}),
        );
        collection.typed().unwrap().update(update).unwrap();
        assert_eq!(
            mutation_path_probes(),
            MutationPathProbes {
                full_shadows: 2,
                ..MutationPathProbes::default()
            }
        );
    }

    #[test]
    fn typed_delete_and_preflight_use_one_full_shadow_without_bridges() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        std::fs::write(root.path().join("target.md"), "target\n").unwrap();
        let collection = Collection::open(root.path()).unwrap();
        let request =
            crate::api::DeleteRequest::new(crate::api::CollectionPath::new("target.md").unwrap());

        reset_mutation_path_probes();
        let preview = collection
            .typed()
            .unwrap()
            .preflight_delete(request.clone())
            .unwrap();
        assert!(preview.value.would_delete);
        assert!(root.path().join("target.md").exists());
        assert_eq!(
            mutation_path_probes(),
            MutationPathProbes {
                full_shadows: 1,
                ..MutationPathProbes::default()
            }
        );

        reset_mutation_path_probes();
        crate::transactions::inject_post_commit_replacement(
            root.path(),
            "target.md",
            Some(b"external replacement".to_vec()),
        );
        let deleted = collection.typed().unwrap().delete(request).unwrap();
        assert!(deleted.value.deleted);
        assert_eq!(
            std::fs::read(root.path().join("target.md")).unwrap(),
            b"external replacement"
        );
        assert_eq!(
            mutation_path_probes(),
            MutationPathProbes {
                full_shadows: 1,
                ..MutationPathProbes::default()
            }
        );
    }

    #[test]
    fn typed_rename_and_preflight_use_one_full_shadow_without_bridges() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        std::fs::write(root.path().join("target.md"), "target\n").unwrap();
        std::fs::write(root.path().join("ref.md"), "See [[target]].\n").unwrap();
        let collection = Collection::open(root.path()).unwrap();
        let request = crate::api::RenameRequest::new(
            crate::api::CollectionPath::new("target.md").unwrap(),
            crate::api::CollectionPath::new("renamed.md").unwrap(),
        );

        reset_mutation_path_probes();
        crate::record_load::reset_snapshot_record_loads_for_test();
        let preview = collection
            .typed()
            .unwrap()
            .preflight_rename(request.clone())
            .unwrap();
        assert!(preview.value.would_rename);
        assert_eq!(
            mutation_path_probes(),
            MutationPathProbes {
                full_shadows: 1,
                ..MutationPathProbes::default()
            }
        );
        assert_eq!(crate::record_load::snapshot_record_loads_for_test(), 2);

        reset_mutation_path_probes();
        crate::record_load::reset_snapshot_record_loads_for_test();
        let renamed = collection.typed().unwrap().rename(request).unwrap();
        assert_eq!(renamed.value.to.as_str(), "renamed.md");
        assert_eq!(
            mutation_path_probes(),
            MutationPathProbes {
                full_shadows: 1,
                ..MutationPathProbes::default()
            }
        );
        assert_eq!(crate::record_load::snapshot_record_loads_for_test(), 4);
    }

    #[test]
    fn rename_result_uses_planned_bytes_and_locked_facts_after_postcommit_replacement() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        std::fs::write(root.path().join("before.md"), "planned\n").unwrap();
        let collection = Collection::open(root.path()).unwrap();
        crate::transactions::inject_post_commit_replacement(
            root.path(),
            "after.md",
            Some(b"external replacement much longer".to_vec()),
        );
        let outcome = collection
            .typed()
            .unwrap()
            .rename(
                crate::api::RenameRequest::new(
                    crate::api::CollectionPath::new("before.md").unwrap(),
                    crate::api::CollectionPath::new("after.md").unwrap(),
                )
                .with_document(),
            )
            .unwrap();
        assert_eq!(
            outcome.value.document.document.as_deref(),
            Some("planned\n")
        );
        assert_eq!(outcome.value.document.file.size, 8);
        assert_eq!(
            outcome.value.document.revision.as_str(),
            revision(b"planned\n")
        );
        assert_eq!(
            std::fs::read(root.path().join("after.md")).unwrap(),
            b"external replacement much longer"
        );
    }

    #[test]
    fn committed_facts_survive_post_commit_path_replacement() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        let collection = Collection::open(root.path()).unwrap();
        crate::transactions::inject_post_commit_replacement(
            root.path(),
            "facts.md",
            Some(b"external replacement much longer than planned".to_vec()),
        );
        let request =
            crate::api::CreateRequest::new(crate::api::CollectionPath::new("facts.md").unwrap())
                .with_body("planned")
                .with_document();
        let outcome = collection.typed().unwrap().create(request).unwrap();
        assert_eq!(outcome.value.document.as_deref(), Some("planned"));
        assert_eq!(outcome.value.revision.as_str(), revision(b"planned"));
        assert_eq!(outcome.value.file.size, 7);
        assert!(!outcome.value.file.mtime.is_empty());
        assert_eq!(
            std::fs::read(root.path().join("facts.md")).unwrap(),
            b"external replacement much longer than planned"
        );

        std::fs::write(root.path().join("facts.md"), "before").unwrap();
        crate::transactions::inject_post_commit_replacement(root.path(), "facts.md", None);
        let update = crate::api::UpdateRequest::replace_document(
            crate::api::CollectionPath::new("facts.md").unwrap(),
            "after",
        );
        let outcome = collection.typed().unwrap().update(update).unwrap();
        assert_eq!(outcome.value.document.as_deref(), Some("after"));
        assert_eq!(outcome.value.revision.as_str(), revision(b"after"));
        assert_eq!(outcome.value.file.size, 5);
        assert!(!outcome.value.file.mtime.is_empty());
        assert!(!root.path().join("facts.md").exists());
    }

    #[test]
    fn mutation_sources_reject_v03_and_legacy_request_bridges() {
        fn rust_sources(root: &std::path::Path, output: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(root).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    rust_sources(&path, output);
                } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                    output.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mutation");
        let mut sources = Vec::new();
        rust_sources(&root, &mut sources);
        for path in sources {
            let source = std::fs::read_to_string(&path).unwrap();
            assert!(
                !source.contains(&["crate", "v03"].join("::")),
                "{}",
                path.display()
            );
            assert!(
                !source.contains(&["create", "legacy", "input"].join("_")),
                "{}",
                path.display()
            );
            for forbidden in [
                ["update", "legacy", "input"].join("_"),
                ["full", "shadow", "collection"].join("_"),
                ["collect", "full", "shadow", "files"].join("_"),
                ["v03", "batch"].join("::"),
                ["from", "legacy"].join("_"),
                ["into", "legacy"].join("_"),
                ["diagnostic", "from", "legacy"].join("_"),
                ["diagnostics", "from", "legacy"].join("_"),
                ["diagnostics", "error"].join("_"),
                ["op", "error"].join("_"),
            ] {
                assert!(
                    !source.contains(&forbidden),
                    "{}: {forbidden}",
                    path.display()
                );
            }
        }
        let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut all_sources = Vec::new();
        rust_sources(&repository.join("src"), &mut all_sources);
        let removed_hydrator = ["hydrate", "persisted", "result"].join("_");
        for path in all_sources {
            let source = std::fs::read_to_string(&path).unwrap();
            assert!(!source.contains(&removed_hydrator), "{}", path.display());
        }
        let batch = std::fs::read_to_string(repository.join("src/v03/batch.rs")).unwrap();
        let runtime_rename_core = batch
            .split("fn prepare_runtime_rename_typed")
            .nth(1)
            .unwrap()
            .split("fn execute_non_record_runtime_operation")
            .next()
            .unwrap();
        assert_eq!(runtime_rename_core.matches("decode_rename(").count(), 1);
        assert_eq!(runtime_rename_core.matches("plan_rename(").count(), 1);
        assert!(!runtime_rename_core.contains("OperationResult"));
        assert!(!runtime_rename_core.contains("recover_v03"));
        let runtime_delete_adapter = batch
            .split("fn prepare_sparse_typed")
            .nth(1)
            .unwrap()
            .split("fn typed_projection_error")
            .next()
            .unwrap();
        assert_eq!(runtime_delete_adapter.matches("plan_delete(").count(), 1);
        for forbidden in [
            "staged_delete(",
            "request.check_backlinks =",
            "planned.broken_links =",
            "planned.types =",
            "authoritative",
        ] {
            assert!(!runtime_delete_adapter.contains(forbidden), "{forbidden}");
        }
        assert!(!batch.contains(&["struct", "ShadowCollection"].join(" ")));
        assert!(!batch.contains(&["fn", "shadow_collection"].join(" ")));
        assert!(!batch.contains(&["pub(crate)", "use", "crate::mutation"].join(" ")));
        for relative in [
            "src/operations/create.rs",
            "src/operations/update.rs",
            "src/operations/delete.rs",
        ] {
            let source = std::fs::read_to_string(repository.join(relative)).unwrap();
            let core = source
                .split("fn create_planned")
                .nth(1)
                .or_else(|| source.split("fn update_planned").nth(1))
                .or_else(|| source.split("fn delete_planned").nth(1))
                .unwrap();
            let edge_marker = if relative.ends_with("update.rs") {
                "fn legacy_prepared_update"
            } else {
                "fn mutation_failure_json"
            };
            let core = core.split(edge_marker).next().unwrap();
            for forbidden in [
                ["from", "legacy"].join("_"),
                ["into", "legacy"].join("_"),
                ["op", "error"].join("_"),
                ["validation", "failed", "error"].join("_"),
                ["serde_json", "to_value"].join("::"),
                ["Create", "Input"].join(""),
                ["Update", "Input"].join(""),
                ["Delete", "Input"].join(""),
                ["Operation", "Result"].join(""),
            ] {
                assert!(!core.contains(&forbidden), "{relative}: {forbidden}");
            }
        }
        let root_source = std::fs::read_to_string(repository.join("src/lib.rs")).unwrap();
        assert!(!root_source.contains(&["full", "shadow", "collection"].join("_")));
        assert!(!root_source.contains(&["collect", "full", "shadow", "files"].join("_")));
    }
}
