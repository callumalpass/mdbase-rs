//! mdbase - Rust implementation of the mdbase specification.
//!
//! Uses SQLite as a backing store for queries, compiling mdbase expressions
//! to SQL WHERE clauses via json_extract().

pub mod errors;

pub mod cache;
pub mod config;
pub mod expressions;
pub mod frontmatter;
pub mod generated;
pub mod links;
pub mod matching;
pub mod operations;
pub mod query;
pub mod types;
pub mod validation;
pub mod watch;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::errors::*;
use crate::frontmatter::parser::{parse_document, is_parse_error, yaml_mapping_to_json};
use crate::frontmatter::serializer;
use crate::types::schema::*;
use crate::types::loader;
use crate::types::inheritance;
use crate::validation::validator;
use crate::validation::merge::detect_type_conflicts;
use crate::expressions::parser::Parser as ExprParser;
use crate::expressions::evaluator::{evaluate as eval_expr, EvalContext};

/// Parsed config settings used at runtime.
#[derive(Debug, Clone)]
pub struct Settings {
    pub extensions: Vec<String>,
    pub exclude: Vec<String>,
    pub include_subfolders: bool,
    pub types_folder: String,
    pub explicit_type_keys: Vec<String>,
    pub default_validation: String,
    pub default_strict: serde_json::Value,
    pub id_field: String,
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
            explicit_type_keys: vec!["type".into(), "types".into()],
            default_validation: "warn".into(),
            default_strict: serde_json::Value::Bool(false),
            id_field: "id".into(),
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
        let config_result = config::load_config(root);
        if config_result.get("valid") != Some(&serde_json::Value::Bool(true)) {
            return Err(config_result);
        }

        let config = &config_result["config"];
        let settings_json = &config["settings"];

