//! Batch operations (§12.7).

use crate::errors::*;
use crate::frontmatter::parser::{parse_document, yaml_mapping_to_json};
use crate::frontmatter::serializer;
use crate::query::engine::QueryEvalContext;
use crate::Collection;

impl Collection {
    pub fn batch_update(
        &self,
        input: &serde_json::Value,
        simulate_io_error: Option<&str>,
        skip_dependents: bool,
    ) -> serde_json::Value {
        let dry_run = input
            .get("dry_run")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Two modes: where+fields or updates (explicit list)
        if let Some(updates) = input.get("updates").and_then(|v| v.as_array()) {
            return self.batch_update_explicit(updates, dry_run, simulate_io_error);
        }

        // Support both input.query.where and input.where formats
        let query = input.get("query");
        let where_clause = query
            .and_then(|q| q.get("where"))
            .or_else(|| input.get("where"));
        let filter_types: Vec<String> = query
            .and_then(|q| q.get("types"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                    .collect()
            })
            .unwrap_or_default();
        let fields = input
            .get("fields")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        // If neither where clause nor query.types provided, error
        if where_clause.is_none() && filter_types.is_empty() {
            return op_error(
                "invalid_input",
                "batch_update requires 'where' or 'updates'",
            );
        }

        // Find matching files using query logic
        let matching_paths = self.query_matching_paths_with_types(where_clause, &filter_types);

        let total = matching_paths.len();
        if total == 0 {
            return serde_json::json!({
                "batch_result": {
                    "total": 0,
                    "succeeded": 0,
                    "failed": 0,
                    "details": [],
                }
            });
        }

        // Validate-all-then-execute: validate all files first
        if self.settings.default_validation == "error" {
            for path in &matching_paths {
                let full_path = self.root.join(path);
                let content = match std::fs::read_to_string(&full_path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let doc = parse_document(&content);
                let existing_mapping = match &doc.frontmatter {
                    Some(serde_yaml::Value::Mapping(m)) => m.clone(),
                    _ => serde_yaml::Mapping::new(),
                };
                let merged = serializer::merge_fields(
                    &existing_mapping,
                    &fields,
                    &self.settings.write_nulls,
                );
                let merged_json = yaml_mapping_to_json(&merged);
                let type_names = self.determine_types(&merged_json);
                let effective = self.apply_defaults(&merged_json, &type_names);
                let validation = self.validate(&effective, &type_names, path);
                if !validation.valid {
                    return validation_failed_error(&validation.issues);
                }
            }
        }

        if dry_run {
            return serde_json::json!({
                "batch_result": {
                    "total": total,
                    "succeeded": total,
                    "failed": 0,
                }
            });
        }

        // Execute updates
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        let mut skipped = 0usize;
        let mut details: Vec<serde_json::Value> = Vec::new();
        let mut failed_paths: Vec<String> = Vec::new();

        // Build backlinks index for skip_dependents checking
        let bl_index_for_skip = if skip_dependents {
            let all_files = self.build_all_files_data();
            Some(self.build_backlinks_index(&all_files))
        } else {
            None
        };

        for path in &matching_paths {
            // Check skip_dependents: if this file has a link TO a failed file, skip it
            // Use backlinks index: if a failed file has this file as a backlink source,
            // then this file links to the failed file and should be skipped
            if skip_dependents && !failed_paths.is_empty() {
                if let Some(ref bl_index) = bl_index_for_skip {
                    // Check if any failed path lists this file as a source (backlink)
                    let has_dep = failed_paths.iter().any(|fp| {
                        bl_index
                            .get(fp)
                            .is_some_and(|sources| sources.contains(path))
                    });
                    if has_dep {
                        skipped += 1;
                        details.push(serde_json::json!({
                            "path": path,
                            "status": "skipped",
                        }));
                        continue;
                    }
                }
            }

            // Check simulated I/O error
            if let Some(err_path) = simulate_io_error {
                if path == err_path {
                    failed += 1;
                    failed_paths.push(path.clone());
                    details.push(serde_json::json!({
                        "path": path,
                        "status": "failed",
                    }));
                    continue;
                }
            }

            let update_result = self.update(&serde_json::json!({
                "path": path,
                "fields": fields,
            }));

            if update_result.get("error").is_some() {
                failed += 1;
                failed_paths.push(path.clone());
                details.push(serde_json::json!({
                    "path": path,
                    "status": "failed",
                }));
            } else {
                succeeded += 1;
                details.push(serde_json::json!({
                    "path": path,
                    "status": "success",
                }));
            }
        }

        let mut result = serde_json::json!({
            "batch_result": {
                "total": total,
                "succeeded": succeeded,
                "failed": failed,
                "details": details,
            }
        });
        if skipped > 0 {
            result["batch_result"]["skipped"] = serde_json::json!(skipped);
        }
        result
    }

    /// Batch update with explicit update list (validate-all-then-execute).
    pub(crate) fn batch_update_explicit(
        &self,
        updates: &[serde_json::Value],
        dry_run: bool,
        simulate_io_error: Option<&str>,
    ) -> serde_json::Value {
        // Validate all first
        if self.settings.default_validation == "error" {
            for update in updates {
                let path = match update.get("path").and_then(|v| v.as_str()) {
                    Some(p) => p,
                    None => continue,
                };
                let fields = update
                    .get("fields")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                let full_path = self.root.join(path);
                let content = match std::fs::read_to_string(&full_path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let doc = parse_document(&content);
                let existing_mapping = match &doc.frontmatter {
                    Some(serde_yaml::Value::Mapping(m)) => m.clone(),
                    _ => serde_yaml::Mapping::new(),
                };
                let merged = serializer::merge_fields(
                    &existing_mapping,
                    &fields,
                    &self.settings.write_nulls,
                );
                let merged_json = yaml_mapping_to_json(&merged);
                let type_names = self.determine_types(&merged_json);
                let effective = self.apply_defaults(&merged_json, &type_names);
                let validation = self.validate(&effective, &type_names, path);
                if !validation.valid {
                    return validation_failed_error(&validation.issues);
                }
            }
        }

        let total = updates.len();
        if dry_run {
            return serde_json::json!({
                "batch_result": {
                    "total": total,
                    "succeeded": total,
                    "failed": 0,
                }
            });
        }

        let mut succeeded = 0usize;
        let mut failed = 0usize;
        let mut details: Vec<serde_json::Value> = Vec::new();

        for update in updates {
            let path = match update.get("path").and_then(|v| v.as_str()) {
                Some(p) => p,
                None => continue,
            };
            let fields = update
                .get("fields")
                .cloned()
                .unwrap_or(serde_json::json!({}));

            if let Some(err_path) = simulate_io_error {
                if path == err_path {
                    failed += 1;
                    details.push(serde_json::json!({ "path": path, "status": "failed" }));
                    continue;
                }
            }

            let result = self.update(&serde_json::json!({ "path": path, "fields": fields }));
            if result.get("error").is_some() {
                failed += 1;
                details.push(serde_json::json!({ "path": path, "status": "failed" }));
            } else {
                succeeded += 1;
                details.push(serde_json::json!({ "path": path, "status": "success" }));
            }
        }

        serde_json::json!({
            "batch_result": {
                "total": total,
                "succeeded": succeeded,
                "failed": failed,
                "details": details,
            }
        })
    }

    /// Batch delete files matching a where clause (§12.4, §12.7).
    pub fn batch_delete(
        &self,
        input: &serde_json::Value,
        simulate_io_error: Option<&str>,
    ) -> serde_json::Value {
        let dry_run = input
            .get("dry_run")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let check_backlinks = input
            .get("check_backlinks")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Support both input.query.where and input.where formats
        let query = input.get("query");
        let where_clause = query
            .and_then(|q| q.get("where"))
            .or_else(|| input.get("where"));
        let filter_types: Vec<String> = query
            .and_then(|q| q.get("types"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                    .collect()
            })
            .unwrap_or_default();

        // If neither where clause nor query.types provided, error
        if where_clause.is_none() && filter_types.is_empty() {
            return op_error("invalid_input", "batch_delete requires 'where'");
        }

        let matching_paths = self.query_matching_paths_with_types(where_clause, &filter_types);

        let total = matching_paths.len();
        if total == 0 {
            return serde_json::json!({
                "batch_result": {
                    "total": 0,
                    "succeeded": 0,
                    "failed": 0,
                    "details": [],
                }
            });
        }

        // Check backlinks before deletion
        let mut broken_links: Vec<serde_json::Value> = Vec::new();
        if check_backlinks {
            let all_files = self.build_all_files_data();
            let bl_index = self.build_backlinks_index(&all_files);
            for path in &matching_paths {
                if let Some(sources) = bl_index.get(path) {
                    for source in sources {
                        // Only report if the source is not also being deleted
                        if !matching_paths.contains(source) {
                            broken_links.push(serde_json::json!({
                                "target": path,
                                "referrer": source,
                            }));
                        }
                    }
                }
            }
        }

        if dry_run {
            let mut result = serde_json::json!({
                "batch_result": {
                    "total": total,
                }
            });
            if !broken_links.is_empty() {
                result["broken_links"] = serde_json::Value::Array(broken_links);
            }
            return result;
        }

        // Execute deletes
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        let mut details: Vec<serde_json::Value> = Vec::new();

        for path in &matching_paths {
            if let Some(err_path) = simulate_io_error {
                if path == err_path {
                    failed += 1;
                    details.push(serde_json::json!({ "path": path, "status": "failed" }));
                    continue;
                }
            }

            let full_path = self.root.join(path);
            match std::fs::remove_file(&full_path) {
                Ok(_) => {
                    succeeded += 1;
                    details.push(serde_json::json!({ "path": path, "status": "success" }));
                }
                Err(_) => {
                    failed += 1;
                    details.push(serde_json::json!({ "path": path, "status": "failed" }));
                }
            }
        }

        let mut result = serde_json::json!({
            "batch_result": {
                "total": total,
                "succeeded": succeeded,
                "failed": failed,
                "details": details,
            }
        });
        if !broken_links.is_empty() {
            result["broken_links"] = serde_json::Value::Array(broken_links);
        }
        result
    }

    /// Query matching file paths (reuses query logic but only returns paths).
    #[allow(dead_code)]
    pub(crate) fn query_matching_paths(&self, where_clause: &serde_json::Value) -> Vec<String> {
        self.query_matching_paths_with_types(Some(where_clause), &[])
    }

    /// Query matching file paths with optional type filtering.
    pub(crate) fn query_matching_paths_with_types(
        &self,
        where_clause: Option<&serde_json::Value>,
        filter_types: &[String],
    ) -> Vec<String> {
        let files = self.scan_collection_files();
        let mut matching: Vec<String> = Vec::new();

        // Build all_files data for asFile() traversal in where clauses
        let all_files_data: Vec<crate::expressions::evaluator::ResolvedFileData> = files
            .iter()
            .filter_map(|fp| {
                let rp = fp
                    .strip_prefix(&self.root)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
                    .replace('\\', "/");
                let content = std::fs::read_to_string(fp).ok()?;
                let doc = parse_document(&content);
                let fm = match &doc.frontmatter {
                    Some(serde_yaml::Value::Mapping(m)) => yaml_mapping_to_json(m),
                    _ => serde_json::json!({}),
                };
                let type_names = self.determine_types_for_path(&fm, Some(&rp));
                let effective = self.apply_defaults(&fm, &type_names);
                let effective = self.coerce_types(&effective, &type_names);
                Some(crate::expressions::evaluator::ResolvedFileData {
                    path: rp,
                    frontmatter: effective,
                    body: doc.body,
                })
            })
            .collect();
        let all_files_arc = std::sync::Arc::new(all_files_data);
        let backlinks_index = self.build_backlinks_index(&all_files_arc);
        let backlinks_arc = std::sync::Arc::new(backlinks_index);

        for file_path in &files {
            let rel_path = file_path
                .strip_prefix(&self.root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
                .replace('\\', "/");

            let content = match std::fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let doc = parse_document(&content);

            let raw_frontmatter = match &doc.frontmatter {
                Some(serde_yaml::Value::Mapping(m)) => yaml_mapping_to_json(m),
                _ => serde_json::json!({}),
            };

            let type_names = self.determine_types_for_path(&raw_frontmatter, Some(&rel_path));

            // Apply type filter if provided
            if !filter_types.is_empty() {
                let matches = type_names.iter().any(|tn| filter_types.contains(tn));
                if !matches {
                    continue;
                }
            }

            let effective = self.apply_defaults(&raw_frontmatter, &type_names);
            let effective = self.coerce_types(&effective, &type_names);
            let effective = self.evaluate_computed_fields(
                effective,
                &type_names,
                &rel_path,
                Some(doc.body.as_str()),
            );

            // If no where clause, match all (type-filtered) files
            let matches = if let Some(where_val) = where_clause {
                let file_metadata = std::fs::metadata(file_path).ok();
                let file_size = file_metadata.as_ref().map(|m| m.len()).unwrap_or(0);
                let file_mtime = file_metadata
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .map(|t| {
                        let dt: chrono::DateTime<chrono::Utc> = t.into();
                        dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
                    });
                let file_ctime = file_metadata
                    .as_ref()
                    .and_then(|m| m.created().ok())
                    .map(|t| {
                        let dt: chrono::DateTime<chrono::Utc> = t.into();
                        dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
                    });

                let eval_ctx = QueryEvalContext {
                    frontmatter: &effective,
                    raw_frontmatter: &raw_frontmatter,
                    file_path: &rel_path,
                    body: &doc.body,
                    type_names: &type_names,
                    formulas: &serde_json::Map::new(),
                    file_size,
                    file_mtime: file_mtime.as_deref(),
                    file_ctime: file_ctime.as_deref(),
                    this_context: None,
                    all_files: Some(all_files_arc.clone()),
                    backlinks_index: Some(backlinks_arc.clone()),
                };
                self.evaluate_where(&eval_ctx, where_val)
            } else {
                true
            };

            if matches {
                matching.push(rel_path);
            }
        }

        matching.sort();
        matching
    }
}
