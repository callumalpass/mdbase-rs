use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::context::{candidate_context, file_value, load_context, namespace_value};
use super::diagnostics;
use super::model::{Candidate, Query};
use super::preflight::{self, CompiledSelection};
use super::result::{build_groups, compare_values, serialize_candidate, sort_candidates};
use crate::query::cache_source::FileRecord;
use crate::v03::{cel, validate_query, Diagnostic, OperationResult};
use crate::Collection;

/// Payload-free phase timings and counters for one canonical v0.3 query.
///
/// Hosts and local profiling tools can retain this structure without risking
/// collection paths, frontmatter, bodies, or query inputs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueryPerformance {
    pub total_us: u64,
    pub schema_us: u64,
    pub preflight_us: u64,
    pub clock_us: u64,
    pub load_us: u64,
    pub cache_open_us: u64,
    pub cache_refresh_us: u64,
    pub records_load_us: u64,
    pub all_files_us: u64,
    pub link_graph_us: u64,
    pub context_us: u64,
    pub evaluate_us: u64,
    pub sort_us: u64,
    pub groups_us: u64,
    pub serialize_us: u64,
    pub records_loaded: usize,
    pub candidates: usize,
    pub results: usize,
    pub cache_used: bool,
    pub cache_fallback: bool,
    pub link_graph_built: bool,
}

pub(crate) fn execute(collection: &Collection, input: &Value) -> OperationResult {
    execute_profiled(collection, input).0
}

