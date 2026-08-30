//! Batch operations (§12.7).

#[cfg(feature = "legacy-collection-mutation")]
use crate::errors::*;
#[cfg(feature = "legacy-collection-mutation")]
use crate::frontmatter::parser::yaml_mapping_to_json;
#[cfg(feature = "legacy-collection-mutation")]
use crate::frontmatter::serializer;
use crate::query::engine::QueryEvalContext;
use crate::Collection;

#[cfg(feature = "legacy-collection-mutation")]
fn invalid_record_batch_detail(
    path: &str,
    invalid: crate::record_load::InvalidRecordView<'_>,
) -> serde_json::Value {
    let reason = match invalid {
        crate::record_load::InvalidRecordView::Frontmatter { reason, .. } => reason,
        crate::record_load::InvalidRecordView::InvalidUtf8 { .. } => {
            crate::record_load::InvalidRecordReason::InvalidUtf8
        }
    };
    let (code, message) = match reason {
        crate::record_load::InvalidRecordReason::InvalidUtf8 => (
            "file_read_failed",
            "Collection record could not be read".to_string(),
        ),
        _ => (
            INVALID_FRONTMATTER,
            format!("Invalid frontmatter: {}", reason.as_str()),
        ),
    };
    serde_json::json!({
        "path": path,
        "status": "failed",
        "error": {"code": code, "message": message},
    })
}

#[cfg(feature = "legacy-collection-mutation")]
fn preflight_serialization(
    record: crate::record_load::ParsedRecordView<'_>,
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), serializer::FrontmatterSerializationError> {
    let authored_mapping = crate::frontmatter::parser::parse_document(record.document)
        .frontmatter
        .and_then(|value| value.as_mapping().cloned())
        .unwrap_or_default();
    let mapping = serializer::reconcile_json_object(&authored_mapping, fields);
    serializer::serialize_document_with_bom(
        record.layout.had_bom(),
        &mapping,
        record.layout.body(record.document),
    )?;
    Ok(())
}

#[cfg(feature = "legacy-collection-mutation")]
fn serialization_failure_detail(path: &str, error: &impl std::fmt::Display) -> serde_json::Value {
    serde_json::json!({
        "path": path,
        "status": "failed",
        "error": {
            "code": FRONTMATTER_SERIALIZATION_FAILED,
            "message": error.to_string(),
        },
    })
}

