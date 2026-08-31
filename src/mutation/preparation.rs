use serde_json::{Map, Value};

use super::{
    LifecycleEvent, PreparationOptions, PreparedCreate, PreparedDelete, PreparedRename,
    PreparedUpdate, ResolvedWriteMembership,
};
use crate::api::{
    CollectionPath, CreateRequest, DeleteRequest, RenameRequest, Revision, UpdateRequest,
};
use crate::diagnostic::Diagnostic;
use crate::frontmatter::parser::{
    parse_document_for_rewrite, yaml_mapping_to_json, FrontmatterState,
};
use crate::frontmatter::serializer;
use crate::record_load::RecordLoadOutcome;
use crate::Collection;

pub(crate) fn prepare_create(
    collection: &Collection,
    mut request: CreateRequest,
    options: PreparationOptions,
) -> Result<PreparedCreate, Vec<Diagnostic>> {
    let mut draft = request.frontmatter.as_object().cloned().unwrap_or_default();
    let canonical_path = request.path.as_ref().map(|path| path.to_string());
    let path = canonical_path.as_deref().unwrap_or("");
    let mut exact_document = options.create_document;
    if let Some(source) = exact_document.as_deref() {
        let candidate = classify_document(source, path)?;
        draft = candidate.frontmatter;
        request.body = candidate.body;
    }
    let membership = ResolvedWriteMembership::resolve_create(
        collection,
        request.type_name.as_deref(),
        request.contract.as_deref(),
        request.contract_version.as_deref(),
        &mut draft,
        path,
    )?;
    draft = collection.apply_mutation_lifecycle(
        LifecycleEvent::Create,
        membership.types(),
        draft,
        None,
        path,
    )?;
    request.frontmatter = Value::Object(draft);
    Ok(PreparedCreate {
        request,
        membership: Some(membership),
        exact_document: exact_document.take(),
        legacy_path: None,
        legacy_revision: None,
    })
}

pub(crate) fn prepare_update(
    collection: &Collection,
    mut request: UpdateRequest,
    _options: PreparationOptions,
) -> Result<PreparedUpdate, Vec<Diagnostic>> {
    let path = request.path.to_string();
    crate::operations::ensure_no_symlink_components_diagnostic(
        &collection.root,
        &path,
        collection.spec_profile,
    )
    .map_err(|mut error| {
        error.path = Some(path.clone());
        vec![error]
    })?;
    crate::operations::ensure_regular_record_file_diagnostic(
        &request.path.under(&collection.root),
        &path,
    )
    .map_err(|mut error| {
        error.path = Some(path.clone());
        vec![error]
    })?;
    let loaded = crate::record_load::load_record(collection, &path).map_err(|_| {
        vec![Diagnostic::error(
            "file_read_failed",
            "Record could not be read.",
            Some(path.clone()),
        )]
    })?;
    let candidate = request
        .document
        .as_deref()
        .map(|source| classify_document(source, &path))
        .transpose()?;
    let (old, prepared_revision) = match loaded {
        RecordLoadOutcome::Parsed {
            raw_frontmatter,
            facts,
            ..
        } => (
            raw_frontmatter.as_object().cloned().unwrap_or_default(),
            facts.revision,
        ),
        RecordLoadOutcome::Invalid { facts, state, .. } => {
            if candidate.is_none() {
                return Err(vec![invalid_record(&path, state.reason().as_str())]);
            }
            (Map::new(), facts.revision)
        }
    };
    if request
        .if_revision
        .as_ref()
        .is_some_and(|expected| expected.as_str() != prepared_revision)
    {
        return Err(vec![Diagnostic::error(
            "concurrent_modification",
            format!("File '{path}' was modified externally"),
            Some(path),
        )]);
    }
    let draft = if let Some(candidate) = &candidate {
        candidate.frontmatter.clone()
    } else {
        let patch = request.patch.as_object().cloned().unwrap_or_default();
        let mut draft = old.clone();
        apply_patch(&mut draft, &patch, &collection.settings.write_nulls);
        draft
    };
    let membership = ResolvedWriteMembership::resolve_update(collection, &draft, &path)?;
    let lifecycle = collection.apply_mutation_lifecycle(
        LifecycleEvent::Update,
        membership.types(),
        draft.clone(),
        Some(&old),
        &path,
    )?;
    if let Some(candidate) = candidate {
        if lifecycle != draft {
            let canonical =
                crate::frontmatter::parser::json_to_yaml_mapping(&Value::Object(lifecycle.clone()));
            let mapping = candidate.authored.as_ref().map_or(canonical, |authored| {
                serializer::reconcile_json_object(authored, &lifecycle)
            });
            request.document = Some(
                serializer::serialize_document_with_bom(
                    candidate.had_bom,
                    &mapping,
                    &candidate.body,
                )
                .map_err(|error| {
                    vec![Diagnostic::error(
                        crate::errors::FRONTMATTER_SERIALIZATION_FAILED,
                        error.to_string(),
                        Some(path.clone()),
                    )]
                })?,
            );
        }
        request.include_document = true;
    } else {
        request.patch = Value::Object(diff_frontmatter(&old, &lifecycle));
    }
    if request.if_revision.is_none() {
        request.if_revision = Revision::parse(prepared_revision.clone()).ok();
    }
    Ok(PreparedUpdate {
        request,
        membership: Some(membership),
        legacy_path: None,
        legacy_revision: None,
        legacy_last_known_mtime: None,
    })
}

