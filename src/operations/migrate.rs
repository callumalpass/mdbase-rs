//! Migrate operation (§12.13).

use crate::errors::*;
use crate::frontmatter::parser::{parse_document, yaml_mapping_to_json};
use crate::operations::ensure_safe_relative_path;
use crate::Collection;

impl Collection {
    /// Apply a migration manifest (§12.13).
    pub fn migrate(&self, input: &serde_json::Value) -> serde_json::Value {
        let id = input.get("id").and_then(|v| v.as_str());
        let path = input.get("path").and_then(|v| v.as_str());
        let dry_run = input
            .get("dry_run")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if (id.is_some() && path.is_some()) || (id.is_none() && path.is_none()) {
            return op_error(
                INVALID_REQUEST,
                "migrate requires exactly one of 'id' or 'path'",
            );
        }

        let manifest_path = if let Some(p) = path {
            if let Err(error) = ensure_safe_relative_path(p, self.spec_profile) {
                return error;
            }
            if let Err(error) = self
                .held_root()
                .ensure_no_symlink_components(std::path::Path::new(p))
            {
                return op_error(PATH_TRAVERSAL, &error.to_string());
            }
            std::path::PathBuf::from(p)
        } else {
            let migrations_dir = std::path::Path::new(&self.settings.migrations_folder);
            if let Err(error) = self
                .held_root()
                .ensure_no_symlink_components(migrations_dir)
            {
                return op_error(PATH_TRAVERSAL, &error.to_string());
            }
            let mut found: Option<std::path::PathBuf> = None;
            let entries = self
                .held_root()
                .files_recursive(migrations_dir)
                .unwrap_or_default();
            for entry_path in entries {
                let ext = entry_path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .unwrap_or("");
                if ext != "md" && ext != "yaml" && ext != "yml" {
                    continue;
                }
                let content = match self.held_root().read_string(&entry_path) {
                    Ok(content) => content,
                    Err(_) => continue,
                };
                let doc = parse_document(&content);
                if let Some(serde_yaml::Value::Mapping(mapping)) = doc.frontmatter {
                    let frontmatter = yaml_mapping_to_json(&mapping);
                    if frontmatter.get("id").and_then(|value| value.as_str()) == id {
                        found = Some(entry_path);
                        break;
                    }
                }
            }
            match found {
                Some(p) => p,
                None => return op_error(INVALID_MIGRATION, "Migration manifest not found"),
            }
        };

        let content = match self.held_root().read_string(&manifest_path) {
            Ok(c) => c,
            Err(e) => {
                return op_error(
                    INVALID_MIGRATION,
                    &format!("Failed to read manifest: {}", e),
                )
            }
        };
        let doc = parse_document(&content);
        let frontmatter = match &doc.frontmatter {
            Some(serde_yaml::Value::Mapping(m)) => yaml_mapping_to_json(m),
            _ => return op_error(INVALID_MIGRATION, "Manifest frontmatter must be a mapping"),
        };

        let manifest_id = match frontmatter.get("id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return op_error(INVALID_MIGRATION, "Manifest missing id"),
        };

        let steps_val = frontmatter.get("steps");
        let steps_seq = match steps_val.and_then(|v| v.as_array()) {
            Some(s) => s,
            None => return op_error(INVALID_MIGRATION, "Manifest steps must be a list"),
        };

        let mut result_steps: Vec<serde_json::Value> = Vec::new();
        for step in steps_seq {
            let step_obj = match step.as_object() {
                Some(o) => o,
                None => return op_error(INVALID_MIGRATION, "Step must be a mapping"),
            };
            let step_id = match step_obj.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return op_error(INVALID_MIGRATION, "Step missing id"),
            };
            let op = match step_obj.get("op").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return op_error(INVALID_MIGRATION, "Step missing op"),
            };

            let mut step_result = serde_json::json!({
                "id": step_id,
                "op": op,
            });

            match step_result.get("op").and_then(|v| v.as_str()) {
                Some("backfill") => {
                    let mut backfill_input = serde_json::Map::new();
                    for (k, v) in step_obj {
                        if k == "id" || k == "op" {
                            continue;
                        }
                        backfill_input.insert(k.clone(), v.clone());
                    }
                    if dry_run {
                        backfill_input.insert("dry_run".to_string(), serde_json::Value::Bool(true));
                    }
                    let backfill_result =
                        self.backfill_legacy(&serde_json::Value::Object(backfill_input));
                    if backfill_result.get("error").is_some() {
                        return op_error(MIGRATION_FAILED, "Migration failed");
                    }
                    step_result["status"] = serde_json::Value::String("success".to_string());
                    step_result["result"] = backfill_result;
                }
                Some("add_field") | Some("rename_field") | Some("change_type")
                | Some("rename_type") | Some("move_path") => {
                    step_result["status"] = serde_json::Value::String("manual".to_string());
                }
                Some(_) | None => {
                    return op_error(INVALID_MIGRATION, "Unknown migration op");
                }
            }

            result_steps.push(step_result);
        }

        serde_json::json!({
            "migration_result": {
                "id": manifest_id,
                "steps": result_steps,
            }
        })
    }
}
