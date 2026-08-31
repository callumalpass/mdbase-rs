use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::context::{candidate_context, file_value, load_context, namespace_value};
use super::diagnostics;
use super::model::{Candidate, Query};
use super::preflight::{self, CompiledSelection};
use super::result::{build_groups, compare_values, serialize_candidate, sort_candidates};
use crate::cel;
use crate::diagnostic::Diagnostic;
use crate::expressions::evaluator::resolve_execution_timezone;
use crate::query::cache_source::{InvalidRecordStub, LocalRecord};
use crate::{Collection, OperationCancellation, OperationCancelled};

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
    /// Bulk snapshot/cache sources opened for candidate records.
    pub record_source_loads: usize,
    /// Point reads performed to hydrate `context.this`.
    pub context_record_loads: usize,
    /// All record sources opened (`record_source_loads + context_record_loads`).
    pub total_source_loads: usize,
    pub candidates: usize,
    pub results: usize,
    pub cache_used: bool,
    pub cache_fallback: bool,
    pub link_graph_built: bool,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct QueryPathProbes {
    pub wire_schema_validations: usize,
    pub wire_query_decodes: usize,
    pub typed_request_json_encodes: usize,
    pub operation_result_decodes: usize,
    pub core_executions: usize,
    /// Bulk snapshot/cache sources opened for candidate records.
    pub record_source_loads: usize,
    /// Point reads performed to hydrate `context.this`.
    pub context_record_loads: usize,
    /// All record sources opened (`record_source_loads + context_record_loads`).
    pub total_source_loads: usize,
}

#[cfg(test)]
thread_local! {
    static QUERY_PATH_PROBES: std::cell::Cell<QueryPathProbes> =
        const { std::cell::Cell::new(QueryPathProbes {
            wire_schema_validations: 0,
            wire_query_decodes: 0,
            typed_request_json_encodes: 0,
            operation_result_decodes: 0,
            core_executions: 0,
            record_source_loads: 0,
            context_record_loads: 0,
            total_source_loads: 0,
        }) };
}

#[cfg(test)]
fn bump_probe(update: impl FnOnce(&mut QueryPathProbes)) {
    QUERY_PATH_PROBES.with(|probes| {
        let mut value = probes.get();
        update(&mut value);
        probes.set(value);
    });
}

#[cfg(test)]
pub(crate) fn reset_query_path_probes() {
    QUERY_PATH_PROBES.with(|probes| probes.set(QueryPathProbes::default()));
}

