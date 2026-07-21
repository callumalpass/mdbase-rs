use std::sync::Arc;

use serde_json::{json, Map, Value};

use super::context::{candidate_context, file_value, load_context, namespace_value};
use super::diagnostics;
use super::model::{Candidate, Query};
use super::preflight::{self, CompiledSelection};
use super::result::{build_groups, serialize_candidate, sort_candidates};
use crate::v03::{cel, validate_query, Diagnostic, OperationResult};
use crate::Collection;

pub(crate) fn execute(collection: &Collection, input: &Value) -> OperationResult {
    let schema_diagnostics = validate_query(input)
        .into_iter()
        .map(diagnostics::invalid_schema)
        .collect::<Vec<_>>();
    if !schema_diagnostics.is_empty() {
        return diagnostics::failed(schema_diagnostics);
    }

    let query = match serde_json::from_value::<Query>(input.clone()) {
        Ok(query) => query,
        Err(error) => {
            return diagnostics::failed(vec![Diagnostic::error(
                "invalid_query",
                format!("Query could not be decoded: {error}"),
                None,
            )]);
        }
    };
    let compiled = match preflight::compile(query) {
        Ok(compiled) => compiled,
        Err(diagnostics) => return diagnostics::failed(diagnostics),
    };
    let clock = match cel::operation_clock(collection.settings.timezone.as_deref()) {
        Ok(clock) => clock,
        Err(error) => {
            return diagnostics::failed(vec![Diagnostic::error(error.code, error.message, None)]);
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
        Err(diagnostic) => return diagnostics::failed(vec![*diagnostic]),
    };

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

    sort_candidates(&mut candidates, &compiled.query.order_by);
    let groups = build_groups(&candidates, &compiled, &clock, &mut query_diagnostics);
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
    let serialized_diagnostics =
        serde_json::to_value(&query_diagnostics).unwrap_or_else(|_| json!([]));
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