        let settings = Settings {
            extensions: settings_json["extensions"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            exclude: settings_json["exclude"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_else(|| vec![".git".into(), "node_modules".into(), ".mdbase".into()]),
            include_subfolders: settings_json["include_subfolders"].as_bool().unwrap_or(true),
            types_folder: settings_json["types_folder"].as_str().unwrap_or("_types").to_string(),
            explicit_type_keys: settings_json["explicit_type_keys"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_else(|| vec!["type".into(), "types".into()]),
            default_validation: settings_json["default_validation"].as_str().unwrap_or("warn").to_string(),
            default_strict: settings_json["default_strict"].clone(),
            id_field: settings_json["id_field"].as_str().unwrap_or("id").to_string(),
            write_nulls: settings_json["write_nulls"].as_str().unwrap_or("omit").to_string(),
            write_empty_lists: settings_json["write_empty_lists"].as_bool().unwrap_or(true),
            rename_update_refs: settings_json["rename_update_refs"].as_bool().unwrap_or(true),
            cache_folder: settings_json["cache_folder"].as_str().unwrap_or(".mdbase").to_string(),
        };

        // Load types
        let load_result = loader::load_types_with_warnings(root, &settings.types_folder)
            .map_err(|e| serde_json::json!({
                "valid": false,
                "error": { "code": "invalid_type_definition", "message": e }
            }))?;

        let mut types = load_result.types;
        let type_warnings = load_result.warnings;

        // Resolve inheritance
        inheritance::resolve_inheritance(&mut types)
            .map_err(|e| {
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
            let computed_names: std::collections::HashSet<&str> = computed_fields.iter().map(|(n, _)| *n).collect();

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

        Ok(())
    }

    /// Evaluate computed fields for a read result (§5.12).
    fn evaluate_computed_fields(
        &self,
        mut frontmatter: serde_json::Value,
        type_names: &[String],
        path: &str,
        body: Option<&str>,
    ) -> serde_json::Value {
        // Collect all computed fields from matched types
        let mut computed: Vec<(String, String)> = Vec::new(); // (name, expression)
        for type_name in type_names {
            if let Some(type_def) = self.types.get(type_name) {
                for (field_name, field_def) in &type_def.fields {
                    if let Some(ref expr) = field_def.computed {
                        // Don't add duplicates
                        if !computed.iter().any(|(n, _)| n == field_name) {
                            computed.push((field_name.clone(), expr.clone()));
                        }
                    }
                }
            }
        }

        if computed.is_empty() {
            return frontmatter;
        }

        // Topological sort to determine evaluation order
        let computed_names: std::collections::HashSet<&str> = computed.iter().map(|(n, _)| n.as_str()).collect();
        let mut deps: HashMap<&str, Vec<&str>> = HashMap::new();
        for (name, expr) in &computed {
            let mut field_deps = Vec::new();
            for dep_name in &computed_names {
                if *dep_name != name.as_str() && expression_references_field(expr, dep_name) {
                    field_deps.push(*dep_name);
                }
            }
            deps.insert(name.as_str(), field_deps);
        }

        // Topological sort
        let mut order: Vec<&str> = Vec::new();
        let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();

        fn topo_visit<'a>(
            node: &'a str,
            deps: &HashMap<&'a str, Vec<&'a str>>,
            visited: &mut std::collections::HashSet<&'a str>,
            order: &mut Vec<&'a str>,
        ) {
            if visited.contains(node) {
                return;
            }
            visited.insert(node);
            if let Some(node_deps) = deps.get(node) {
                for dep in node_deps {
                    topo_visit(dep, deps, visited, order);
                }
            }
            order.push(node);
        }

        for (name, _) in &computed {
            topo_visit(name, &deps, &mut visited, &mut order);
        }

        // Evaluate computed fields in dependency order
        for field_name in order {
            if let Some((_, expr)) = computed.iter().find(|(n, _)| n == field_name) {
                let ctx = EvalContext {
                    frontmatter: frontmatter.clone(),
                    file_path: Some(path.to_string()),
                    body: body.map(String::from),
                };
                if let Ok(parsed) = ExprParser::parse(expr) {
                    match eval_expr(&parsed, &ctx) {
                        Ok(value) => {
                            if let Some(obj) = frontmatter.as_object_mut() {
                                obj.insert(field_name.to_string(), value);
                            }
                        }
                        Err(_) => {
                            // On evaluation error, set to null
                            if let Some(obj) = frontmatter.as_object_mut() {
                                obj.insert(field_name.to_string(), serde_json::Value::Null);
                            }
                        }
                    }
                }
            }
        }

        frontmatter
    }

    /// Check if a path is excluded from the collection.
    fn is_excluded(&self, rel_path: &str) -> bool {
        // Check types folder
        if rel_path.starts_with(&format!("{}/", self.settings.types_folder))
            || rel_path == self.settings.types_folder
        {
            return true;
        }

        // Check cache folder
        if rel_path.starts_with(&format!("{}/", self.settings.cache_folder))
            || rel_path == self.settings.cache_folder
        {
            return true;
        }

        // Check default .mdbase even if custom cache_folder
        if self.settings.cache_folder != ".mdbase"
            && (rel_path.starts_with(".mdbase/") || rel_path == ".mdbase")
        {
            return true;
        }

        // Check mdbase.yaml
        if rel_path == "mdbase.yaml" {
            return true;
        }

        // Check exclude patterns
        for pattern in &self.settings.exclude {
            if match_glob_pattern(pattern, rel_path) {
                return true;
            }
        }

        // Check include_subfolders
        if !self.settings.include_subfolders && rel_path.contains('/') {
            return true;
        }

        // Check nested collection boundary (§2.8)
        // If any parent directory of this path contains mdbase.yaml,
        // the file is inside a nested collection and not part of this one.
        if self.is_in_nested_collection(rel_path) {
            return true;
        }

        false
    }

    /// Check if a relative path is inside a nested collection.
    /// Returns true if any parent directory along the path contains a mdbase.yaml file.
    fn is_in_nested_collection(&self, rel_path: &str) -> bool {
        let path = std::path::Path::new(rel_path);
        let mut current = std::path::PathBuf::new();
        // Check each parent directory component (not the file itself)
        for component in path.parent().into_iter().flat_map(|p| p.components()) {
            current.push(component);
            let config_path = self.root.join(&current).join("mdbase.yaml");
            if config_path.exists() {
                return true;
            }
        }
        false
    }

    /// Check if a file extension is valid for this collection.
    fn is_valid_extension(&self, path: &str) -> bool {
        if path.ends_with(".md") {
            return true;
        }
        for ext in &self.settings.extensions {
            if path.ends_with(&format!(".{}", ext)) {
                return true;
            }
        }
        false
    }

    /// Determine the type(s) of a file from its frontmatter.
    /// Type names are canonicalized to lowercase for lookup.
    fn determine_types(&self, frontmatter: &serde_json::Value) -> Vec<String> {
        self.determine_types_for_path(frontmatter, None)
    }

    /// Determine types for a file at the given path.
    /// If explicit type keys are found in frontmatter, uses those (and stops match rule evaluation).
    /// Otherwise evaluates match rules from all types.
    pub fn determine_types_for_path(
        &self,
        frontmatter: &serde_json::Value,
        rel_path: Option<&str>,
    ) -> Vec<String> {
        let mut types = Vec::new();
        let mut has_explicit = false;

        if let Some(obj) = frontmatter.as_object() {
            for key in &self.settings.explicit_type_keys {
                if let Some(val) = obj.get(key) {
                    match val {
                        serde_json::Value::String(s) => {
                            if !s.is_empty() {
                                types.push(s.to_lowercase());
                                has_explicit = true;
                            }
                        }
                        serde_json::Value::Array(arr) => {
                            for item in arr {
                                if let Some(s) = item.as_str() {
                                    types.push(s.to_lowercase());
                                    has_explicit = true;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // If explicit types found, stop here (§6.6)
        if has_explicit {
            return types;
        }

        // Evaluate match rules from all types
        if let Some(path) = rel_path {
            for (type_name, type_def) in &self.types {
                if let Some(ref rules) = type_def.match_rules {
                    if matching::engine::matches_rules(rules, path, frontmatter) {
                        if !types.contains(type_name) {
                            types.push(type_name.clone());
                        }
                    }
                }
            }
        }

        types
    }

    /// Apply defaults from type definitions to frontmatter (for effective frontmatter).
    fn apply_defaults(
        &self,
        frontmatter: &serde_json::Value,
        type_names: &[String],
    ) -> serde_json::Value {
        let mut result = frontmatter.clone();
        let obj = match result.as_object_mut() {
            Some(o) => o,
            None => return result,
        };

        for type_name in type_names {
            if let Some(type_def) = self.types.get(type_name) {
                for (field_name, field_def) in &type_def.fields {
                    if let Some(default) = &field_def.default {
                        if !obj.contains_key(field_name) {
                            // Field is missing — apply default
                            obj.insert(field_name.clone(), default.clone());
                        } else if obj.get(field_name).map_or(false, |v| v.is_null()) && field_def.generated.is_some() {
                            // Field is null AND was generated (e.g., derived with missing source)
                            // Apply default as effective value
                            obj.insert(field_name.clone(), default.clone());
                        }
                    }
                }
            }
        }

        result
    }

    /// Generate values for fields with generated strategies.
    fn apply_generated(
        &self,
        frontmatter: &mut serde_json::Map<String, serde_json::Value>,
        type_names: &[String],
        is_create: bool,
    ) {
        for type_name in type_names {
            if let Some(type_def) = self.types.get(type_name) {
                for (field_name, field_def) in &type_def.fields {
                    if let Some(strategy) = &field_def.generated {
                        let should_generate = match strategy {
                            GeneratedStrategy::NowOnWrite => true,
                            _ => is_create && !frontmatter.contains_key(field_name),
                        };

                        if should_generate {
                            let value = match strategy {
                                GeneratedStrategy::Ulid => {
                                    serde_json::Value::String(ulid::Ulid::new().to_string())
                                }
                                GeneratedStrategy::Uuid => {
                                    serde_json::Value::String(uuid::Uuid::new_v4().to_string())
                                }
                                GeneratedStrategy::Now | GeneratedStrategy::NowOnWrite => {
                                    serde_json::Value::String(
                                        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                                    )
                                }
                                GeneratedStrategy::Derived { from, transform } => {
                                    if let Some(source) = frontmatter.get(from) {
                                        if source.is_null() {
                                            serde_json::Value::Null
                                        } else {
                                            apply_transform(source, transform)
                                        }
                                    } else {
                                        serde_json::Value::Null
                                    }
                                }
                            };
                            frontmatter.insert(field_name.clone(), value);
                        }
                    }
                }
            }
        }
    }

    /// Coerce field values to match their declared types (§7.16).
    fn coerce_types(
        &self,
        frontmatter: &serde_json::Value,
        type_names: &[String],
    ) -> serde_json::Value {
        let mut result = frontmatter.clone();
        let obj = match result.as_object_mut() {
            Some(o) => o,
            None => return result,
        };

        for type_name in type_names {
            if let Some(type_def) = self.types.get(type_name) {
                for (field_name, field_def) in &type_def.fields {
                    if let Some(value) = obj.get(field_name).cloned() {
                        if let Some(coerced) = coerce_value(&value, &field_def.field_type) {
                            obj.insert(field_name.clone(), coerced);
                        }
                    }
                }
            }
        }

        serde_json::Value::Object(obj.clone())
    }

    /// Get the config default_strict as a StrictMode.
    fn config_strict_mode(&self) -> StrictMode {
        match &self.settings.default_strict {
            serde_json::Value::Bool(true) => StrictMode::Error,
            serde_json::Value::Bool(false) => StrictMode::Off,
            serde_json::Value::String(s) if s == "warn" => StrictMode::Warn,
            _ => StrictMode::Off,
        }
    }

    /// Validate frontmatter against matched types.
    fn validate(
        &self,
        frontmatter: &serde_json::Value,
        type_names: &[String],
        path: &str,
    ) -> ValidationResult {
        let mut all_issues = Vec::new();
        let config_strict = self.config_strict_mode();

        // Detect multi-type conflicts
        if type_names.len() > 1 {
            let type_defs: Vec<&crate::types::schema::TypeDef> = type_names.iter()
                .filter_map(|tn| self.types.get(tn))
                .collect();
            let conflict_issues = detect_type_conflicts(&type_defs, path);
            all_issues.extend(conflict_issues);
        }

        // Build union of all field names for multi-type strict mode
        let union_fields: std::collections::HashSet<String> = if type_names.len() > 1 {
            type_names.iter()
                .filter_map(|tn| self.types.get(tn))
                .flat_map(|td| td.fields.keys().cloned())
                .collect()
        } else {
            std::collections::HashSet::new()
        };
        let union_ref = if type_names.len() > 1 { Some(&union_fields) } else { None };

        for type_name in type_names {
            if let Some(type_def) = self.types.get(type_name) {
                let result = validator::validate_frontmatter_full_multi(
                    frontmatter,
                    type_def,
                    path,
                    Some(&config_strict),
                    Some(&self.settings.explicit_type_keys),
                    union_ref,
                );
                all_issues.extend(result.issues);
            }
        }

        let has_errors = all_issues.iter().any(|i| i.severity == Severity::Error);
        ValidationResult {
            valid: !has_errors,
            issues: all_issues,
        }
    }

    /// Scan all markdown files in the collection.
    fn scan_collection_files(&self) -> Vec<PathBuf> {
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
                    let rel = path.strip_prefix(&self.root)
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if !self.is_excluded(&rel) {
                        self.scan_dir_recursive(&path, files);
                    }
                }
            } else if path.is_file() {
                let rel = path.strip_prefix(&self.root)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                if !self.is_excluded(&rel) && self.is_valid_extension(&rel) {
                    files.push(path);
                }
            }
        }
    }

    /// Check cross-file uniqueness for a file being created or updated.
    /// Returns issues for duplicate id_field and unique field values.
    /// `exclude_path` is the relative path of the file being updated (to exclude self from checks).
    fn check_uniqueness(&self, frontmatter: &serde_json::Value, type_names: &[String], exclude_path: &str) -> Vec<Issue> {
        let mut issues = Vec::new();
        let files = self.scan_collection_files();

        for type_name in type_names {
            let type_def = match self.types.get(type_name) {
                Some(td) => td,
                None => continue,
            };

            // Collect unique fields to check
            let mut unique_fields: Vec<(&str, &str)> = Vec::new(); // (field_name, value_str)
            let mut unique_values_temp: Vec<String> = Vec::new();
            for (field_name, field_def) in &type_def.fields {
                if field_def.unique {
                    if let Some(val) = frontmatter.get(field_name) {
                        if !val.is_null() {
                            let val_str = match val.as_str() {
                                Some(s) => s.to_string(),
                                None => val.to_string(),
                            };
                            unique_values_temp.push(val_str);
                            unique_fields.push((field_name.as_str(), ""));
                        }
                    }
                }
            }
            // Fix lifetime: store separately
            let unique_checks: Vec<(String, String)> = type_def.fields.iter()
                .filter(|(_, fd)| fd.unique)
                .filter_map(|(fname, _)| {
                    frontmatter.get(fname).and_then(|val| {
                        if val.is_null() { return None; }
                        let val_str = match val.as_str() {
                            Some(s) => s.to_string(),
                            None => val.to_string(),
                        };
                        Some((fname.clone(), val_str))
                    })
                })
                .collect();

            // Check id_field
            let id_field = &self.settings.id_field;
            let id_value = frontmatter.get(id_field).and_then(|v| {
                if v.is_null() { None }
                else { Some(match v.as_str() { Some(s) => s.to_string(), None => v.to_string() }) }
            });

            // Scan other files
            for file_path in &files {
                let rel_path = file_path.strip_prefix(&self.root)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                // Normalize path separators for comparison
                let rel_normalized = rel_path.replace('\\', "/");
                let exclude_normalized = exclude_path.replace('\\', "/");
                if rel_normalized == exclude_normalized {
                    continue;
                }

                let content = match std::fs::read_to_string(file_path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                let doc = parse_document(&content);
                let other_fm = match &doc.frontmatter {
                    Some(serde_yaml::Value::Mapping(m)) => yaml_mapping_to_json(m),
                    _ => continue,
                };

                // Check id_field duplicate
                if let Some(ref our_id) = id_value {
                    if let Some(other_val) = other_fm.get(id_field) {
                        if !other_val.is_null() {
                            let other_str = match other_val.as_str() {
                                Some(s) => s.to_string(),
                                None => other_val.to_string(),
                            };
                            if &other_str == our_id {
                                issues.push(Issue {
                                    code: "duplicate_id".to_string(),
                                    message: format!("Duplicate {} value '{}' (also in {})", id_field, our_id, rel_path),
                                    path: Some(exclude_path.to_string()),
                                    field: Some(id_field.clone()),
                                    severity: Severity::Error,
                                    expected: None,
                                    actual: None,
                                    type_name: Some(type_name.clone()),
                                    line: None,
                                    column: None,
                                });
                            }
                        }
                    }
                }

                // Check unique fields
                for (field_name, our_val) in &unique_checks {
                    if let Some(other_val) = other_fm.get(field_name.as_str()) {
                        if !other_val.is_null() {
                            let other_str = match other_val.as_str() {
                                Some(s) => s.to_string(),
                                None => other_val.to_string(),
                            };
                            if &other_str == our_val {
                                issues.push(Issue {
                                    code: "duplicate_value".to_string(),
                                    message: format!("Duplicate unique value '{}' for field '{}' (also in {})", our_val, field_name, rel_path),
                                    path: Some(exclude_path.to_string()),
                                    field: Some(field_name.clone()),
                                    severity: Severity::Error,
                                    expected: None,
                                    actual: None,
                                    type_name: Some(type_name.clone()),
                                    line: None,
                                    column: None,
                                });
                            }
                        }
                    }
                }
            }
        }

        issues
    }

    /// Check link fields with validate_exists: true.
    /// Verifies that wiki-link targets actually exist in the collection.
    fn check_link_exists(&self, frontmatter: &serde_json::Value, type_names: &[String], path: &str) -> Vec<Issue> {
        let mut issues = Vec::new();

        for type_name in type_names {
            let type_def = match self.types.get(type_name) {
                Some(td) => td,
                None => continue,
            };

            for (field_name, field_def) in &type_def.fields {
                // Determine the effective link field def (could be the field itself or list items)
                let link_field = if field_def.field_type == "link" {
                    Some(field_def)
                } else if field_def.field_type == "list" {
                    field_def.items.as_ref().and_then(|item| {
                        if item.field_type == "link" { Some(item.as_ref()) } else { None }
                    })
                } else {
                    None
                };

                let link_field = match link_field {
                    Some(lf) => lf,
                    None => continue,
                };

                let value = match frontmatter.get(field_name) {
                    Some(v) if !v.is_null() => v,
                    _ => continue,
                };

                // Handle both single link and list of links
                let link_values: Vec<&str> = if let Some(s) = value.as_str() {
                    vec![s]
                } else if let Some(arr) = value.as_array() {
                    arr.iter().filter_map(|v| v.as_str()).collect()
                } else {
                    continue
                };

                for link_str in link_values {
                    let link_issues = self.validate_single_link(
                        link_str, field_name, link_field, type_name, path,
                    );
                    issues.extend(link_issues);
                }
            }
        }

        issues
    }

    /// Validate a single link value.
    fn validate_single_link(
        &self,
        link_str: &str,
        field_name: &str,
        field_def: &FieldDef,
        type_name: &str,
        path: &str,
    ) -> Vec<Issue> {
        let mut issues = Vec::new();

        // Extract target from [[...]] wiki-link syntax
        let target = if link_str.starts_with("[[") && link_str.ends_with("]]") {
            &link_str[2..link_str.len()-2]
        } else {
            link_str
        };

        // Remove display text after | if present
        let target = target.split('|').next().unwrap_or(target).trim();

        if target.is_empty() {
            return issues;
        }

        // Normalize path (resolve ./ and ../)
        let source_dir = std::path::Path::new(path).parent().unwrap_or(std::path::Path::new(""));
        let normalized = normalize_link_path(target, &source_dir.to_string_lossy());

        // Check for path traversal (escaping collection root)
        if normalized.starts_with("../") || normalized.starts_with("..\\") || normalized == ".." {
            issues.push(Issue {
                code: "path_traversal".to_string(),
                message: format!("Link target '{}' escapes collection root", target),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: None,
                actual: None,
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
            return issues;
        }

        // Resolve link target
        if field_def.validate_exists == Some(true) {
            let matches = self.resolve_link_matches(&normalized, target);

            if matches.is_empty() {
                issues.push(Issue {
                    code: "link_not_found".to_string(),
                    message: format!("Link target '{}' not found for field '{}'", target, field_name),
                    path: Some(path.to_string()),
                    field: Some(field_name.to_string()),
                    severity: Severity::Error,
                    expected: None,
                    actual: None,
                    type_name: Some(type_name.to_string()),
                    line: None,
                    column: None,
                });
            } else if matches.len() > 1 {
                issues.push(Issue {
                    code: "ambiguous_link".to_string(),
                    message: format!("Link '{}' matches multiple files for field '{}'", target, field_name),
                    path: Some(path.to_string()),
                    field: Some(field_name.to_string()),
                    severity: Severity::Error,
                    expected: None,
                    actual: None,
                    type_name: Some(type_name.to_string()),
                    line: None,
                    column: None,
                });
            } else if let Some(ref target_type) = field_def.target {
                // Check target type constraint
                let matched_path = &matches[0];
                let target_types = self.get_file_types(matched_path);
                if !target_types.iter().any(|t| t == target_type) {
                    issues.push(Issue {
                        code: "link_wrong_type".to_string(),
                        message: format!(
                            "Link target '{}' is type {:?}, expected '{}'",
                            target, target_types, target_type
                        ),
                        path: Some(path.to_string()),
                        field: Some(field_name.to_string()),
                        severity: Severity::Error,
                        expected: None,
                        actual: None,
                        type_name: Some(type_name.to_string()),
                        line: None,
                        column: None,
                    });
                }
            }
        } else if let Some(ref target_type) = field_def.target {
            // Even without validate_exists, check target type if we can resolve
            let matches = self.resolve_link_matches(&normalized, target);
            if matches.len() == 1 {
                let matched_path = &matches[0];
                let target_types = self.get_file_types(matched_path);
                if !target_types.iter().any(|t| t == target_type) {
                    issues.push(Issue {
                        code: "link_wrong_type".to_string(),
                        message: format!(
                            "Link target '{}' is type {:?}, expected '{}'",
                            target, target_types, target_type
                        ),
                        path: Some(path.to_string()),
                        field: Some(field_name.to_string()),
                        severity: Severity::Error,
                        expected: None,
                        actual: None,
                        type_name: Some(type_name.to_string()),
                        line: None,
                        column: None,
                    });
                }
            } else if matches.len() > 1 {
                issues.push(Issue {
                    code: "ambiguous_link".to_string(),
                    message: format!("Link '{}' matches multiple files for field '{}'", target, field_name),
                    path: Some(path.to_string()),
                    field: Some(field_name.to_string()),
                    severity: Severity::Error,
                    expected: None,
                    actual: None,
                    type_name: Some(type_name.to_string()),
                    line: None,
                    column: None,
                });
            }
        }

        issues
    }

    /// Resolve a link target to matching file paths.
    fn resolve_link_matches(&self, normalized: &str, original: &str) -> Vec<String> {
        let files = self.scan_collection_files();
        let mut matches = Vec::new();

        for file_path in &files {
            let rel_path = match file_path.strip_prefix(&self.root) {
                Ok(p) => p.to_string_lossy().to_string().replace('\\', "/"),
                Err(_) => continue,
            };

            // Check exact path match (with .md extension)
            let normalized_with_ext = if !normalized.ends_with(".md") && !normalized.ends_with(".mdx") {
                format!("{}.md", normalized)
            } else {
                normalized.to_string()
            };
            if rel_path == normalized_with_ext || rel_path == normalized {
                matches.push(rel_path);
                continue;
            }

            // Check file stem match (for simple name links)
            let stem = std::path::Path::new(&rel_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");

            if stem.eq_ignore_ascii_case(original) {
                matches.push(rel_path);
                continue;
            }

            // Also check against the id_field
            if !original.contains('/') && !original.contains('.') {
                // For simple names, also check id field
                if let Ok(content) = std::fs::read_to_string(file_path) {
                    let doc = parse_document(&content);
                    if let Some(serde_yaml::Value::Mapping(m)) = &doc.frontmatter {
                        let json = yaml_mapping_to_json(m);
                        if let Some(id_val) = json.get(&self.settings.id_field).and_then(|v| v.as_str()) {
                            if id_val == original {
                                if !matches.iter().any(|m| *m == rel_path) {
                                    matches.push(rel_path);
                                }
                            }
                        }
                    }
                }
            }
        }

        matches
    }

    /// Get the types associated with a file by reading it and running type matching.
    fn get_file_types(&self, rel_path: &str) -> Vec<String> {
        let full_path = self.root.join(rel_path);
        if let Ok(content) = std::fs::read_to_string(&full_path) {
            let doc = parse_document(&content);
            if let Some(serde_yaml::Value::Mapping(m)) = &doc.frontmatter {
                let json = yaml_mapping_to_json(m);
                return self.determine_types_for_path(&json, Some(rel_path));
            }
        }
        Vec::new()
    }

    // -----------------------------------------------------------------------
    // Operations
    // -----------------------------------------------------------------------

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

    /// Create a file (§12.1).
    pub fn create(&self, input: &serde_json::Value) -> serde_json::Value {
        let type_name = input.get("type").and_then(|v| v.as_str());
        let frontmatter_input = input.get("frontmatter")
            .or_else(|| input.get("fields"))
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let body = input.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let path_input = input.get("path").and_then(|v| v.as_str());

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

        // Determine path
        let path = match path_input {
            Some(p) => {
                // Empty path check
                if p.is_empty() {
                    return op_error(PATH_REQUIRED, "path must not be empty");
                }
                // Null byte check
                if p.contains('\0') {
                    return op_error(INVALID_PATH, "Path contains null bytes");
                }
                // Path traversal check
                if p.contains("..") {
                    return op_error(INVALID_PATH, "Path contains path traversal");
                }
                p.to_string()
            }
            None => {
                // Try to derive from filename_pattern
                if let Some(tn) = type_names.first() {
                    if let Some(type_def) = self.types.get(tn) {
                        if let Some(pattern) = &type_def.filename_pattern {
                            match derive_path(pattern, &frontmatter_input) {
                                Some(p) => p,
                                None => return op_error(PATH_REQUIRED, "Cannot determine path"),
                            }
                        } else {
                            return op_error(PATH_REQUIRED, "No path provided and no filename_pattern");
                        }
                    } else {
                        return op_error(PATH_REQUIRED, "Cannot determine path");
                    }
                } else {
                    return op_error(PATH_REQUIRED, "No path provided");
                }
            }
        };

        // Check existence
        let full_path = self.root.join(&path);
        if full_path.exists() {
            return op_error(PATH_CONFLICT, &format!("File already exists: {}", path));
        }

        // Build frontmatter
        let mut fm_obj = match frontmatter_input.as_object() {
            Some(o) => o.clone(),
            None => serde_json::Map::new(),
        };

        // Add type key if specified
        if let Some(tn) = type_name {
            if !fm_obj.contains_key("type") && !fm_obj.contains_key("types") {
                fm_obj.insert("type".to_string(), serde_json::Value::String(tn.to_string()));
            }
        }

        // Generate values
        self.apply_generated(&mut fm_obj, &type_names, true);

        // Apply defaults for effective frontmatter (for validation and output)
        let effective = self.apply_defaults(&serde_json::Value::Object(fm_obj.clone()), &type_names);

        // Validate
        let mut result_warnings: Vec<serde_json::Value> = Vec::new();
        if self.settings.default_validation == "error" {
            let validation = self.validate(&effective, &type_names, &path);
            if !validation.valid {
                let issues: Vec<serde_json::Value> = validation.issues.iter().map(issue_to_json).collect();
                return serde_json::json!({
                    "error": {
                        "code": VALIDATION_FAILED,
                        "message": "Validation failed",
                        "issues": issues,
                    }
                });
            }
        } else if self.settings.default_validation == "warn" {
            let validation = self.validate(&effective, &type_names, &path);
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

        // Write file
        let yaml_mapping = frontmatter::parser::json_to_yaml_mapping(&serde_json::Value::Object(fm_obj));
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

        let mut result = serde_json::json!({
            "path": path,
            "types": type_names,
            "frontmatter": effective,
            "body": body,
            "valid": true,
        });
        if !result_warnings.is_empty() {
            result["warnings"] = serde_json::Value::Array(result_warnings);
        }
        result
    }

    /// Update a file (§12.3).
    pub fn update(&self, input: &serde_json::Value) -> serde_json::Value {
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return op_error(INVALID_PATH, "path is required"),
        };

        let fields = input.get("fields")
            .or_else(|| input.get("frontmatter"))
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let new_body = input.get("body").and_then(|v| v.as_str());

        let full_path = self.root.join(path);
        if !full_path.exists() {
            return op_error(FILE_NOT_FOUND, &format!("File not found: {}", path));
        }

        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(e) => return op_error(FILE_NOT_FOUND, &format!("Failed to read: {}", e)),
        };

        let doc = parse_document(&content);

        let existing_mapping = match &doc.frontmatter {
            Some(serde_yaml::Value::Mapping(m)) => m.clone(),
            _ => serde_yaml::Mapping::new(),
        };

        // Merge fields
        let merged = serializer::merge_fields(&existing_mapping, &fields, &self.settings.write_nulls);
        let merged_json = yaml_mapping_to_json(&merged);

        // Determine types
        let type_names = self.determine_types(&merged_json);

        // Apply generated (now_on_write)
        let mut merged_obj = match merged_json.as_object() {
            Some(o) => o.clone(),
            None => serde_json::Map::new(),
        };
        self.apply_generated(&mut merged_obj, &type_names, false);

        // Apply defaults for effective frontmatter
        let effective = self.apply_defaults(&serde_json::Value::Object(merged_obj.clone()), &type_names);

        // Validate
        if self.settings.default_validation == "error" {
            let mut validation = self.validate(&effective, &type_names, path);

            // Cross-file uniqueness checks for update
            let uniqueness_issues = self.check_uniqueness(&effective, &type_names, path);
            validation.issues.extend(uniqueness_issues.iter().cloned());
            if !uniqueness_issues.is_empty() {
                validation.valid = false;
            }

            if !validation.valid {
                let issues: Vec<serde_json::Value> = validation.issues.iter().map(issue_to_json).collect();
                return serde_json::json!({
                    "error": {
                        "code": VALIDATION_FAILED,
                        "message": "Validation failed",
                        "issues": issues,
                    }
                });
            }
        }

        // Write file
        let write_mapping = frontmatter::parser::json_to_yaml_mapping(&serde_json::Value::Object(merged_obj));
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
        let effective = self.evaluate_computed_fields(effective, &type_names, path, Some(body));

        let mut result = serde_json::json!({
            "path": path,
            "frontmatter": effective,
            "body": body,
        });
        if !result_warnings.is_empty() {
            result["warnings"] = serde_json::Value::Array(result_warnings);
        }
        result
    }

    /// Delete a file (§12.4).
    pub fn delete(&self, input: &serde_json::Value) -> serde_json::Value {
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return op_error(INVALID_PATH, "path is required"),
        };

        let full_path = self.root.join(path);
        if !full_path.exists() {
            return op_error(FILE_NOT_FOUND, &format!("File not found: {}", path));
        }

        if let Err(e) = std::fs::remove_file(&full_path) {
            return op_error("io_error", &format!("Failed to delete: {}", e));
        }

        serde_json::json!({
            "path": path,
            "deleted": true,
        })
    }

    /// Rename a file (§12.5).
    pub fn rename(&self, input: &serde_json::Value) -> serde_json::Value {
        let from = input.get("from").or_else(|| input.get("path")).and_then(|v| v.as_str());
        let to = input.get("to").or_else(|| input.get("new_path")).and_then(|v| v.as_str());

        let from = match from {
            Some(p) => p,
            None => return op_error(PATH_REQUIRED, "'from' is required"),
        };
        let to = match to {
            Some(p) => p,
            None => return op_error(PATH_REQUIRED, "'to' is required"),
        };

        let from_path = self.root.join(from);
        let to_path = self.root.join(to);

        if !from_path.exists() {
            return op_error(FILE_NOT_FOUND, &format!("Source not found: {}", from));
        }

        if to_path.exists() {
            return op_error(PATH_CONFLICT, &format!("Target already exists: {}", to));
        }

        // Create parent dirs
        if let Some(parent) = to_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        // Check for null bytes in paths
        if to.contains('\0') || from.contains('\0') {
            return op_error(INVALID_PATH, "Path contains null bytes");
        }

        if let Err(e) = std::fs::rename(&from_path, &to_path) {
            let error_str = e.to_string();
            if error_str.contains("NUL") || error_str.contains("null") {
                return op_error(INVALID_PATH, &format!("Invalid path: {}", e));
            }
            return op_error("io_error", &format!("Failed to rename: {}", e));
        }

        serde_json::json!({
            "from": from,
            "to": to,
        })
    }

    /// Validate files (§9).
    pub fn validate_op(&self, input: &serde_json::Value) -> serde_json::Value {
        let path = input.get("path").and_then(|v| v.as_str());
        let _type_filter = input.get("type").and_then(|v| v.as_str());
        let collection_only = input.get("collection_only").and_then(|v| v.as_bool()).unwrap_or(false);

        // collection_only mode: just check that collection is valid
        if collection_only {
            return serde_json::json!({"valid": true, "issues": []});
        }

        if let Some(path) = path {
            // Check if inline frontmatter is provided in input
            let inline_fm = input.get("frontmatter");

            let raw_frontmatter = if let Some(fm) = inline_fm {
                // Use inline frontmatter directly - convert to JSON object
                match fm {
                    serde_json::Value::Object(_) => fm.clone(),
                    _ => serde_json::json!({}),
                }
            } else {
                // Check if file exists
                let full_path = self.root.join(path);
                if !full_path.exists() {
                    return op_error(FILE_NOT_FOUND, &format!("File not found: {}", path));
                }

                // Read and parse
                let content = match std::fs::read_to_string(&full_path) {
                    Ok(c) => c,
                    Err(_) => return op_error(INVALID_FRONTMATTER, "Failed to read file"),
                };

                let doc = parse_document(&content);

                // Check for parse errors
                if let Some(ref fm) = doc.frontmatter {
                    if is_parse_error(fm) {
                        return serde_json::json!({
                            "valid": false,
                            "path": path,
                            "issues": [{
                                "code": INVALID_FRONTMATTER,
                                "message": "Failed to parse YAML frontmatter",
                                "severity": "error",
                                "path": path,
                            }],
                        });
                    }
                }

                // Check for non-mapping frontmatter
                match &doc.frontmatter {
                    Some(serde_yaml::Value::Mapping(m)) => yaml_mapping_to_json(m),
                    Some(serde_yaml::Value::Null) | None => serde_json::json!({}),
                    Some(_) => {
                        return serde_json::json!({
                            "valid": false,
                            "path": path,
                            "issues": [{
                                "code": INVALID_FRONTMATTER,
                                "message": "Frontmatter must be a YAML mapping",
                                "severity": "error",
                                "path": path,
                            }],
                        });
                    }
                }
            };

            let type_names = self.determine_types_for_path(&raw_frontmatter, Some(path));
            let effective = self.apply_defaults(&raw_frontmatter, &type_names);
            let effective = self.coerce_types(&effective, &type_names);

            let mut all_issues = Vec::new();

            // Check for unknown types
            for tn in &type_names {
                if !self.types.contains_key(tn) {
                    all_issues.push(Issue {
                        code: UNKNOWN_TYPE.to_string(),
                        message: format!("Unknown type '{}'", tn),
                        path: Some(path.to_string()),
                        field: None,
                        severity: Severity::Error,
                        expected: None,
                        actual: None,
                        type_name: Some(tn.clone()),
                        line: None,
                        column: None,
                    });
                }
            }

            // Detect multi-type conflicts
            if type_names.len() > 1 {
                let type_defs: Vec<&crate::types::schema::TypeDef> = type_names.iter()
                    .filter_map(|tn| self.types.get(tn))
                    .collect();
                let conflict_issues = detect_type_conflicts(&type_defs, path);
                all_issues.extend(conflict_issues);
            }

            // Validate against types
            // Build union of all field names across all types for multi-type strict mode
            let config_strict = self.config_strict_mode();
            let union_fields: std::collections::HashSet<String> = if type_names.len() > 1 {
                type_names.iter()
                    .filter_map(|tn| self.types.get(tn))
                    .flat_map(|td| td.fields.keys().cloned())
                    .collect()
            } else {
                std::collections::HashSet::new()
            };
            let union_ref = if type_names.len() > 1 { Some(&union_fields) } else { None };

            for type_name in &type_names {
                if let Some(type_def) = self.types.get(type_name) {
                    let result = validator::validate_frontmatter_full_multi(
                        &effective,
                        type_def,
                        path,
                        Some(&config_strict),
                        Some(&self.settings.explicit_type_keys),
                        union_ref,
                    );
                    all_issues.extend(result.issues);

                    // Check filename pattern
                    if let Some(ref pattern) = type_def.filename_pattern {
                        let derived = derive_path(pattern, &effective);
                        if let Some(expected_path) = derived {
                            if expected_path != path {
                                all_issues.push(Issue {
                                    code: "filename_pattern_mismatch".to_string(),
                                    message: format!(
                                        "File path '{}' does not match expected pattern '{}'",
                                        path, expected_path
                                    ),
                                    path: Some(path.to_string()),
                                    field: None,
                                    severity: Severity::Warning,
                                    expected: Some(serde_json::json!(expected_path)),
                                    actual: Some(serde_json::json!(path)),
                                    type_name: Some(type_name.clone()),
                                    line: None,
                                    column: None,
                                });
                            }
                        }
                    }
                }
            }

            // Cross-file uniqueness checking
            let uniqueness_issues = self.check_uniqueness(&effective, &type_names, path);
            all_issues.extend(uniqueness_issues);

            // Link validate_exists checking
            let link_issues = self.check_link_exists(&effective, &type_names, path);
            all_issues.extend(link_issues);

            let has_errors = all_issues.iter().any(|i| i.severity == Severity::Error);
            let issues_json: Vec<serde_json::Value> = all_issues.iter().map(issue_to_json).collect();

            return serde_json::json!({
                "valid": !has_errors,
                "path": path,
                "types": type_names,
                "issues": issues_json,
            });
        }

        // Validate all files in collection
        let mut all_issues = Vec::new();
        let files = self.scan_collection_files();

        // Track unique values per (type, field) and id values per type
        let mut unique_values: HashMap<(String, String), HashMap<String, Vec<String>>> = HashMap::new();
        let mut id_values: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();

        for file_path in &files {
            let rel_path = file_path.strip_prefix(&self.root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();

            let content = match std::fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let doc = parse_document(&content);
            let raw_fm = match &doc.frontmatter {
                Some(serde_yaml::Value::Mapping(m)) => yaml_mapping_to_json(m),
                _ => continue,
            };

            let type_names = self.determine_types_for_path(&raw_fm, Some(&rel_path));
            let effective = self.apply_defaults(&raw_fm, &type_names);
            let effective = self.coerce_types(&effective, &type_names);

            // Detect multi-type conflicts
            if type_names.len() > 1 {
                let type_defs_coll: Vec<&crate::types::schema::TypeDef> = type_names.iter()
                    .filter_map(|tn| self.types.get(tn))
                    .collect();
                let conflict_issues = detect_type_conflicts(&type_defs_coll, &rel_path);
                all_issues.extend(conflict_issues);
            }

            // Validate individual file
            let config_strict = self.config_strict_mode();
            let union_fields_coll: std::collections::HashSet<String> = if type_names.len() > 1 {
                type_names.iter()
                    .filter_map(|tn| self.types.get(tn))
                    .flat_map(|td| td.fields.keys().cloned())
                    .collect()
            } else {
                std::collections::HashSet::new()
            };
            let union_ref_coll = if type_names.len() > 1 { Some(&union_fields_coll) } else { None };

            for tn in &type_names {
                if let Some(type_def) = self.types.get(tn) {
                    let result = validator::validate_frontmatter_full_multi(
                        &effective,
                        type_def,
                        &rel_path,
                        Some(&config_strict),
                        Some(&self.settings.explicit_type_keys),
                        union_ref_coll,
                    );
                    all_issues.extend(result.issues);

                    // Track unique fields
                    for (field_name, field_def) in &type_def.fields {
                        if field_def.unique {
                            if let Some(val) = effective.get(field_name) {
                                if !val.is_null() {
                                    let key = (tn.clone(), field_name.clone());
                                    let val_str = match val.as_str() {
                                        Some(s) => s.to_string(),
                                        None => val.to_string(),
                                    };
                                    unique_values.entry(key)
                                        .or_default()
                                        .entry(val_str)
                                        .or_default()
                                        .push(rel_path.clone());
                                }
                            }
                        }
                    }

                    // Track id_field
                    let id_field = &self.settings.id_field;
                    if let Some(val) = effective.get(id_field) {
                        if !val.is_null() {
                            let val_str = match val.as_str() {
                                Some(s) => s.to_string(),
                                None => val.to_string(),
                            };
                            id_values.entry(tn.clone())
                                .or_default()
                                .entry(val_str)
                                .or_default()
                                .push(rel_path.clone());
                        }
                    }
                }
            }
        }

        // Check for duplicate unique values
        for ((type_name, field_name), values) in &unique_values {
            for (val, paths) in values {
                if paths.len() > 1 {
                    for p in paths {
                        all_issues.push(Issue {
                            code: DUPLICATE_VALUE.to_string(),
                            message: format!("Duplicate value '{}' for unique field '{}' in type '{}'", val, field_name, type_name),
                            path: Some(p.clone()),
                            field: Some(field_name.clone()),
                            severity: Severity::Error,
                            expected: None,
                            actual: Some(serde_json::json!(val)),
                            type_name: Some(type_name.clone()),
                            line: None,
                            column: None,
                        });
                    }
                }
            }
        }

        // Check for duplicate id values
        for (type_name, values) in &id_values {
            for (val, paths) in values {
                if paths.len() > 1 {
                    for p in paths {
                        all_issues.push(Issue {
                            code: DUPLICATE_ID.to_string(),
                            message: format!("Duplicate id '{}' in type '{}'", val, type_name),
                            path: Some(p.clone()),
                            field: Some(self.settings.id_field.clone()),
                            severity: Severity::Error,
                            expected: None,
                            actual: Some(serde_json::json!(val)),
                            type_name: Some(type_name.clone()),
                            line: None,
                            column: None,
                        });
                    }
                }
            }
        }

        let has_errors = all_issues.iter().any(|i| i.severity == Severity::Error);
        let issues_json: Vec<serde_json::Value> = all_issues.iter().map(issue_to_json).collect();

        serde_json::json!({
            "valid": !has_errors,
            "issues": issues_json,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn op_error(code: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "error": { "code": code, "message": message }
    })
}

fn issue_to_json(issue: &Issue) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "code": issue.code,
        "message": issue.message,
        "severity": match issue.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        },
    });
    if let Some(ref path) = issue.path {
        obj["path"] = serde_json::Value::String(path.clone());
    }
    if let Some(ref field) = issue.field {
        obj["field"] = serde_json::Value::String(field.clone());
    }
    if let Some(ref tn) = issue.type_name {
        obj["type"] = serde_json::Value::String(tn.clone());
    }
    obj
}

fn apply_transform(source: &serde_json::Value, transform: &str) -> serde_json::Value {
    let s = match source {
        serde_json::Value::String(s) => s.clone(),
        _ => source.to_string(),
    };

    let result = match transform {
        "slugify" => slugify(&s),
        "lowercase" => s.to_lowercase(),
        "uppercase" => s.to_uppercase(),
        _ => s,
    };

    serde_json::Value::String(result)
}

fn slugify(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Derive a file path from a filename pattern and frontmatter.
fn derive_path(pattern: &str, frontmatter: &serde_json::Value) -> Option<String> {
    let mut result = pattern.to_string();
    let obj = frontmatter.as_object()?;

    // Replace {field} placeholders
    let re = regex::Regex::new(r"\{(\w+)\}").ok()?;
    for cap in re.captures_iter(pattern) {
        let field = &cap[1];
        let value = match obj.get(field) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(v) => v.to_string().trim_matches('"').to_string(),
            None => return None,
        };
        result = result.replace(&format!("{{{}}}", field), &value);
    }

    Some(result)
}

/// Coerce a value to the expected field type (§7.16).
fn coerce_value(value: &serde_json::Value, target_type: &str) -> Option<serde_json::Value> {
    match target_type {
        "string" => match value {
            serde_json::Value::String(_) => None, // already correct
            serde_json::Value::Number(n) => Some(serde_json::Value::String(n.to_string())),
            serde_json::Value::Bool(b) => Some(serde_json::Value::String(b.to_string())),
            _ => None,
        },
        "integer" => match value {
            serde_json::Value::Number(_) => None, // already correct
            serde_json::Value::String(s) => {
                // Try parsing as i64 first, then as f64 with integer check
                if let Ok(n) = s.parse::<i64>() {
                    Some(serde_json::json!(n))
                } else if let Ok(f) = s.parse::<f64>() {
                    if f.fract() == 0.0 && f.is_finite() {
                        Some(serde_json::json!(f as i64))
                    } else {
                        None // Keep as string, validation will catch it
                    }
                } else {
                    None
                }
            }
            _ => None,
        },
        "number" => match value {
            serde_json::Value::Number(_) => None, // already correct
            serde_json::Value::String(s) => {
                s.parse::<f64>().ok().and_then(|f| {
                    serde_json::Number::from_f64(f).map(serde_json::Value::Number)
                })
            }
            _ => None,
        },
        "boolean" => match value {
            serde_json::Value::Bool(_) => None, // already correct
            serde_json::Value::String(s) => match s.to_lowercase().as_str() {
                "true" | "yes" | "on" => Some(serde_json::Value::Bool(true)),
                "false" | "no" | "off" => Some(serde_json::Value::Bool(false)),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// Simple glob pattern matcher for exclude patterns.
fn match_glob_pattern(pattern: &str, path: &str) -> bool {
    if pattern.ends_with("/**") {
        let prefix = &pattern[..pattern.len() - 3];
        return path.starts_with(&format!("{}/", prefix)) || path == prefix;
    }

    if pattern.starts_with("*.") {
        let ext = &pattern[1..]; // e.g., ".draft.md"
        return path.ends_with(ext);
    }

    if pattern.contains('*') {
        // Simple wildcard matching
        let parts: Vec<&str> = pattern.split('*').collect();
        if parts.len() == 2 {
            return path.starts_with(parts[0]) && path.ends_with(parts[1]);
        }
    }

    // Exact match (directory name)
    path == pattern || path.starts_with(&format!("{}/", pattern))
}

/// Normalize a link path by resolving . and .. segments relative to a source directory.
fn normalize_link_path(target: &str, source_dir: &str) -> String {
    // If the target is absolute-ish (starts with /) treat it as relative to root
    if target.starts_with('/') {
        let cleaned = target.trim_start_matches('/');
        return normalize_segments(cleaned);
    }

    // If target contains relative segments (./ or ../), resolve relative to source dir
    if target.starts_with("./") || target.starts_with("../") || target == "." || target == ".."
        || target.contains("/./") || target.contains("/../")
    {
        let combined = if source_dir.is_empty() {
            target.to_string()
        } else {
            format!("{}/{}", source_dir, target)
        };
        return normalize_segments(&combined);
    }

    // Plain name - no normalization needed
    target.to_string()
}

/// Normalize path segments by resolving . and ..
fn normalize_segments(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.is_empty() || parts.last() == Some(&"..") {
                    parts.push("..");
                } else {
                    parts.pop();
                }
            }
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// Check if an expression string references a given field name as an identifier.
fn expression_references_field(expr_str: &str, field_name: &str) -> bool {
    // Parse the expression and walk the AST looking for Ident nodes
    if let Ok(expr) = ExprParser::parse(expr_str) {
        expr_contains_ident(&expr, field_name)
    } else {
        // If parsing fails, do a simple string check as fallback
        expr_str.contains(field_name)
    }
}

fn expr_contains_ident(expr: &crate::expressions::ast::Expr, name: &str) -> bool {
    use crate::expressions::ast::Expr;
    match expr {
        Expr::Ident(s) => s == name,
        Expr::Dot(obj, _) => expr_contains_ident(obj, name),
        Expr::Index(obj, idx) => expr_contains_ident(obj, name) || expr_contains_ident(idx, name),
        Expr::BinOp(l, _, r) => expr_contains_ident(l, name) || expr_contains_ident(r, name),
        Expr::UnaryOp(_, e) => expr_contains_ident(e, name),
        Expr::NullCoalesce(l, r) => expr_contains_ident(l, name) || expr_contains_ident(r, name),
        Expr::Array(elements) => elements.iter().any(|e| expr_contains_ident(e, name)),
        Expr::Call(f, args) => {
            expr_contains_ident(f, name) || args.iter().any(|a| expr_contains_ident(a, name))
        }
        Expr::Conditional(c, t, e) => {
            expr_contains_ident(c, name) || expr_contains_ident(t, name) || expr_contains_ident(e, name)
        }
        _ => false,
    }
}
