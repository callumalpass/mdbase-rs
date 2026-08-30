use serde_json::{Map, Value};

use super::{
    LifecycleEvent, PreparationOptions, PreparedCreate, PreparedUpdate, ResolvedWriteMembership,
};
use crate::api::{CreateRequest, Revision, UpdateRequest};
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