#[cfg(test)]
pub(crate) fn query_path_probes() -> QueryPathProbes {
    QUERY_PATH_PROBES.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn record_typed_request_json_encode() {
    bump_probe(|probes| probes.typed_request_json_encodes += 1);
}

#[cfg(test)]
pub(crate) fn record_wire_schema_validation() {
    bump_probe(|probes| probes.wire_schema_validations += 1);
}

#[cfg(test)]
pub(crate) fn record_wire_query_decode() {
    bump_probe(|probes| probes.wire_query_decodes += 1);
}

/// Dynamic rows and metadata produced by the already-parsed query core.
pub(crate) struct QueryExecution {
    pub records: Vec<Value>,
    pub total_count: usize,
    pub has_more: bool,
    pub meta: Value,
    pub diagnostics: Vec<Diagnostic>,
}

pub(crate) type QueryEvaluation = Result<QueryExecution, Vec<Diagnostic>>;

pub(crate) fn execute_typed(collection: &Collection, query: Query) -> QueryEvaluation {
    let context = crate::runtime::OperationContext::internal();
    execute_model_profiled_cancellable(
        collection,
        query,
        context.cancellation(),
        false,
        Instant::now(),
        0,
    )
    .expect("the context-free compatibility context is active")
    .0
}

pub(crate) fn execute_model_profiled_cancellable(
    collection: &Collection,
    query: Query,
    cancellation: &OperationCancellation,
    runtime_cache: bool,
    total_started: Instant,
    schema_us: u64,
) -> Result<(QueryEvaluation, QueryPerformance), OperationCancelled> {
    let mut performance = QueryPerformance {
        schema_us,
        ..QueryPerformance::default()
    };
    macro_rules! finish {
        ($result:expr) => {{
            performance.total_us = micros(total_started.elapsed());
            return Ok(($result, performance));
        }};
    }

    cancellation.check()?;
    #[cfg(test)]
    bump_probe(|probes| probes.core_executions += 1);
    let phase = Instant::now();
    let compiled = match preflight::compile(query) {
        Ok(compiled) => compiled,
        Err(diagnostics) => finish!(Err(diagnostics)),
    };
    performance.preflight_us = micros(phase.elapsed());
    cancellation.check()?;

    let phase = Instant::now();
    let timezone = match resolve_execution_timezone(
        compiled.query.timezone.as_deref(),
        collection.settings.timezone.as_deref(),
    ) {
        Ok(timezone) => timezone,
        Err(message) => {
            finish!(Err(vec![Diagnostic::error(
                "invalid_timezone",
                message,
                None,
            )]));
        }
    };
    let clock = match cel::operation_clock(timezone) {
        Ok(clock) => clock,
        Err(error) => {
            finish!(Err(vec![Diagnostic::error(
                error.code,
                error.message,
                None,
            )]));
        }
    };
    performance.clock_us = micros(phase.elapsed());
    cancellation.check()?;

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
        let loaded = if runtime_cache {
            collection.load_runtime_query_metadata_page_profiled_cancellable(
                &compiled.query.types,
                &order_by,
                compiled.query.offset,
                compiled.query.limit,
                cancellation,
            )
        } else {
            collection.load_query_metadata_page_profiled_cancellable(
                &compiled.query.types,
                &order_by,
                compiled.query.offset,
                compiled.query.limit,
                cancellation,
            )
        };
        cancellation.check()?;
        if let Some(error) = crate::runtime::OperationContext::current()
            .and_then(|context| context.capture_limit_error())
        {
            finish!(Err(vec![Diagnostic::error(
                error.code(),
                error.to_string(),
                None,
            )]));
        }
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
    let needs_bodies = compiled.query.include_body
        || needs_link_graph
        || needs_file_body_metadata
        || collection
            .type_plans
            .values()
            .any(|plan| !plan.computed.is_empty());
    performance.record_source_loads = 1;
    performance.total_source_loads = 1;
    #[cfg(test)]
    bump_probe(|probes| {
        probes.record_source_loads += 1;
        probes.total_source_loads += 1;
    });
    let loaded = if runtime_cache {
        collection.load_runtime_query_data_profiled_cancellable(
            true,
            needs_link_graph,
            needs_bodies,
            cancellation,
        )
    } else {
        collection.load_query_data_profiled_cancellable_with_bodies(
            true,
            needs_link_graph,
            needs_bodies,
            cancellation,
        )
    };
    let (snapshot, load_profile) = match loaded {
        Ok(loaded) => loaded,
        Err(error) => {
            cancellation.check()?;
            let path = error.path().map(|path| {
                path.strip_prefix(&collection.root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/")
            });
            finish!(Err(vec![Diagnostic::error(
                "collection_snapshot_failed",
                error.to_string(),
                path,
            )]));
        }
    };
    let local_records = snapshot
        .records
        .into_iter()
        .map(LocalRecord::Parsed)
        .chain(
            snapshot
                .invalid_records
                .into_iter()
                .map(LocalRecord::Invalid),
        )
        .collect::<Vec<_>>();
    let all_files = snapshot.all_files;
    let backlinks = snapshot.backlinks;
    performance.load_us = micros(phase.elapsed());
    if let Some(load) = load_profile {
        apply_load_performance(&mut performance, &load);
    } else {
        performance.records_loaded = local_records.len();
    }
    cancellation.check()?;

    if metadata_page_plan {
        let phase = Instant::now();
        let mut ordered = local_records
            .iter()
            .filter(|record| {
                compiled.query.types.is_empty()
                    || record.types().iter().any(|actual| {
                        compiled
                            .query
                            .types
                            .iter()
                            .any(|wanted| actual.eq_ignore_ascii_case(wanted))
                    })
            })
            .collect::<Vec<_>>();
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
            left.path().cmp(right.path())
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

    let records = local_records
        .iter()
        .filter_map(|record| match record {
            LocalRecord::Parsed(record) => Some(record),
            LocalRecord::Invalid(_) => None,
        })
        .collect::<Vec<_>>();
    let type_definitions = Arc::new(collection.types.clone());
    let phase = Instant::now();
    let (context, context_record_loads) = match load_context(
        collection,
        &compiled.query,
        all_files.clone(),
        backlinks.clone(),
        type_definitions.clone(),
    ) {
        Ok(context) => context,
        Err(diagnostic) => finish!(Err(vec![*diagnostic])),
    };
    performance.context_us = micros(phase.elapsed());
    performance.context_record_loads = context_record_loads;
    performance.total_source_loads = performance
        .record_source_loads
        .saturating_add(context_record_loads);
    #[cfg(test)]
    bump_probe(|probes| {
        probes.context_record_loads += context_record_loads;
        probes.total_source_loads += context_record_loads;
    });
    cancellation.check()?;

    let phase = Instant::now();
    let mut query_diagnostics = Vec::new();
    let mut candidates = Vec::new();
    for record in &records {
        cancellation.check()?;
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
    cancellation.check()?;

    let phase = Instant::now();
    sort_candidates(&mut candidates, &compiled.query.order_by);
    performance.sort_us = micros(phase.elapsed());
    cancellation.check()?;

    let phase = Instant::now();
    let groups = build_groups(&candidates, &compiled, &clock, &mut query_diagnostics);
    performance.groups_us = micros(phase.elapsed());
    cancellation.check()?;
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
    let result = Ok(QueryExecution {
        records: results,
        total_count,
        has_more,
        meta,
        diagnostics: query_diagnostics,
    });
    performance.total_us = micros(total_started.elapsed());
    Ok((result, performance))
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
    performance.record_source_loads = 1;
    performance.total_source_loads = performance
        .record_source_loads
        .saturating_add(performance.context_record_loads);
    performance.cache_used = load.cache_used;
    performance.cache_fallback = load.cache_fallback;
    performance.link_graph_built = load.built_link_graph;
}

fn build_metadata_page_result(
    collection: &Collection,
    compiled: &preflight::CompiledQuery,
    records: &[&LocalRecord],
    total_count: usize,
    has_more: bool,
    performance: &mut QueryPerformance,
) -> QueryEvaluation {
    let phase = Instant::now();
    let mut query_diagnostics = Vec::new();
    let results = records
        .iter()
        .map(|record| {
            let LocalRecord::Parsed(record) = record else {
                let LocalRecord::Invalid(stub) = record else {
                    unreachable!()
                };
                query_diagnostics.push(diagnostics::invalid_record(&stub.rel_path, &stub.reason));
                return serialize_invalid_stub(stub);
            };
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
    let meta = json!({
        "total_count": total_count,
        "has_more": has_more,
    });
    Ok(QueryExecution {
        records: results,
        total_count,
        has_more,
        meta,
        diagnostics: query_diagnostics,
    })
}

fn millis_to_micros(milliseconds: f64) -> u64 {
    if !milliseconds.is_finite() || milliseconds <= 0.0 {
        return 0;
    }
    (milliseconds * 1_000.0).min(u64::MAX as f64) as u64
}

fn serialize_invalid_stub(stub: &InvalidRecordStub) -> Value {
    json!({
        "path": stub.rel_path,
        "types": stub.type_names,
        "file": {
            "path": stub.rel_path,
            "name": std::path::Path::new(&stub.rel_path).file_name().and_then(|value| value.to_str()).unwrap_or(""),
            "folder": std::path::Path::new(&stub.rel_path).parent().and_then(|value| value.to_str()).unwrap_or(""),
            "size": stub.file_size,
            "mtime": stub.file_mtime_iso,
            "ctime": stub.file_ctime_iso,
            "revision": stub.source_revision,
        }
    })
}

fn record_metadata_value(record: &LocalRecord, field: &str) -> Value {
    match field {
        "file.path" => Value::String(record.path().to_string()),
        "file.name" => Value::String(
            std::path::Path::new(record.path())
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("")
                .to_string(),
        ),
        "file.folder" => Value::String(
            std::path::Path::new(record.path())
                .parent()
                .and_then(|value| value.to_str())
                .unwrap_or("")
                .to_string(),
        ),
        "file.size" => json!(match record {
            LocalRecord::Parsed(record) => record.file_size,
            LocalRecord::Invalid(stub) => stub.file_size,
        }),
        "file.mtime" => match record {
            LocalRecord::Parsed(record) => &record.file_mtime_iso,
            LocalRecord::Invalid(stub) => &stub.file_mtime_iso,
        }
        .as_ref()
        .map_or(Value::Null, |value| Value::String(value.clone())),
        "file.ctime" => match record {
            LocalRecord::Parsed(record) => &record.file_ctime_iso,
            LocalRecord::Invalid(stub) => &stub.file_ctime_iso,
        }
        .as_ref()
        .map_or(Value::Null, |value| Value::String(value.clone())),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::api::{CollectionPath, QueryRequest};
    use crate::Collection;

    use super::{query_path_probes, reset_query_path_probes};

    #[test]
    fn typed_query_skips_wire_and_envelope_paths_and_loads_once() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  timezone: UTC\n  validation: warn\n",
        )
        .unwrap();
        fs::write(root.path().join("one.md"), "---\ntitle: One\n---\nBody\n").unwrap();
        let collection = Collection::open(root.path()).unwrap();
        let request = QueryRequest {
            context: Some(CollectionPath::new("one.md").unwrap()),
            ..QueryRequest::builder().where_expression("true")
        };

        let invalid = QueryRequest {
            timezone: Some(String::new()),
            ..QueryRequest::default()
        };
        reset_query_path_probes();
        assert!(collection.typed().unwrap().query(invalid).is_err());
        assert_eq!(query_path_probes(), super::QueryPathProbes::default());

        reset_query_path_probes();
        let typed = collection.typed().unwrap().query(request.clone()).unwrap();
        assert_eq!(typed.value.total_count, 1);
        assert_eq!(
            query_path_probes(),
            super::QueryPathProbes {
                core_executions: 1,
                record_source_loads: 1,
                context_record_loads: 1,
                total_source_loads: 2,
                ..super::QueryPathProbes::default()
            }
        );

        reset_query_path_probes();
        let wire = crate::v03::query::execute(&collection, &request.to_wire());
        assert!(wire.valid, "{wire:#?}");
        assert_eq!(
            query_path_probes(),
            super::QueryPathProbes {
                wire_schema_validations: 1,
                wire_query_decodes: 1,
                typed_request_json_encodes: 1,
                core_executions: 1,
                record_source_loads: 1,
                context_record_loads: 1,
                total_source_loads: 2,
                ..super::QueryPathProbes::default()
            }
        );

        let missing = QueryRequest {
            context: Some(CollectionPath::new("missing.md").unwrap()),
            ..QueryRequest::builder().where_expression("true")
        };
        reset_query_path_probes();
        assert!(collection.typed().unwrap().query(missing.clone()).is_err());
        assert_eq!(
            query_path_probes(),
            super::QueryPathProbes {
                core_executions: 1,
                record_source_loads: 1,
                total_source_loads: 1,
                ..super::QueryPathProbes::default()
            }
        );

        reset_query_path_probes();
        let (wire, performance) =
            crate::v03::query::execute_profiled(&collection, &missing.to_wire());
        assert!(!wire.valid, "{wire:#?}");
        assert_eq!(performance.context_record_loads, 0);
        assert_eq!(performance.total_source_loads, 1);
        assert_eq!(
            query_path_probes(),
            super::QueryPathProbes {
                wire_schema_validations: 1,
                wire_query_decodes: 1,
                typed_request_json_encodes: 1,
                core_executions: 1,
                record_source_loads: 1,
                total_source_loads: 1,
                ..super::QueryPathProbes::default()
            }
        );

        let (invalid_path, performance) = crate::v03::query::execute_profiled(
            &collection,
            &serde_json::json!({"context": {"this": {"path": "../outside.md"}}}),
        );
        assert!(!invalid_path.valid, "{invalid_path:#?}");
        assert_eq!(performance.context_record_loads, 0);
        assert_eq!(performance.total_source_loads, 1);
    }
}
