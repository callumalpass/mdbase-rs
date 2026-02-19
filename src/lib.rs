//! mdbase - Rust implementation of the mdbase specification.
//!
//! Uses SQLite as a backing store for queries, compiling mdbase expressions
//! to SQL WHERE clauses via json_extract().

pub mod errors;

pub mod api;
pub mod cache;
pub mod config;
pub mod expressions;
pub mod frontmatter;
pub mod generated;
pub mod init;
pub mod links;
pub mod matching;
pub mod operations;
pub mod query;
pub mod types;
pub mod validation;
pub mod watch;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::expressions::expression_references_field;
use crate::types::inheritance;
use crate::types::loader;
use crate::types::schema::*;

/// Parsed config settings used at runtime.
#[derive(Debug, Clone)]
pub struct Settings {
    pub extensions: Vec<String>,
    pub exclude: Vec<String>,
    pub include_subfolders: bool,
    pub types_folder: String,
    pub migrations_folder: String,
    pub explicit_type_keys: Vec<String>,
    pub write_defaults: bool,
    pub default_validation: String,
    pub default_strict: serde_json::Value,
    pub timezone: Option<String>,
    pub id_field: String,
    pub id_field_explicit: bool,
    pub write_nulls: String,
    pub write_empty_lists: bool,
    pub rename_update_refs: bool,
    pub cache_folder: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            extensions: Vec::new(),
            exclude: vec![".git".into(), "node_modules".into(), ".mdbase".into()],
            include_subfolders: true,
            types_folder: "_types".into(),
            migrations_folder: "_types/_migrations".into(),
            explicit_type_keys: vec!["type".into(), "types".into()],
            write_defaults: true,
            default_validation: "warn".into(),
            default_strict: serde_json::Value::Bool(false),
            timezone: None,
            id_field: "id".into(),
            id_field_explicit: false,
            write_nulls: "omit".into(),
            write_empty_lists: true,
            rename_update_refs: true,
            cache_folder: ".mdbase".into(),
        }
    }
}

/// A loaded mdbase collection.
pub struct Collection {
    pub root: PathBuf,
    pub settings: Settings,
    pub types: HashMap<String, TypeDef>,
    pub type_warnings: Vec<String>,
}

