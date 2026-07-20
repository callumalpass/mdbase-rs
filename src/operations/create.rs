//! Create operation (§12.1).

use crate::api::operations::{CreateInput, CreateOutput};
use crate::errors::*;
use crate::frontmatter;
use crate::frontmatter::serializer;
use crate::generated::derive_path;
use crate::matching::engine::matches_rules;
use crate::operations::ensure_safe_relative_path;
use crate::Collection;

impl Collection {
    /// Create a file (§12.1).
    pub fn create(&self, input: &serde_json::Value) -> serde_json::Value {
        let input = CreateInput::parse(input);
        let type_name = input.type_name.as_deref();
        let frontmatter_input = input.frontmatter;
        let body = input.body.as_str();
        let path_input = input.path.as_deref();
        let if_revision = input.if_revision.as_deref();

        // Determine type names
        let mut type_names: Vec<String> = Vec::new();
        if let Some(tn) = type_name {
            let tn_lower = tn.to_lowercase();
            if !self.types.contains_key(&tn_lower) {
                return op_error(UNKNOWN_TYPE, &format!("Unknown type: {}", tn));
            }
            type_names.push(tn_lower);
        }
        // Also check frontmatter for type key
        let fm_types = self.determine_types(&frontmatter_input);
        for t in fm_types {
            if !type_names.contains(&t) {
                type_names.push(t);
            }
        }

        // Build frontmatter early so path_pattern can use generated/default values
        let mut fm_obj = match frontmatter_input.as_object() {
            Some(o) => o.clone(),
            None => serde_json::Map::new(),
        };

        // Add type key if specified and explicit_type_keys is non-empty
        if let Some(tn) = type_name {
            if !self.settings.explicit_type_keys.is_empty()
                && !fm_obj.contains_key("type")
                && !fm_obj.contains_key("types")
            {
                fm_obj.insert(
                    "type".to_string(),
                    serde_json::Value::String(tn.to_string()),
                );
            }
        }

        // Generate values before path derivation so path_pattern can use them
        self.apply_generated(&mut fm_obj, &type_names, true, path_input);

        // Apply defaults to a temporary copy for path derivation
        let fm_with_defaults =
            self.apply_defaults(&serde_json::Value::Object(fm_obj.clone()), &type_names);

        // Determine path
        let path = match path_input {
            Some(p) => {
                // Empty path check
                if p.is_empty() {
                    return op_error(PATH_REQUIRED, "path must not be empty");
                }
                if let Err(error) = ensure_safe_relative_path(p, self.spec_profile) {
                    return error;
                }
                p.to_string()
            }
            None => {
                // Try to derive from path_pattern or filename_pattern
                if let Some(tn) = type_names.first() {
                    if let Some(type_def) = self.types.get(tn) {
                        let pattern = type_def
                            .path_pattern
                            .as_ref()
                            .or(type_def.filename_pattern.as_ref());
                        if let Some(pattern) = pattern {
                            match derive_path(pattern, &fm_with_defaults) {
                                Some(p) => p,
                                None => return op_error(PATH_REQUIRED, "Cannot determine path"),
                            }
                        } else {
                            return op_error(
                                PATH_REQUIRED,
                                "No path provided and no filename_pattern",
                            );
                        }
                    } else {
                        return op_error(PATH_REQUIRED, "Cannot determine path");
                    }
                } else {
                    return op_error(PATH_REQUIRED, "No path provided");
                }
            }
        };
        if path.is_empty() {
            return op_error(PATH_REQUIRED, "path must not be empty");
        }
        if let Err(error) = ensure_safe_relative_path(&path, self.spec_profile) {
            return error;
        }

        // Check existence
        let full_path = self.root.join(&path);
        if full_path.exists() {
            return op_error(PATH_CONFLICT, &format!("File already exists: {}", path));
        }
        if if_revision.is_some() {
            return op_error(
                CONCURRENT_MODIFICATION,
                &format!("File '{}' no longer matches the requested revision", path),
            );
        }

        // Apply defaults for effective frontmatter (for validation and output)
        let effective =
            self.apply_defaults(&serde_json::Value::Object(fm_obj.clone()), &type_names);

        // Check match rules (§6.3, §6.4): created file must satisfy type match rules
        for tn in &type_names {
            if let Some(type_def) = self.types.get(tn) {
                if let Some(ref rules) = type_def.match_rules {
                    if !matches_rules(rules, &path, &effective) {
                        return op_error(
                            "match_failed",
                            &format!(
                                "Created file does not satisfy match rules for type '{}'",
                                tn
                            ),
                        );
                    }
                }
            }
        }

        // Validate
        let mut result_warnings: Vec<serde_json::Value> = Vec::new();
        if self.settings.default_validation == "error" {
            let validation = self.validate(&effective, &type_names, &path);
            if !validation.valid {
                return validation_failed_error(&validation.issues);
            }
        } else if self.settings.default_validation == "warn" {
            let validation = self.validate(&effective, &type_names, &path);
            // §5.5: strict: true causes validation failure regardless of validation level
            let has_strict_errors = validation
                .issues
                .iter()
                .any(|i| i.code == UNKNOWN_FIELD && i.severity == Severity::Error);
            if has_strict_errors {
                return validation_failed_error(&validation.issues);
            }
            for issue in &validation.issues {
                result_warnings.push(issue_to_json(issue));
            }
        }
        for type_name in &type_names {
            if let Some(type_def) = self.types.get(type_name) {
                for (field_name, field_def) in &type_def.fields {
                    if field_def.deprecated.is_some() && fm_obj.contains_key(field_name) {
                        result_warnings.push(serde_json::json!({
                            "code": "deprecated_field",
                            "message": format!("Field '{}' is deprecated", field_name),
                            "field": field_name,
                        }));
                    }
                }
            }
        }

        // Build frontmatter for writing (honor write_defaults/write_nulls)
        let mut write_obj = fm_obj.clone();
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
        let yaml_mapping =
            frontmatter::parser::json_to_yaml_mapping(&serde_json::Value::Object(write_obj));
        let content = serializer::serialize_document(&yaml_mapping, body);

        if let Some(parent) = full_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        if let Err(e) = std::fs::write(&full_path, &content) {
            let error_str = e.to_string();
            if error_str.contains("NUL") || error_str.contains("null byte") {
                return op_error(INVALID_PATH, &format!("Invalid path: {}", e));
            }
            return op_error("io_error", &format!("Failed to write file: {}", e));
        }

        CreateOutput {
            path,
            types: type_names,
            frontmatter: effective,
            body: body.to_string(),
            valid: true,
            warnings: result_warnings,
        }
        .into_json()
    }
}