pub(crate) fn prepare_delete(
    collection: &Collection,
    request: DeleteRequest,
    options: PreparationOptions,
    legacy_last_known_mtime: Option<u64>,
) -> Result<PreparedDelete, Vec<Diagnostic>> {
    let path = request.path.to_string();
    crate::operations::mutation_record_path_diagnostic(collection, &path).map_err(
        |mut error| {
            error.path = Some(path.clone());
            vec![error]
        },
    )?;
    crate::operations::ensure_no_symlink_components_diagnostic(
        &collection.root,
        &path,
        collection.spec_profile,
    )
    .map_err(|mut error| {
        error.path = Some(path.clone());
        vec![error]
    })?;
    crate::operations::ensure_regular_record_file_diagnostic(
        &request.path.under(&collection.root),
        &path,
    )
    .map_err(|mut error| {
        error.path = Some(path.clone());
        vec![error]
    })?;

    let (before_revision, before_frontmatter, before_body, types, broken_links) =
        if request.check_backlinks {
            let snapshot = collection
                .capture_collection_snapshot(&crate::OperationCancellation::new())
                .map_err(|error| {
                    vec![Diagnostic::error(
                        "collection_snapshot_failed",
                        error.to_string(),
                        Some(path.clone()),
                    )]
                })?;
            let target = snapshot.entry(&path).ok_or_else(|| {
                vec![Diagnostic::error(
                    crate::errors::FILE_NOT_FOUND,
                    format!("File not found: {path}"),
                    Some(path.clone()),
                )]
            })?;
            let broken_links = collection
                .build_backlinks_index_for_snapshot(&snapshot)
                .get(&path)
                .into_iter()
                .flatten()
                .map(|source| serde_json::json!({"path": source}))
                .collect();
            let (frontmatter, body) = delete_record_projection(target.outcome());
            (
                target.facts().revision.clone(),
                frontmatter,
                body,
                target.type_names().to_vec(),
                broken_links,
            )
        } else {
            let loaded = crate::record_load::load_record(collection, &path).map_err(|error| {
                vec![Diagnostic::error(
                    if error.kind() == std::io::ErrorKind::NotFound {
                        crate::errors::FILE_NOT_FOUND
                    } else {
                        "file_read_failed"
                    },
                    if error.kind() == std::io::ErrorKind::NotFound {
                        format!("File not found: {path}")
                    } else {
                        "Record could not be read.".to_string()
                    },
                    Some(path.clone()),
                )]
            })?;
            let (frontmatter, body) = delete_record_projection(&loaded);
            (
                loaded.facts().revision.clone(),
                frontmatter,
                body,
                loaded.type_names().to_vec(),
                Vec::new(),
            )
        };

    if request
        .if_revision
        .as_ref()
        .is_some_and(|expected| expected.as_str() != before_revision)
    {
        return Err(vec![Diagnostic::error(
            crate::errors::CONCURRENT_MODIFICATION,
            format!("File '{path}' was modified externally"),
            Some(path),
        )]);
    }
    if let Some(known_ms) = legacy_last_known_mtime {
        let current_ms = std::fs::metadata(request.path.under(&collection.root))
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as u64);
        if current_ms.is_some_and(|current| current != known_ms) {
            return Err(vec![Diagnostic::error(
                crate::errors::CONCURRENT_MODIFICATION,
                format!("File '{}' was modified externally", request.path),
                Some(request.path.to_string()),
            )]);
        }
    }

    Ok(PreparedDelete {
        request,
        dry_run: options.dry_run,
        before_revision,
        before_frontmatter,
        before_body,
        types,
        broken_links,
        legacy_last_known_mtime,
    })
}