pub(crate) fn execute_profiled(
    collection: &Collection,
    input: &Value,
) -> (OperationResult, QueryPerformance) {
    let total_started = Instant::now();
    let mut performance = QueryPerformance::default();
    macro_rules! finish {
        ($result:expr) => {{
            performance.total_us = micros(total_started.elapsed());
            return ($result, performance);
        }};
    }

    let phase = Instant::now();
    let schema_diagnostics = validate_query(input)
        .into_iter()
        .map(diagnostics::invalid_schema)
        .collect::<Vec<_>>();
    performance.schema_us = micros(phase.elapsed());
    if !schema_diagnostics.is_empty() {
        finish!(diagnostics::failed(schema_diagnostics));
    }

    let phase = Instant::now();
    let query = match serde_json::from_value::<Query>(input.clone()) {
        Ok(query) => query,
        Err(error) => {
            finish!(diagnostics::failed(vec![Diagnostic::error(
                "invalid_query",
                format!("Query could not be decoded: {error}"),
                None,
            )]));
        }
    };
    let compiled = match preflight::compile(query) {
        Ok(compiled) => compiled,
        Err(diagnostics) => finish!(diagnostics::failed(diagnostics)),
    };
    performance.preflight_us = micros(phase.elapsed());

    let phase = Instant::now();
    let clock = match cel::operation_clock(collection.settings.timezone.as_deref()) {
        Ok(clock) => clock,
        Err(error) => {
            finish!(diagnostics::failed(vec![Diagnostic::error(
                error.code,
                error.message,
                None,
            )]));
        }
    };
    performance.clock_us = micros(phase.elapsed());

    // A failing CEL type matcher contributes query diagnostics for every
    // candidate. Keep the full evaluation plan in that uncommon case so the
    // metadata-page optimization cannot change observable diagnostics.
    let has_diagnostic_matchers = collection.types.values().any(|type_definition| {
        type_definition
            .match_rules
            .as_ref()
            .is_some_and(|rules| rules.match_expr.is_some())
    });
    let metadata_page_plan = compiled.supports_metadata_page_plan() && !has_diagnostic_matchers;
    if metadata_page_plan {
        let order_by = compiled
            .query
            .order_by
            .iter()
            .map(|order| {
                (
                    order.field.as_str(),
                    matches!(order.direction, super::model::Direction::Desc),
                )
            })
            .collect::<Vec<_>>();
        let phase = Instant::now();
        let loaded = collection.load_query_metadata_page_profiled(
            &order_by,
            compiled.query.offset,
            compiled.query.limit,
        );
        if let Some(page) = loaded {
            performance.load_us = micros(phase.elapsed());
            apply_load_performance(&mut performance, &page.performance);
            performance.candidates = page.total;
            let offset = usize::try_from(compiled.query.offset)
                .unwrap_or(usize::MAX)
                .min(page.total);
            let has_more = offset.saturating_add(page.records.len()) < page.total;
            let records = page.records.iter().collect::<Vec<_>>();
            let result = build_metadata_page_result(
                collection,
                &compiled,
                &records,
                page.total,
                has_more,
                &mut performance,
            );
            finish!(result);
        }
    }

    let phase = Instant::now();
    let needs_link_graph = compiled.requires_link_graph();
    let needs_file_body_metadata = compiled.requires_file_body_metadata();
    let (snapshot, load_profile) = match collection.load_query_data_profiled(true, needs_link_graph)
    {
        Ok(loaded) => loaded,
        Err(error) => {
            let path = error.path().map(|path| {
                path.strip_prefix(&collection.root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/")
            });
            finish!(diagnostics::failed(vec![Diagnostic::error(
                "collection_snapshot_failed",
                error.to_string(),
                path,
            )]));
        }
    };
    let records = snapshot.records;
    let all_files = snapshot.all_files;
    let backlinks = snapshot.backlinks;
    performance.load_us = micros(phase.elapsed());
    if let Some(load) = load_profile {
        apply_load_performance(&mut performance, &load);
    } else {
        performance.records_loaded = records.len();
    }

    if metadata_page_plan {
        let phase = Instant::now();
        let mut ordered = records.iter().collect::<Vec<_>>();
        ordered.sort_by(|left, right| {
            for order in &compiled.query.order_by {
                let comparison = compare_values(
                    &record_metadata_value(left, &order.field),
                    &record_metadata_value(right, &order.field),
                    order.direction,
                );
                if comparison != std::cmp::Ordering::Equal {
                    return comparison;
                }
            }
            left.rel_path.cmp(&right.rel_path)
        });
        performance.sort_us = micros(phase.elapsed());
        performance.candidates = ordered.len();

        let total_count = ordered.len();
        let offset = usize::try_from(compiled.query.offset)
            .unwrap_or(usize::MAX)
            .min(total_count);
        let available = &ordered[offset..];
        let page = if let Some(limit) = compiled.query.limit {
            available
                .iter()
                .take(usize::try_from(limit).unwrap_or(usize::MAX))
                .copied()
                .collect::<Vec<_>>()
        } else {
            available.to_vec()
        };
        let has_more = offset.saturating_add(page.len()) < total_count;

        let result = build_metadata_page_result(
            collection,
            &compiled,
            &page,
            total_count,
            has_more,
            &mut performance,
        );
        finish!(result);
    }

    let type_definitions = Arc::new(collection.types.clone());
    let phase = Instant::now();
    let context = match load_context(
        collection,
        &compiled.query,
        all_files.clone(),
        backlinks.clone(),
        type_definitions.clone(),
    ) {
        Ok(context) => context,
        Err(diagnostic) => finish!(diagnostics::failed(vec![*diagnostic])),
    };
    performance.context_us = micros(phase.elapsed());

    let phase = Instant::now();
    let mut query_diagnostics = Vec::new();
    let mut candidates = Vec::new();
    for record in &records {
        let (types, match_failures) = collection
            .determine_types_for_path_checked(&record.raw_frontmatter, Some(&record.rel_path));
        query_diagnostics.extend(match_failures.into_iter().map(|(type_name, failure)| {
            diagnostics::evaluation(
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
        let file = file_value(record, &effective, needs_file_body_metadata);
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
                    query_diagnostics.push(diagnostics::evaluation(
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
                    query_diagnostics.push(diagnostics::evaluation(
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
                                query_diagnostics.push(diagnostics::evaluation(
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
    performance.evaluate_us = micros(phase.elapsed());
    performance.candidates = candidates.len();

    let phase = Instant::now();
    sort_candidates(&mut candidates, &compiled.query.order_by);
    performance.sort_us = micros(phase.elapsed());

    let phase = Instant::now();
    let groups = build_groups(&candidates, &compiled, &clock, &mut query_diagnostics);
    performance.groups_us = micros(phase.elapsed());
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
    let phase = Instant::now();
    let results = page
        .into_iter()
        .map(|candidate| serialize_candidate(candidate, &compiled.query))
        .collect::<Vec<_>>();
    performance.serialize_us = micros(phase.elapsed());
    performance.results = results.len();
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
    let serialized_diagnostics =
        serde_json::to_value(&query_diagnostics).unwrap_or_else(|_| json!([]));
    let result = OperationResult {
        valid: true,
        result: json!({
            "results": results,
            "meta": meta,
            "diagnostics": serialized_diagnostics,
        }),
        diagnostics: query_diagnostics,
    };
    performance.total_us = micros(total_started.elapsed());
    (result, performance)
}

fn micros(duration: std::time::Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn apply_load_performance(
    performance: &mut QueryPerformance,
    load: &crate::query::cache_source::LoadQueryPerf,
) {
    performance.cache_open_us = millis_to_micros(load.try_open_cache_ms);
    performance.cache_refresh_us = millis_to_micros(load.refresh_cache_ms);
    performance.records_load_us = millis_to_micros(load.load_records_ms);
    performance.all_files_us = millis_to_micros(load.build_all_files_ms);
    performance.link_graph_us = millis_to_micros(load.build_backlinks_ms);
    performance.records_loaded = load.file_records;
    performance.cache_used = load.cache_used;
    performance.cache_fallback = load.cache_fallback;
    performance.link_graph_built = load.built_link_graph;
}

fn build_metadata_page_result(
    collection: &Collection,
    compiled: &preflight::CompiledQuery,
    records: &[&FileRecord],
    total_count: usize,
    has_more: bool,
    performance: &mut QueryPerformance,
) -> OperationResult {
    let phase = Instant::now();
    let mut query_diagnostics = Vec::new();
    let results = records
        .iter()
        .map(|record| {
            let (types, failures) = collection
                .determine_types_for_path_checked(&record.raw_frontmatter, Some(&record.rel_path));
            query_diagnostics.extend(failures.into_iter().map(|(type_name, failure)| {
                diagnostics::evaluation(
                    &record.rel_path,
                    "match.expr",
                    "match",
                    failure,
                    Some(type_name),
                )
            }));
            let effective = collection.apply_defaults(&record.raw_frontmatter, &types);
            let effective = collection.coerce_types(&effective, &types);
            let effective = collection.evaluate_computed_fields(
                effective,
                &types,
                &record.rel_path,
                Some(&record.body),
            );
            let candidate = Candidate {
                path: record.rel_path.clone(),
                types,
                raw: record.raw_frontmatter.clone(),
                effective: effective.clone(),
                body: record.body.clone(),
                file: file_value(record, &effective, true),
                projections: Map::new(),
                values: Map::new(),
            };
            serialize_candidate(&candidate, &compiled.query)
        })
        .collect::<Vec<_>>();
    performance.evaluate_us = micros(phase.elapsed());
    performance.results = results.len();
    let serialized_diagnostics =
        serde_json::to_value(&query_diagnostics).unwrap_or_else(|_| json!([]));
    let meta = json!({
        "total_count": total_count,
        "has_more": has_more,
    });
    OperationResult {
        valid: true,
        result: json!({
            "results": results,
            "meta": meta,
            "diagnostics": serialized_diagnostics,
        }),
        diagnostics: query_diagnostics,
    }
}

fn millis_to_micros(milliseconds: f64) -> u64 {
    if !milliseconds.is_finite() || milliseconds <= 0.0 {
        return 0;
    }
    (milliseconds * 1_000.0).min(u64::MAX as f64) as u64
}

fn record_metadata_value(record: &FileRecord, field: &str) -> Value {
    match field {
        "file.path" => Value::String(record.rel_path.clone()),
        "file.name" => Value::String(
            std::path::Path::new(&record.rel_path)
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("")
                .to_string(),
        ),
        "file.folder" => Value::String(
            std::path::Path::new(&record.rel_path)
                .parent()
                .and_then(|value| value.to_str())
                .unwrap_or("")
                .to_string(),
        ),
        "file.size" => json!(record.file_size),
        "file.mtime" => record
            .file_mtime_iso
            .as_ref()
            .map_or(Value::Null, |value| Value::String(value.clone())),
        "file.ctime" => record
            .file_ctime_iso
            .as_ref()
            .map_or(Value::Null, |value| Value::String(value.clone())),
        _ => Value::Null,
    }
}
