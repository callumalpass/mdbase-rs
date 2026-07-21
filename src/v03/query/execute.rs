use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde_json::{json, Map, Value};

use super::model::{Direction, FrontmatterMode, OrderBy, Query};
use super::preflight::{self, CompiledQuery, CompiledSelection};
use crate::expressions::evaluator::{
    extract_embeds_from_body, extract_links_from_body, extract_tags_from_body, EvalContext,
    EvaluationClock, NoteNamespaceSource, ResolvedFileData,
};
use crate::query::cache_source::FileRecord;
use crate::v03::{cel, validate_query, Diagnostic, OperationResult};
use crate::Collection;

type LinkGraph = Option<Arc<HashMap<String, Vec<String>>>>;

struct Candidate {
    path: String,
    types: Vec<String>,
    raw: Value,
    effective: Value,
    body: String,
    file: Value,
    projections: Map<String, Value>,
    values: Map<String, Value>,
}

pub(crate) fn execute(collection: &Collection, input: &Value) -> OperationResult {
    let schema_diagnostics = validate_query(input)
        .into_iter()
        .map(invalid_schema_diagnostic)
        .collect::<Vec<_>>();
    if !schema_diagnostics.is_empty() {
        return failed(schema_diagnostics);
    }

    let query = match serde_json::from_value::<Query>(input.clone()) {
        Ok(query) => query,
        Err(error) => {
            return failed(vec![Diagnostic::error(
                "invalid_query",
                format!("Query could not be decoded: {error}"),
                None,
            )]);
        }
    };
    let compiled = match preflight::compile(query) {
        Ok(compiled) => compiled,
        Err(diagnostics) => return failed(diagnostics),
    };
    let clock = match cel::operation_clock(collection.settings.timezone.as_deref()) {
        Ok(clock) => clock,
        Err(error) => {
            return failed(vec![Diagnostic::error(error.code, error.message, None)]);
        }
    };

    let ((records, all_files, backlinks), _) = collection.load_query_data_profiled(false, true);
    let type_definitions = Arc::new(collection.types.clone());
    let context = match load_context(
        collection,
        &compiled.query,
        all_files.clone(),
        backlinks.clone(),
        type_definitions.clone(),
    ) {
        Ok(context) => context,
        Err(diagnostic) => return failed(vec![*diagnostic]),
    };

    let mut diagnostics = Vec::new();
    let mut candidates = Vec::new();
    for record in &records {
        let (types, match_failures) = collection
            .determine_types_for_path_checked(&record.raw_frontmatter, Some(&record.rel_path));
        diagnostics.extend(match_failures.into_iter().map(|(type_name, failure)| {
            query_evaluation_diagnostic(
                &record.rel_path,
                "match.expr",
                "match",
                failure,
                Some(type_name),
            )
        }));
        if !compiled.query.types.is_empty()
            && !types.iter().any(|type_name| {
                compiled
                    .query
                    .types
                    .iter()
                    .any(|wanted| wanted.eq_ignore_ascii_case(type_name))
            })
        {
            continue;
        }

        let effective = collection.apply_defaults(&record.raw_frontmatter, &types);
        let effective = collection.coerce_types(&effective, &types);
        let effective = collection.evaluate_computed_fields(
            effective,
            &types,
            &record.rel_path,
            Some(&record.body),
        );
        let file = file_value(record, &effective);
        let mut projections = Map::new();
        for (name, expression) in &compiled.projections {
            let context = candidate_context(
                collection,
                record,
                &types,
                &effective,
                &projections,
                context.clone(),
                all_files.clone(),
                backlinks.clone(),
                type_definitions.clone(),
            );
            match cel::evaluate_compiled(expression, &context, &clock) {
                Ok(value) => {
                    projections.insert(name.clone(), value);
                }
                Err(error) => {
                    diagnostics.push(query_evaluation_diagnostic(
                        &record.rel_path,
                        &format!("projections.{name}"),
                        "query_projection",
                        error,
                        None,
                    ));
                    projections.insert(name.clone(), Value::Null);
                }
            }
        }

        let expression_context = candidate_context(
            collection,
            record,
            &types,
            &effective,
            &projections,
            context.clone(),
            all_files.clone(),
            backlinks.clone(),
            type_definitions.clone(),
        );
        if let Some(where_expression) = &compiled.where_expression {
            match cel::evaluate_compiled(where_expression, &expression_context, &clock) {
                Ok(Value::Bool(true)) => {}
                Ok(_) => continue,
                Err(error) => {
                    diagnostics.push(query_evaluation_diagnostic(
                        &record.rel_path,
                        "where",
                        "query_filter",
                        error,
                        None,
                    ));
                    continue;
                }
            }
        }

        let mut values = Map::new();
        for selection in &compiled.selections {
            match selection {
                CompiledSelection::Field { source, name } => {
                    values.insert(
                        name.clone(),
                        namespace_value(source, &effective, &projections, &values, &file),
                    );
                }
                CompiledSelection::Expression { expression, name } => {
                    let value =
                        match cel::evaluate_compiled(expression, &expression_context, &clock) {
                            Ok(value) => value,
                            Err(error) => {
                                diagnostics.push(query_evaluation_diagnostic(
                                    &record.rel_path,
                                    &format!("select.{name}"),
                                    "query_selection",
                                    error,
                                    None,
                                ));
                                Value::Null
                            }
                        };
                    values.insert(name.clone(), value);
                }
            }
        }

        candidates.push(Candidate {
            path: record.rel_path.clone(),
            types,
            raw: record.raw_frontmatter.clone(),
            effective,
            body: record.body.clone(),
            file,
            projections,
            values,
        });
    }

    sort_candidates(&mut candidates, &compiled.query.order_by);
    let groups = build_groups(&candidates, &compiled, &clock, &mut diagnostics);
    let total_count = candidates.len();
    let offset = usize::try_from(compiled.query.offset)
        .unwrap_or(usize::MAX)
        .min(total_count);
    let available = &candidates[offset..];
    let page = if let Some(limit) = compiled.query.limit {
        available
            .iter()
            .take(usize::try_from(limit).unwrap_or(usize::MAX))
            .collect::<Vec<_>>()
    } else {
        available.iter().collect::<Vec<_>>()
    };
    let has_more = offset.saturating_add(page.len()) < total_count;
    let results = page
        .into_iter()
        .map(|candidate| serialize_candidate(candidate, &compiled.query))
        .collect::<Vec<_>>();
    let mut meta = json!({
        "total_count": total_count,
        "has_more": has_more,
    });
    if let Some(context) = &compiled.query.context {
        meta["context"] = json!({"path": context.this.path});
    }
    if let Some(groups) = groups {
        meta["groups"] = Value::Array(groups);
    }
    let serialized_diagnostics = serde_json::to_value(&diagnostics).unwrap_or_else(|_| json!([]));
    OperationResult {
        valid: true,
        result: json!({
            "results": results,
            "meta": meta,
            "diagnostics": serialized_diagnostics,
        }),
        diagnostics,
    }
}

