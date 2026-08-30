//! Backfill operation (§12.8).

use std::collections::{HashMap, HashSet};

use crate::errors::*;

use crate::frontmatter::serializer;
use crate::Collection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChangeKind {
    Default,
    Generated,
}

fn invalid_record_detail(
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

struct BackfillPlan {
    path: String,
    expected_revision: String,
    output: String,
    effective: serde_json::Value,
    type_names: Vec<String>,
    changed_fields: Vec<String>,
}

#[cfg(test)]
fn injected_backfill_replacements(
) -> &'static std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf, std::path::PathBuf>> {
    static REPLACEMENTS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf, std::path::PathBuf>>,
    > = std::sync::OnceLock::new();
    REPLACEMENTS.get_or_init(Default::default)
}

#[cfg(test)]
pub(crate) fn inject_backfill_replacement(path: &std::path::Path, replacement: std::path::PathBuf) {
    injected_backfill_replacements()
        .lock()
        .expect("backfill replacement lock")
        .insert(path.to_path_buf(), replacement);
}

#[cfg(test)]
fn apply_injected_backfill_replacement(path: &std::path::Path) {
    if let Some(replacement) = injected_backfill_replacements()
        .lock()
        .expect("backfill replacement lock")
        .remove(path)
    {
        std::fs::rename(replacement, path).expect("injected backfill replacement");
    }
}

impl Collection {
    /// Backfill missing defaults/generated values across files (§12.8).
    pub fn backfill(&self, input: &serde_json::Value) -> serde_json::Value {
        let type_filter = input.get("type").and_then(|v| v.as_str());
        let where_clause = input.get("where");
        let dry_run = input
            .get("dry_run")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let apply_defaults = input
            .get("apply")
            .and_then(|v| v.get("defaults"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let apply_generated = input
            .get("apply")
            .and_then(|v| v.get("generated"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let fields_filter: Option<HashSet<String>> =
            input.get("fields").and_then(|v| v.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            });

        if type_filter.is_none() && where_clause.is_none() {
            return op_error(INVALID_REQUEST, "backfill requires 'type' or 'where'");
        }

        let filter_types: Vec<String> = type_filter
            .map(|t| vec![t.to_lowercase()])
            .unwrap_or_default();

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
                    "skipped": 0,
                    "details": [],
                }
            });
        }

        let mut plans: Vec<BackfillPlan> = Vec::new();
        let mut skipped = 0usize;
        let mut noop_success = 0usize;
        let mut planning_failed = 0usize;
        let mut details: Vec<serde_json::Value> = Vec::new();
        let mut generated = crate::generated::GeneratedValueContext::from_snapshot(self, &snapshot);

