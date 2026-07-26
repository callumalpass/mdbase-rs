//! Query execution engine (§10).

use crate::expressions::ast::Expr;
use crate::expressions::evaluator::{evaluate as eval_expr, EvalContext, ResolvedFileData};
use crate::expressions::is_truthy_value;
use crate::expressions::parser::Parser as ExprParser;
use crate::Collection;
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

/// Context for evaluating query where clauses.
pub(crate) struct QueryEvalContext<'a> {
    pub frontmatter: &'a serde_json::Value,
    pub raw_frontmatter: &'a serde_json::Value,
    pub file_path: &'a str,
    pub body: &'a str,
    pub type_names: &'a [String],
    pub formulas: &'a serde_json::Map<String, serde_json::Value>,
    pub file_size: u64,
    pub file_mtime: Option<&'a str>,
    pub file_ctime: Option<&'a str>,
    pub this_context: Option<Box<EvalContext>>,
    pub all_files: Option<std::sync::Arc<Vec<ResolvedFileData>>>,
    pub backlinks_index: Option<std::sync::Arc<HashMap<String, Vec<String>>>>,
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn query_phase_profiling_enabled() -> bool {
    match std::env::var("MDBASE_PROFILE_QUERY_PHASES") {
        Ok(val) => {
            let v = val.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes" || v == "on"
        }
        Err(_) => false,
    }
}

fn expr_uses_link_graph(expr: &Expr) -> bool {
    match expr {
        Expr::Dot(obj, field) => {
            // file.backlinks
            if field == "backlinks" {
                if let Expr::Ident(name) = obj.as_ref() {
                    if name == "file" {
                        return true;
                    }
                }
            }
            expr_uses_link_graph(obj)
        }
        Expr::Index(obj, idx) => expr_uses_link_graph(obj) || expr_uses_link_graph(idx),
        Expr::BinOp(left, _, right) => expr_uses_link_graph(left) || expr_uses_link_graph(right),
        Expr::UnaryOp(_, expr) => expr_uses_link_graph(expr),
        Expr::NullCoalesce(left, right) => {
            expr_uses_link_graph(left) || expr_uses_link_graph(right)
        }
        Expr::Call(func, args) => {
            // Any .asFile(...) call
            if let Expr::Dot(_, method) = func.as_ref() {
                if method == "asFile" {
                    return true;
                }
            }
            expr_uses_link_graph(func) || args.iter().any(expr_uses_link_graph)
        }
        Expr::Conditional(cond, then_expr, else_expr) => {
            expr_uses_link_graph(cond)
                || expr_uses_link_graph(then_expr)
                || expr_uses_link_graph(else_expr)
        }
        Expr::Array(items) => items.iter().any(expr_uses_link_graph),
        Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::Str(_) | Expr::Ident(_) => false,
    }
}

