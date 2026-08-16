use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use super::expression::{
    self, serialize_bases_file, BasesEvaluationContext, BasesFile, BasesLink, BasesTimezone,
};
use super::model::{
    identifier, presentation_for, stable_named_view_ids, BaseFilter, BaseGroupBy,
    NamedViewDescriptor, ObsidianBaseDocument, ObsidianBaseView, ViewDocumentDescriptor,
    ViewPresentation, ViewPropertyDescriptor, ViewReferenceInput, ViewSourceDescriptor,
};
use crate::expressions::evaluator::{
    extract_embeds_from_body, extract_links_from_body, extract_tags_from_body,
};
use crate::query::cache_source::FileRecord;
use crate::v03::{validate_view, Diagnostic, OperationResult};
use crate::Collection;

pub(super) fn list_views(collection: &Collection, _input: &Value) -> OperationResult {
    let mut diagnostics = Vec::new();
    let mut documents = canonical_documents(collection, &mut diagnostics);
    documents.extend(obsidian_documents(collection, &mut diagnostics));
    documents.sort_by(|left, right| left.source.path.cmp(&right.source.path));
    OperationResult {
        valid: !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == "error"),
        result: json!({
            "views": documents,
            "meta": { "total_count": documents.len() },
        }),
        diagnostics,
    }
}

pub(super) fn execute_view(collection: &Collection, input: &Value) -> OperationResult {
    let mut request = match serde_json::from_value::<ViewReferenceInput>(input.clone()) {
        Ok(request) => request,
        Err(error) => {
            return failed(
                "invalid_request",
                format!("View execution input is invalid: {error}"),
                None,
            )
        }
    };
    if input.get("context") == Some(&Value::Null) {
        request.context = Some(None);
    }
    if request.path.ends_with(".base") {
        execute_obsidian(collection, &request)
    } else {
        execute_canonical(collection, &request)
    }
}

fn canonical_documents(
    collection: &Collection,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<ViewDocumentDescriptor> {
    let snapshot = match collection.load_query_data_profiled(false, false) {
        Ok((snapshot, _)) => snapshot,
        Err(error) => {
            diagnostics.push(snapshot_diagnostic(collection, &error));
            return Vec::new();
        }
    };
    snapshot
        .records
        .iter()
        .filter(|record| {
            record.type_names.iter().any(|name| name == "view")
                || record.raw_frontmatter.get("type").and_then(Value::as_str) == Some("view")
        })
        .filter_map(|record| canonical_descriptor(collection, record, diagnostics))
        .collect()
}

fn canonical_descriptor(
    collection: &Collection,
    record: &FileRecord,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<ViewDocumentDescriptor> {
    let schema_diagnostics = validate_view(&record.raw_frontmatter, &record.rel_path);
    if schema_diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == "error")
    {
        diagnostics.extend(schema_diagnostics.into_iter().map(|mut diagnostic| {
            diagnostic.severity = "warning".to_string();
            diagnostic
        }));
        return None;
    }
    let named_views = record
        .raw_frontmatter
        .get("views")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|view| {
            Some(NamedViewDescriptor {
                id: view.get("id")?.as_str()?.to_string(),
                name: view.get("name")?.as_str()?.to_string(),
                properties: canonical_properties(&record.raw_frontmatter, view),
                presentation: canonical_presentation(view.get("presentation")),
            })
        })
        .collect::<Vec<_>>();
    let ids = named_views
        .iter()
        .map(|view| view.id.as_str())
        .collect::<Vec<_>>();
    if ids.iter().copied().collect::<HashSet<_>>().len() != ids.len() {
        let mut diagnostic = Diagnostic::error(
            "invalid_view",
            "View record contains duplicate named-view IDs.",
            Some(record.rel_path.clone()),
        );
        diagnostic.severity = "warning".to_string();
        diagnostics.push(diagnostic);
        return None;
    }
    let source = collection.root.join(&record.rel_path);
    Some(ViewDocumentDescriptor {
        source: ViewSourceDescriptor {
            path: record.rel_path.clone(),
            format: "mdbase.view".to_string(),
            revision: file_revision(&source).unwrap_or_default(),
            writable: true,
        },
        id: record
            .raw_frontmatter
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(&record.rel_path)
            .to_string(),
        name: record
            .raw_frontmatter
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&record.rel_path)
            .to_string(),
        views: named_views,
    })
}

