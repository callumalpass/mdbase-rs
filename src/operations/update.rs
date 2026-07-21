//! Update operation (§12.3).

use crate::api::operations::{UpdateInput, UpdateOutput};
use crate::errors::*;
use crate::frontmatter::parser::{parse_document, yaml_mapping_to_json};
use crate::frontmatter::serializer;
use crate::operations::{ensure_revision, ensure_safe_relative_path};
use crate::Collection;

impl Collection {
    /// Update a file (§12.3).
    pub fn update(&self, input: &serde_json::Value) -> serde_json::Value {
        let input = match UpdateInput::parse(input) {
            Ok(parsed) => parsed,
            Err(err) => return err,
        };
        let UpdateInput {
            path,
            fields,
            body: new_body,
            last_known_mtime,
            if_revision,
        } = input;
        if let Err(error) = ensure_safe_relative_path(&path, self.spec_profile) {
            return error;
        }

        let new_body = new_body.as_deref();
        let full_path = self.root.join(&path);
        if !full_path.exists() {
            return op_error(FILE_NOT_FOUND, &format!("File not found: {}", path));
        }

        // Concurrent modification detection: record mtime at read time
        let read_mtime = std::fs::metadata(&full_path)
            .and_then(|m| m.modified())
            .ok();

        // If caller provides last_known_mtime, check for external modifications
        if let Some(known_ms) = last_known_mtime {
            if let Some(current) = &read_mtime {
                let current_ms = current
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                if current_ms != known_ms {
                    return op_error(
                        CONCURRENT_MODIFICATION,
                        &format!("File '{}' was modified externally", path),
                    );
                }
            }
        }

        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(e) => return op_error(FILE_NOT_FOUND, &format!("Failed to read: {}", e)),
        };
        if let Err(error) = ensure_revision(&full_path, &path, if_revision.as_deref()) {
            return error;
        }

        let doc = parse_document(&content);

        let existing_mapping = match &doc.frontmatter {
            Some(serde_yaml::Value::Mapping(m)) => m.clone(),
            _ => serde_yaml::Mapping::new(),
        };

        // Merge fields
        let merged =
            serializer::merge_fields(&existing_mapping, &fields, &self.settings.write_nulls);
        let merged_json = yaml_mapping_to_json(&merged);

        // Determine types
        let type_names = self.determine_types(&merged_json);

        // Apply generated (now_on_write)
        let mut merged_obj = match merged_json.as_object() {
            Some(o) => o.clone(),
            None => serde_json::Map::new(),
        };
        self.apply_generated(&mut merged_obj, &type_names, false, Some(&path));

        // Apply defaults for effective frontmatter
        let effective =
            self.apply_defaults(&serde_json::Value::Object(merged_obj.clone()), &type_names);

        // Validate
        if self.settings.default_validation == "error" {
            let mut validation = self.validate(&effective, &type_names, &path);

            // Cross-file uniqueness checks for update
            let uniqueness_issues = self.check_uniqueness(&effective, &type_names, &path);
            validation.issues.extend(uniqueness_issues.iter().cloned());
            if !uniqueness_issues.is_empty() {
                validation.valid = false;
            }

            if !validation.valid {
                return validation_failed_error(&validation.issues);
            }
        }

        // Concurrent modification check before write
        if let Some(recorded) = &read_mtime {
            if let Ok(current) = std::fs::metadata(&full_path).and_then(|m| m.modified()) {
                if current != *recorded {
                    return op_error(
                        CONCURRENT_MODIFICATION,
                        &format!("File '{}' was modified during operation", path),
                    );
                }
            }
        }

        // Build frontmatter for writing (honor write_defaults/write_nulls)
        let mut write_obj = merged_obj.clone();
        if self.settings.write_defaults {
            if let Some(eff_map) = effective.as_object() {
                for (k, v) in eff_map {
                    if !write_obj.contains_key(k) {
                        write_obj.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        if self.settings.write_nulls == "omit" {
            write_obj.retain(|_, v| !v.is_null());
        }

        // Write file
        let write_mapping =
            crate::frontmatter::parser::json_to_yaml_mapping(&serde_json::Value::Object(write_obj));
        let body = match new_body {
            Some(b) => b,
            None => &doc.body,
        };
        let output = serializer::serialize_document(&write_mapping, body);

        if let Err(e) = std::fs::write(&full_path, &output) {
            return op_error("io_error", &format!("Failed to write: {}", e));
        }

        // Collect warnings (deprecated fields, etc.)
        let mut result_warnings: Vec<serde_json::Value> = Vec::new();
        for type_name in &type_names {
            if let Some(type_def) = self.types.get(type_name) {
                if let Some(fields_obj) = fields.as_object() {
                    for field_name in fields_obj.keys() {
                        if let Some(field_def) = type_def.fields.get(field_name) {
                            if field_def.deprecated.is_some() {
                                result_warnings.push(serde_json::json!({
                                    "code": "deprecated_field",
                                    "message": format!("Field '{}' is deprecated", field_name),
                                    "field": field_name,
                                }));
                            }
                        }
                    }
                }
            }
        }

        // Evaluate computed fields for the returned result (not written to disk)
        let effective = self.evaluate_computed_fields(effective, &type_names, &path, Some(body));

        UpdateOutput {
            path,
            frontmatter: effective,
            body: body.to_string(),
            warnings: result_warnings,
        }
        .into_json()
    }
}