pub(crate) fn prepare_rename(
    collection: &Collection,
    request: RenameRequest,
    options: PreparationOptions,
    legacy_last_known_mtime: Option<u64>,
    legacy_ref_mtimes: std::collections::HashMap<String, u64>,
    legacy_simulations: Vec<(CollectionPath, String)>,
) -> Result<PreparedRename, Vec<Diagnostic>> {
    let from = request.from.to_string();
    let to = request.to.to_string();
    for path in [&request.from, &request.to] {
        crate::operations::mutation_record_path_diagnostic(collection, path.as_str()).map_err(
            |mut error| {
                error.path = Some(path.to_string());
                vec![error]
            },
        )?;
        crate::operations::ensure_no_symlink_components_diagnostic(
            &collection.root,
            path.as_str(),
            collection.spec_profile,
        )
        .map_err(|mut error| {
            error.path = Some(path.to_string());
            vec![error]
        })?;
    }
    crate::operations::ensure_regular_record_file_diagnostic(
        &request.from.under(&collection.root),
        &from,
    )
    .map_err(|mut error| {
        error.path = Some(from.clone());
        vec![error]
    })?;
    if request.to.under(&collection.root).exists() {
        return Err(vec![Diagnostic::error(
            crate::errors::PATH_CONFLICT,
            format!("Target already exists: {to}"),
            Some(to),
        )]);
    }

    let snapshot = collection
        .capture_collection_snapshot(&crate::OperationCancellation::new())
        .map_err(|error| {
            vec![Diagnostic::error(
                "collection_snapshot_failed",
                error.to_string(),
                Some(from.clone()),
            )]
        })?;
    let source = snapshot.entry(&from).ok_or_else(|| {
        vec![Diagnostic::error(
            crate::errors::FILE_NOT_FOUND,
            format!("Source not found: {from}"),
            Some(from.clone()),
        )]
    })?;
    let source_revision = source.facts().revision.clone();
    if request
        .if_revision
        .as_ref()
        .is_some_and(|expected| expected.as_str() != source_revision)
    {
        return Err(vec![Diagnostic::error(
            crate::errors::CONCURRENT_MODIFICATION,
            format!("File '{from}' no longer matches the requested revision"),
            Some(from),
        )]);
    }
    if legacy_last_known_mtime
        .is_some_and(|known| source.facts().mtime_ns.max(0) as u64 / 1_000_000 != known)
    {
        return Err(vec![Diagnostic::error(
            crate::errors::CONCURRENT_MODIFICATION,
            format!("File '{}' was modified externally", request.from),
            Some(request.from.to_string()),
        )]);
    }
    let source_bytes = std::fs::read(request.from.under(&collection.root)).map_err(|error| {
        vec![Diagnostic::error(
            "file_read_failed",
            error.to_string(),
            Some(request.from.to_string()),
        )]
    })?;
    if content_revision(&source_bytes) != source_revision {
        return Err(vec![Diagnostic::error(
            crate::errors::CONCURRENT_MODIFICATION,
            format!("File '{}' was modified externally", request.from),
            Some(request.from.to_string()),
        )]);
    }
    let source_id = source
        .raw_frontmatter()
        .and_then(Value::as_object)
        .and_then(|frontmatter| frontmatter.get(&collection.settings.id_field))
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut warnings = Vec::new();
    let mut reference_failures = Vec::new();
    let reference_plans = if request.update_refs {
        collection.plan_reference_rewrites(
            &snapshot,
            request.from.as_ref(),
            request.to.as_ref(),
            &source_id,
            &mut warnings,
            &mut reference_failures,
        )
    } else {
        Vec::new()
    };
    Ok(PreparedRename {
        request,
        dry_run: options.dry_run,
        source_revision,
        source_types: source.type_names().to_vec(),
        source_id,
        source_frontmatter: source
            .raw_frontmatter()
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new())),
        source_effective_frontmatter: source
            .effective_frontmatter()
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new())),
        source_body: source.body().unwrap_or_default().to_string(),
        source_bytes,
        reference_plans,
        warnings,
        reference_failures,
        legacy_ref_mtimes,
        legacy_simulations,
    })
}

