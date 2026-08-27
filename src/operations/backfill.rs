//! Backfill operation (§12.8).

use std::collections::{HashMap, HashSet};

use crate::errors::*;
use crate::frontmatter::parser::{parse_document_for_rewrite, yaml_mapping_to_json};
use crate::frontmatter::serializer;
use crate::types::schema::GeneratedStrategy;
use crate::Collection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChangeKind {
    Default,
    Generated,
}

struct BackfillPlan {
    path: String,
    body: String,
    had_bom: bool,
    write_obj: serde_json::Map<String, serde_json::Value>,
    changed_fields: Vec<String>,
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

        let matching_paths = self.query_matching_paths_with_types(where_clause, &filter_types);
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
        let mut details: Vec<serde_json::Value> = Vec::new();

        for path in &matching_paths {
            let full_path = self.root.join(path);
            let content = match std::fs::read_to_string(&full_path) {
                Ok(c) => c,
                Err(e) => {
                    details.push(serde_json::json!({
                        "path": path,
                        "status": "failed",
                        "error": { "code": "io_error", "message": e.to_string() }
                    }));
                    continue;
                }
            };
            let (doc, had_bom) = parse_document_for_rewrite(&content);
            let raw_frontmatter = match &doc.frontmatter {
                Some(serde_yaml::Value::Mapping(m)) => yaml_mapping_to_json(m),
                _ => serde_json::json!({}),
            };
            let raw_obj = raw_frontmatter.as_object().cloned().unwrap_or_default();

            let type_names = self.determine_types_for_path(&raw_frontmatter, Some(path));

            let mut working = raw_obj.clone();
            let mut changes: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
            let mut change_kinds: HashMap<String, ChangeKind> = HashMap::new();

            if apply_generated {
                let mut generated_fields: Vec<(String, GeneratedStrategy)> = Vec::new();
                let mut seen = HashSet::new();
                for type_name in &type_names {
                    if let Some(type_def) = self.types.get(type_name) {
                        for (field_name, field_def) in &type_def.fields {
                            if field_def.generated.is_none() {
                                continue;
                            }
                            if let Some(ref filter) = fields_filter {
                                if !filter.contains(field_name) {
                                    continue;
                                }
                            }
                            if raw_obj.contains_key(field_name) {
                                continue;
                            }
                            if seen.insert(field_name.clone()) {
                                generated_fields.push((
                                    field_name.clone(),
                                    field_def.generated.clone().unwrap(),
                                ));
                            }
                        }
                    }
                }

                generated_fields.sort_by(|a, b| {
                    let a_dep = match &a.1 {
                        GeneratedStrategy::Derived { from, .. } => Some(from.clone()),
                        _ => None,
                    };
                    let b_dep = match &b.1 {
                        GeneratedStrategy::Derived { from, .. } => Some(from.clone()),
                        _ => None,
                    };
                    match (&a_dep, &b_dep) {
                        (None, Some(_)) => std::cmp::Ordering::Less,
                        (Some(_), None) => std::cmp::Ordering::Greater,
                        (Some(a_from), _) if *a_from == b.0 => std::cmp::Ordering::Greater,
                        (_, Some(b_from)) if *b_from == a.0 => std::cmp::Ordering::Less,
                        _ => std::cmp::Ordering::Equal,
                    }
                });

                for (field_name, strategy) in generated_fields {
                    if working.contains_key(&field_name) {
                        continue;
                    }
                    let value = self.generate_value(
                        &strategy,
                        &field_name,
                        &type_names,
                        &working,
                        Some(path),
                    );
                    working.insert(field_name.clone(), value.clone());
                    changes.insert(field_name.clone(), value);
                    change_kinds.insert(field_name.clone(), ChangeKind::Generated);
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

            // Validation (abort all on first failure)
            if self.settings.default_validation == "error" {
                let effective =
                    self.apply_defaults(&serde_json::Value::Object(working.clone()), &type_names);
                let validation = self.validate(&effective, &type_names, path);
                if !validation.valid {
                    return validation_failed_error(&validation.issues);
                }
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

            plans.push(BackfillPlan {
                path: path.to_string(),
                body: doc.body.clone(),
                had_bom,
                write_obj,
                changed_fields: changes.keys().cloned().collect(),
            });
        }

        if dry_run {
            return serde_json::json!({
                "batch_result": {
                    "total": total,
                    "succeeded": plans.len() + noop_success,
                    "failed": 0,
                    "skipped": skipped,
                    "details": details,
                }
            });
        }

        let mut succeeded = noop_success;
        let mut failed = 0usize;

        for plan in plans {
            let full_path = self.root.join(&plan.path);
            let yaml_mapping = crate::frontmatter::parser::json_to_yaml_mapping(
                &serde_json::Value::Object(plan.write_obj),
            );
            let output =
                serializer::serialize_document_with_bom(plan.had_bom, &yaml_mapping, &plan.body);
            if let Err(e) = crate::operations::atomic_write(&full_path, output.as_bytes()) {
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