impl Collection {
    /// Query the collection (§10).
    pub fn query(&self, input: &serde_json::Value) -> serde_json::Value {
        let profile_query = query_phase_profiling_enabled();
        let query_total_start = Instant::now();
        let mut perf_validate_where_ms = 0.0;
        let mut perf_validate_formulas_ms = 0.0;
        let mut perf_computed_fields_ms = 0.0;
        let mut perf_formula_order_ms = 0.0;
        let mut perf_formula_eval_ms = 0.0;
        let mut perf_where_eval_ms = 0.0;
        let mut perf_body_extract_ms = 0.0;
        let mut perf_entry_build_ms = 0.0;
        let mut perf_groupby_ms = 0.0;
        let mut perf_pagination_ms = 0.0;
        let mut perf_summaries_ms = 0.0;
        let mut perf_records_total = 0usize;
        let mut perf_records_after_folder = 0usize;
        let mut perf_records_after_type = 0usize;
        let mut perf_records_after_where = 0usize;

        // Extract query parameters - support both input.query.X and input.X
        let query = input.get("query").unwrap_or(input);

        let filter_types: Vec<String> = query
            .get("types")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                    .collect()
            })
            .unwrap_or_default();

        let folder = query.get("folder").and_then(|v| v.as_str());
        let where_clause = query.get("where");
        let order_by = query.get("order_by").and_then(|v| v.as_array());
        let limit = query.get("limit").and_then(|v| v.as_u64());
        let offset = query.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
        let include_body = query
            .get("include_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // GroupBy clause
        let group_by = query.get("groupBy").or_else(|| query.get("group_by"));

        // Property summaries: field -> summary_type (e.g., "priority" -> "Average")
        let property_summaries: HashMap<String, String> = query
            .get("property_summaries")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        // Custom summaries: name -> formula expression
        let custom_summaries: HashMap<String, String> = query
            .get("summaries")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        // Formulas (Query+ profile)
        let formulas: HashMap<String, String> = query
            .get("formulas")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        // Build 'this' context from context_file if provided
        let this_context: Option<Box<EvalContext>> = query
            .get("context_file")
            .or_else(|| input.get("context_file"))
            .and_then(|v| v.as_str())
            .and_then(|cf_path| {
                let read_result = self.read(&serde_json::json!({"path": cf_path}));
                if read_result.get("error").is_some() {
                    return None;
                }
                let fm = read_result
                    .get("frontmatter")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                let raw_fm = read_result.get("raw_frontmatter").cloned();
                let body = read_result
                    .get("body")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let file_size = read_result.pointer("/file/size").and_then(|v| v.as_u64());
                let file_mtime = read_result
                    .pointer("/file/mtime")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                Some(Box::new(EvalContext {
                    frontmatter: fm,
                    raw_frontmatter: raw_fm,
                    file_path: Some(cf_path.to_string()),
                    body,
                    file_size,
                    file_mtime,
                    file_ctime: None,
                    this_context: None,
                    all_files: None,
                    traversal_depth: std::cell::Cell::new(0),
                    backlinks_index: None,
                    type_names: None,
                    types: None,
                    note_namespace_source: Default::default(),
                    string_concat: true,
                }))
            });

        // Pre-validate where clause expressions
        if let Some(where_val) = where_clause {
            let validate_start = Instant::now();
            if let Err(err) = self.validate_where_clause(where_val) {
                return err;
            }
            perf_validate_where_ms = elapsed_ms(validate_start);
        }

        // Pre-validate formula expressions and check for circular references
        if !formulas.is_empty() {
            let validate_start = Instant::now();
            if let Err(err) = self.validate_formulas(&formulas) {
                return err;
            }
            perf_validate_formulas_ms = elapsed_ms(validate_start);
        }

        let needs_link_graph = where_clause
            .map(|where_val| self.where_clause_uses_link_graph(where_val))
            .unwrap_or(false);

        // Load file records from cache (with incremental refresh) or disk fallback
        let load_start = Instant::now();
        let ((file_records, all_files_arc, backlinks_arc), load_query_perf) =
            self.load_query_data_profiled(profile_query, needs_link_graph);
        let perf_load_query_data_ms = elapsed_ms(load_start);

        let mut candidates: Vec<serde_json::Value> = Vec::new();

        let loop_start = Instant::now();
        for record in &file_records {
            perf_records_total += 1;
            let rel_path = &record.rel_path;

            // Folder filter
            if let Some(folder_prefix) = folder {
                let folder_prefix = folder_prefix.trim_end_matches('/');
                if !rel_path.starts_with(folder_prefix)
                    || (rel_path.len() > folder_prefix.len()
                        && rel_path.as_bytes()[folder_prefix.len()] != b'/')
                {
                    continue;
                }
            }
            perf_records_after_folder += 1;

            // Type filter
            if !filter_types.is_empty() {
                let matches_type = record.type_names.iter().any(|t| filter_types.contains(t));
                if !matches_type {
                    continue;
                }
            }
            perf_records_after_type += 1;

            // Computed fields are still evaluated live (they can reference body)
            let computed_start = Instant::now();
            let effective = self.evaluate_computed_fields(
                record.effective_frontmatter.clone(),
                &record.type_names,
                rel_path,
                Some(record.body.as_str()),
            );
            perf_computed_fields_ms += elapsed_ms(computed_start);

            // Evaluate formulas in dependency order
            let formula_order_start = Instant::now();
            let formula_order = self.topological_sort_formulas(&formulas);
            perf_formula_order_ms += elapsed_ms(formula_order_start);
            let mut formula_values = serde_json::Map::new();
            let mut formula_error: Option<serde_json::Value> = None;
            let formula_eval_start = Instant::now();
            for fname in &formula_order {
                let fexpr = match formulas.get(fname) {
                    Some(e) => e,
                    None => continue,
                };
                // Build frontmatter with formula results available
                let mut fm_with_formulas = match effective.as_object() {
                    Some(m) => m.clone(),
                    None => serde_json::Map::new(),
                };
                // Add formula namespace: formula.X accessible as nested object
                let formula_obj = serde_json::Value::Object(formula_values.clone());
                fm_with_formulas.insert("formula".to_string(), formula_obj);

                let fctx = EvalContext {
                    frontmatter: serde_json::Value::Object(fm_with_formulas),
                    raw_frontmatter: None,
                    file_path: Some(rel_path.clone()),
                    body: Some(record.body.clone()),
                    file_size: None,
                    file_mtime: None,
                    file_ctime: None,
                    this_context: None,
                    all_files: None,
                    traversal_depth: std::cell::Cell::new(0),
                    backlinks_index: None,
                    type_names: None,
                    types: None,
                    note_namespace_source: Default::default(),
                    string_concat: true,
                };
                match ExprParser::parse(fexpr) {
                    Ok(parsed) => {
                        match eval_expr(&parsed, &fctx) {
                            Ok(val) => {
                                formula_values.insert(fname.clone(), val);
                            }
                            Err(e) => {
                                // Propagate fatal formula errors as query-level errors
                                if e.code == "division_by_zero"
                                    || e.code == "unknown_function"
                                    || (e.code == "type_error" && !e.message.contains("null"))
                                {
                                    formula_error = Some(serde_json::json!({
                                        "error": { "code": "formula_evaluation_error", "message": format!("Formula '{}': {}", fname, e.message) }
                                    }));
                                    break;
                                }
                                formula_values.insert(fname.clone(), serde_json::Value::Null);
                            }
                        }
                    }
                    Err(_) => {
                        formula_values.insert(fname.clone(), serde_json::Value::Null);
                    }
                }
            }
            perf_formula_eval_ms += elapsed_ms(formula_eval_start);
            if let Some(err) = formula_error {
                return err;
            }

            // File metadata from record
            let file_name = Path::new(rel_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let file_folder = Path::new(rel_path)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or("");

            // Build eval context for where clause (includes formulas and types)
            let eval_ctx = QueryEvalContext {
                frontmatter: &effective,
                raw_frontmatter: &record.raw_frontmatter,
                file_path: rel_path,
                body: &record.body,
                type_names: &record.type_names,
                formulas: &formula_values,
                file_size: record.file_size,
                file_mtime: record.file_mtime_iso.as_deref(),
                file_ctime: record.file_ctime_iso.as_deref(),
                this_context: this_context.clone(),
                all_files: all_files_arc.clone(),
                backlinks_index: backlinks_arc.clone(),
            };

            // Where filter
            if let Some(where_val) = where_clause {
                let where_start = Instant::now();
                if !self.evaluate_where(&eval_ctx, where_val) {
                    perf_where_eval_ms += elapsed_ms(where_start);
                    continue;
                }
                perf_where_eval_ms += elapsed_ms(where_start);
            }
            perf_records_after_where += 1;

            // Extract body metadata
            let body_extract_start = Instant::now();
            let body_tags = crate::expressions::evaluator::extract_tags_from_body(&record.body);
            let body_links = crate::expressions::evaluator::extract_links_from_body(&record.body);
            let body_embeds = crate::expressions::evaluator::extract_embeds_from_body(&record.body);
            perf_body_extract_ms += elapsed_ms(body_extract_start);

            // Combine frontmatter tags + body tags
            let entry_build_start = Instant::now();
            let mut all_tags: Vec<String> = Vec::new();
            if let Some(fm_tags) = effective.get("tags").and_then(|v| v.as_array()) {
                for t in fm_tags {
                    if let Some(s) = t.as_str() {
                        all_tags.push(s.to_string());
                    }
                }
            }
            for t in &body_tags {
                if !all_tags.contains(t) {
                    all_tags.push(t.clone());
                }
            }

            let mut entry = serde_json::json!({
                "path": rel_path,
                "types": record.type_names,
                "frontmatter": effective,
                "body": if include_body { serde_json::Value::String(record.body.clone()) } else { serde_json::Value::Null },
                "file": {
                    "name": file_name,
                    "folder": file_folder,
                    "size": record.file_size,
                    "mtime": record.file_mtime_iso.as_deref().unwrap_or(""),
                    "tags": all_tags,
                    "links": body_links,
                    "embeds": body_embeds,
                },
            });

            if !formula_values.is_empty() {
                entry["formulas"] = serde_json::Value::Object(formula_values);
            }

            candidates.push(entry);
            perf_entry_build_ms += elapsed_ms(entry_build_start);
        }
        let perf_loop_total_ms = elapsed_ms(loop_start);

        // Sort
        let sort_start = Instant::now();
        if let Some(order_by_clauses) = order_by {
            let sort_specs: Vec<(String, bool)> = order_by_clauses
                .iter()
                .map(|clause| {
                    let field = clause.get("field").and_then(|v| v.as_str()).unwrap_or("");
                    let direction = clause
                        .get("direction")
                        .and_then(|v| v.as_str())
                        .unwrap_or("asc");
                    (field.to_string(), direction == "asc")
                })
                .collect();

            candidates.sort_by(|a, b| {
                for (field, ascending) in &sort_specs {
                    let av = self.get_sort_value(a, field);
                    let bv = self.get_sort_value(b, field);
                    let cmp = self.compare_sort_values(&av, &bv, field, a, b);
                    let cmp = if *ascending { cmp } else { cmp.reverse() };
                    if cmp != std::cmp::Ordering::Equal {
                        return cmp;
                    }
                }
                // Tiebreaker: ascending file.path
                let ap = a.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let bp = b.get("path").and_then(|v| v.as_str()).unwrap_or("");
                ap.cmp(bp)
            });
        } else {
            // Default sort: by file.path ascending
            candidates.sort_by(|a, b| {
                let ap = a.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let bp = b.get("path").and_then(|v| v.as_str()).unwrap_or("");
                ap.cmp(bp)
            });
        }
        let perf_sort_ms = elapsed_ms(sort_start);

        // GroupBy handling
        if let Some(gb) = group_by {
            let groupby_start = Instant::now();
            let gb_property = gb.get("property").and_then(|v| v.as_str()).unwrap_or("");
            let gb_direction = gb
                .get("direction")
                .and_then(|v| v.as_str())
                .unwrap_or("ASC");

            // Group candidates by property value (preserve insertion order with Vec)
            let mut group_keys_ordered: Vec<serde_json::Value> = Vec::new();
            let mut groups_map: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
            for candidate in &candidates {
                let key = candidate
                    .get("frontmatter")
                    .and_then(|fm| fm.get(gb_property))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let key_str = if key.is_null() {
                    "\0null".to_string()
                } else {
                    key.to_string()
                };
                if !groups_map.contains_key(&key_str) {
                    group_keys_ordered.push(key);
                }
                groups_map
                    .entry(key_str)
                    .or_default()
                    .push(candidate.clone());
            }

            // Sort groups by key
            let mut group_keys = group_keys_ordered;
            group_keys.sort_by(|a, b| {
                // Null sorts last in ASC, first in DESC
                match (a.is_null(), b.is_null()) {
                    (true, true) => std::cmp::Ordering::Equal,
                    (true, false) => std::cmp::Ordering::Greater,
                    (false, true) => std::cmp::Ordering::Less,
                    _ => {
                        let a_str = match a {
                            serde_json::Value::String(s) => s.clone(),
                            _ => a.to_string(),
                        };
                        let b_str = match b {
                            serde_json::Value::String(s) => s.clone(),
                            _ => b.to_string(),
                        };
                        a_str.cmp(&b_str)
                    }
                }
            });
            if gb_direction.eq_ignore_ascii_case("DESC") {
                group_keys.reverse();
            }

            // Build group results
            let mut groups_result: Vec<serde_json::Value> = Vec::new();
            for key in &group_keys {
                let key_str = if key.is_null() {
                    "\0null".to_string()
                } else {
                    key.to_string()
                };
                let group_candidates = groups_map.get(&key_str).unwrap();
                let mut group_obj = serde_json::json!({
                    "key": key,
                    "results": group_candidates,
                });

                // Compute per-group summaries if property_summaries present
                if !property_summaries.is_empty() {
                    let summaries = self.compute_summaries(
                        group_candidates,
                        &property_summaries,
                        &custom_summaries,
                    );
                    group_obj["summaries"] = summaries;
                }

                groups_result.push(group_obj);
            }

            let mut grouped_result = serde_json::json!({
                "groups": groups_result,
                "meta": {
                    "total_count": candidates.len(),
                    "has_more": false,
                },
            });
            perf_groupby_ms = elapsed_ms(groupby_start);
            if profile_query {
                grouped_result["perf"] = serde_json::json!({
                    "query_total_ms": elapsed_ms(query_total_start),
                    "validate_where_ms": perf_validate_where_ms,
                    "validate_formulas_ms": perf_validate_formulas_ms,
                    "load_query_data_ms": perf_load_query_data_ms,
                    "loop_total_ms": perf_loop_total_ms,
                    "loop_breakdown_ms": {
                        "computed_fields": perf_computed_fields_ms,
                        "formula_order": perf_formula_order_ms,
                        "formula_eval": perf_formula_eval_ms,
                        "where_eval": perf_where_eval_ms,
                        "body_extract": perf_body_extract_ms,
                        "entry_build": perf_entry_build_ms,
                    },
                    "sort_ms": perf_sort_ms,
                    "groupby_ms": perf_groupby_ms,
                    "pagination_ms": perf_pagination_ms,
                    "summaries_ms": perf_summaries_ms,
                    "records": {
                        "total_scanned": perf_records_total,
                        "after_folder": perf_records_after_folder,
                        "after_type": perf_records_after_type,
                        "after_where": perf_records_after_where,
                        "candidates": candidates.len(),
                    },
                    "load_query_data_detail": load_query_perf.map(|p| serde_json::json!({
                        "total_ms": p.total_ms,
                        "try_open_cache_ms": p.try_open_cache_ms,
                        "refresh_cache_ms": p.refresh_cache_ms,
                        "scan_files_ms": p.scan_files_ms,
                        "load_records_ms": p.load_records_ms,
                        "build_all_files_ms": p.build_all_files_ms,
                        "build_backlinks_ms": p.build_backlinks_ms,
                        "backlinks_breakdown_ms": {
                            "frontmatter_extract": p.backlinks_frontmatter_extract_ms,
                            "body_links_extract": p.backlinks_body_links_extract_ms,
                            "body_embeds_extract": p.backlinks_body_embeds_extract_ms,
                            "resolve_targets": p.backlinks_resolve_targets_ms,
                        },
                        "backlinks_counters": {
                            "files_processed": p.backlinks_files_processed,
                            "targets_scanned": p.backlinks_targets_scanned,
                            "resolve_calls": p.backlinks_resolve_calls,
                        },
                        "file_records": p.file_records,
                        "cache_used": p.cache_used,
                        "cache_fallback": p.cache_fallback,
                        "built_link_graph": p.built_link_graph,
                    })),
                });
            }
            return grouped_result;
        }

        // Pagination
        let pagination_start = Instant::now();
        let total_count = candidates.len();
        let offset = offset as usize;
        let results = if let Some(lim) = limit {
            let lim = lim as usize;
            if offset >= candidates.len() {
                Vec::new()
            } else {
                candidates[offset..].iter().take(lim).cloned().collect()
            }
        } else if offset >= candidates.len() {
            Vec::new()
        } else {
            candidates[offset..].to_vec()
        };

        let has_more = if let Some(lim) = limit {
            offset + (lim as usize) < total_count
        } else {
            false
        };
        perf_pagination_ms = elapsed_ms(pagination_start);

        // Compute summaries
        let mut result = serde_json::json!({
            "results": results,
            "meta": {
                "total_count": total_count,
                "has_more": has_more,
            },
        });

        if !property_summaries.is_empty() {
            let summaries_start = Instant::now();
            let summaries =
                self.compute_summaries(&candidates, &property_summaries, &custom_summaries);
            result["summaries"] = summaries;
            perf_summaries_ms = elapsed_ms(summaries_start);
        }

        if profile_query {
            result["perf"] = serde_json::json!({
                "query_total_ms": elapsed_ms(query_total_start),
                "validate_where_ms": perf_validate_where_ms,
                "validate_formulas_ms": perf_validate_formulas_ms,
                "load_query_data_ms": perf_load_query_data_ms,
                "loop_total_ms": perf_loop_total_ms,
                "loop_breakdown_ms": {
                    "computed_fields": perf_computed_fields_ms,
                    "formula_order": perf_formula_order_ms,
                    "formula_eval": perf_formula_eval_ms,
                    "where_eval": perf_where_eval_ms,
                    "body_extract": perf_body_extract_ms,
                    "entry_build": perf_entry_build_ms,
                },
                "sort_ms": perf_sort_ms,
                "groupby_ms": perf_groupby_ms,
                "pagination_ms": perf_pagination_ms,
                "summaries_ms": perf_summaries_ms,
                "records": {
                    "total_scanned": perf_records_total,
                    "after_folder": perf_records_after_folder,
                    "after_type": perf_records_after_type,
                    "after_where": perf_records_after_where,
                    "candidates": candidates.len(),
                },
                "load_query_data_detail": load_query_perf.map(|p| serde_json::json!({
                    "total_ms": p.total_ms,
                    "try_open_cache_ms": p.try_open_cache_ms,
                    "refresh_cache_ms": p.refresh_cache_ms,
                    "scan_files_ms": p.scan_files_ms,
                    "load_records_ms": p.load_records_ms,
                    "build_all_files_ms": p.build_all_files_ms,
                    "build_backlinks_ms": p.build_backlinks_ms,
                    "backlinks_breakdown_ms": {
                        "frontmatter_extract": p.backlinks_frontmatter_extract_ms,
                        "body_links_extract": p.backlinks_body_links_extract_ms,
                        "body_embeds_extract": p.backlinks_body_embeds_extract_ms,
                        "resolve_targets": p.backlinks_resolve_targets_ms,
                    },
                    "backlinks_counters": {
                        "files_processed": p.backlinks_files_processed,
                        "targets_scanned": p.backlinks_targets_scanned,
                        "resolve_calls": p.backlinks_resolve_calls,
                    },
                    "file_records": p.file_records,
                    "cache_used": p.cache_used,
                    "cache_fallback": p.cache_fallback,
                    "built_link_graph": p.built_link_graph,
                })),
            });
        }

        result
    }

    fn where_clause_uses_link_graph(&self, where_val: &serde_json::Value) -> bool {
        match where_val {
            serde_json::Value::String(expr_str) => self.where_expr_uses_link_graph(expr_str),
            serde_json::Value::Object(map) => {
                if let Some(and_val) = map.get("and") {
                    if let Some(arr) = and_val.as_array() {
                        if arr
                            .iter()
                            .any(|clause| self.where_clause_uses_link_graph(clause))
                        {
                            return true;
                        }
                    }
                }
                if let Some(or_val) = map.get("or") {
                    if let Some(arr) = or_val.as_array() {
                        if arr
                            .iter()
                            .any(|clause| self.where_clause_uses_link_graph(clause))
                        {
                            return true;
                        }
                    }
                }
                if let Some(not_val) = map.get("not") {
                    return self.where_clause_uses_link_graph(not_val);
                }
                false
            }
            _ => false,
        }
    }

    fn where_expr_uses_link_graph(&self, expr_str: &str) -> bool {
        match ExprParser::parse(expr_str) {
            Ok(parsed) => expr_uses_link_graph(&parsed),
            Err(_) => expr_str.contains(".asFile(") || expr_str.contains("file.backlinks"),
        }
    }

    /// Get a value for sorting from a result entry.
    pub(crate) fn get_sort_value(
        &self,
        entry: &serde_json::Value,
        field: &str,
    ) -> serde_json::Value {
        // Handle formula.X fields
        if let Some(formula_field) = field.strip_prefix("formula.") {
            return entry
                .get("formulas")
                .and_then(|f| f.get(formula_field))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
        }
        // Handle file.X fields (including nested like file.embeds.length)
        if let Some(file_field) = field.strip_prefix("file.") {
            if let Some(file_obj) = entry.get("file") {
                // Handle nested properties like embeds.length, tags.length
                if let Some((prop, sub)) = file_field.split_once('.') {
                    let prop_val = file_obj
                        .get(prop)
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    return match sub {
                        "length" => {
                            if let Some(arr) = prop_val.as_array() {
                                serde_json::json!(arr.len())
                            } else if let Some(s) = prop_val.as_str() {
                                serde_json::json!(s.len())
                            } else {
                                serde_json::json!(0)
                            }
                        }
                        _ => serde_json::Value::Null,
                    };
                }
                return file_obj
                    .get(file_field)
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
            }
            return serde_json::Value::Null;
        }
        // Regular frontmatter field
        entry
            .get("frontmatter")
            .and_then(|fm| fm.get(field))
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }

    /// Compare two sort values with null handling and enum order.
    pub(crate) fn compare_sort_values(
        &self,
        a: &serde_json::Value,
        b: &serde_json::Value,
        field: &str,
        a_entry: &serde_json::Value,
        b_entry: &serde_json::Value,
    ) -> std::cmp::Ordering {
        let a_null = a.is_null();
        let b_null = b.is_null();

        // Null handling: nulls sort last in ascending (we handle reversal in caller)
        if a_null && b_null {
            return std::cmp::Ordering::Equal;
        }
        if a_null {
            return std::cmp::Ordering::Greater;
        }
        if b_null {
            return std::cmp::Ordering::Less;
        }

        // Check if this is an enum field - need to find field def from types
        if !field.starts_with("formula.") && !field.starts_with("file.") {
            if let Some(enum_values) = self.get_enum_values_for_field(field, a_entry, b_entry) {
                let a_str = a.as_str().unwrap_or("");
                let b_str = b.as_str().unwrap_or("");
                let a_idx = enum_values
                    .iter()
                    .position(|v| v == a_str)
                    .unwrap_or(usize::MAX);
                let b_idx = enum_values
                    .iter()
                    .position(|v| v == b_str)
                    .unwrap_or(usize::MAX);
                return a_idx.cmp(&b_idx);
            }
        }

        // Standard comparison
        match (a, b) {
            (serde_json::Value::Number(an), serde_json::Value::Number(bn)) => {
                let af = an.as_f64().unwrap_or(0.0);
                let bf = bn.as_f64().unwrap_or(0.0);
                af.partial_cmp(&bf).unwrap_or(std::cmp::Ordering::Equal)
            }
            (serde_json::Value::String(a_s), serde_json::Value::String(b_s)) => a_s.cmp(b_s),
            (serde_json::Value::Bool(ab), serde_json::Value::Bool(bb)) => ab.cmp(bb),
            // §10.3: Lists sort by length
            (serde_json::Value::Array(a_arr), serde_json::Value::Array(b_arr)) => {
                a_arr.len().cmp(&b_arr.len())
            }
            // §10.3: Objects sort by key count
            (serde_json::Value::Object(a_obj), serde_json::Value::Object(b_obj)) => {
                a_obj.len().cmp(&b_obj.len())
            }
            _ => std::cmp::Ordering::Equal,
        }
    }

    /// Find enum values for a field from the types of result entries.
    pub(crate) fn get_enum_values_for_field(
        &self,
        field: &str,
        a_entry: &serde_json::Value,
        _b_entry: &serde_json::Value,
    ) -> Option<Vec<String>> {
        // Look up field definition from the entry's types
        let type_names = a_entry
            .get("types")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        for tn in &type_names {
            if let Some(td) = self.types.get(tn) {
                if let Some(fd) = td.fields.get(field) {
                    if fd.field_type == "enum" {
                        if let Some(ref vals) = fd.values {
                            return Some(vals.clone());
                        }
                    }
                }
            }
        }
        None
    }

    /// Evaluate a where clause (string expression or YAML and/or/not structure).
    pub(crate) fn evaluate_where(
        &self,
        ctx: &QueryEvalContext,
        where_val: &serde_json::Value,
    ) -> bool {
        match where_val {
            serde_json::Value::String(expr_str) => self.evaluate_where_expr(ctx, expr_str),
            serde_json::Value::Object(map) => {
                if let Some(and_val) = map.get("and") {
                    if let Some(arr) = and_val.as_array() {
                        return arr.iter().all(|clause| self.evaluate_where(ctx, clause));
                    }
                }
                if let Some(or_val) = map.get("or") {
                    if let Some(arr) = or_val.as_array() {
                        return arr.iter().any(|clause| self.evaluate_where(ctx, clause));
                    }
                }
                if let Some(not_val) = map.get("not") {
                    return !self.evaluate_where(ctx, not_val);
                }
                // Unknown structure - treat as false
                false
            }
            _ => false,
        }
    }

    /// Validate a where clause (pre-check before scanning files).
    /// Returns Err with error JSON if the clause has a syntax error.
    pub(crate) fn validate_where_clause(
        &self,
        where_val: &serde_json::Value,
    ) -> Result<(), serde_json::Value> {
        match where_val {
            serde_json::Value::String(expr_str) => self.validate_single_expr(expr_str),
            serde_json::Value::Object(map) => {
                if let Some(and_val) = map.get("and") {
                    if let Some(arr) = and_val.as_array() {
                        for clause in arr {
                            self.validate_where_clause(clause)?;
                        }
                    }
                }
                if let Some(or_val) = map.get("or") {
                    if let Some(arr) = or_val.as_array() {
                        for clause in arr {
                            self.validate_where_clause(clause)?;
                        }
                    }
                }
                if let Some(not_val) = map.get("not") {
                    self.validate_where_clause(not_val)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Validate a single expression string - check for parse errors, unknown functions, etc.
    pub(crate) fn validate_single_expr(&self, expr_str: &str) -> Result<(), serde_json::Value> {
        match ExprParser::parse(expr_str) {
            Ok(parsed) => {
                // Try evaluating with empty context to catch static errors
                let ctx = EvalContext::empty();
                match eval_expr(&parsed, &ctx) {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        match e.code.as_str() {
                            "wrong_argument_count" | "expression_depth_exceeded" => {
                                Err(serde_json::json!({
                                    "error": { "code": e.code, "message": e.message }
                                }))
                            }
                            "unknown_function" => {
                                // ext:: functions are expected to be unknown -- don't abort the query
                                if e.message.contains("ext.")
                                    || e.message.contains("ext::")
                                    || e.message.contains("extension")
                                {
                                    Ok(())
                                } else {
                                    Err(serde_json::json!({
                                        "error": { "code": e.code, "message": e.message }
                                    }))
                                }
                            }
                            _ => Ok(()), // Other errors depend on context
                        }
                    }
                }
            }
            Err(msg) => {
                let code = if msg.contains("expression_depth_exceeded") {
                    "expression_depth_exceeded"
                } else {
                    "invalid_expression"
                };
                Err(serde_json::json!({
                    "error": { "code": code, "message": msg }
                }))
            }
        }
    }

    /// Evaluate a single where expression string against file context.
    pub(crate) fn evaluate_where_expr(&self, ctx: &QueryEvalContext, expr_str: &str) -> bool {
        let parsed = match ExprParser::parse(expr_str) {
            Ok(p) => p,
            Err(_) => return false,
        };

        let mut enriched_fm = if self.spec_profile == crate::SpecProfile::V03 {
            let known_fields = ctx
                .type_names
                .iter()
                .filter_map(|type_name| self.types.get(type_name))
                .flat_map(|type_definition| type_definition.fields.keys().cloned())
                .collect::<std::collections::BTreeSet<_>>();
            // Build the v0.3 record/raw/presence namespaces while retaining
            // top-level effective field access.
            crate::v03::cel::enrich_record_bindings(
                ctx.frontmatter,
                ctx.raw_frontmatter,
                known_fields.iter(),
            )
        } else {
            ctx.frontmatter.clone()
        };

        // Add types array to context
        if let serde_json::Value::Object(ref mut map) = enriched_fm {
            let types_arr: Vec<serde_json::Value> = ctx
                .type_names
                .iter()
                .map(|t| serde_json::Value::String(t.clone()))
                .collect();
            map.insert("types".to_string(), serde_json::Value::Array(types_arr));
        }

        // Add formula namespace values
        if !ctx.formulas.is_empty() {
            if let serde_json::Value::Object(ref mut map) = enriched_fm {
                map.insert(
                    "formula".to_string(),
                    serde_json::Value::Object(ctx.formulas.clone()),
                );
            }
        }

        let eval_ctx = EvalContext {
            frontmatter: enriched_fm,
            raw_frontmatter: Some(ctx.raw_frontmatter.clone()),
            file_path: Some(ctx.file_path.to_string()),
            body: Some(ctx.body.to_string()),
            file_size: Some(ctx.file_size),
            file_mtime: ctx.file_mtime.map(String::from),
            file_ctime: ctx.file_ctime.map(String::from),
            this_context: ctx.this_context.clone(),
            all_files: ctx.all_files.clone(),
            traversal_depth: std::cell::Cell::new(0),
            backlinks_index: ctx.backlinks_index.clone(),
            type_names: Some(ctx.type_names.to_vec()),
            types: Some(std::sync::Arc::new(self.types.clone())),
            note_namespace_source: Default::default(),
            string_concat: false,
        };

        match eval_expr(&parsed, &eval_ctx) {
            Ok(val) => is_truthy_value(&val),
            Err(_) => false,
        }
    }
}
