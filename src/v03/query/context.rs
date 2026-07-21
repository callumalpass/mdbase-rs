use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Map, Value};

use super::model::Query;
use crate::expressions::evaluator::{
    extract_embeds_from_body, extract_links_from_body, extract_tags_from_body, EvalContext,
    NoteNamespaceSource, ResolvedFileData,
};
use crate::query::cache_source::FileRecord;
use crate::types::schema::TypeDef;
use crate::v03::{cel, Diagnostic};
use crate::Collection;

pub(super) type LinkGraph = Option<Arc<HashMap<String, Vec<String>>>>;

pub(super) fn load_context(
    collection: &Collection,
    query: &Query,
    all_files: Option<Arc<Vec<ResolvedFileData>>>,
    backlinks: LinkGraph,
    type_definitions: Arc<HashMap<String, TypeDef>>,
) -> Result<Option<Box<EvalContext>>, Box<Diagnostic>> {
    let Some(context) = &query.context else {
        return Ok(None);
    };
    let path = &context.this.path;
    let read = collection.read(&json!({"path": path}));
    if read.get("error").is_some() {
        return Err(Box::new(Diagnostic::error(
            "context_not_found",
            format!("Query context record '{path}' was not found."),
            Some(path.clone()),
        )));
    }
    let effective = read
        .get("frontmatter")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let raw = read
        .get("raw_frontmatter")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let types = read
        .get("types")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect::<Vec<_>>();
    let mut bindings = cel::enrich_record_bindings(
        &effective,
        &raw,
        cel::known_fields(collection, &types).iter(),
    );
    if let Some(object) = bindings.as_object_mut() {
        object.insert(
            "types".to_string(),
            Value::Array(types.iter().cloned().map(Value::String).collect()),
        );
    }
    Ok(Some(Box::new(EvalContext {
        frontmatter: bindings,
        raw_frontmatter: Some(raw),
        file_path: Some(path.clone()),
        body: read.get("body").and_then(Value::as_str).map(String::from),
        file_size: read.pointer("/file/size").and_then(Value::as_u64),
        file_mtime: read
            .pointer("/file/mtime")
            .and_then(Value::as_str)
            .map(String::from),
        file_ctime: None,
        this_context: None,
        all_files,
        traversal_depth: std::cell::Cell::new(0),
        backlinks_index: backlinks,
        type_names: Some(types),
        types: Some(type_definitions),
        note_namespace_source: NoteNamespaceSource::Effective,
        string_concat: false,
    })))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn candidate_context(
    collection: &Collection,
    record: &FileRecord,
    types: &[String],
    effective: &Value,
    projections: &Map<String, Value>,
    this_context: Option<Box<EvalContext>>,
    all_files: Option<Arc<Vec<ResolvedFileData>>>,
    backlinks: LinkGraph,
    type_definitions: Arc<HashMap<String, TypeDef>>,
) -> EvalContext {
    let mut bindings = cel::enrich_record_bindings(
        effective,
        &record.raw_frontmatter,
        cel::known_fields(collection, types).iter(),
    );
    if let Some(object) = bindings.as_object_mut() {
        object.insert("projection".to_string(), Value::Object(projections.clone()));
        object.insert(
            "types".to_string(),
            Value::Array(types.iter().cloned().map(Value::String).collect()),
        );
    }
    EvalContext {
        frontmatter: bindings,
        raw_frontmatter: Some(record.raw_frontmatter.clone()),
        file_path: Some(record.rel_path.clone()),
        body: Some(record.body.clone()),
        file_size: Some(record.file_size),
        file_mtime: record.file_mtime_iso.clone(),
        file_ctime: record.file_ctime_iso.clone(),
        this_context,
        all_files,
        traversal_depth: std::cell::Cell::new(0),
        backlinks_index: backlinks,
        type_names: Some(types.to_vec()),
        types: Some(type_definitions),
        note_namespace_source: NoteNamespaceSource::Effective,
        string_concat: false,
    }
}

pub(super) fn file_value(record: &FileRecord, effective: &Value) -> Value {
    let mut tags = effective
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect::<Vec<_>>();
    for tag in extract_tags_from_body(&record.body) {
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    json!({
        "path": record.rel_path,
        "name": std::path::Path::new(&record.rel_path)
            .file_name().and_then(|value| value.to_str()).unwrap_or(""),
        "folder": std::path::Path::new(&record.rel_path)
            .parent().and_then(|value| value.to_str()).unwrap_or(""),
        "size": record.file_size,
        "mtime": record.file_mtime_iso,
        "ctime": record.file_ctime_iso,
        "tags": tags,
        "links": extract_links_from_body(&record.body),
        "embeds": extract_embeds_from_body(&record.body),
    })
}

pub(super) fn namespace_value(
    field: &str,
    effective: &Value,
    projections: &Map<String, Value>,
    values: &Map<String, Value>,
    file: &Value,
) -> Value {
    if let Some(name) = field.strip_prefix("projection.") {
        return projections.get(name).cloned().unwrap_or(Value::Null);
    }
    if let Some(name) = field.strip_prefix("file.") {
        return file.get(name).cloned().unwrap_or(Value::Null);
    }
    values
        .get(field)
        .or_else(|| effective.get(field))
        .cloned()
        .unwrap_or(Value::Null)
}