impl Collection {
    /// Open a collection from a root directory.
    pub fn open(root: &Path) -> Result<Self, serde_json::Value> {
        let config_result = config::load_config_for_open(root);
        if config_result.get("valid") != Some(&serde_json::Value::Bool(true)) {
            return Err(config_result);
        }

        let config = &config_result["config"];
        let settings_json = &config["settings"];

        let settings = Settings {
            extensions: settings_json["extensions"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            exclude: settings_json["exclude"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_else(|| vec![".git".into(), "node_modules".into(), ".mdbase".into()]),
            include_subfolders: settings_json["include_subfolders"]
                .as_bool()
                .unwrap_or(true),
            types_folder: settings_json["types_folder"]
                .as_str()
                .unwrap_or("_types")
                .to_string(),
            migrations_folder: settings_json["migrations_folder"]
                .as_str()
                .unwrap_or("_types/_migrations")
                .to_string(),
            explicit_type_keys: settings_json["explicit_type_keys"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_else(|| vec!["type".into(), "types".into()]),
            write_defaults: settings_json["write_defaults"].as_bool().unwrap_or(true),
            default_validation: settings_json["default_validation"]
                .as_str()
                .unwrap_or("warn")
                .to_string(),
            default_strict: settings_json["default_strict"].clone(),
            timezone: settings_json["timezone"].as_str().map(|s| s.to_string()),
            id_field: settings_json["id_field"]
                .as_str()
                .unwrap_or("id")
                .to_string(),
            id_field_explicit: settings_json["id_field_explicit"]
                .as_bool()
                .unwrap_or(false),
            write_nulls: settings_json["write_nulls"]
                .as_str()
                .unwrap_or("omit")
                .to_string(),
            write_empty_lists: settings_json["write_empty_lists"].as_bool().unwrap_or(true),
            rename_update_refs: settings_json["rename_update_refs"]
                .as_bool()
                .unwrap_or(true),
            cache_folder: settings_json["cache_folder"]
                .as_str()
                .unwrap_or(".mdbase")
                .to_string(),
        };

        // Load types
        let load_result = loader::load_types_with_warnings(
            root,
            &settings.types_folder,
            &settings.migrations_folder,
        )
        .map_err(|e| {
            serde_json::json!({
                "valid": false,
                "error": { "code": "invalid_type_definition", "message": e }
            })
        })?;

        let mut types = load_result.types;
        let type_warnings = load_result.warnings;

        // Resolve inheritance
        inheritance::resolve_inheritance(&mut types).map_err(|e| {
            // Determine error code based on the message
            let code = if e.contains("Circular") {
                "circular_inheritance"
            } else if e.contains("Unknown type") || e.contains("extends unknown") {
                "missing_parent_type"
            } else {
                "invalid_type_definition"
            };
            serde_json::json!({
                "valid": false,
                "error": { "code": code, "message": e }
            })
        })?;

        // Validate computed fields (§5.12)
        for type_def in types.values() {
            Self::validate_computed_fields(type_def)?;
        }

        Ok(Collection {
            root: root.to_path_buf(),
            settings,
            types,
            type_warnings,
        })
    }

    /// Validate computed field constraints (§5.12).
    fn validate_computed_fields(type_def: &TypeDef) -> Result<(), serde_json::Value> {
        let mut computed_fields: Vec<(&str, &str)> = Vec::new(); // (name, expression)

        for (name, field) in &type_def.fields {
            if let Some(ref expr) = field.computed {
                // Computed fields MUST NOT be required
                if field.required {
                    return Err(serde_json::json!({
                        "valid": false,
                        "error": { "code": "invalid_type_definition", "message": format!("Computed field '{}' cannot be required", name) }
                    }));
                }
                // Computed fields MUST NOT have default
                if field.default.is_some() {
                    return Err(serde_json::json!({
                        "valid": false,
                        "error": { "code": "invalid_type_definition", "message": format!("Computed field '{}' cannot have a default value", name) }
                    }));
                }
                // Computed fields MUST NOT have generated
                if field.generated.is_some() {
                    return Err(serde_json::json!({
                        "valid": false,
                        "error": { "code": "invalid_type_definition", "message": format!("Computed field '{}' cannot have a generated strategy", name) }
                    }));
                }
                computed_fields.push((name, expr));
            }
        }

        // Check for circular dependencies
        if computed_fields.len() > 1 {
            // Build dependency graph: for each computed field, find which other
            // computed fields its expression references
            let computed_names: std::collections::HashSet<&str> =
                computed_fields.iter().map(|(n, _)| *n).collect();

            // Simple dependency detection: check if expression contains field name as identifier
            for (name, expr) in &computed_fields {
                // Check for self-reference
                if expression_references_field(expr, name) {
                    return Err(serde_json::json!({
                        "valid": false,
                        "error": { "code": "circular_computed", "message": format!("Computed field '{}' references itself", name) }
                    }));
                }
            }

            // Check for cycles using topological sort
            let mut deps: HashMap<&str, Vec<&str>> = HashMap::new();
            for (name, expr) in &computed_fields {
                let mut field_deps = Vec::new();
                for dep_name in &computed_names {
                    if dep_name != name && expression_references_field(expr, dep_name) {
                        field_deps.push(*dep_name);
                    }
                }
                deps.insert(name, field_deps);
            }

            // Topological sort to detect cycles
            let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();
            let mut in_progress: std::collections::HashSet<&str> = std::collections::HashSet::new();

            fn has_cycle<'a>(
                node: &'a str,
                deps: &HashMap<&'a str, Vec<&'a str>>,
                visited: &mut std::collections::HashSet<&'a str>,
                in_progress: &mut std::collections::HashSet<&'a str>,
            ) -> bool {
                if in_progress.contains(node) {
                    return true;
                }
                if visited.contains(node) {
                    return false;
                }
                in_progress.insert(node);
                if let Some(node_deps) = deps.get(node) {
                    for dep in node_deps {
                        if has_cycle(dep, deps, visited, in_progress) {
                            return true;
                        }
                    }
                }
                in_progress.remove(node);
                visited.insert(node);
                false
            }

            for (name, _) in &computed_fields {
                if has_cycle(name, &deps, &mut visited, &mut in_progress) {
                    return Err(serde_json::json!({
                        "valid": false,
                        "error": { "code": "circular_computed", "message": format!("Circular dependency in computed field '{}'", name) }
                    }));
                }
            }
        } else if computed_fields.len() == 1 {
            // Check self-reference for single computed field
            let (name, expr) = computed_fields[0];
            if expression_references_field(expr, name) {
                return Err(serde_json::json!({
                    "valid": false,
                    "error": { "code": "circular_computed", "message": format!("Computed field '{}' references itself", name) }
                }));
            }
        }

        // Check that match rules don't reference computed fields (§6.4)
        if !computed_fields.is_empty() {
            if let Some(ref match_rules) = type_def.match_rules {
                // Check where clause field names
                if let Some(ref where_clause) = match_rules.where_clause {
                    if let Some(obj) = where_clause.as_object() {
                        for field_name in obj.keys() {
                            if computed_fields.iter().any(|(name, _)| *name == field_name) {
                                return Err(serde_json::json!({
                                    "valid": false,
                                    "error": { "code": "invalid_type_definition", "message": format!("Match rule 'where' cannot reference computed field '{}'", field_name) }
                                }));
                            }
                        }
                    }
                }
                // Check fields_present
                if let Some(ref fields_present) = match_rules.fields_present {
                    for field_name in fields_present {
                        if computed_fields.iter().any(|(name, _)| *name == field_name) {
                            return Err(serde_json::json!({
                                "valid": false,
                                "error": { "code": "invalid_type_definition", "message": format!("Match rule 'fields_present' cannot reference computed field '{}'", field_name) }
                            }));
                        }
                    }
                }
            }
        }

        Ok(())
    }

    pub(crate) fn config_strict_mode(&self) -> StrictMode {
        match &self.settings.default_strict {
            serde_json::Value::Bool(true) => StrictMode::Error,
            serde_json::Value::Bool(false) => StrictMode::Off,
            serde_json::Value::String(s) if s == "warn" => StrictMode::Warn,
            _ => StrictMode::Off,
        }
    }

    /// Scan all markdown files in the collection.
    pub(crate) fn scan_collection_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        self.scan_dir_recursive(&self.root, &mut files);
        files
    }

    fn scan_dir_recursive(&self, dir: &Path, files: &mut Vec<PathBuf>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();

            if path.is_dir() {
                if self.settings.include_subfolders {
                    let rel = path
                        .strip_prefix(&self.root)
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if !self.is_excluded(&rel) {
                        self.scan_dir_recursive(&path, files);
                    }
                }
            } else if path.is_file() {
                let rel = path
                    .strip_prefix(&self.root)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                if !self.is_excluded(&rel) && self.is_valid_extension(&rel) {
                    files.push(path);
                }
            }
        }
    }
}

// Standalone helper functions have been extracted to their respective modules:
// - op_error, issue_to_json -> crate::errors
// - apply_transform, slugify, derive_path -> crate::generated
// - coerce_value -> crate::validation::coercion
// - match_glob_pattern -> crate::matching::glob
// - count_leading_dotdot, normalize_link_path, normalize_segments -> crate::links::parser
// - compute_relative_path -> crate::links::resolver
// - expression_references_field, expr_contains_ident, is_truthy_value -> crate::expressions
// - QueryEvalContext -> crate::query::engine
