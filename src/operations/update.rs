//! Update operation (§12.3).

use crate::api::operations::{UpdateInput, UpdateOutput};
use crate::errors::*;
use crate::frontmatter::parser::{parse_document, yaml_mapping_to_json};
use crate::frontmatter::serializer;
use crate::operations::{
    atomic_write, ensure_no_symlink_components, ensure_regular_record_file, ensure_revision,
    mutation_record_path,
};
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
            document,
            last_known_mtime,
            if_revision,
        } = input;
        let path = match mutation_record_path(self, &path) {
            Ok(path) => path,
            Err(error) => return error,
        };
        if let Err(error) =
            ensure_no_symlink_components(&self.root, path.as_str(), self.spec_profile)
        {
            return error;
        }
        let _write_lock = match crate::transactions::WriteLock::acquire(self) {
            Ok(write_lock) => write_lock,
            Err(error) => return op_error(error.code(), &error.to_string()),
        };

        let new_body = new_body.as_deref();
        let full_path = path.under(&self.root);
        if let Err(error) = ensure_regular_record_file(&full_path, path.as_str()) {
            return error;
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
                        &format!("File '{}' was modified externally", path.as_str()),
                    );
                }
            }
        }

        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(e) => return op_error(FILE_NOT_FOUND, &format!("Failed to read: {}", e)),
        };
        if let Err(error) = ensure_revision(&full_path, path.as_str(), if_revision.as_deref()) {
            return error;
        }

        let doc = parse_document(&content);
        let replacement_doc = document.as_deref().map(parse_document);
        let replacement_mapping = match replacement_doc
            .as_ref()
            .and_then(|doc| doc.frontmatter.as_ref())
        {
            Some(serde_yaml::Value::Mapping(mapping)) => Some(mapping.clone()),
            Some(_) => {
                return op_error(
                    INVALID_FRONTMATTER,
                    "Replacement document frontmatter must be a YAML mapping",
                )
            }
            None if document.is_some() => Some(serde_yaml::Mapping::new()),
            None => None,
        };

        let existing_mapping = match replacement_mapping.as_ref() {
            Some(mapping) => mapping.clone(),
            None => match &doc.frontmatter {
                Some(serde_yaml::Value::Mapping(m)) => m.clone(),
                _ => serde_yaml::Mapping::new(),
            },
        };

        // Merge fields
        let merged = if document.is_some() {
            existing_mapping.clone()
        } else {
            serializer::merge_fields(&existing_mapping, &fields, &self.settings.write_nulls)
        };
        let merged_json = yaml_mapping_to_json(&merged);

        // Determine types
        let type_names = self.determine_types(&merged_json);

        // Apply generated (now_on_write)
        let mut merged_obj = match merged_json.as_object() {
            Some(o) => o.clone(),
            None => serde_json::Map::new(),
        };
        self.apply_generated(&mut merged_obj, &type_names, false, Some(path.as_str()));

        // Apply defaults for effective frontmatter
        let effective =
            self.apply_defaults(&serde_json::Value::Object(merged_obj.clone()), &type_names);

        // Validate
        if self.settings.default_validation == "error" {
            let validation_frontmatter = if self.spec_profile == crate::SpecProfile::V03 {
                &serde_json::Value::Object(merged_obj.clone())
            } else {
                &effective
            };
            let mut validation = self.validate(validation_frontmatter, &type_names, path.as_str());

            // Cross-file uniqueness checks for update
            let uniqueness_issues = self.check_uniqueness(&effective, &type_names, path.as_str());
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
                        &format!("File '{}' was modified during operation", path.as_str()),
                    );
                }
            }
        }

        // Build frontmatter for writing (honor write_defaults/write_nulls)
        let mut write_obj = merged_obj.clone();
        if self.spec_profile != crate::SpecProfile::V03 && self.settings.write_defaults {
            if let Some(eff_map) = effective.as_object() {
                for (k, v) in eff_map {
                    if !write_obj.contains_key(k) {
                        write_obj.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        if self.settings.write_nulls == "omit" && document.is_none() {
            write_obj.retain(|_, v| !v.is_null());
        }

        // Write file
        let write_mapping =
            crate::frontmatter::parser::json_to_yaml_mapping(&serde_json::Value::Object(write_obj));
        let body = match replacement_doc.as_ref() {
            Some(candidate) => candidate.body.as_str(),
            None => match new_body {
                Some(b) => b,
                None => &doc.body,
            },
        };
        let output = if document.is_some() && replacement_mapping.as_ref() == Some(&write_mapping) {
            document.as_deref().unwrap_or_default().to_string()
        } else {
            serializer::serialize_document(&write_mapping, body)
        };

        if let Err(error) =
            ensure_no_symlink_components(&self.root, path.as_str(), self.spec_profile)
        {
            return error;
        }
        if let Err(e) = atomic_write(&full_path, output.as_bytes()) {
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
        let effective =
            self.evaluate_computed_fields(effective, &type_names, path.as_str(), Some(body));

        UpdateOutput {
            path: path.to_string(),
            frontmatter: effective,
            body: body.to_string(),
            warnings: result_warnings,
        }
        .into_json()
    }
}