        for path in &matching_paths {
            let Some(entry) = snapshot.entry(path) else {
                return op_error(
                    "collection_snapshot_failed",
                    "selected backfill record is absent from its snapshot",
                );
            };
            let Some(raw_frontmatter) = entry.raw_frontmatter().cloned() else {
                planning_failed += 1;
                if let Some(invalid) = entry.invalid() {
                    details.push(invalid_record_detail(entry.relative_path(), invalid));
                }
                continue;
            };
            let raw_obj = raw_frontmatter.as_object().cloned().unwrap_or_default();
            let type_names = entry.type_names().to_vec();
            let body = entry.body().unwrap_or_default();
            let had_bom = entry.had_bom().unwrap_or(false);

            let mut working = raw_obj.clone();
            let mut candidate_generated = generated.clone();
            let mut changes: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
            let mut change_kinds: HashMap<String, ChangeKind> = HashMap::new();

            if apply_generated {
                let mut missing_generated = HashSet::new();
                for type_name in &type_names {
                    if let Some(type_def) = self.types.get(type_name) {
                        for (field_name, field_def) in &type_def.fields {
                            if field_def.generated.is_some()
                                && !raw_obj.contains_key(field_name)
                                && fields_filter
                                    .as_ref()
                                    .is_none_or(|filter| filter.contains(field_name))
                            {
                                missing_generated.insert(field_name.clone());
                            }
                        }
                    }
                }
                let generated_fields = match candidate_generated.apply_generated_filtered(
                    self,
                    &mut working,
                    &type_names,
                    true,
                    Some(path),
                    Some(&missing_generated),
                ) {
                    Ok(fields) => fields,
                    Err(error) => return op_error(error.code(), &error.to_string()),
                };
                for field_name in generated_fields {
                    let value = working
                        .get(&field_name)
                        .cloned()
                        .expect("generated field was inserted");
                    changes.insert(field_name.clone(), value);
                    change_kinds.insert(field_name, ChangeKind::Generated);
                }
            }

            if apply_defaults {
                for type_name in &type_names {
                    if let Some(type_def) = self.types.get(type_name) {
                        for (field_name, field_def) in &type_def.fields {
                            if field_def.default.is_none() {
                                continue;
                            }
                            if let Some(ref filter) = fields_filter {
                                if !filter.contains(field_name) {
                                    continue;
                                }
                            }
                            if working.contains_key(field_name) {
                                continue;
                            }
                            let val = field_def.default.clone().unwrap();
                            working.insert(field_name.clone(), val.clone());
                            changes.insert(field_name.clone(), val);
                            change_kinds.insert(field_name.clone(), ChangeKind::Default);
                        }
                    }
                }
            }

            if changes.is_empty() {
                if fields_filter.is_some() {
                    skipped += 1;
                    details.push(serde_json::json!({
                        "path": path,
                        "status": "skipped",
                        "reason": "No missing fields to backfill",
                    }));
                } else {
                    noop_success += 1;
                    details.push(serde_json::json!({
                        "path": path,
                        "status": "success",
                        "reason": "No missing fields to backfill",
                    }));
                }
                continue;
            }

            // Build write map honoring write_defaults/write_nulls
            let mut write_obj = raw_obj.clone();
            for (field, value) in &changes {
                if change_kinds.get(field) == Some(&ChangeKind::Default)
                    && !self.settings.write_defaults
                {
                    continue;
                }
                if self.settings.write_nulls == "omit" && value.is_null() {
                    continue;
                }
                write_obj.insert(field.clone(), value.clone());
            }

            let effective = self.coerce_types(
                &self.apply_defaults(&serde_json::Value::Object(write_obj.clone()), &type_names),
                &type_names,
            );
            let authored_mapping = entry
                .outcome()
                .document()
                .and_then(|document| {
                    crate::frontmatter::parser::parse_document(document)
                        .frontmatter
                        .and_then(|value| value.as_mapping().cloned())
                })
                .unwrap_or_default();
            let yaml_mapping = serializer::reconcile_json_object(&authored_mapping, &write_obj);
            let output = match serializer::serialize_document_with_bom(had_bom, &yaml_mapping, body)
            {
                Ok(output) => output,
                Err(error) => {
                    planning_failed += 1;
                    details.push(serde_json::json!({
                        "path": path,
                        "status": "failed",
                        "error": {
                            "code": FRONTMATTER_SERIALIZATION_FAILED,
                            "message": error.to_string()
                        }
                    }));
                    continue;
                }
            };
            generated = candidate_generated;
            plans.push(BackfillPlan {
                path: path.to_string(),
                expected_revision: entry.facts().revision.clone(),
                output,
                effective,
                type_names,
                changed_fields: changes.keys().cloned().collect(),
            });
        }

        // Validate the complete final effective corpus before the first write.
        if self.settings.default_validation == "error" {
            let mut corpus = snapshot
                .entries()
                .iter()
                .filter_map(|entry| {
                    entry
                        .effective_frontmatter()
                        .map(|frontmatter| (entry.relative_path().to_string(), frontmatter.clone()))
                })
                .collect::<HashMap<_, _>>();
            for plan in &plans {
                corpus.insert(plan.path.clone(), plan.effective.clone());
            }
            let corpus = corpus.into_iter().collect::<Vec<_>>();
            let mut resolved_files = snapshot.resolved_files_data();
            for plan in &plans {
                if let Some(file) = resolved_files
                    .iter_mut()
                    .find(|file| file.path == plan.path)
                {
                    file.frontmatter = plan.effective.clone();
                }
            }
            let resolution_index = self.build_link_resolution_index(&resolved_files);
            let mut issues = Vec::new();
            for plan in &plans {
                let validation = self.validate(&plan.effective, &plan.type_names, &plan.path);
                issues.extend(validation.issues);
                issues.extend(self.check_uniqueness_in_corpus(
                    &plan.effective,
                    &plan.type_names,
                    &plan.path,
                    &corpus,
                ));
                issues.extend(self.check_link_exists(
                    &plan.effective,
                    &plan.type_names,
                    &plan.path,
                    &resolution_index,
                ));
            }
            if issues
                .iter()
                .any(|issue| issue.severity == crate::errors::Severity::Error)
            {
                return validation_failed_error(&issues);
            }
        }

