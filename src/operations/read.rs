//! Read operation (§12.2).

use std::path::Path;
use crate::errors::*;
use crate::frontmatter::parser::{parse_document, is_parse_error, yaml_mapping_to_json};
use crate::Collection;

impl Collection {
    /// Read a file (§12.2).
    pub fn read(&self, input: &serde_json::Value) -> serde_json::Value {
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return op_error(INVALID_PATH, "path is required"),
        };

        // Check exclusions
        if self.is_excluded(path) || !self.is_valid_extension(path) {
            return op_error(FILE_NOT_FOUND, &format!("File not found: {}", path));
        }

        let full_path = self.root.join(path);
        if !full_path.exists() {
            return op_error(FILE_NOT_FOUND, &format!("File not found: {}", path));
        }

        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(_e) => return op_error(INVALID_FRONTMATTER, "File contains invalid UTF-8"),
        };

        let doc = parse_document(&content);

        // Check for parse errors
        if let Some(ref fm) = doc.frontmatter {
            if is_parse_error(fm) {
                return op_error(INVALID_FRONTMATTER, "Failed to parse YAML frontmatter");
            }
        }

        // Get frontmatter as JSON
        let mut warnings: Vec<serde_json::Value> = Vec::new();
        let raw_frontmatter = match &doc.frontmatter {
            Some(serde_yaml::Value::Mapping(m)) => yaml_mapping_to_json(m),
            Some(serde_yaml::Value::Null) => {
                let validation_level = &self.settings.default_validation;
                if validation_level == "error" {
                    return op_error(INVALID_FRONTMATTER, "Frontmatter is null");
                }
                serde_json::json!({})
            }
            None => serde_json::json!({}),
            Some(_other) => {
                // Non-mapping frontmatter (list, scalar) - structural error
                let validation_level = &self.settings.default_validation;
                if validation_level == "off" {
                    // At "off" level, treat as empty frontmatter silently
                    serde_json::json!({})
                } else if validation_level == "warn" {
                    // At "warn" level, treat as empty with warning
                    warnings.push(serde_json::json!({
                        "code": INVALID_FRONTMATTER,
                        "message": "Frontmatter must be a YAML mapping",
                    }));
                    serde_json::json!({})
                } else {
                    // At "error" level, non-mapping frontmatter is an error
                    return op_error(INVALID_FRONTMATTER, "Frontmatter must be a YAML mapping");
                }
            }
        };

        // Determine types (using path for match rule evaluation)
        let type_names = self.determine_types_for_path(&raw_frontmatter, Some(path));

        // Apply defaults for effective frontmatter
        let effective = self.apply_defaults(&raw_frontmatter, &type_names);

        // Apply type coercion (§7.16)
        let effective = self.coerce_types(&effective, &type_names);

        // Evaluate computed fields (§5.12)
        let effective = self.evaluate_computed_fields(effective, &type_names, path, Some(doc.body.as_str()));

        // File metadata
        let file_name = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let folder = Path::new(path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("");
        let file_metadata = std::fs::metadata(&full_path).ok();
        let file_size = file_metadata.as_ref().map(|m| m.len()).unwrap_or(0);
        let file_mtime = file_metadata.as_ref().and_then(|m| m.modified().ok()).map(|t| {
            let dt: chrono::DateTime<chrono::Utc> = t.into();
            dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
        });

        // Validation
        let validation_level = &self.settings.default_validation;
        let validation = if validation_level == "off" {
            ValidationResult { valid: true, issues: Vec::new() }
        } else {
            self.validate(&effective, &type_names, path)
        };
        let issues_json: Vec<serde_json::Value> = validation.issues.iter().map(issue_to_json).collect();

        // At "warn" level, validation issues don't make the result invalid
        let effective_valid = if validation_level == "warn" {
            true
        } else {
            validation.valid
        };

        let mut result = serde_json::json!({
            "path": path,
            "types": type_names,
            "frontmatter": effective,
            "raw_frontmatter": raw_frontmatter,
            "body": doc.body,
            "file": {
                "name": file_name,
                "folder": folder,
                "size": file_size,
                "mtime": file_mtime.as_deref().unwrap_or(""),
            },
            "valid": effective_valid,
            "validation": {
                "valid": validation.valid,
                "issues": issues_json,
            },
        });

        if !warnings.is_empty() {
            result["warnings"] = serde_json::Value::Array(warnings);
        }

        result
    }
}