fn timestamp_from_ns(value: i64) -> Option<String> {
    let seconds = value.div_euclid(1_000_000_000);
    let nanoseconds = value.rem_euclid(1_000_000_000) as u32;
    chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, nanoseconds)
        .map(|time| time.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

impl Collection {
    #[cfg(feature = "legacy-collection-mutation")]
    pub(crate) fn batch_update_legacy(
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

        // Select and preflight from one authoritative generation.
        let snapshot = match self.capture_collection_snapshot_current() {
            Ok(snapshot) => snapshot,
            Err(error) => return op_error("collection_snapshot_failed", &error.to_string()),
        };
        let matching_paths =
            match self.query_matching_paths_with_types(&snapshot, where_clause, &filter_types) {
                Ok(paths) => paths,
                Err(error) => return op_error("collection_snapshot_failed", &error.to_string()),
            };

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

        let mut ineligible_paths = std::collections::HashSet::new();
        let mut details = Vec::new();
        for path in &matching_paths {
            let Some(entry) = snapshot.entry(path) else {
                return op_error(
                    "collection_snapshot_failed",
                    "selected record is absent from its snapshot",
                );
            };
            if let Some(invalid) = entry.invalid() {
                ineligible_paths.insert(path.clone());
                details.push(invalid_record_batch_detail(path, invalid));
            }
        }

        // Finalize generated values and validate the complete proposed state
        // once. Reservations advance only after an item passes local preflight.
        let mut generated = crate::generated::GeneratedValueContext::from_snapshot(self, &snapshot);
        let mut proposals = Vec::new();
        let mut prepared_updates = std::collections::HashMap::new();
        let mut corpus = snapshot
            .entries()
            .iter()
            .filter_map(|entry| {
                entry
                    .effective_frontmatter()
                    .map(|frontmatter| (entry.relative_path().to_string(), frontmatter.clone()))
            })
            .collect::<Vec<_>>();
        for path in &matching_paths {
            if ineligible_paths.contains(path) {
                continue;
            }
            let entry = snapshot.entry(path).expect("selected snapshot entry");
            let Some(parsed) = entry.parsed() else {
                continue;
            };
            let existing_mapping =
                crate::frontmatter::parser::json_to_yaml_mapping(parsed.raw_frontmatter);
            let merged =
                serializer::merge_fields(&existing_mapping, &fields, &self.settings.write_nulls);
            let merged_json = yaml_mapping_to_json(&merged);
            let type_names = self.determine_types_for_path(&merged_json, Some(path));
            let mut proposed_raw = merged_json.as_object().cloned().unwrap_or_default();
            let mut candidate_generated = generated.clone();
            if let Err(error) = candidate_generated.apply_generated(
                self,
                &mut proposed_raw,
                &type_names,
                false,
                Some(path),
            ) {
                return op_error(error.code(), &error.to_string());
            }
            let raw_value = serde_json::Value::Object(proposed_raw.clone());
            let effective =
                self.coerce_types(&self.apply_defaults(&raw_value, &type_names), &type_names);
            if let Err(error) = preflight_serialization(parsed, &proposed_raw) {
                ineligible_paths.insert(path.clone());
                details.push(serialization_failure_detail(path, &error));
                continue;
            }
            if self.settings.default_validation == "error" {
                let validation = self.validate(&effective, &type_names, path);
                if !validation.valid {
                    return validation_failed_error(&validation.issues);
                }
            }
            generated = candidate_generated;
            if let Some((_, frontmatter)) =
                corpus.iter_mut().find(|(candidate, _)| candidate == path)
            {
                *frontmatter = effective.clone();
            }
            proposals.push((path.clone(), effective, type_names));
            prepared_updates.insert(
                path.clone(),
                crate::operations::update::PrevalidatedUpdate {
                    expected_revision: entry.facts().revision.clone(),
                    raw_frontmatter: proposed_raw,
                },
            );
        }
        if self.settings.default_validation == "error" {
            for (path, effective, type_names) in &proposals {
                let issues = self.check_uniqueness_in_corpus(effective, type_names, path, &corpus);
                if !issues.is_empty() {
                    return validation_failed_error(&issues);
                }
            }
        }

        if dry_run {
            if details.is_empty() {
                return serde_json::json!({
                    "batch_result": {
                        "total": total,
                        "succeeded": total,
                        "failed": 0,
                    }
                });
            }
            return serde_json::json!({
                "batch_result": {
                    "total": total,
                    "succeeded": total - details.len(),
                    "failed": details.len(),
                    "details": details,
                }
            });
        }

        // Execute only records proven parsed by the captured generation.
        let mut succeeded = 0usize;
        let mut failed = details.len();
        let mut skipped = 0usize;
        let mut failed_paths: Vec<String> = Vec::new();

        // Build backlinks index for skip_dependents checking
        let bl_index_for_skip = if skip_dependents {
            match self.build_backlinks_index_for_snapshot(&snapshot) {
                Ok(index) => Some(index),
                Err(error) => return op_error(&error.code, &error.message),
            }
        } else {
            None
        };

        for path in &matching_paths {
            if ineligible_paths.contains(path) {
                continue;
            }
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

            let prepared = prepared_updates
                .remove(path)
                .expect("eligible batch update was finalized");
            let update_result = self.update_prevalidated(
                &serde_json::json!({
                    "path": path,
                    "fields": fields,
                }),
                prepared,
            );

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
    #[cfg(feature = "legacy-collection-mutation")]
    pub(crate) fn batch_update_explicit(
        &self,
        updates: &[serde_json::Value],
        dry_run: bool,
        simulate_io_error: Option<&str>,
    ) -> serde_json::Value {
        for update in updates {
            let Some(path) = update.get("path").and_then(|value| value.as_str()) else {
                return op_error(INVALID_PATH, "Each batch update requires path");
            };
            if let Err(error) =
                crate::operations::ensure_safe_relative_path(path, self.spec_profile)
            {
                return error;
            }
            if let Err(error) = self
                .held_root()
                .ensure_no_symlink_components(std::path::Path::new(path))
            {
                return op_error(PATH_TRAVERSAL, &error.to_string());
            }
        }

        let snapshot = match self.capture_collection_snapshot_current() {
            Ok(snapshot) => snapshot,
            Err(error) => return op_error("collection_snapshot_failed", &error.to_string()),
        };

        let mut ineligible_paths = std::collections::HashSet::new();
        let mut details = Vec::new();
        for update in updates {
            let path = update
                .get("path")
                .and_then(|value| value.as_str())
                .expect("explicit update paths were validated");
            let Some(entry) = snapshot.entry(path) else {
                return op_error(FILE_NOT_FOUND, &format!("File not found: {path}"));
            };
            if let Some(invalid) = entry.invalid() {
                ineligible_paths.insert(path.to_string());
                details.push(invalid_record_batch_detail(path, invalid));
            }
        }

        let mut generated = crate::generated::GeneratedValueContext::from_snapshot(self, &snapshot);
        let mut working_raw = snapshot
            .entries()
            .iter()
            .filter_map(|entry| {
                entry
                    .raw_frontmatter()
                    .map(|frontmatter| (entry.relative_path().to_string(), frontmatter.clone()))
            })
            .collect::<std::collections::HashMap<_, _>>();
        let mut effective_corpus = snapshot
            .entries()
            .iter()
            .filter_map(|entry| {
                entry
                    .effective_frontmatter()
                    .map(|frontmatter| (entry.relative_path().to_string(), frontmatter.clone()))
            })
            .collect::<std::collections::HashMap<_, _>>();
        let mut final_proposals = std::collections::HashMap::new();
        let mut prepared_updates = Vec::with_capacity(updates.len());
        for update in updates {
            let path = update
                .get("path")
                .and_then(|value| value.as_str())
                .expect("explicit update paths were validated");
            if ineligible_paths.contains(path) {
                prepared_updates.push(None);
                continue;
            }
            let fields = update
                .get("fields")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            let existing = working_raw.get(path).expect("eligible snapshot record");
            let existing_mapping = crate::frontmatter::parser::json_to_yaml_mapping(existing);
            let merged =
                serializer::merge_fields(&existing_mapping, &fields, &self.settings.write_nulls);
            let merged_json = yaml_mapping_to_json(&merged);
            let type_names = self.determine_types_for_path(&merged_json, Some(path));
            let mut proposed_raw = merged_json.as_object().cloned().unwrap_or_default();
            let mut candidate_generated = generated.clone();
            if let Err(error) = candidate_generated.apply_generated(
                self,
                &mut proposed_raw,
                &type_names,
                false,
                Some(path),
            ) {
                return op_error(error.code(), &error.to_string());
            }
            let raw_value = serde_json::Value::Object(proposed_raw.clone());
            let effective =
                self.coerce_types(&self.apply_defaults(&raw_value, &type_names), &type_names);
            let entry = snapshot.entry(path).expect("explicit snapshot entry");
            let Some(parsed) = entry.parsed() else {
                prepared_updates.push(None);
                continue;
            };
            if let Err(error) = preflight_serialization(parsed, &proposed_raw) {
                ineligible_paths.insert(path.to_string());
                details.push(serialization_failure_detail(path, &error));
                prepared_updates.push(None);
                continue;
            }
            if self.settings.default_validation == "error" {
                let validation = self.validate(&effective, &type_names, path);
                if !validation.valid {
                    return validation_failed_error(&validation.issues);
                }
            }
            generated = candidate_generated;
            working_raw.insert(path.to_string(), raw_value);
            effective_corpus.insert(path.to_string(), effective.clone());
            final_proposals.insert(path.to_string(), (effective, type_names));
            prepared_updates.push(Some(crate::operations::update::PrevalidatedUpdate {
                expected_revision: entry.facts().revision.clone(),
                raw_frontmatter: proposed_raw,
            }));
        }
        if self.settings.default_validation == "error" {
            let corpus = effective_corpus.into_iter().collect::<Vec<_>>();
            for (path, (effective, type_names)) in final_proposals {
                let issues =
                    self.check_uniqueness_in_corpus(&effective, &type_names, &path, &corpus);
                if !issues.is_empty() {
                    return validation_failed_error(&issues);
                }
            }
        }

        let total = updates.len();
        if dry_run {
            if details.is_empty() {
                return serde_json::json!({
                    "batch_result": {
                        "total": total,
                        "succeeded": total,
                        "failed": 0,
                    }
                });
            }
            return serde_json::json!({
                "batch_result": {
                    "total": total,
                    "succeeded": total - details.len(),
                    "failed": details.len(),
                    "details": details,
                }
            });
        }

        let mut succeeded = 0usize;
        let mut failed = details.len();

        for (index, update) in updates.iter().enumerate() {
            let path = update
                .get("path")
                .and_then(|value| value.as_str())
                .expect("explicit update paths were validated");
            if ineligible_paths.contains(path) {
                continue;
            }
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

            let prepared = prepared_updates[index]
                .take()
                .expect("eligible explicit update was finalized");
            let result = self.update_prevalidated(
                &serde_json::json!({ "path": path, "fields": fields }),
                prepared,
            );
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
    #[cfg(feature = "legacy-collection-mutation")]
    pub(crate) fn batch_delete_legacy(
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

        let snapshot = match self.capture_collection_snapshot_current() {
            Ok(snapshot) => snapshot,
            Err(error) => return op_error("collection_snapshot_failed", &error.to_string()),
        };
        let matching_paths =
            match self.query_matching_paths_with_types(&snapshot, where_clause, &filter_types) {
                Ok(paths) => paths,
                Err(error) => return op_error("collection_snapshot_failed", &error.to_string()),
            };

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
            let bl_index = match self.build_backlinks_index_for_snapshot(&snapshot) {
                Ok(index) => index,
                Err(error) => return op_error(&error.code, &error.message),
            };
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

            let deleted = self.delete_legacy(&serde_json::json!({"path": path}));
            if deleted.get("error").is_some() {
                failed += 1;
                details.push(serde_json::json!({ "path": path, "status": "failed" }));
            } else {
                succeeded += 1;
                details.push(serde_json::json!({ "path": path, "status": "success" }));
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

    /// Query matching paths from one already captured authoritative generation.
    pub(crate) fn query_matching_paths_with_types(
        &self,
        snapshot: &crate::snapshot::AuthoritativeCollectionSnapshot,
        where_clause: Option<&serde_json::Value>,
        filter_types: &[String],
    ) -> Result<Vec<String>, crate::CollectionSnapshotError> {
        let Some(where_value) = where_clause else {
            let mut matching = snapshot
                .entries()
                .iter()
                .filter(|entry| {
                    filter_types.is_empty()
                        || entry
                            .type_names()
                            .iter()
                            .any(|name| filter_types.contains(name))
                })
                .map(|entry| entry.relative_path().to_string())
                .collect::<Vec<_>>();
            matching.sort();
            return Ok(matching);
        };

        let needs_link_graph = self.where_clause_uses_link_graph(where_value);
        let (all_files_arc, backlinks_arc) = if needs_link_graph {
            let resolved_files = std::sync::Arc::new(snapshot.resolved_files_data());
            let backlinks = std::sync::Arc::new(
                self.build_backlinks_index_for_snapshot_files(snapshot, &resolved_files)
                    .map_err(|error| crate::CollectionSnapshotError::CacheUnavailable {
                        reason: format!("{}: {}", error.code, error.message),
                    })?,
            );
            (Some(resolved_files), Some(backlinks))
        } else {
            (None, None)
        };
        let mut matching = Vec::new();

        for entry in snapshot.entries() {
            let rel_path = entry.relative_path();
            let type_names = entry.type_names();
            if !filter_types.is_empty()
                && !type_names.iter().any(|name| filter_types.contains(name))
            {
                continue;
            }
            let (Some(body), Some(base_effective)) = (entry.body(), entry.effective_frontmatter())
            else {
                continue;
            };
            let raw_frontmatter = entry
                .raw_frontmatter()
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let effective = self.evaluate_computed_fields(
                base_effective.clone(),
                type_names,
                rel_path,
                Some(body),
            );
            let facts = entry.facts();
            let file_mtime = timestamp_from_ns(facts.mtime_ns);
            let file_ctime = facts.ctime_ns.and_then(timestamp_from_ns);
            let eval_ctx = QueryEvalContext {
                frontmatter: &effective,
                raw_frontmatter: &raw_frontmatter,
                file_path: rel_path,
                body,
                type_names,
                formulas: &serde_json::Map::new(),
                file_size: facts.size,
                file_mtime: file_mtime.as_deref(),
                file_ctime: file_ctime.as_deref(),
                this_context: None,
                all_files: all_files_arc.clone(),
                backlinks_index: backlinks_arc.clone(),
            };
            let matches = self.evaluate_where(&eval_ctx, where_value);
            if matches {
                matching.push(rel_path.to_string());
            }
        }
        matching.sort();
        Ok(matching)
    }
}

#[cfg(all(test, feature = "legacy-collection-mutation"))]
mod snapshot_batch_tests {
    use super::*;

    #[test]
    fn type_only_selection_at_two_thousand_records_uses_indexed_lookups_and_no_link_projection() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        for index in 0..2_048 {
            std::fs::write(
                root.path().join(format!("{index:04}.md")),
                "---\ntype: note\n---\nbody\n",
            )
            .unwrap();
        }
        let collection = Collection::open(root.path()).unwrap();
        let snapshot = collection
            .capture_collection_snapshot(&crate::OperationCancellation::new())
            .unwrap();
        crate::snapshot::reset_snapshot_projection_counters_for_test();
        crate::expressions::reset_computed_field_evaluations_for_test();

        for index in (0..2_048).rev() {
            assert!(snapshot.entry(&format!("{index:04}.md")).is_some());
        }
        let paths = collection
            .query_matching_paths_with_types(&snapshot, None, &["note".to_string()])
            .unwrap();

        assert_eq!(paths.len(), 2_048);
        assert_eq!(crate::snapshot::snapshot_entry_lookups_for_test(), 2_048);
        assert_eq!(crate::snapshot::snapshot_resolved_projections_for_test(), 0);
        assert_eq!(crate::expressions::computed_field_evaluations_for_test(), 0);
    }

    #[test]
    fn n_updates_use_one_capture_plus_two_mutation_boundary_loads_each() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: error\n",
        )
        .unwrap();
        for index in 0..4 {
            std::fs::write(root.path().join(format!("{index}.md")), "body\n").unwrap();
        }
        let collection = Collection::open(root.path()).unwrap();
        crate::reset_snapshot_scan_calls_for_test();
        crate::record_load::reset_snapshot_record_loads_for_test();

        let result = collection.batch_update(
            &serde_json::json!({"where": "true", "fields": {"title": "updated"}}),
            None,
            false,
        );
        assert_eq!(result["batch_result"]["succeeded"], 4, "{result:#}");
        assert_eq!(crate::snapshot_scan_calls_for_test(), 1);
        assert_eq!(crate::record_load::snapshot_record_loads_for_test(), 12);
    }

    #[test]
    fn external_edit_after_prevalidation_is_not_adopted() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        let record = root.path().join("record.md");
        std::fs::write(&record, "---\ntitle: original\n---\n").unwrap();
        let replacement = root.path().join("replacement.tmp");
        std::fs::write(&replacement, "---\ntitle: external\n---\n").unwrap();
        let collection = Collection::open(root.path()).unwrap();
        crate::operations::update::inject_prevalidated_replacement(&record, replacement);

        let result = collection.batch_update(
            &serde_json::json!({"where": "true", "fields": {"title": "batch"}}),
            None,
            false,
        );
        assert_eq!(result["batch_result"]["failed"], 1, "{result:#}");
        assert!(std::fs::read_to_string(record)
            .unwrap()
            .contains("title: external"));
    }
}