        if dry_run {
            return serde_json::json!({
                "batch_result": {
                    "total": total,
                    "succeeded": plans.len() + noop_success,
                    "failed": planning_failed,
                    "skipped": skipped,
                    "details": details,
                }
            });
        }

        let mut succeeded = noop_success;
        let mut failed = planning_failed;

        for plan in plans {
            #[cfg(test)]
            apply_injected_backfill_replacement(&self.root.join(&plan.path));
            let current = match crate::record_load::load_record(self, &plan.path) {
                Ok(current) => current,
                Err(error) => {
                    failed += 1;
                    details.push(serde_json::json!({
                        "path": plan.path,
                        "status": "failed",
                        "error": {
                            "code": "file_read_failed",
                            "message": format!("Failed to revalidate record: {error}")
                        }
                    }));
                    continue;
                }
            };
            if current.facts().revision != plan.expected_revision {
                failed += 1;
                details.push(serde_json::json!({
                    "path": plan.path,
                    "status": "failed",
                    "error": {
                        "code": CONCURRENT_MODIFICATION,
                        "message": "File was modified externally"
                    }
                }));
                continue;
            }
            if let Err(e) = self
                .held_root()
                .atomic_write(std::path::Path::new(&plan.path), plan.output.as_bytes())
            {
                failed += 1;
                details.push(serde_json::json!({
                    "path": plan.path,
                    "status": "failed",
                    "error": { "code": "io_error", "message": e.to_string() }
                }));
            } else {
                succeeded += 1;
                details.push(serde_json::json!({
                    "path": plan.path,
                    "status": "success",
                    "changed_fields": plan.changed_fields,
                }));
            }
        }

        serde_json::json!({
            "batch_result": {
                "total": total,
                "succeeded": succeeded,
                "failed": failed,
                "skipped": skipped,
                "details": details,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::inject_backfill_replacement;
    use crate::Collection;
    use serde_json::json;
    use std::fs;

    #[test]
    fn external_edit_after_planning_is_never_overwritten_and_dry_run_does_not_reload() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("mdbase.yaml"),
            "spec_version: 0.2.0\nsettings:\n  validation: error\n  write_defaults: true\n",
        )
        .unwrap();
        fs::create_dir(root.path().join("_types")).unwrap();
        fs::write(
            root.path().join("_types/item.md"),
            "---\nname: item\nfields:\n  value: { type: string, default: filled }\n---\n",
        )
        .unwrap();
        let record = root.path().join("record.md");
        fs::write(&record, "---\ntype: item\n---\noriginal\n").unwrap();
        let replacement = root.path().join("replacement.tmp");
        fs::write(&replacement, "---\ntype: item\n---\nexternal\n").unwrap();
        let collection = Collection::open(root.path()).unwrap();
        inject_backfill_replacement(&record, replacement);

        let dry_run = collection.backfill(&json!({"type": "item", "dry_run": true}));
        assert_eq!(dry_run["batch_result"]["succeeded"], 1, "{dry_run:#}");
        assert!(fs::read_to_string(&record).unwrap().contains("original"));

        let result = collection.backfill(&json!({"type": "item"}));
        assert_eq!(result["batch_result"]["failed"], 1, "{result:#}");
        assert_eq!(
            result["batch_result"]["details"][0]["error"]["code"], "concurrent_modification",
            "{result:#}"
        );
        let persisted = fs::read_to_string(record).unwrap();
        assert!(persisted.contains("external"), "{persisted}");
        assert!(!persisted.contains("value:"), "{persisted}");
    }
}