fn load_context(
    collection: &Collection,
    query: &Query,
    all_files: Option<Arc<Vec<ResolvedFileData>>>,
    backlinks: LinkGraph,
    type_definitions: Arc<HashMap<String, crate::types::schema::TypeDef>>,
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
fn candidate_context(
    collection: &Collection,
    record: &FileRecord,
    types: &[String],
    effective: &Value,
    projections: &Map<String, Value>,
    this_context: Option<Box<EvalContext>>,
    all_files: Option<Arc<Vec<ResolvedFileData>>>,
    backlinks: LinkGraph,
    type_definitions: Arc<HashMap<String, crate::types::schema::TypeDef>>,
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

fn file_value(record: &FileRecord, effective: &Value) -> Value {
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

fn namespace_value(
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

fn candidate_value(candidate: &Candidate, field: &str) -> Value {
    if let Some(name) = field.strip_prefix("projection.") {
        return candidate
            .projections
            .get(name)
            .cloned()
            .unwrap_or(Value::Null);
    }
    if let Some(name) = field.strip_prefix("file.") {
        return candidate.file.get(name).cloned().unwrap_or(Value::Null);
    }
    candidate
        .values
        .get(field)
        .or_else(|| candidate.effective.get(field))
        .cloned()
        .unwrap_or(Value::Null)
}

fn sort_candidates(candidates: &mut [Candidate], order_by: &[OrderBy]) {
    candidates.sort_by(|left, right| {
        for order in order_by {
            let comparison = compare_values(
                &candidate_value(left, &order.field),
                &candidate_value(right, &order.field),
                order.direction,
            );
            if comparison != Ordering::Equal {
                return comparison;
            }
        }
        left.path.cmp(&right.path)
    });
}

fn compare_values(left: &Value, right: &Value, direction: Direction) -> Ordering {
    let ascending = match (left.is_null(), right.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => match (left, right) {
            (Value::Number(left), Value::Number(right)) => left
                .as_f64()
                .partial_cmp(&right.as_f64())
                .unwrap_or(Ordering::Equal),
            (Value::String(left), Value::String(right)) => left.cmp(right),
            (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
            (Value::Array(left), Value::Array(right)) => left.len().cmp(&right.len()),
            (Value::Object(left), Value::Object(right)) => left.len().cmp(&right.len()),
            _ => Ordering::Equal,
        },
    };
    match direction {
        Direction::Asc => ascending,
        Direction::Desc => ascending.reverse(),
    }
}

fn build_groups(
    candidates: &[Candidate],
    compiled: &CompiledQuery,
    clock: &EvaluationClock,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Vec<Value>> {
    if compiled.query.group_by.is_empty() && compiled.query.summaries.is_empty() {
        return None;
    }
    if compiled.query.group_by.is_empty() {
        return Some(vec![json!({
            "values": {},
            "count": candidates.len(),
            "summaries": summarize(candidates.iter(), compiled, clock, diagnostics),
        })]);
    }

    let mut groups = BTreeMap::<String, (Map<String, Value>, Vec<&Candidate>)>::new();
    for candidate in candidates {
        let values = compiled
            .query
            .group_by
            .iter()
            .map(|group| {
                (
                    group.field.clone(),
                    candidate_value(candidate, &group.field),
                )
            })
            .collect::<Map<_, _>>();
        let key = serde_json::to_string(&values).expect("JSON values always serialize");
        groups
            .entry(key)
            .or_insert_with(|| (values, Vec::new()))
            .1
            .push(candidate);
    }
    let mut groups = groups.into_values().collect::<Vec<_>>();
    groups.sort_by(|(left, _), (right, _)| {
        for group in &compiled.query.group_by {
            let comparison = compare_values(
                left.get(&group.field).unwrap_or(&Value::Null),
                right.get(&group.field).unwrap_or(&Value::Null),
                group.direction,
            );
            if comparison != Ordering::Equal {
                return comparison;
            }
        }
        Ordering::Equal
    });
    Some(
        groups
            .into_iter()
            .map(|(values, candidates)| {
                json!({
                    "values": values,
                    "count": candidates.len(),
                    "summaries": summarize(candidates.into_iter(), compiled, clock, diagnostics),
                })
            })
            .collect(),
    )
}

fn summarize<'a>(
    candidates: impl Iterator<Item = &'a Candidate>,
    compiled: &CompiledQuery,
    clock: &EvaluationClock,
    diagnostics: &mut Vec<Diagnostic>,
) -> Value {
    let candidates = candidates.collect::<Vec<_>>();
    let mut output = Map::new();
    for summary in &compiled.query.summaries {
        let values = candidates
            .iter()
            .map(|candidate| candidate_value(candidate, &summary.field))
            .collect::<Vec<_>>();
        let result = if preflight::is_builtin_summary(&summary.function) {
            builtin_summary(&summary.function, &values).map_err(|message| Diagnostic {
                severity: "warning".to_string(),
                code: "expression_evaluation_error".to_string(),
                message,
                path: None,
                field: Some(format!("summaries.{}", summary.output_name())),
                type_name: None,
                schema_location: None,
                details: Some(json!({"context": "query_summary"})),
            })
        } else {
            let expression = &compiled.summary_functions[&summary.function];
            let mut context = EvalContext::empty();
            context.frontmatter = json!({"values": values});
            context.string_concat = false;
            cel::evaluate_compiled(expression, &context, clock).map_err(|error| {
                query_evaluation_diagnostic(
                    "query",
                    &format!("summaries.{}", summary.output_name()),
                    "query_summary",
                    error,
                    None,
                )
            })
        };
        match result {
            Ok(value) => {
                output.insert(summary.output_name().to_string(), value);
            }
            Err(diagnostic) => {
                diagnostics.push(diagnostic);
                output.insert(summary.output_name().to_string(), Value::Null);
            }
        }
    }
    Value::Object(output)
}

fn builtin_summary(function: &str, values: &[Value]) -> Result<Value, String> {
    if function == "count" {
        return Ok(json!(values.len()));
    }
    if matches!(function, "empty" | "filled") {
        let empty = values.iter().filter(|value| is_empty(value)).count();
        return Ok(json!(if function == "empty" {
            empty
        } else {
            values.len() - empty
        }));
    }
    let non_null = values
        .iter()
        .filter(|value| !value.is_null())
        .collect::<Vec<_>>();
    if non_null.is_empty() {
        return Ok(Value::Null);
    }
    match function {
        "sum" | "average" | "minimum" | "maximum" => {
            let numbers = non_null
                .iter()
                .map(|value| value.as_f64())
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| format!("Summary '{function}' received a non-numeric value."))?;
            let value = match function {
                "sum" => numbers.iter().sum(),
                "average" => numbers.iter().sum::<f64>() / numbers.len() as f64,
                "minimum" => numbers.iter().copied().fold(f64::INFINITY, f64::min),
                "maximum" => numbers.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                _ => unreachable!(),
            };
            Ok(number_value(value))
        }
        "earliest" | "latest" => {
            let strings = non_null
                .iter()
                .map(|value| value.as_str())
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| format!("Summary '{function}' received a non-string value."))?;
            let selected = if function == "earliest" {
                strings.into_iter().min()
            } else {
                strings.into_iter().max()
            };
            Ok(selected.map_or(Value::Null, |value| json!(value)))
        }
        _ => unreachable!("preflight accepts only known built-ins"),
    }
}

fn number_value(value: f64) -> Value {
    if value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
        json!(value as i64)
    } else {
        serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(value) => value.is_empty(),
        Value::Array(value) => value.is_empty(),
        Value::Object(value) => value.is_empty(),
        Value::Bool(_) | Value::Number(_) => false,
    }
}

fn serialize_candidate(candidate: &Candidate, query: &Query) -> Value {
    let mut result = Map::from_iter([
        ("path".to_string(), Value::String(candidate.path.clone())),
        ("file".to_string(), candidate.file.clone()),
        (
            "types".to_string(),
            Value::Array(candidate.types.iter().cloned().map(Value::String).collect()),
        ),
    ]);
    match query.frontmatter {
        FrontmatterMode::Effective => {
            result.insert("frontmatter".to_string(), candidate.effective.clone());
        }
        FrontmatterMode::Raw => {
            result.insert("frontmatter".to_string(), candidate.raw.clone());
        }
        FrontmatterMode::Both => {
            result.insert("frontmatter".to_string(), candidate.effective.clone());
            result.insert("raw_frontmatter".to_string(), candidate.raw.clone());
        }
    }
    if query.select.is_some() {
        result.insert(
            "values".to_string(),
            Value::Object(candidate.values.clone()),
        );
    }
    if query.include_body {
        result.insert("body".to_string(), Value::String(candidate.body.clone()));
    }
    Value::Object(result)
}

fn invalid_schema_diagnostic(mut diagnostic: Diagnostic) -> Diagnostic {
    let original_code = diagnostic.code;
    diagnostic.code = "invalid_query".to_string();
    diagnostic.path = None;
    let mut details = diagnostic
        .details
        .take()
        .and_then(|details| details.as_object().cloned())
        .unwrap_or_default();
    details.insert("schema_code".to_string(), Value::String(original_code));
    diagnostic.details = Some(Value::Object(details));
    diagnostic
}

fn query_evaluation_diagnostic(
    path: &str,
    field: &str,
    context: &str,
    failure: cel::CelFailure,
    type_name: Option<String>,
) -> Diagnostic {
    Diagnostic {
        severity: "warning".to_string(),
        code: "expression_evaluation_error".to_string(),
        message: failure.message,
        path: Some(path.to_string()),
        field: Some(field.to_string()),
        type_name,
        schema_location: None,
        details: Some(json!({
            "context": context,
            "evaluator_code": failure.code,
        })),
    }
}

fn failed(diagnostics: Vec<Diagnostic>) -> OperationResult {
    OperationResult {
        valid: false,
        result: json!({}),
        diagnostics,
    }
}