fn content_revision(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn delete_record_projection(record: &RecordLoadOutcome) -> (Map<String, Value>, String) {
    let frontmatter = record
        .parsed()
        .and_then(|parsed| parsed.raw_frontmatter.as_object())
        .cloned()
        .unwrap_or_default();
    let body = record.body().unwrap_or_default().to_string();
    (frontmatter, body)
}

struct Candidate {
    frontmatter: Map<String, Value>,
    body: String,
    had_bom: bool,
    authored: Option<serde_yaml::Mapping>,
}

fn classify_document(source: &str, path: &str) -> Result<Candidate, Vec<Diagnostic>> {
    let (document, had_bom) = parse_document_for_rewrite(source);
    let (frontmatter, authored) = match document.frontmatter_state() {
        FrontmatterState::Absent => (Map::new(), None),
        FrontmatterState::Mapping(mapping) => (
            yaml_mapping_to_json(mapping)
                .as_object()
                .cloned()
                .unwrap_or_default(),
            Some(mapping.clone()),
        ),
        FrontmatterState::InvalidYaml => {
            return Err(vec![Diagnostic::error(
                "invalid_frontmatter",
                "Failed to parse replacement document YAML frontmatter.",
                Some(path.to_string()),
            )])
        }
        FrontmatterState::Null | FrontmatterState::NonMapping(_) => {
            return Err(vec![Diagnostic::error(
                "invalid_frontmatter",
                "Replacement document frontmatter must be a YAML mapping.",
                Some(path.to_string()),
            )])
        }
    };
    Ok(Candidate {
        frontmatter,
        body: document.body,
        had_bom,
        authored,
    })
}

fn apply_patch(draft: &mut Map<String, Value>, patch: &Map<String, Value>, write_nulls: &str) {
    for (field, value) in patch {
        if value.is_null() && write_nulls == "omit" {
            draft.remove(field);
        } else {
            draft.insert(field.clone(), value.clone());
        }
    }
}

fn diff_frontmatter(before: &Map<String, Value>, after: &Map<String, Value>) -> Map<String, Value> {
    let mut fields = Map::new();
    for (field, value) in after {
        if before.get(field) != Some(value) {
            fields.insert(field.clone(), value.clone());
        }
    }
    for field in before.keys() {
        if !after.contains_key(field) {
            fields.insert(field.clone(), Value::Null);
        }
    }
    fields
}

fn invalid_record(path: &str, reason: &str) -> Diagnostic {
    let mut diagnostic = Diagnostic::error(
        "invalid_frontmatter",
        format!("Invalid frontmatter: {reason}"),
        Some(path.to_string()),
    );
    diagnostic.details = Some(serde_json::json!({"reason": reason}));
    diagnostic
}