fn canonical_presentation(value: Option<&Value>) -> Option<ViewPresentation> {
    let object = value.and_then(Value::as_object);
    object.map(|object| ViewPresentation {
        renderer: object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("mdbase.table")
            .to_string(),
        fallback: object
            .get("fallback")
            .and_then(Value::as_str)
            .unwrap_or("mdbase.table")
            .to_string(),
        mappings: object
            .get("mappings")
            .and_then(Value::as_object)
            .map(|value| {
                value
                    .iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default(),
        options: object
            .get("options")
            .and_then(Value::as_object)
            .map(|value| {
                value
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

fn canonical_properties(record: &Value, view: &Value) -> Vec<ViewPropertyDescriptor> {
    let metadata = record.get("properties").and_then(Value::as_object);
    view.get("select")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|selection| match selection {
            Value::String(selector) => {
                let key = selector.rsplit('.').next().unwrap_or(selector);
                let property_metadata =
                    metadata.and_then(|values| values.get(selector).or_else(|| values.get(key)));
                Some(property_descriptor(key, property_metadata, None))
            }
            Value::Object(selection) => {
                let key = selection.get("name")?.as_str()?;
                let property_metadata = metadata.and_then(|values| values.get(key));
                Some(property_descriptor(key, property_metadata, Some(selection)))
            }
            _ => None,
        })
        .collect()
}

fn obsidian_properties(
    document: &ObsidianBaseDocument,
    view: &ObsidianBaseView,
) -> Vec<ViewPropertyDescriptor> {
    view.order
        .iter()
        .map(|key| {
            let metadata = document
                .properties
                .get(key)
                .or_else(|| document.properties.get(&format!("note.{key}")));
            let object = metadata.and_then(Value::as_object);
            ViewPropertyDescriptor {
                key: key.clone(),
                label: object
                    .and_then(|value| value.get("displayName").or_else(|| value.get("label")))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                description: object
                    .and_then(|value| value.get("description"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                format: object
                    .and_then(|value| value.get("format"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                hidden: object
                    .and_then(|value| value.get("hidden"))
                    .and_then(Value::as_bool),
            }
        })
        .collect()
}

fn property_descriptor(
    key: &str,
    metadata: Option<&Value>,
    selection: Option<&Map<String, Value>>,
) -> ViewPropertyDescriptor {
    let metadata = metadata.and_then(Value::as_object);
    ViewPropertyDescriptor {
        key: key.to_string(),
        label: selection
            .and_then(|value| value.get("label"))
            .and_then(Value::as_str)
            .or_else(|| {
                metadata
                    .and_then(|value| value.get("label"))
                    .and_then(Value::as_str)
            })
            .map(str::to_string),
        description: selection
            .and_then(|value| value.get("description"))
            .and_then(Value::as_str)
            .or_else(|| {
                metadata
                    .and_then(|value| value.get("description"))
                    .and_then(Value::as_str)
            })
            .map(str::to_string),
        format: metadata
            .and_then(|value| value.get("format"))
            .and_then(Value::as_str)
            .map(str::to_string),
        hidden: metadata
            .and_then(|value| value.get("hidden"))
            .and_then(Value::as_bool),
    }
}

fn obsidian_documents(
    collection: &Collection,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<ViewDocumentDescriptor> {
    obsidian_source_paths(collection)
        .into_iter()
        .filter_map(|path| {
            let relative = relative_path(&collection.root, &path)?;
            let source = match fs::read_to_string(&path) {
                Ok(source) => source,
                Err(error) => {
                    let mut diagnostic = Diagnostic::error(
                        "invalid_view",
                        format!("Could not read Obsidian Base: {error}"),
                        Some(relative),
                    );
                    diagnostic.severity = "warning".to_string();
                    diagnostics.push(diagnostic);
                    return None;
                }
            };
            let document = match serde_yaml::from_str::<ObsidianBaseDocument>(&source) {
                Ok(document) => document,
                Err(error) => {
                    let mut diagnostic = Diagnostic::error(
                        "invalid_view",
                        format!("Could not parse Obsidian Base: {error}"),
                        Some(relative),
                    );
                    diagnostic.severity = "warning".to_string();
                    diagnostics.push(diagnostic);
                    return None;
                }
            };
            let ids = stable_named_view_ids(&document.views);
            let views = document
                .views
                .iter()
                .zip(ids)
                .map(|(view, id)| NamedViewDescriptor {
                    id,
                    name: view.name.clone(),
                    properties: obsidian_properties(&document, view),
                    presentation: Some(presentation_for(view)),
                })
                .collect::<Vec<_>>();
            let name = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or(&relative)
                .to_string();
            Some(ViewDocumentDescriptor {
                source: ViewSourceDescriptor {
                    path: relative.clone(),
                    format: "obsidian.base".to_string(),
                    revision: revision(source.as_bytes()),
                    writable: true,
                },
                id: identifier(&name, "base"),
                name,
                views,
            })
        })
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct CanonicalViewQuery {
    pub query: Value,
    pub view_path: String,
    pub view_id: String,
    pub context_path: Option<String>,
    pub required_context_types: Vec<String>,
}

pub(crate) fn prepare_hosted_canonical_view(
    document: &Value,
    input: &Value,
) -> Result<CanonicalViewQuery, OperationResult> {
    let mut request =
        serde_json::from_value::<ViewReferenceInput>(input.clone()).map_err(|error| {
            failed(
                "invalid_request",
                format!("View execution input is invalid: {error}"),
                None,
            )
        })?;
    if input.get("context") == Some(&Value::Null) {
        request.context = Some(None);
    }
    prepare_canonical_view_query(document, &request)
}

pub(crate) fn verify_canonical_view_context(
    prepared: &CanonicalViewQuery,
    actual_types: Option<&[String]>,
) -> Result<(), OperationResult> {
    if prepared.required_context_types.is_empty() {
        return Ok(());
    }
    let matches = actual_types.is_some_and(|actual| {
        prepared.required_context_types.iter().any(|required| {
            actual
                .iter()
                .any(|actual| actual.eq_ignore_ascii_case(required))
        })
    });
    if matches {
        Ok(())
    } else {
        Err(failed(
            "context_type_mismatch",
            "The invocation context does not match an allowed context type.",
            prepared.context_path.clone(),
        ))
    }
}

fn execute_canonical(collection: &Collection, request: &ViewReferenceInput) -> OperationResult {
    if request.render {
        return failed(
            "unsupported_presentation",
            "This provider supports headless view execution.",
            Some(request.path.clone()),
        );
    }
    let read = collection.read(&json!({"path": request.path}));
    if read.get("error").is_some() {
        return failed(
            "view_not_found",
            format!("View record '{}' was not found.", request.path),
            Some(request.path.clone()),
        );
    }
    let document = read
        .get("frontmatter")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let prepared = match prepare_canonical_view_query(&document, request) {
        Ok(prepared) => prepared,
        Err(result) => return result,
    };
    let context_types = prepared.context_path.as_deref().map(|context_path| {
        collection
            .read(&json!({"path": context_path}))
            .get("types")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect::<Vec<_>>()
    });
    if let Err(result) = verify_canonical_view_context(&prepared, context_types.as_deref()) {
        return result;
    }
    let mut result = match collection.v03_operations() {
        Ok(operations) => operations.query(&prepared.query),
        Err(diagnostic) => {
            return OperationResult {
                valid: false,
                result: json!({}),
                diagnostics: vec![*diagnostic],
            }
        }
    };
    if result.valid {
        result.result["meta"]["view"] = json!({"path": prepared.view_path, "id": prepared.view_id});
        result.result["meta"]["context"] = prepared
            .context_path
            .map(|path| json!({"path": path}))
            .unwrap_or(Value::Null);
    }
    result
}

fn prepare_canonical_view_query(
    document: &Value,
    request: &ViewReferenceInput,
) -> Result<CanonicalViewQuery, OperationResult> {
    if request.render {
        return Err(failed(
            "unsupported_presentation",
            "This provider supports headless view execution.",
            Some(request.path.clone()),
        ));
    }
    let schema_diagnostics = validate_view(document, &request.path);
    if !schema_diagnostics.is_empty() {
        return Err(OperationResult {
            valid: false,
            result: json!({}),
            diagnostics: schema_diagnostics,
        });
    }
    let views = document
        .get("views")
        .and_then(Value::as_array)
        .expect("schema validated views");
    let ids = views
        .iter()
        .filter_map(|view| view.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if ids.iter().copied().collect::<HashSet<_>>().len() != ids.len() {
        return Err(failed(
            "invalid_view",
            "View record contains duplicate named-view IDs.",
            Some(request.path.clone()),
        ));
    }
    let Some(view) = views
        .iter()
        .find(|view| view.get("id").and_then(Value::as_str) == Some(&request.view_id))
    else {
        return Err(failed(
            "view_not_found",
            format!("Named view '{}' was not found.", request.view_id),
            Some(request.path.clone()),
        ));
    };
    let shared = document
        .get("query")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut query = Map::new();
    for key in ["types", "where", "projections"] {
        if let Some(value) = shared.get(key) {
            query.insert(key.to_string(), value.clone());
        }
    }
    for key in [
        "types",
        "select",
        "order_by",
        "group_by",
        "summaries",
        "limit",
        "offset",
        "include_body",
        "frontmatter_mode",
    ] {
        if let Some(value) = view.get(key) {
            query.insert(key.to_string(), value.clone());
        }
    }
    if let Some(limit) = request.limit {
        query.insert("limit".to_string(), json!(limit));
    }
    if let Some(offset) = request.offset {
        query.insert("offset".to_string(), json!(offset));
    }
    if let Some(timezone) = &request.timezone {
        query.insert("timezone".to_string(), json!(timezone));
    }
    let shared_where = shared.get("where").and_then(Value::as_str);
    let local_where = view.get("where").and_then(Value::as_str);
    if let (Some(shared), Some(local)) = (shared_where, local_where) {
        query.insert(
            "where".to_string(),
            Value::String(format!("({shared}) && ({local})")),
        );
    } else if let Some(local) = local_where {
        query.insert("where".to_string(), Value::String(local.to_string()));
    }
    let shared_projections = shared
        .get("projections")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let local_projections = view
        .get("projections")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut projections = shared_projections;
    for (name, definition) in local_projections {
        if let Some(existing) = projections.get(&name) {
            if canonical_json(existing) != canonical_json(&definition) {
                return Err(failed(
                    "invalid_view",
                    format!("Projection '{name}' has conflicting definitions."),
                    Some(request.path.clone()),
                ));
            }
        } else {
            projections.insert(name, definition);
        }
    }
    if !projections.is_empty() {
        query.insert("projections".to_string(), Value::Object(projections));
    }
    if let Some(functions) = document.get("summary_functions") {
        query.insert("summary_functions".to_string(), functions.clone());
    }
    let context_declaration = view
        .get("context")
        .or_else(|| shared.get("context"))
        .and_then(Value::as_object);
    let context_was_supplied = request.context.is_some();
    let context_path = request
        .context
        .as_ref()
        .and_then(|context| context.as_ref())
        .map(|context| context.path.clone())
        .or_else(|| {
            if context_was_supplied {
                return None;
            }
            let on_missing = context_declaration
                .and_then(|value| value.get("this"))
                .and_then(Value::as_object)
                .and_then(|value| value.get("on_missing"))
                .and_then(Value::as_str)
                .unwrap_or("view");
            (on_missing == "view").then(|| request.path.clone())
        });
    if !context_was_supplied
        && context_path.is_none()
        && context_declaration
            .and_then(|value| value.get("this"))
            .and_then(Value::as_object)
            .and_then(|value| value.get("on_missing"))
            .and_then(Value::as_str)
            == Some("error")
    {
        return Err(failed(
            "context_required",
            "This named view requires an invocation context.",
            Some(request.path.clone()),
        ));
    }
    if let Some(context_path) = context_path.as_deref() {
        query.insert(
            "context".to_string(),
            json!({"this": {"path": context_path}}),
        );
    }
    let required_context_types = context_declaration
        .and_then(|value| value.get("this"))
        .and_then(Value::as_object)
        .and_then(|value| value.get("types"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect();
    Ok(CanonicalViewQuery {
        query: Value::Object(query),
        view_path: request.path.clone(),
        view_id: request.view_id.clone(),
        context_path,
        required_context_types,
    })
}

fn execute_obsidian(collection: &Collection, request: &ViewReferenceInput) -> OperationResult {
    if request.render {
        return failed(
            "unsupported_presentation",
            "This provider supports headless view execution.",
            Some(request.path.clone()),
        );
    }
    let relative = match safe_view_path(collection, &request.path) {
        Ok(path) => path,
        Err(diagnostic) => {
            return OperationResult {
                valid: false,
                result: json!({}),
                diagnostics: vec![*diagnostic],
            }
        }
    };
    if !obsidian_source_paths(collection).contains(&relative) {
        return failed(
            "view_not_found",
            "The requested Obsidian Base is not enabled by collection configuration.",
            Some(request.path.clone()),
        );
    }
    let source = match fs::read_to_string(&relative) {
        Ok(source) => source,
        Err(error) => {
            return failed(
                "view_not_found",
                format!("Could not read view source: {error}"),
                Some(request.path.clone()),
            )
        }
    };
    let document = match serde_yaml::from_str::<ObsidianBaseDocument>(&source) {
        Ok(document) => document,
        Err(error) => {
            return failed(
                "invalid_view",
                format!("Could not parse view source: {error}"),
                Some(request.path.clone()),
            )
        }
    };
    let ids = stable_named_view_ids(&document.views);
    let Some((_, view)) = ids
        .iter()
        .zip(&document.views)
        .find(|(id, _)| **id == request.view_id)
    else {
        return failed(
            "view_not_found",
            format!("Named view '{}' was not found.", request.view_id),
            Some(request.path.clone()),
        );
    };

    let mut diagnostics = validate_base_expressions(&document, view, &request.path);
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == "error")
    {
        return OperationResult {
            valid: false,
            result: json!({}),
            diagnostics,
        };
    }
    let timezone = match crate::expressions::evaluator::resolve_execution_timezone(
        request.timezone.as_deref(),
        collection.settings.timezone.as_deref(),
    ) {
        Ok(timezone) => timezone,
        Err(error) => return failed("invalid_timezone", error, None),
    };
    let timezone = match BasesTimezone::from_setting(timezone) {
        Ok(timezone) => timezone,
        Err(error) => return failed("invalid_config", error, Some("mdbase.yaml".to_string())),
    };
    let include_backlinks = base_uses_backlinks(&document, view);
    let snapshot = match collection.load_query_data_profiled(false, include_backlinks) {
        Ok((snapshot, _)) => snapshot,
        Err(error) => {
            return OperationResult {
                valid: false,
                result: json!({}),
                diagnostics: vec![snapshot_diagnostic(collection, &error)],
            }
        }
    };
    let records = snapshot.records;
    let backlinks = snapshot.backlinks;
    let all_files = Arc::new(records.iter().map(record_file).collect::<Vec<_>>());
    let link_resolutions = Arc::new(link_resolutions(&all_files));
    let formulas = Arc::new(document.formulas.clone());
    let property_types = Arc::new(BTreeMap::new());
    let clock = UtcClock::capture();
    let mut rows = Vec::new();
    for record in &records {
        let mut file = record_file(record);
        if let Some(index) = backlinks.as_ref() {
            file.backlinks = index
                .get(&record.rel_path)
                .into_iter()
                .flatten()
                .map(|path| BasesLink {
                    path: path.clone(),
                    resolved_path: Some(Some(path.clone())),
                    ..Default::default()
                })
                .collect();
        }
        let context = BasesEvaluationContext {
            note: record
                .effective_frontmatter
                .as_object()
                .cloned()
                .unwrap_or_default(),
            file: file.clone(),
            this_file: request
                .context
                .as_ref()
                .and_then(|context| context.as_ref())
                .and_then(|context| all_files.iter().find(|file| file.path == context.path))
                .cloned(),
            files: all_files.clone(),
            formulas: formulas.clone(),
            property_types: property_types.clone(),
            link_resolutions: link_resolutions.clone(),
            now: Some(clock.clone()),
            timezone: timezone.clone(),
            work_limit: None,
            cancellation: None,
        };
        match combined_filter_matches(document.filters.as_ref(), view.filters.as_ref(), &context) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                diagnostics.push(Diagnostic {
                    severity: "warning".to_string(),
                    code: "expression_evaluation_error".to_string(),
                    message: error,
                    path: Some(record.rel_path.clone()),
                    field: Some("filters".to_string()),
                    type_name: None,
                    schema_location: None,
                    details: Some(json!({"dialect": "obsidian.bases"})),
                });
                continue;
            }
        }
        let mut computed_values = Map::new();
        for property in view
            .order
            .iter()
            .chain(view.sort.iter().map(|sort| &sort.property))
        {
            if !computed_values.contains_key(property) {
                let value = evaluate_property(property, &context).unwrap_or(Value::Null);
                computed_values.insert(property.clone(), value);
            }
        }
        if let Some(group_by) = &view.group_by {
            let property = group_by.property();
            computed_values
                .entry(property.to_string())
                .or_insert_with(|| evaluate_property(property, &context).unwrap_or(Value::Null));
        }
        let mut values: Map<String, Value> = view
            .order
            .iter()
            .map(|property| {
                (
                    property.clone(),
                    computed_values
                        .get(property)
                        .cloned()
                        .unwrap_or(Value::Null),
                )
            })
            .collect();
        if let Some(group_by) = &view.group_by {
            let property = group_by.property();
            values.entry(property.to_string()).or_insert_with(|| {
                computed_values
                    .get(property)
                    .cloned()
                    .unwrap_or(Value::Null)
            });
        }
        rows.push(BaseRow {
            record,
            file,
            values,
            computed_values,
        });
    }
    sort_rows(&mut rows, view);
    let total_count = rows.len();
    let offset = usize::try_from(request.offset.unwrap_or(0))
        .unwrap_or(usize::MAX)
        .min(total_count);
    let available = &rows[offset..];
    let limit = request
        .limit
        .or(view.limit)
        .and_then(|value| usize::try_from(value).ok());
    let page = limit
        .map(|limit| &available[..available.len().min(limit)])
        .unwrap_or(available);
    let results = page.iter().map(serialize_base_row).collect::<Vec<_>>();
    let mut meta = json!({
        "total_count": total_count,
        "has_more": offset.saturating_add(page.len()) < total_count,
        "view": {"path": request.path, "id": request.view_id},
    });
    if let Some(group_by) = &view.group_by {
        meta["groups"] = base_groups(&rows, group_by);
    }
    OperationResult {
        valid: true,
        result: json!({"results": results, "meta": meta}),
        diagnostics,
    }
}

fn snapshot_diagnostic(
    collection: &Collection,
    error: &crate::snapshot::SnapshotError,
) -> Diagnostic {
    let path = error.path().map(|path| {
        path.strip_prefix(&collection.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    });
    Diagnostic::error("collection_snapshot_failed", error.to_string(), path)
}

struct BaseRow<'a> {
    record: &'a FileRecord,
    file: BasesFile,
    values: Map<String, Value>,
    computed_values: Map<String, Value>,
}

fn serialize_base_row(row: &BaseRow<'_>) -> Value {
    json!({
        "path": row.record.rel_path,
        "file": serialize_bases_file(&row.file),
        "effective_frontmatter": row.record.effective_frontmatter,
        "types": row.record.type_names,
        "values": row.values,
    })
}

fn sort_rows(rows: &mut [BaseRow<'_>], view: &ObsidianBaseView) {
    rows.sort_by(|left, right| {
        for sort in &view.sort {
            let left_value = left
                .computed_values
                .get(&sort.property)
                .unwrap_or(&Value::Null);
            let right_value = right
                .computed_values
                .get(&sort.property)
                .unwrap_or(&Value::Null);
            let comparison = compare_json(left_value, right_value);
            if !comparison.is_eq() {
                return if sort.direction.eq_ignore_ascii_case("DESC") {
                    comparison.reverse()
                } else {
                    comparison
                };
            }
        }
        left.record.rel_path.cmp(&right.record.rel_path)
    });
}

fn base_groups(rows: &[BaseRow<'_>], group_by: &BaseGroupBy) -> Value {
    let mut groups = BTreeMap::<String, (Value, usize)>::new();
    for row in rows {
        let value = row
            .computed_values
            .get(group_by.property())
            .cloned()
            .unwrap_or(Value::Null);
        let key = canonical_json(&value);
        let entry = groups.entry(key).or_insert((value, 0));
        entry.1 += 1;
    }
    let mut groups = groups.into_values().collect::<Vec<_>>();
    groups.sort_by(|left, right| compare_json(&left.0, &right.0));
    if group_by.direction().eq_ignore_ascii_case("DESC") {
        groups.reverse();
    }
    Value::Array(groups.into_iter().map(|(value, count)| json!({"values": {group_by.property(): value}, "count": count, "summaries": {}})).collect())
}

pub(crate) fn evaluate_property(
    property: &str,
    context: &BasesEvaluationContext,
) -> Result<Value, String> {
    if let Some(name) = property.strip_prefix("formula.") {
        expression::evaluate(&format!("formula.{name}"), context)
    } else if property.starts_with("file.") || property.starts_with("note[") {
        expression::evaluate(property, context)
    } else {
        expression::evaluate(&format!("note[{property:?}]"), context)
    }
}

pub(crate) fn combined_filter_matches(
    shared: Option<&BaseFilter>,
    local: Option<&BaseFilter>,
    context: &BasesEvaluationContext,
) -> Result<bool, String> {
    Ok(filter_matches(shared, context)? && filter_matches(local, context)?)
}

fn filter_matches(
    filter: Option<&BaseFilter>,
    context: &BasesEvaluationContext,
) -> Result<bool, String> {
    let Some(filter) = filter else {
        return Ok(true);
    };
    match filter {
        BaseFilter::Expression(expression) => expression::matches(expression, context),
        BaseFilter::Logical(logical) => {
            if let Some(filters) = &logical.and {
                for filter in filters.values() {
                    if !filter_matches(Some(filter), context)? {
                        return Ok(false);
                    }
                }
            }
            if let Some(filters) = &logical.or {
                let mut matched = false;
                for filter in filters.values() {
                    if filter_matches(Some(filter), context)? {
                        matched = true;
                        break;
                    }
                }
                if !matched {
                    return Ok(false);
                }
            }
            if let Some(filters) = &logical.not {
                for filter in filters.values() {
                    if filter_matches(Some(filter), context)? {
                        return Ok(false);
                    }
                }
            }
            Ok(true)
        }
    }
}

pub(crate) fn validate_base_expressions(
    document: &ObsidianBaseDocument,
    view: &ObsidianBaseView,
    path: &str,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for (field, expression) in document
        .formulas
        .iter()
        .map(|(name, expression)| (format!("formulas.{name}"), expression.as_str()))
        .chain(
            filter_expressions(document.filters.as_ref())
                .into_iter()
                .map(|expression| ("filters".to_string(), expression)),
        )
        .chain(
            filter_expressions(view.filters.as_ref())
                .into_iter()
                .map(|expression| ("views.filters".to_string(), expression)),
        )
    {
        if let Err(error) = expression::validate(expression) {
            diagnostics.push(
                Diagnostic::error(
                    "invalid_view",
                    format!("Invalid Obsidian Bases expression: {error}"),
                    Some(path.to_string()),
                )
                .with_field(field),
            );
        }
    }
    diagnostics
}

pub(crate) fn base_uses_backlinks(
    document: &ObsidianBaseDocument,
    view: &ObsidianBaseView,
) -> bool {
    document
        .formulas
        .values()
        .map(String::as_str)
        .chain(filter_expressions(document.filters.as_ref()))
        .chain(filter_expressions(view.filters.as_ref()))
        .any(|expression| expression.contains("backlinks"))
        || view
            .order
            .iter()
            .map(String::as_str)
            .chain(view.sort.iter().map(|sort| sort.property.as_str()))
            .chain(view.group_by.iter().map(BaseGroupBy::property))
            .any(|property| property.contains("backlinks"))
}

fn filter_expressions(filter: Option<&BaseFilter>) -> Vec<&str> {
    let Some(filter) = filter else {
        return Vec::new();
    };
    match filter {
        BaseFilter::Expression(expression) => vec![expression],
        BaseFilter::Logical(logical) => logical
            .and
            .iter()
            .chain(&logical.or)
            .chain(&logical.not)
            .flat_map(|filters| filters.values())
            .flat_map(|filter| filter_expressions(Some(filter)))
            .collect(),
    }
}

fn record_file(record: &FileRecord) -> BasesFile {
    let mut file = file_parts(&record.rel_path);
    file.size = record.file_size;
    file.properties = record
        .effective_frontmatter
        .as_object()
        .cloned()
        .unwrap_or_default();
    file.ctime = record.file_ctime_iso.clone();
    file.mtime = record.file_mtime_iso.clone();
    file.tags = frontmatter_tags(&record.effective_frontmatter);
    for tag in extract_tags_from_body(&record.body) {
        if !file.tags.contains(&tag) {
            file.tags.push(tag);
        }
    }
    file.links = extract_links_from_body(&record.body)
        .into_iter()
        .map(|path| BasesLink {
            path,
            ..Default::default()
        })
        .collect();
    file.embeds = extract_embeds_from_body(&record.body)
        .into_iter()
        .map(|path| BasesLink {
            path,
            ..Default::default()
        })
        .collect();
    file
}

fn file_parts(path: &str) -> BasesFile {
    let name = path.rsplit('/').next().unwrap_or(path).to_string();
    let (basename, extension) = name
        .rsplit_once('.')
        .map(|(basename, extension)| (basename.to_string(), extension.to_string()))
        .unwrap_or((name.clone(), String::new()));
    BasesFile {
        path: path.to_string(),
        name,
        basename,
        folder: path
            .rsplit_once('/')
            .map(|(folder, _)| folder.to_string())
            .unwrap_or_default(),
        extension,
        ..Default::default()
    }
}

fn frontmatter_tags(frontmatter: &Value) -> Vec<String> {
    match frontmatter.get("tags") {
        Some(Value::String(value)) => vec![value.trim_start_matches('#').to_string()],
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(|value| value.trim_start_matches('#').to_string())
            .collect(),
        _ => Vec::new(),
    }
}

fn link_resolutions(files: &[BasesFile]) -> BTreeMap<String, Option<String>> {
    let mut resolutions = BTreeMap::new();
    for file in files {
        for key in [&file.path, &file.name, &file.basename] {
            resolutions
                .entry(key.clone())
                .or_insert_with(|| Some(file.path.clone()));
            resolutions
                .entry(key.to_lowercase())
                .or_insert_with(|| Some(file.path.clone()));
        }
        let path_without_extension = file
            .path
            .strip_suffix(".md")
            .unwrap_or(&file.path)
            .to_string();
        resolutions
            .entry(path_without_extension)
            .or_insert_with(|| Some(file.path.clone()));
    }
    resolutions
}

pub(crate) fn obsidian_source_paths(collection: &Collection) -> Vec<PathBuf> {
    let includes = collection
        .config_extensions
        .get("x-obsidian")
        .and_then(|value| value.get("bases"))
        .and_then(|value| value.get("include"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    if includes.is_empty() {
        return Vec::new();
    }
    let patterns = includes
        .iter()
        .filter_map(|pattern| glob_regex(pattern).ok())
        .collect::<Vec<_>>();
    WalkDir::new(&collection.root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("base"))
        .filter(|entry| {
            relative_path(&collection.root, entry.path()).is_some_and(|path| {
                super::normalized_source_path(&path).is_some()
                    && patterns.iter().any(|pattern| pattern.is_match(&path))
            })
        })
        .map(|entry| entry.into_path())
        .collect()
}

pub(crate) fn is_configured_obsidian_source(collection: &Collection, path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    collection
        .config_extensions
        .get("x-obsidian")
        .and_then(|value| value.get("bases"))
        .and_then(|value| value.get("include"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|pattern| glob_regex(pattern).ok())
        .any(|pattern| pattern.is_match(&normalized))
}

fn glob_regex(pattern: &str) -> Result<regex::Regex, regex::Error> {
    let mut output = String::from("^");
    let chars = pattern.replace('\\', "/").chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '*' if chars.get(index + 1) == Some(&'*') => {
                if chars.get(index + 2) == Some(&'/') {
                    output.push_str("(?:.*/)?");
                    index += 3;
                } else {
                    output.push_str(".*");
                    index += 2;
                }
            }
            '*' => {
                output.push_str("[^/]*");
                index += 1;
            }
            '?' => {
                output.push_str("[^/]");
                index += 1;
            }
            character => {
                output.push_str(&regex::escape(&character.to_string()));
                index += 1;
            }
        }
    }
    output.push('$');
    regex::Regex::new(&output)
}

fn safe_view_path(collection: &Collection, path: &str) -> Result<PathBuf, Box<Diagnostic>> {
    let normalized = super::normalized_source_path(path).ok_or_else(|| {
        Box::new(Diagnostic::error(
            "invalid_path",
            "View source path must be portable and must not use hidden filesystem components.",
            Some(path.to_string()),
        ))
    })?;
    crate::operations::ensure_safe_relative_path(normalized.as_str(), collection.spec_profile)
        .map_err(|_| {
            Box::new(Diagnostic::error(
                "invalid_path",
                "View source path must remain inside the collection.",
                Some(path.to_string()),
            ))
        })?;
    crate::operations::ensure_no_symlink_components(
        &collection.root,
        normalized.as_str(),
        collection.spec_profile,
    )
    .map_err(|_| {
        Box::new(Diagnostic::error(
            "path_traversal",
            "View source path traverses a symbolic link.",
            Some(path.to_string()),
        ))
    })?;
    Ok(normalized.under(&collection.root))
}

fn relative_path(root: &Path, path: &Path) -> Option<String> {
    Some(
        path.strip_prefix(root)
            .ok()?
            .to_string_lossy()
            .replace('\\', "/"),
    )
}
fn file_revision(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|bytes| revision(&bytes))
}
fn revision(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", hex_digest(&digest))
}
fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(object) => format!("{{{}}}", {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort();
            keys.into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        canonical_json(&object[key])
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        }),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}
fn compare_json(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Greater,
        (_, Value::Null) => std::cmp::Ordering::Less,
        (Value::Number(left), Value::Number(right)) => left
            .as_f64()
            .partial_cmp(&right.as_f64())
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(left), Value::String(right)) => left.cmp(right),
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        _ => canonical_json(left).cmp(&canonical_json(right)),
    }
}

struct UtcClock;
impl UtcClock {
    fn capture() -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        chrono::DateTime::<chrono::Utc>::from_timestamp(now.as_secs() as i64, now.subsec_nanos())
            .unwrap_or(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH)
            .to_rfc3339()
    }
}

fn failed(code: &str, message: impl Into<String>, path: Option<String>) -> OperationResult {
    OperationResult {
        valid: false,
        result: json!({}),
        diagnostics: vec![Diagnostic::error(code, message, path)],
    }
}

trait DiagnosticField {
    fn with_field(self, field: String) -> Self;
}
impl DiagnosticField for Diagnostic {
    fn with_field(mut self, field: String) -> Self {
        self.field = Some(field);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globstar_matches_direct_and_nested_bases() {
        let pattern = glob_regex("TaskNotes/Views/**/*.base").unwrap();
        assert!(pattern.is_match("TaskNotes/Views/tasks.base"));
        assert!(pattern.is_match("TaskNotes/Views/nested/tasks.base"));
        assert!(!pattern.is_match("Views/tasks.base"));
    }
}
