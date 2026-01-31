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
            explicit_type_keys: vec!["type".into(), "types".into()],
            default_validation: "warn".into(),
            default_strict: serde_json::Value::Bool(false),
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
            id_field_explicit: settings_json["id_field_explicit"].as_bool().unwrap_or(false),
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
                    raw_frontmatter: None,
                    file_path: Some(path.to_string()),
                    body: body.map(String::from),
                    file_size: None, file_mtime: None, file_ctime: None,
                    this_context: None,
                    all_files: None,
                    traversal_depth: std::cell::Cell::new(0),
                    backlinks_index: None,
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

    /// Build all files data for asFile() traversal in expressions.
    pub fn build_all_files_data(&self) -> Vec<crate::expressions::evaluator::ResolvedFileData> {
        let files = self.scan_collection_files();
        files.iter()
            .filter_map(|fp| {
                let rp = fp.strip_prefix(&self.root)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
                    .replace('\\', "/");
                let content = std::fs::read_to_string(fp).ok()?;
                let doc = parse_document(&content);
                let fm = match &doc.frontmatter {
                    Some(serde_yaml::Value::Mapping(m)) => yaml_mapping_to_json(m),
                    _ => serde_json::json!({}),
                };
                let type_names = self.determine_types_for_path(&fm, Some(&rp));
                let effective = self.apply_defaults(&fm, &type_names);
                let effective = self.coerce_types(&effective, &type_names);
                Some(crate::expressions::evaluator::ResolvedFileData {
                    path: rp,
                    frontmatter: effective,
                    body: doc.body,
                })
            })
            .collect()
    }

    /// Build backlinks index from all files data.
    /// Returns a map: target_path → Vec<source_path> (deduplicated).
    pub fn build_backlinks_index(&self, all_files: &[crate::expressions::evaluator::ResolvedFileData]) -> HashMap<String, Vec<String>> {
        use crate::expressions::evaluator::{extract_links_from_body, extract_embeds_from_body, extract_links_from_fm_value};

        let mut index: HashMap<String, Vec<String>> = HashMap::new();

        // Collect all known file paths for resolution
        let known_paths: Vec<&str> = all_files.iter().map(|f| f.path.as_str()).collect();

        for file_data in all_files {
            let source_path = &file_data.path;
            let mut targets: Vec<String> = Vec::new();

            // Extract links from frontmatter values
            if let serde_json::Value::Object(ref map) = file_data.frontmatter {
                for (_key, val) in map {
                    extract_links_from_fm_value(val, &mut targets);
                }
            }

            // Extract links from body
            let body_links = extract_links_from_body(&file_data.body);
            targets.extend(body_links);

            // Extract embeds from body
            let body_embeds = extract_embeds_from_body(&file_data.body);
            targets.extend(body_embeds);

            // Resolve each target and add to backlinks index
            let mut seen_targets: Vec<String> = Vec::new();
            for target in &targets {
                // Resolve the target to a file path
                let resolved = self.resolve_link_target(target, source_path, &known_paths);
                if let Some(resolved_path) = resolved {
                    if !seen_targets.contains(&resolved_path) {
                        seen_targets.push(resolved_path.clone());
                        index.entry(resolved_path)
                            .or_insert_with(Vec::new)
                            .push(source_path.clone());
                    }
                }
            }
        }

        // Deduplicate source entries per target
        for sources in index.values_mut() {
            sources.sort();
            sources.dedup();
        }

        index
    }

    /// Resolve a link target string to a file path.
    fn resolve_link_target(&self, target: &str, source_path: &str, known_paths: &[&str]) -> Option<String> {
        // Strip wikilink syntax
        let target = if target.starts_with("[[") && target.ends_with("]]") {
            let inner = &target[2..target.len()-2];
            inner.split('|').next().unwrap_or(inner).split('#').next().unwrap_or(inner).trim()
        } else {
            // Strip anchor from markdown links
            target.split('#').next().unwrap_or(target).trim()
        };

        if target.is_empty() { return None; }

        // Handle relative paths (./foo, ../foo)
        let resolved_target = if target.starts_with("./") || target.starts_with("../") {
            let source_dir = std::path::Path::new(source_path)
                .parent()
                .unwrap_or(std::path::Path::new(""));
            let joined = source_dir.join(target);
            // Normalize path
            let mut components = Vec::new();
            for c in joined.components() {
                match c {
                    std::path::Component::ParentDir => { components.pop(); }
                    std::path::Component::CurDir => {}
                    _ => { components.push(c); }
                }
            }
            let normalized: PathBuf = components.iter().collect();
            normalized.to_string_lossy().to_string().replace('\\', "/")
        } else {
            target.to_string()
        };

        // Exact path match
        if known_paths.contains(&resolved_target.as_str()) {
            return Some(resolved_target.clone());
        }

        // With .md extension
        if !resolved_target.ends_with(".md") && !resolved_target.ends_with(".mdx") {
            let with_md = format!("{}.md", resolved_target);
            if known_paths.contains(&with_md.as_str()) {
                return Some(with_md);
            }
        }

        // Basename match (for wikilinks without path)
        if !resolved_target.contains('/') {
            let target_lower = resolved_target.to_lowercase();
            for path in known_paths {
                let basename = std::path::Path::new(path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                if basename == resolved_target || basename.to_lowercase() == target_lower {
                    return Some(path.to_string());
                }
            }
            // Also try matching against ID field and title in frontmatter
            let files = self.scan_collection_files();
            for fp in &files {
                let rp = fp.strip_prefix(&self.root)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
                    .replace('\\', "/");
                if !known_paths.contains(&rp.as_str()) { continue; }
                if let Ok(content) = std::fs::read_to_string(fp) {
                    let doc = parse_document(&content);
                    if let Some(serde_yaml::Value::Mapping(m)) = &doc.frontmatter {
                        let fm = yaml_mapping_to_json(m);
                        // Check ID field
                        if let Some(id) = fm.get(&self.settings.id_field).and_then(|v| v.as_str()) {
                            if id == resolved_target || id.to_lowercase() == target_lower {
                                return Some(rp);
                            }
                        }
                        // Check title field
                        if let Some(title) = fm.get("title").and_then(|v| v.as_str()) {
                            if title == resolved_target || title.to_lowercase() == target_lower {
                                return Some(rp);
                            }
                        }
                    }
                }
            }
        }

        None
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
        let source_dir_str = source_dir.to_string_lossy();
        let normalized = normalize_link_path(target, &source_dir_str);

        // Check for path traversal (escaping collection root)
        // Per spec §8.13: count leading ../ segments in the raw target.
        // If the target has >= 2 leading ../ segments AND those segments would reach or
        // exceed the collection root boundary, flag as path_traversal.
        let leading_dotdot_count = count_leading_dotdot(target);
        let source_depth = if source_dir_str.is_empty() { 0 } else {
            source_dir_str.split('/').filter(|s| !s.is_empty()).count()
        };
        let reaches_root = leading_dotdot_count >= source_depth && leading_dotdot_count >= 2;
        if reaches_root || normalized.starts_with("../") || normalized.starts_with("..\\") || normalized == ".." {
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
        let check_backlinks = input.get("check_backlinks").and_then(|v| v.as_bool()).unwrap_or(false);

        let full_path = self.root.join(path);
        if !full_path.exists() {
            return op_error(FILE_NOT_FOUND, &format!("File not found: {}", path));
        }

        // Check backlinks before deletion
        let mut broken_links: Vec<serde_json::Value> = Vec::new();
        if check_backlinks {
            let all_files = self.build_all_files_data();
            let bl_index = self.build_backlinks_index(&all_files);
            if let Some(sources) = bl_index.get(path) {
                for source in sources {
                    broken_links.push(serde_json::json!({
                        "path": source,
                    }));
                }
            }
        }

        if let Err(e) = std::fs::remove_file(&full_path) {
            return op_error("io_error", &format!("Failed to delete: {}", e));
        }

        let mut result = serde_json::json!({
            "path": path,
            "deleted": true,
        });
        if !broken_links.is_empty() {
            result["broken_links"] = serde_json::Value::Array(broken_links);
        }
        result
    }

    /// Batch update files matching a where clause (§12.7).
    pub fn batch_update(&self, input: &serde_json::Value, simulate_io_error: Option<&str>, skip_dependents: bool) -> serde_json::Value {
        let dry_run = input.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);

        // Two modes: where+fields or updates (explicit list)
        if let Some(updates) = input.get("updates").and_then(|v| v.as_array()) {
            return self.batch_update_explicit(updates, dry_run, simulate_io_error);
        }

        let where_clause = match input.get("where") {
            Some(v) => v.clone(),
            None => return op_error("invalid_input", "batch_update requires 'where' or 'updates'"),
        };
        let fields = input.get("fields").cloned().unwrap_or(serde_json::json!({}));

        // Find matching files using query logic
        let matching_paths = self.query_matching_paths(&serde_json::Value::String(
            where_clause.as_str().unwrap_or("").to_string()
        ));

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

        // Validate-all-then-execute: validate all files first
        if self.settings.default_validation == "error" {
            for path in &matching_paths {
                let full_path = self.root.join(path);
                let content = match std::fs::read_to_string(&full_path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let doc = parse_document(&content);
                let existing_mapping = match &doc.frontmatter {
                    Some(serde_yaml::Value::Mapping(m)) => m.clone(),
                    _ => serde_yaml::Mapping::new(),
                };
                let merged = serializer::merge_fields(&existing_mapping, &fields, &self.settings.write_nulls);
                let merged_json = yaml_mapping_to_json(&merged);
                let type_names = self.determine_types(&merged_json);
                let effective = self.apply_defaults(&merged_json, &type_names);
                let validation = self.validate(&effective, &type_names, path);
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
        }

        if dry_run {
            return serde_json::json!({
                "batch_result": {
                    "total": total,
                    "succeeded": total,
                    "failed": 0,
                }
            });
        }

        // Execute updates
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        let mut skipped = 0usize;
        let mut details: Vec<serde_json::Value> = Vec::new();
        let mut failed_paths: Vec<String> = Vec::new();

        // Build backlinks index for skip_dependents checking
        let bl_index_for_skip = if skip_dependents {
            let all_files = self.build_all_files_data();
            Some(self.build_backlinks_index(&all_files))
        } else {
            None
        };

        for path in &matching_paths {
            // Check skip_dependents: if this file has a link TO a failed file, skip it
            // Use backlinks index: if a failed file has this file as a backlink source,
            // then this file links to the failed file and should be skipped
            if skip_dependents && !failed_paths.is_empty() {
                if let Some(ref bl_index) = bl_index_for_skip {
                    // Check if any failed path lists this file as a source (backlink)
                    let has_dep = failed_paths.iter().any(|fp| {
                        bl_index.get(fp).map_or(false, |sources| sources.contains(path))
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

            let update_result = self.update(&serde_json::json!({
                "path": path,
                "fields": fields,
            }));

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
    fn batch_update_explicit(&self, updates: &[serde_json::Value], dry_run: bool, simulate_io_error: Option<&str>) -> serde_json::Value {
        // Validate all first
        if self.settings.default_validation == "error" {
            for update in updates {
                let path = match update.get("path").and_then(|v| v.as_str()) {
                    Some(p) => p,
                    None => continue,
                };
                let fields = update.get("fields").cloned().unwrap_or(serde_json::json!({}));
                let full_path = self.root.join(path);
                let content = match std::fs::read_to_string(&full_path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let doc = parse_document(&content);
                let existing_mapping = match &doc.frontmatter {
                    Some(serde_yaml::Value::Mapping(m)) => m.clone(),
                    _ => serde_yaml::Mapping::new(),
                };
                let merged = serializer::merge_fields(&existing_mapping, &fields, &self.settings.write_nulls);
                let merged_json = yaml_mapping_to_json(&merged);
                let type_names = self.determine_types(&merged_json);
                let effective = self.apply_defaults(&merged_json, &type_names);
                let validation = self.validate(&effective, &type_names, path);
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
        }

        let total = updates.len();
        if dry_run {
            return serde_json::json!({
                "batch_result": {
                    "total": total,
                    "succeeded": total,
                    "failed": 0,
                }
            });
        }

        let mut succeeded = 0usize;
        let mut failed = 0usize;
        let mut details: Vec<serde_json::Value> = Vec::new();

        for update in updates {
            let path = match update.get("path").and_then(|v| v.as_str()) {
                Some(p) => p,
                None => continue,
            };
            let fields = update.get("fields").cloned().unwrap_or(serde_json::json!({}));

            if let Some(err_path) = simulate_io_error {
                if path == err_path {
                    failed += 1;
                    details.push(serde_json::json!({ "path": path, "status": "failed" }));
                    continue;
                }
            }

            let result = self.update(&serde_json::json!({ "path": path, "fields": fields }));
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
    pub fn batch_delete(&self, input: &serde_json::Value, simulate_io_error: Option<&str>) -> serde_json::Value {
        let dry_run = input.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);
        let check_backlinks = input.get("check_backlinks").and_then(|v| v.as_bool()).unwrap_or(false);

        let where_clause = match input.get("where") {
            Some(v) => v.clone(),
            None => return op_error("invalid_input", "batch_delete requires 'where'"),
        };

        let matching_paths = self.query_matching_paths(&serde_json::Value::String(
            where_clause.as_str().unwrap_or("").to_string()
        ));

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
            let all_files = self.build_all_files_data();
            let bl_index = self.build_backlinks_index(&all_files);
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

            let full_path = self.root.join(path);
            match std::fs::remove_file(&full_path) {
                Ok(_) => {
                    succeeded += 1;
                    details.push(serde_json::json!({ "path": path, "status": "success" }));
                }
                Err(_) => {
                    failed += 1;
                    details.push(serde_json::json!({ "path": path, "status": "failed" }));
                }
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

    /// Query matching file paths (reuses query logic but only returns paths).
    fn query_matching_paths(&self, where_clause: &serde_json::Value) -> Vec<String> {
        let files = self.scan_collection_files();
        let mut matching: Vec<String> = Vec::new();

        // Build all_files data for asFile() traversal in where clauses
        let all_files_data: Vec<crate::expressions::evaluator::ResolvedFileData> = files.iter()
            .filter_map(|fp| {
                let rp = fp.strip_prefix(&self.root)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
                    .replace('\\', "/");
                let content = std::fs::read_to_string(fp).ok()?;
                let doc = parse_document(&content);
                let fm = match &doc.frontmatter {
                    Some(serde_yaml::Value::Mapping(m)) => yaml_mapping_to_json(m),
                    _ => serde_json::json!({}),
                };
                let type_names = self.determine_types_for_path(&fm, Some(&rp));
                let effective = self.apply_defaults(&fm, &type_names);
                let effective = self.coerce_types(&effective, &type_names);
                Some(crate::expressions::evaluator::ResolvedFileData {
                    path: rp,
                    frontmatter: effective,
                    body: doc.body,
                })
            })
            .collect();
        let all_files_arc = std::sync::Arc::new(all_files_data);
        let backlinks_index = self.build_backlinks_index(&all_files_arc);
        let backlinks_arc = std::sync::Arc::new(backlinks_index);

        for file_path in &files {
            let rel_path = file_path.strip_prefix(&self.root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
                .replace('\\', "/");

            let content = match std::fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let doc = parse_document(&content);

            let raw_frontmatter = match &doc.frontmatter {
                Some(serde_yaml::Value::Mapping(m)) => yaml_mapping_to_json(m),
                _ => serde_json::json!({}),
            };

            let type_names = self.determine_types_for_path(&raw_frontmatter, Some(&rel_path));
            let effective = self.apply_defaults(&raw_frontmatter, &type_names);
            let effective = self.coerce_types(&effective, &type_names);
            let effective = self.evaluate_computed_fields(effective, &type_names, &rel_path, Some(doc.body.as_str()));

            let file_metadata = std::fs::metadata(file_path).ok();
            let file_size = file_metadata.as_ref().map(|m| m.len()).unwrap_or(0);
            let file_mtime = file_metadata.as_ref().and_then(|m| m.modified().ok()).map(|t| {
                let dt: chrono::DateTime<chrono::Utc> = t.into();
                dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
            });
            let file_ctime = file_metadata.as_ref().and_then(|m| m.created().ok()).map(|t| {
                let dt: chrono::DateTime<chrono::Utc> = t.into();
                dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
            });

            let eval_ctx = QueryEvalContext {
                frontmatter: &effective,
                raw_frontmatter: &raw_frontmatter,
                file_path: &rel_path,
                body: &doc.body,
                type_names: &type_names,
                formulas: &serde_json::Map::new(),
                file_size,
                file_mtime: file_mtime.as_deref(),
                file_ctime: file_ctime.as_deref(),
                this_context: None,
                all_files: Some(all_files_arc.clone()),
                backlinks_index: Some(backlinks_arc.clone()),
            };

            if self.evaluate_where(&eval_ctx, where_clause) {
                matching.push(rel_path);
            }
        }

        matching.sort();
        matching
    }

    /// Rebuild the cache (§13.3.4, §13.8).
    /// Since this implementation uses file-scan queries, this is a no-op that returns success.
    pub fn cache_rebuild(&self) -> serde_json::Value {
        serde_json::json!({ "success": true })
    }

    /// Clear the cache (§13.3.5, §13.8).
    /// Since this implementation uses file-scan queries, this is a no-op that returns success.
    pub fn cache_clear(&self) -> serde_json::Value {
        serde_json::json!({ "success": true })
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

        // Read the source file's id before rename (for id-stability check)
        let source_id = std::fs::read_to_string(&from_path).ok().and_then(|content| {
            let doc = parse_document(&content);
            if let Some(serde_yaml::Value::Mapping(m)) = &doc.frontmatter {
                let json = yaml_mapping_to_json(m);
                json.get(&self.settings.id_field).and_then(|v| v.as_str()).map(|s| s.to_string())
            } else {
                None
            }
        });

        if let Err(e) = std::fs::rename(&from_path, &to_path) {
            let error_str = e.to_string();
            if error_str.contains("NUL") || error_str.contains("null") {
                return op_error(INVALID_PATH, &format!("Invalid path: {}", e));
            }
            return op_error("io_error", &format!("Failed to rename: {}", e));
        }

        // Determine if we should update references
        let update_refs = input.get("update_refs")
            .and_then(|v| v.as_bool())
            .unwrap_or(self.settings.rename_update_refs);

        let mut references_updated: Vec<serde_json::Value> = Vec::new();
        let mut warnings: Vec<serde_json::Value> = Vec::new();

        if update_refs {
            self.update_references_after_rename(
                from, to, &source_id,
                &mut references_updated, &mut warnings,
            );
        }

        let mut result = serde_json::json!({
            "from": from,
            "to": to,
        });
        if !references_updated.is_empty() {
            result["references_updated"] = serde_json::Value::Array(references_updated);
        }
        if !warnings.is_empty() {
            result["warnings"] = serde_json::Value::Array(warnings);
        }
        result
    }

    /// Update references in all collection files after a rename.
    fn update_references_after_rename(
        &self,
        from: &str,
        to: &str,
        source_id: &Option<String>,
        references_updated: &mut Vec<serde_json::Value>,
        warnings: &mut Vec<serde_json::Value>,
    ) {
        let from_stem = std::path::Path::new(from).file_stem()
            .and_then(|s| s.to_str()).unwrap_or("").to_string();
        let to_stem = std::path::Path::new(to).file_stem()
            .and_then(|s| s.to_str()).unwrap_or("").to_string();
        let from_no_ext = from.strip_suffix(".md").or_else(|| from.strip_suffix(".mdx")).unwrap_or(from);
        let to_no_ext = to.strip_suffix(".md").or_else(|| to.strip_suffix(".mdx")).unwrap_or(to);

        let files = self.scan_collection_files();

        for file_path in &files {
            let rel_path = match file_path.strip_prefix(&self.root) {
                Ok(p) => p.to_string_lossy().to_string().replace('\\', "/"),
                Err(_) => continue,
            };

            // Skip the old path (doesn't exist anymore)
            if rel_path == from {
                continue;
            }

            let content = match std::fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let doc = parse_document(&content);
            let source_dir = std::path::Path::new(&rel_path).parent()
                .map(|p| p.to_string_lossy().to_string()).unwrap_or_default();

            let mut fm_changed = false;
            let mut body_changed = false;
            let mut fm_yaml = match &doc.frontmatter {
                Some(v @ serde_yaml::Value::Mapping(_)) => v.clone(),
                _ => continue,
            };

            // Update frontmatter link fields
            self.update_fm_links(
                &mut fm_yaml, from, to, &from_stem, &to_stem,
                from_no_ext, to_no_ext, &source_dir, &rel_path,
                source_id, &mut fm_changed, references_updated, warnings,
            );

            // Update body links
            let mut new_body = doc.body.clone();
            if self.update_body_links(
                &mut new_body, from, to, &from_stem, &to_stem,
                from_no_ext, to_no_ext, &source_dir,
            ) {
                body_changed = true;
                references_updated.push(serde_json::json!({
                    "path": rel_path,
                    "location": "body",
                }));
            }

            // Write back if changed
            if fm_changed || body_changed {
                let new_fm = if fm_changed { &fm_yaml } else { doc.frontmatter.as_ref().unwrap() };
                let mut output = String::new();
                output.push_str("---\n");
                let yaml_str = serde_yaml::to_string(new_fm).unwrap_or_default();
                output.push_str(&yaml_str);
                if !yaml_str.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str("---\n");
                if !new_body.is_empty() {
                    output.push_str(&new_body);
                    if !new_body.ends_with('\n') {
                        output.push('\n');
                    }
                }
                let _ = std::fs::write(file_path, output);
            }
        }
    }

    /// Check if a link value resolves to the renamed file.
    fn link_resolves_to(&self, link_val: &str, from_path: &str, from_stem: &str, from_no_ext: &str, source_dir: &str) -> bool {
        let is_wikilink = link_val.starts_with("[[") && link_val.ends_with("]]");
        let is_md_link = link_val.contains("](");
        let is_bare_path = link_val.starts_with("./") || link_val.starts_with("../") || link_val.contains('/');

        // Only process actual link-formatted values
        if !is_wikilink && !is_md_link && !is_bare_path {
            return false;
        }

        // Strip wikilink syntax
        let target = if is_wikilink {
            let inner = &link_val[2..link_val.len()-2];
            inner.split('|').next().unwrap_or(inner).split('#').next().unwrap_or(inner).trim()
        } else if is_md_link {
            // Markdown link: extract path from [text](path)
            if let Some(start) = link_val.find("](") {
                let rest = &link_val[start+2..];
                let end = rest.find(')').unwrap_or(rest.len());
                rest[..end].split('#').next().unwrap_or(&rest[..end]).trim()
            } else { return false; }
        } else {
            // Bare path
            link_val.split('#').next().unwrap_or(link_val).trim()
        };

        if target.is_empty() { return false; }

        // Normalize the target relative to source file
        let normalized = normalize_link_path(target, source_dir);

        // Check if it resolves to the from path
        let norm_with_md = if !normalized.ends_with(".md") && !normalized.ends_with(".mdx") {
            format!("{}.md", normalized)
        } else {
            normalized.clone()
        };

        if norm_with_md == from_path || normalized == from_path || normalized == from_no_ext {
            return true;
        }

        // Check stem match for simple wikilinks (only wikilinks can match by stem)
        if is_wikilink && !target.contains('/') && !target.contains('.') {
            if target == from_stem {
                return true;
            }
        }

        false
    }

    /// Rewrite a link value to point to the new path.
    fn rewrite_link_value(&self, link_val: &str, from_stem: &str, to_stem: &str,
                          from_no_ext: &str, to_no_ext: &str, to_path: &str, source_dir: &str) -> String {
        if link_val.starts_with("[[") && link_val.ends_with("]]") {
            // Wikilink: [[target]], [[target|alias]], [[target#anchor]]
            let inner = &link_val[2..link_val.len()-2];
            let (target_part, rest) = if let Some(pipe_pos) = inner.find('|') {
                (&inner[..pipe_pos], &inner[pipe_pos..])
            } else {
                (inner, "")
            };
            let (name_part, anchor) = if let Some(hash_pos) = target_part.find('#') {
                (&target_part[..hash_pos], &target_part[hash_pos..])
            } else {
                (target_part, "")
            };

            // Determine new name
            let new_name = if name_part == from_stem || name_part.trim() == from_stem {
                // Simple name -> use new stem
                to_stem.to_string()
            } else if name_part.contains('/') {
                // Path-based wikilink -> use new path without extension
                to_no_ext.to_string()
            } else {
                to_stem.to_string()
            };

            format!("[[{}{}{}]]", new_name, anchor, rest)
        } else if link_val.contains("](") {
            // Markdown link: [text](path) or ![alt](path)
            let prefix_end = link_val.find("](").unwrap();
            let prefix = &link_val[..prefix_end+2]; // includes "]("
            let rest_start = prefix_end + 2;
            let rest = &link_val[rest_start..];
            let paren_end = rest.rfind(')').unwrap_or(rest.len());
            let path_and_anchor = &rest[..paren_end];
            let suffix = &rest[paren_end..]; // the closing ")"

            let (path_part, anchor) = if let Some(hash_pos) = path_and_anchor.find('#') {
                (&path_and_anchor[..hash_pos], &path_and_anchor[hash_pos..])
            } else {
                (path_and_anchor, "")
            };

            // Compute new relative path from source_dir to to_path
            let new_rel = compute_relative_path(source_dir, to_path);
            format!("{}{}{}{}", prefix, new_rel, anchor, suffix)
        } else {
            // Bare path
            let (path_part, anchor) = if let Some(hash_pos) = link_val.find('#') {
                (&link_val[..hash_pos], &link_val[hash_pos..])
            } else {
                (link_val, "")
            };
            let new_rel = compute_relative_path(source_dir, to_path);
            format!("{}{}", new_rel, anchor)
        }
    }

    /// Update frontmatter link fields to point to the new path.
    fn update_fm_links(
        &self,
        fm: &mut serde_yaml::Value,
        from: &str, to: &str, from_stem: &str, to_stem: &str,
        from_no_ext: &str, to_no_ext: &str, source_dir: &str, rel_path: &str,
        source_id: &Option<String>,
        changed: &mut bool,
        refs_updated: &mut Vec<serde_json::Value>,
        warnings: &mut Vec<serde_json::Value>,
    ) {
        if let serde_yaml::Value::Mapping(map) = fm {
            let keys: Vec<serde_yaml::Value> = map.keys().cloned().collect();
            for key in keys {
                let key_str = key.as_str().map(|s| s.to_string()).unwrap_or_default();
                if let Some(val) = map.get_mut(&key) {
                    match val {
                        serde_yaml::Value::String(s) => {
                            let resolves = self.link_resolves_to(s, from, from_stem, from_no_ext, source_dir);
                            if resolves {
                                // Check for id-stability: if the link resolves via id and id didn't change, skip
                                if self.should_skip_id_stable_link(s, source_id, from_stem) {
                                    continue;
                                }
                                // Check for ambiguity
                                if self.is_ambiguous_link(s, from) {
                                    warnings.push(serde_json::json!({
                                        "path": rel_path,
                                        "message": format!("Ambiguous link '{}' not updated", s),
                                    }));
                                    continue;
                                }
                                let new_val = self.rewrite_link_value(s, from_stem, to_stem, from_no_ext, to_no_ext, to, source_dir);
                                *s = new_val;
                                *changed = true;
                                refs_updated.push(serde_json::json!({
                                    "path": rel_path,
                                    "field": key_str,
                                }));
                            }
                        }
                        serde_yaml::Value::Sequence(items) => {
                            for (idx, item) in items.iter_mut().enumerate() {
                                if let serde_yaml::Value::String(s) = item {
                                    if self.link_resolves_to(s, from, from_stem, from_no_ext, source_dir) {
                                        if self.should_skip_id_stable_link(s, source_id, from_stem) {
                                            continue;
                                        }
                                        if self.is_ambiguous_link(s, from) {
                                            warnings.push(serde_json::json!({
                                                "path": rel_path,
                                                "message": format!("Ambiguous link '{}' not updated", s),
                                            }));
                                            continue;
                                        }
                                        let new_val = self.rewrite_link_value(s, from_stem, to_stem, from_no_ext, to_no_ext, to, source_dir);
                                        *s = new_val;
                                        *changed = true;
                                        refs_updated.push(serde_json::json!({
                                            "path": rel_path,
                                            "field": format!("{}[{}]", key_str, idx),
                                        }));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Check if a wikilink resolves via id_field and the id didn't change (id-stability).
    /// Per spec §12.5: implementations SHOULD NOT rewrite the link when the id_field
    /// value hasn't changed, to avoid unnecessary churn.
    /// We apply this only when the link target matches the id AND the id differs
    /// from the old filename stem (so the link was genuinely id-based, not filename-based).
    fn should_skip_id_stable_link(&self, link_val: &str, source_id: &Option<String>, _from_stem: &str) -> bool {
        if !self.settings.id_field_explicit {
            return false;
        }
        if let Some(id) = source_id {
            // Only wikilinks can resolve via id_field. Markdown links and bare paths
            // resolve by path and always need updating.
            if link_val.starts_with("[[") && link_val.ends_with("]]") {
                let inner = &link_val[2..link_val.len()-2];
                let target = inner.split('|').next().unwrap_or(inner).split('#').next().unwrap_or(inner).trim();
                // Simple name (no path separators or extensions) that matches the
                // renamed file's id_field value → link still resolves via id lookup
                if !target.contains('/') && !target.contains('.') {
                    if target == id.as_str() {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Check if a link was ambiguous before the rename (matched multiple files).
    /// We check post-rename state but also account for the old file that was renamed.
    fn is_ambiguous_link(&self, link_val: &str, from_path: &str) -> bool {
        let target = if link_val.starts_with("[[") && link_val.ends_with("]]") {
            let inner = &link_val[2..link_val.len()-2];
            inner.split('|').next().unwrap_or(inner).split('#').next().unwrap_or(inner).trim().to_string()
        } else {
            return false; // Only wikilinks can be ambiguous
        };

        if target.is_empty() || target.contains('/') || target.contains('.') {
            return false; // Path-based links are not ambiguous
        }

        // Count files on disk matching this simple name
        let files = self.scan_collection_files();
        let mut match_count = 0;
        for file_path in &files {
            let rel_path = match file_path.strip_prefix(&self.root) {
                Ok(p) => p.to_string_lossy().to_string().replace('\\', "/"),
                Err(_) => continue,
            };

            let stem = std::path::Path::new(&rel_path)
                .file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if stem == target {
                match_count += 1;
            }
        }

        // Also count the old (now-renamed) file: its old stem may have matched the target
        let from_stem = std::path::Path::new(from_path)
            .file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if from_stem == target {
            match_count += 1;
        }

        match_count > 1
    }

    /// Update body links (wikilinks and markdown links) to point to the new path.
    /// Returns true if any changes were made.
    fn update_body_links(
        &self,
        body: &mut String,
        from: &str, to: &str, from_stem: &str, to_stem: &str,
        from_no_ext: &str, to_no_ext: &str, source_dir: &str,
    ) -> bool {
        let mut changed = false;

        // Process line by line, skipping fenced code blocks and inline code
        let mut result = String::with_capacity(body.len());
        let mut in_fence = false;
        let mut fence_marker: Option<char> = None;
        let mut fence_count = 0;

        for line in body.split('\n') {
            let trimmed = line.trim_start();

            if !in_fence {
                // Check for opening fence
                if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                    let fc = trimmed.chars().next().unwrap();
                    let cnt = trimmed.chars().take_while(|&c| c == fc).count();
                    in_fence = true;
                    fence_marker = Some(fc);
                    fence_count = cnt;
                    result.push_str(line);
                    result.push('\n');
                    continue;
                }
                // Process this line for link replacements (outside code blocks)
                let new_line = self.replace_links_in_line(
                    line, from, to, from_stem, to_stem, from_no_ext, to_no_ext, source_dir,
                );
                if new_line != line {
                    changed = true;
                }
                result.push_str(&new_line);
                result.push('\n');
            } else {
                // Check for closing fence
                if let Some(fc) = fence_marker {
                    if trimmed.starts_with(fc) {
                        let cnt = trimmed.chars().take_while(|&c| c == fc).count();
                        if cnt >= fence_count && trimmed[cnt * fc.len_utf8()..].trim().is_empty() {
                            in_fence = false;
                        }
                    }
                }
                result.push_str(line);
                result.push('\n');
            }
        }

        // Remove trailing newline added by split/join
        if result.ends_with('\n') && !body.ends_with('\n') {
            result.pop();
        }
        // Handle case where body ends with \n but we added an extra
        if body.ends_with('\n') && result.ends_with("\n\n") && !body.ends_with("\n\n") {
            result.pop();
        }

        if changed {
            *body = result;
        }
        changed
    }

    /// Replace link references in a single line (outside code blocks).
    fn replace_links_in_line(
        &self,
        line: &str,
        _from: &str, to: &str, from_stem: &str, to_stem: &str,
        from_no_ext: &str, to_no_ext: &str, source_dir: &str,
    ) -> String {
        let mut result = String::with_capacity(line.len());
        let chars: Vec<char> = line.chars().collect();
        let len = chars.len();
        let mut i = 0;

        while i < len {
            // Skip inline code
            if chars[i] == '`' {
                let start = i;
                let bt_count = chars[i..].iter().take_while(|&&c| c == '`').count();
                i += bt_count;
                let mut found = false;
                while i + bt_count <= len {
                    if chars[i] == '`' {
                        let close_count = chars[i..].iter().take_while(|&&c| c == '`').count();
                        if close_count == bt_count {
                            for c in &chars[start..i + close_count] {
                                result.push(*c);
                            }
                            i += close_count;
                            found = true;
                            break;
                        }
                        i += close_count;
                    } else {
                        i += 1;
                    }
                }
                if !found {
                    for c in &chars[start..] { result.push(*c); }
                    break;
                }
                continue;
            }

            // Check for ![ (could be embed ![[...]] or markdown image ![alt](path))
            if chars[i] == '!' && i + 1 < len && chars[i + 1] == '[' {
                if i + 2 < len && chars[i + 2] == '[' {
                    // Wikilink embed: ![[target]]
                    let link_start = i;
                    i += 3; // skip ![[
                    let content_start = i;
                    while i < len && !(chars[i] == ']' && i + 1 < len && chars[i + 1] == ']') {
                        i += 1;
                    }
                    if i < len {
                        let inner: String = chars[content_start..i].iter().collect();
                        i += 2; // skip ]]
                        if self.link_resolves_to(&format!("[[{}]]", inner), _from, from_stem, from_no_ext, source_dir) {
                            let new_inner = self.rewrite_wikilink_inner(&inner, from_stem, to_stem, from_no_ext, to_no_ext);
                            result.push_str(&format!("![[{}]]", new_inner));
                        } else {
                            for c in &chars[link_start..i] { result.push(*c); }
                        }
                        continue;
                    }
                    for c in &chars[link_start..len] { result.push(*c); }
                    break;
                } else {
                    // Markdown image: ![alt](path)
                    let link_start = i;
                    i += 2; // skip ![
                    let mut depth = 1;
                    while i < len && depth > 0 {
                        if chars[i] == '[' { depth += 1; }
                        if chars[i] == ']' { depth -= 1; }
                        i += 1;
                    }
                    if i < len && chars[i] == '(' {
                        let paren_start = i + 1;
                        i += 1;
                        let mut pdepth = 1;
                        while i < len && pdepth > 0 {
                            if chars[i] == '(' { pdepth += 1; }
                            if chars[i] == ')' { pdepth -= 1; }
                            i += 1;
                        }
                        let href: String = chars[paren_start..i-1].iter().collect();
                        if !href.starts_with("http://") && !href.starts_with("https://") {
                            if self.link_resolves_to(&href, _from, from_stem, from_no_ext, source_dir) {
                                let text_part: String = chars[link_start..paren_start-1].iter().collect();
                                let (_, anchor) = if let Some(hp) = href.find('#') {
                                    (&href[..hp], &href[hp..])
                                } else {
                                    (href.as_str(), "")
                                };
                                let new_rel = compute_relative_path(source_dir, to);
                                result.push_str(&format!("{}({}{}", text_part, new_rel, anchor));
                                result.push(')');
                                continue;
                            }
                        }
                        for c in &chars[link_start..i] { result.push(*c); }
                        continue;
                    }
                    for c in &chars[link_start..i] { result.push(*c); }
                    continue;
                }
            }

            // Wikilink: [[target]]
            if i + 1 < len && chars[i] == '[' && chars[i + 1] == '[' {
                let link_start = i;
                i += 2;
                let content_start = i;
                while i < len && !(chars[i] == ']' && i + 1 < len && chars[i + 1] == ']') {
                    i += 1;
                }
                if i < len {
                    let inner: String = chars[content_start..i].iter().collect();
                    i += 2;
                    if self.link_resolves_to(&format!("[[{}]]", inner), _from, from_stem, from_no_ext, source_dir) {
                        let new_inner = self.rewrite_wikilink_inner(&inner, from_stem, to_stem, from_no_ext, to_no_ext);
                        result.push_str(&format!("[[{}]]", new_inner));
                    } else {
                        for c in &chars[link_start..i] { result.push(*c); }
                    }
                    continue;
                }
                for c in &chars[link_start..len] { result.push(*c); }
                break;
            }

            // Markdown link: [text](path)
            if chars[i] == '[' {
                let link_start = i;
                i += 1;
                let mut depth = 1;
                while i < len && depth > 0 {
                    if chars[i] == '[' { depth += 1; }
                    if chars[i] == ']' { depth -= 1; }
                    i += 1;
                }
                if i < len && chars[i] == '(' {
                    let paren_start = i + 1;
                    i += 1;
                    let mut pdepth = 1;
                    while i < len && pdepth > 0 {
                        if chars[i] == '(' { pdepth += 1; }
                        if chars[i] == ')' { pdepth -= 1; }
                        i += 1;
                    }
                    let href: String = chars[paren_start..i-1].iter().collect();
                    if !href.starts_with("http://") && !href.starts_with("https://") {
                        if self.link_resolves_to(&href, _from, from_stem, from_no_ext, source_dir) {
                            let text_part: String = chars[link_start..paren_start-1].iter().collect();
                            let (_, anchor) = if let Some(hp) = href.find('#') {
                                (&href[..hp], &href[hp..])
                            } else {
                                (href.as_str(), "")
                            };
                            let new_rel = compute_relative_path(source_dir, to);
                            result.push_str(&format!("{}({}{}", text_part, new_rel, anchor));
                            result.push(')');
                            continue;
                        }
                    }
                    for c in &chars[link_start..i] { result.push(*c); }
                    continue;
                }
                for c in &chars[link_start..i] { result.push(*c); }
                continue;
            }

            result.push(chars[i]);
            i += 1;
        }

        result
    }

    /// Rewrite the inner content of a wikilink (without the [[ ]] brackets).
    fn rewrite_wikilink_inner(&self, inner: &str, from_stem: &str, to_stem: &str,
                               from_no_ext: &str, to_no_ext: &str) -> String {
        let (target_part, rest) = if let Some(pipe_pos) = inner.find('|') {
            (&inner[..pipe_pos], &inner[pipe_pos..])
        } else {
            (inner, "")
        };
        let (name_part, anchor) = if let Some(hash_pos) = target_part.find('#') {
            (&target_part[..hash_pos], &target_part[hash_pos..])
        } else {
            (target_part, "")
        };
        let new_name = if name_part.contains('/') {
            to_no_ext.to_string()
        } else {
            to_stem.to_string()
        };
        format!("{}{}{}", new_name, anchor, rest)
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

    // -----------------------------------------------------------------------
    // Query operation (§10)
    // -----------------------------------------------------------------------

    /// Parse a link value into its components.
    pub fn parse_link(&self, input: &serde_json::Value) -> serde_json::Value {
        let value = match input.get("value").and_then(|v| v.as_str()) {
            Some(v) => v,
            None => return serde_json::json!({"error": {"code": "invalid_input", "message": "parse_link requires 'value' field"}}),
        };

        let raw = value.to_string();

        // Wikilink: [[target]], [[target|alias]], [[target#anchor]], [[target#anchor|alias]]
        if value.starts_with("[[") && value.ends_with("]]") {
            let inner = &value[2..value.len()-2];
            // Split on | for alias
            let (target_part, alias) = if let Some(pipe_idx) = inner.find('|') {
                (&inner[..pipe_idx], Some(inner[pipe_idx+1..].to_string()))
            } else {
                (inner, None)
            };
            // Split on # for anchor
            let (target, anchor) = if let Some(hash_idx) = target_part.find('#') {
                (target_part[..hash_idx].to_string(), Some(target_part[hash_idx+1..].to_string()))
            } else {
                (target_part.to_string(), None)
            };
            let is_relative = target.starts_with("./") || target.starts_with("../");
            return serde_json::json!({
                "link": {
                    "raw": raw,
                    "target": target,
                    "alias": alias,
                    "anchor": anchor,
                    "format": "wikilink",
                    "is_relative": is_relative,
                }
            });
        }

        // Markdown link: [text](path) or [text](path#anchor)
        if value.starts_with('[') && value.contains("](") && value.ends_with(')') {
            let bracket_end = value.find("](").unwrap();
            let text = &value[1..bracket_end];
            let path_str = &value[bracket_end+2..value.len()-1];
            let (path, anchor) = if let Some(hash_idx) = path_str.find('#') {
                (path_str[..hash_idx].to_string(), Some(path_str[hash_idx+1..].to_string()))
            } else {
                (path_str.to_string(), None)
            };
            let is_relative = path.starts_with("./") || path.starts_with("../");
            let alias = Some(text.to_string());
            return serde_json::json!({
                "link": {
                    "raw": raw,
                    "target": path,
                    "alias": alias,
                    "anchor": anchor,
                    "format": "markdown",
                    "is_relative": is_relative,
                }
            });
        }

        // Bare/path
        let is_relative = value.starts_with("./") || value.starts_with("../");
        serde_json::json!({
            "link": {
                "raw": raw,
                "target": value,
                "alias": serde_json::Value::Null,
                "anchor": serde_json::Value::Null,
                "format": "path",
                "is_relative": is_relative,
            }
        })
    }

    /// Resolve a link field to a target file path.
    pub fn resolve_link(&self, input: &serde_json::Value) -> serde_json::Value {
        let source_path = match input.get("path").and_then(|v| v.as_str()) {
            Some(v) => v,
            None => return serde_json::json!({"error": {"code": "invalid_input", "message": "resolve_link requires 'path' field"}}),
        };
        let field_name = match input.get("field").and_then(|v| v.as_str()) {
            Some(v) => v,
            None => return serde_json::json!({"error": {"code": "invalid_input", "message": "resolve_link requires 'field' field"}}),
        };

        // Read the source file to get the field value
        let read_result = self.read(&serde_json::json!({"path": source_path}));
        let fm = match read_result.get("frontmatter") {
            Some(fm) => fm,
            None => return serde_json::json!({"error": {"code": "file_not_found", "message": format!("Cannot read {}", source_path)}}),
        };

        let field_val = match fm.get(field_name).and_then(|v| v.as_str()) {
            Some(v) => v.to_string(),
            None => return serde_json::json!({"resolved_path": serde_json::Value::Null}),
        };

        // Parse the link value
        let parse_result = self.parse_link(&serde_json::json!({"value": field_val}));
        let target = match parse_result.get("link").and_then(|l| l.get("target")).and_then(|t| t.as_str()) {
            Some(t) => t.to_string(),
            None => return serde_json::json!({"resolved_path": serde_json::Value::Null}),
        };
        let is_relative = parse_result.get("link")
            .and_then(|l| l.get("is_relative"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let format = parse_result.get("link")
            .and_then(|l| l.get("format"))
            .and_then(|v| v.as_str())
            .unwrap_or("wikilink");

        let source_dir = Path::new(source_path).parent().and_then(|p| p.to_str()).unwrap_or("");

        // Determine field type constraints
        let target_type = self.get_field_target_type(source_path, field_name);

        // Resolution logic
        let resolved = if target.starts_with('/') {
            // Leading slash: resolve from collection root
            Some(target.trim_start_matches('/').to_string())
        } else if is_relative || format == "markdown" || format == "path" {
            // Relative path resolution
            self.resolve_relative_link(&target, source_dir)
        } else if target.contains('/') {
            // Absolute-like path in wikilink
            if target.starts_with('/') {
                // Resolve from root
                Some(target.trim_start_matches('/').to_string())
            } else {
                // Resolve from root
                Some(target.clone())
            }
        } else {
            // Simple name - try id_field, then filename
            self.resolve_simple_name(&target, source_dir, target_type.as_deref())
        };

        // Check for path traversal
        if let Some(ref path) = resolved {
            if path.starts_with("../") || path.contains("/../") {
                return serde_json::json!({
                    "error": {"code": "path_traversal", "message": "Link escapes collection root"},
                    "issues": [{"code": "path_traversal", "field": field_name, "severity": "error"}]
                });
            }
        }

        // Try adding .md extension if needed
        if let Some(ref path) = resolved {
            let full_path = self.root.join(path);
            if full_path.exists() {
                return serde_json::json!({"resolved_path": path});
            }
            // Try with .md
            let md_path = format!("{}.md", path);
            let md_full = self.root.join(&md_path);
            if md_full.exists() {
                return serde_json::json!({"resolved_path": md_path});
            }
            // Try configured extensions
            for ext in &self.settings.extensions {
                let ext_path = format!("{}.{}", path, ext);
                let ext_full = self.root.join(&ext_path);
                if ext_full.exists() {
                    return serde_json::json!({"resolved_path": ext_path});
                }
            }
        }

        serde_json::json!({"resolved_path": serde_json::Value::Null})
    }

    /// Resolve a relative link path.
    fn resolve_relative_link(&self, target: &str, source_dir: &str) -> Option<String> {
        let base = if target.starts_with("./") || target.starts_with("../") {
            // Relative to source directory
            if source_dir.is_empty() {
                target.to_string()
            } else {
                format!("{}/{}", source_dir, target)
            }
        } else {
            // Markdown links are relative to containing file directory
            if source_dir.is_empty() {
                target.to_string()
            } else {
                format!("{}/{}", source_dir, target)
            }
        };

        // Normalize path (resolve . and ..)
        let mut segments: Vec<&str> = Vec::new();
        for seg in base.split('/') {
            match seg {
                "." => {}
                ".." => { segments.pop(); }
                s if !s.is_empty() => { segments.push(s); }
                _ => {}
            }
        }
        Some(segments.join("/"))
    }

    /// Resolve a simple name (no path separators) via id_field, then filename.
    fn resolve_simple_name(&self, name: &str, source_dir: &str, target_type: Option<&str>) -> Option<String> {
        let files = self.scan_collection_files();
        let id_field_name = if self.settings.id_field.is_empty() { "id" } else { &self.settings.id_field };

        let mut id_matches: Vec<String> = Vec::new();
        let mut filename_matches: Vec<String> = Vec::new();

        for file_path in &files {
            let rel_path = file_path.strip_prefix(&self.root).ok()
                .and_then(|p| p.to_str())
                .unwrap_or("")
                .to_string();

            // Read file content once
            let content = match std::fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let doc = crate::frontmatter::parser::parse_document(&content);
            let fm = if let Some(ref yaml_fm) = doc.frontmatter {
                crate::frontmatter::parser::yaml_to_json(yaml_fm)
            } else {
                continue;
            };

            // Check target type constraint
            if let Some(constraint_type) = target_type {
                let file_types = self.determine_types_for_path(&fm, Some(&rel_path));
                if !file_types.iter().any(|t| t.to_lowercase() == constraint_type.to_lowercase()) {
                    continue;
                }
            }

            // Check id_field match
            if let Some(id_val) = fm.get(id_field_name).and_then(|v| v.as_str()) {
                if id_val == name {
                    id_matches.push(rel_path.clone());
                }
            }

            // Check filename match
            let basename = Path::new(&rel_path).file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if basename == name {
                filename_matches.push(rel_path.clone());
            }
        }

        // Prefer id matches over filename matches
        let candidates = if !id_matches.is_empty() { id_matches } else { filename_matches };

        if candidates.is_empty() {
            return None;
        }
        if candidates.len() == 1 {
            return Some(candidates[0].clone());
        }

        // Tiebreaker: same directory > shortest path > alphabetical
        let mut sorted = candidates;
        sorted.sort_by(|a, b| {
            let a_same = Path::new(a).parent().and_then(|p| p.to_str()).unwrap_or("") == source_dir;
            let b_same = Path::new(b).parent().and_then(|p| p.to_str()).unwrap_or("") == source_dir;
            if a_same != b_same {
                return if a_same { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater };
            }
            let a_depth = a.matches('/').count();
            let b_depth = b.matches('/').count();
            if a_depth != b_depth {
                return a_depth.cmp(&b_depth);
            }
            a.cmp(b)
        });
        Some(sorted[0].clone())
    }

    /// Get the target type constraint for a field.
    fn get_field_target_type(&self, source_path: &str, field_name: &str) -> Option<String> {
        // Read source file to get its type, then look up the field definition
        let read_result = self.read(&serde_json::json!({"path": source_path}));
        let fm = read_result.get("frontmatter")?;
        let file_types = self.determine_types_for_path(fm, Some(source_path));
        for type_name in &file_types {
            if let Some(type_def) = self.types.get(&type_name.to_lowercase()) {
                if let Some(field_def) = type_def.fields.get(field_name) {
                    return field_def.target.clone();
                }
            }
        }
        None
    }

    // -----------------------------------------------------------------------

    /// Execute a query against the collection.
    pub fn query(&self, input: &serde_json::Value) -> serde_json::Value {
        // Extract query parameters - support both input.query.X and input.X
        let query = input.get("query").unwrap_or(input);

        let filter_types: Vec<String> = query.get("types")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_lowercase())).collect())
            .unwrap_or_default();

        let folder = query.get("folder").and_then(|v| v.as_str());
        let where_clause = query.get("where");
        let order_by = query.get("order_by").and_then(|v| v.as_array());
        let limit = query.get("limit").and_then(|v| v.as_u64());
        let offset = query.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
        let include_body = query.get("include_body").and_then(|v| v.as_bool()).unwrap_or(false);

        // GroupBy clause
        let group_by = query.get("groupBy").or_else(|| query.get("group_by"));

        // Property summaries: field → summary_type (e.g., "priority" → "Average")
        let property_summaries: HashMap<String, String> = query.get("property_summaries")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        // Custom summaries: name → formula expression
        let custom_summaries: HashMap<String, String> = query.get("summaries")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        // Formulas (Query+ profile)
        let formulas: HashMap<String, String> = query.get("formulas")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        // Build 'this' context from context_file if provided
        let this_context: Option<Box<EvalContext>> = query.get("context_file")
            .or_else(|| input.get("context_file"))
            .and_then(|v| v.as_str())
            .and_then(|cf_path| {
                let read_result = self.read(&serde_json::json!({"path": cf_path}));
                if read_result.get("error").is_some() { return None; }
                let fm = read_result.get("frontmatter").cloned()
                    .unwrap_or(serde_json::json!({}));
                let raw_fm = read_result.get("raw_frontmatter").cloned();
                let body = read_result.get("body").and_then(|v| v.as_str()).map(String::from);
                let file_size = read_result.pointer("/file/size").and_then(|v| v.as_u64());
                let file_mtime = read_result.pointer("/file/mtime").and_then(|v| v.as_str()).map(String::from);
                Some(Box::new(EvalContext {
                    frontmatter: fm,
                    raw_frontmatter: raw_fm,
                    file_path: Some(cf_path.to_string()),
                    body,
                    file_size,
                    file_mtime,
                    file_ctime: None,
                    this_context: None,
                    all_files: None,
                    traversal_depth: std::cell::Cell::new(0),
                    backlinks_index: None,
                }))
            });

        // Pre-validate where clause expressions
        if let Some(where_val) = where_clause {
            if let Err(err) = self.validate_where_clause(where_val) {
                return err;
            }
        }

        // Pre-validate formula expressions and check for circular references
        if !formulas.is_empty() {
            if let Err(err) = self.validate_formulas(&formulas) {
                return err;
            }
        }

        // Scan all files and build result candidates
        let files = self.scan_collection_files();

        // Pre-build all_files data for asFile() traversal
        let all_files_data: Vec<crate::expressions::evaluator::ResolvedFileData> = files.iter()
            .filter_map(|fp| {
                let rp = fp.strip_prefix(&self.root)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
                    .replace('\\', "/");
                let content = std::fs::read_to_string(fp).ok()?;
                let doc = parse_document(&content);
                let fm = match &doc.frontmatter {
                    Some(serde_yaml::Value::Mapping(m)) => yaml_mapping_to_json(m),
                    _ => serde_json::json!({}),
                };
                let type_names = self.determine_types_for_path(&fm, Some(&rp));
                let effective = self.apply_defaults(&fm, &type_names);
                let effective = self.coerce_types(&effective, &type_names);
                Some(crate::expressions::evaluator::ResolvedFileData {
                    path: rp,
                    frontmatter: effective,
                    body: doc.body,
                })
            })
            .collect();
        let all_files_arc = std::sync::Arc::new(all_files_data);
        let backlinks_index = self.build_backlinks_index(&all_files_arc);
        let backlinks_arc = std::sync::Arc::new(backlinks_index);

        let mut candidates: Vec<serde_json::Value> = Vec::new();

        for file_path in &files {
            let rel_path = file_path.strip_prefix(&self.root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            // Normalize backslashes to forward slashes
            let rel_path = rel_path.replace('\\', "/");

            // Folder filter
            if let Some(folder_prefix) = folder {
                let folder_prefix = folder_prefix.trim_end_matches('/');
                if !rel_path.starts_with(folder_prefix)
                    || (rel_path.len() > folder_prefix.len()
                        && rel_path.as_bytes()[folder_prefix.len()] != b'/') {
                    continue;
                }
            }

            // Read file content
            let content = match std::fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let doc = parse_document(&content);

            // Get frontmatter
            let raw_frontmatter = match &doc.frontmatter {
                Some(serde_yaml::Value::Mapping(m)) => yaml_mapping_to_json(m),
                _ => serde_json::json!({}),
            };

            // Determine types
            let type_names = self.determine_types_for_path(&raw_frontmatter, Some(&rel_path));

            // Type filter
            if !filter_types.is_empty() {
                let matches_type = type_names.iter().any(|t| filter_types.contains(t));
                if !matches_type {
                    continue;
                }
            }

            // Apply defaults for effective frontmatter
            let effective = self.apply_defaults(&raw_frontmatter, &type_names);
            let effective = self.coerce_types(&effective, &type_names);
            let effective = self.evaluate_computed_fields(effective, &type_names, &rel_path, Some(doc.body.as_str()));

            // Evaluate formulas in dependency order
            let formula_order = self.topological_sort_formulas(&formulas);
            let mut formula_values = serde_json::Map::new();
            let mut formula_error: Option<serde_json::Value> = None;
            for fname in &formula_order {
                let fexpr = match formulas.get(fname) {
                    Some(e) => e,
                    None => continue,
                };
                // Build frontmatter with formula results available
                let mut fm_with_formulas = match effective.as_object() {
                    Some(m) => m.clone(),
                    None => serde_json::Map::new(),
                };
                // Add formula namespace: formula.X accessible as nested object
                let formula_obj = serde_json::Value::Object(formula_values.clone());
                fm_with_formulas.insert("formula".to_string(), formula_obj);

                let fctx = EvalContext {
                    frontmatter: serde_json::Value::Object(fm_with_formulas),
                    raw_frontmatter: None,
                    file_path: Some(rel_path.clone()),
                    body: Some(doc.body.clone()),
                    file_size: None, file_mtime: None, file_ctime: None,
                    this_context: None,
                    all_files: None,
                    traversal_depth: std::cell::Cell::new(0),
                    backlinks_index: None,
                };
                match ExprParser::parse(fexpr) {
                    Ok(parsed) => {
                        match eval_expr(&parsed, &fctx) {
                            Ok(val) => { formula_values.insert(fname.clone(), val); }
                            Err(e) => {
                                // Propagate fatal formula errors as query-level errors
                                if e.code == "division_by_zero" || e.code == "unknown_function"
                                    || (e.code == "type_error" && !e.message.contains("null")) {
                                    formula_error = Some(serde_json::json!({
                                        "error": { "code": "formula_evaluation_error", "message": format!("Formula '{}': {}", fname, e.message) }
                                    }));
                                    break;
                                }
                                formula_values.insert(fname.clone(), serde_json::Value::Null);
                            }
                        }
                    }
                    Err(_) => { formula_values.insert(fname.clone(), serde_json::Value::Null); }
                }
            }
            if let Some(err) = formula_error {
                return err;
            }

            // Compute file metadata early (needed for where clause evaluation)
            let file_name = Path::new(&rel_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let file_folder = Path::new(&rel_path)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or("");
            let file_metadata = std::fs::metadata(file_path).ok();
            let file_size = file_metadata.as_ref().map(|m| m.len()).unwrap_or(0);
            let file_mtime = file_metadata.as_ref().and_then(|m| m.modified().ok()).map(|t| {
                let dt: chrono::DateTime<chrono::Utc> = t.into();
                dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
            });
            let file_ctime = file_metadata.as_ref().and_then(|m| m.created().ok()).map(|t| {
                let dt: chrono::DateTime<chrono::Utc> = t.into();
                dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
            });

            // Build eval context for where clause (includes formulas and types)
            let eval_ctx = QueryEvalContext {
                frontmatter: &effective,
                raw_frontmatter: &raw_frontmatter,
                file_path: &rel_path,
                body: &doc.body,
                type_names: &type_names,
                formulas: &formula_values,
                file_size,
                file_mtime: file_mtime.as_deref(),
                file_ctime: file_ctime.as_deref(),
                this_context: this_context.clone(),
                all_files: Some(all_files_arc.clone()),
                backlinks_index: Some(backlinks_arc.clone()),
            };

            // Where filter
            if let Some(where_val) = where_clause {
                if !self.evaluate_where(&eval_ctx, where_val) {
                    continue;
                }
            }

            // Extract body metadata
            let body_tags = crate::expressions::evaluator::extract_tags_from_body(&doc.body);
            let body_links = crate::expressions::evaluator::extract_links_from_body(&doc.body);
            let body_embeds = crate::expressions::evaluator::extract_embeds_from_body(&doc.body);

            // Combine frontmatter tags + body tags
            let mut all_tags: Vec<String> = Vec::new();
            if let Some(fm_tags) = effective.get("tags").and_then(|v| v.as_array()) {
                for t in fm_tags {
                    if let Some(s) = t.as_str() {
                        all_tags.push(s.to_string());
                    }
                }
            }
            for t in &body_tags {
                if !all_tags.contains(t) {
                    all_tags.push(t.clone());
                }
            }

            let mut entry = serde_json::json!({
                "path": rel_path,
                "types": type_names,
                "frontmatter": effective,
                "body": if include_body { serde_json::Value::String(doc.body.clone()) } else { serde_json::Value::Null },
                "file": {
                    "name": file_name,
                    "folder": file_folder,
                    "size": file_size,
                    "mtime": file_mtime.as_deref().unwrap_or(""),
                    "tags": all_tags,
                    "links": body_links,
                    "embeds": body_embeds,
                },
            });

            if !formula_values.is_empty() {
                entry["formulas"] = serde_json::Value::Object(formula_values);
            }

            candidates.push(entry);
        }

        // Sort
        if let Some(order_by_clauses) = order_by {
            let sort_specs: Vec<(String, bool)> = order_by_clauses.iter().map(|clause| {
                let field = clause.get("field").and_then(|v| v.as_str()).unwrap_or("");
                let direction = clause.get("direction").and_then(|v| v.as_str()).unwrap_or("asc");
                (field.to_string(), direction == "asc")
            }).collect();

            candidates.sort_by(|a, b| {
                for (field, ascending) in &sort_specs {
                    let av = self.get_sort_value(a, field);
                    let bv = self.get_sort_value(b, field);
                    let cmp = self.compare_sort_values(&av, &bv, field, a, b);
                    let cmp = if *ascending { cmp } else { cmp.reverse() };
                    if cmp != std::cmp::Ordering::Equal {
                        return cmp;
                    }
                }
                // Tiebreaker: ascending file.path
                let ap = a.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let bp = b.get("path").and_then(|v| v.as_str()).unwrap_or("");
                ap.cmp(bp)
            });
        } else {
            // Default sort: by file.path ascending
            candidates.sort_by(|a, b| {
                let ap = a.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let bp = b.get("path").and_then(|v| v.as_str()).unwrap_or("");
                ap.cmp(bp)
            });
        }

        // GroupBy handling
        if let Some(gb) = group_by {
            let gb_property = gb.get("property").and_then(|v| v.as_str()).unwrap_or("");
            let gb_direction = gb.get("direction").and_then(|v| v.as_str()).unwrap_or("ASC");

            // Group candidates by property value (preserve insertion order with Vec)
            let mut group_keys_ordered: Vec<serde_json::Value> = Vec::new();
            let mut groups_map: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
            for candidate in &candidates {
                let key = candidate.get("frontmatter")
                    .and_then(|fm| fm.get(gb_property))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let key_str = if key.is_null() { "\0null".to_string() } else { key.to_string() };
                if !groups_map.contains_key(&key_str) {
                    group_keys_ordered.push(key);
                }
                groups_map.entry(key_str).or_default().push(candidate.clone());
            }

            // Sort groups by key
            let mut group_keys = group_keys_ordered;
            group_keys.sort_by(|a, b| {
                // Null sorts last in ASC, first in DESC
                match (a.is_null(), b.is_null()) {
                    (true, true) => std::cmp::Ordering::Equal,
                    (true, false) => std::cmp::Ordering::Greater,
                    (false, true) => std::cmp::Ordering::Less,
                    _ => {
                        let a_str = match a { serde_json::Value::String(s) => s.clone(), _ => a.to_string() };
                        let b_str = match b { serde_json::Value::String(s) => s.clone(), _ => b.to_string() };
                        a_str.cmp(&b_str)
                    }
                }
            });
            if gb_direction.eq_ignore_ascii_case("DESC") {
                group_keys.reverse();
            }

            // Build group results
            let mut groups_result: Vec<serde_json::Value> = Vec::new();
            for key in &group_keys {
                let key_str = if key.is_null() { "\0null".to_string() } else { key.to_string() };
                let group_candidates = groups_map.get(&key_str).unwrap();
                let mut group_obj = serde_json::json!({
                    "key": key,
                    "results": group_candidates,
                });

                // Compute per-group summaries if property_summaries present
                if !property_summaries.is_empty() {
                    let summaries = self.compute_summaries(group_candidates, &property_summaries, &custom_summaries);
                    group_obj["summaries"] = summaries;
                }

                groups_result.push(group_obj);
            }

            return serde_json::json!({
                "groups": groups_result,
                "meta": {
                    "total_count": candidates.len(),
                    "has_more": false,
                },
            });
        }

        // Pagination
        let total_count = candidates.len();
        let offset = offset as usize;
        let results = if let Some(lim) = limit {
            let lim = lim as usize;
            if offset >= candidates.len() {
                Vec::new()
            } else {
                candidates[offset..].iter().take(lim).cloned().collect()
            }
        } else {
            if offset >= candidates.len() {
                Vec::new()
            } else {
                candidates[offset..].to_vec()
            }
        };

        let has_more = if let Some(lim) = limit {
            offset + (lim as usize) < total_count
        } else {
            false
        };

        // Compute summaries
        let mut result = serde_json::json!({
            "results": results,
            "meta": {
                "total_count": total_count,
                "has_more": has_more,
            },
        });

        if !property_summaries.is_empty() {
            let summaries = self.compute_summaries(&candidates, &property_summaries, &custom_summaries);
            result["summaries"] = summaries;
        }

        result
    }

    /// Compute property summaries over a set of candidates.
    fn compute_summaries(
        &self,
        candidates: &[serde_json::Value],
        property_summaries: &HashMap<String, String>,
        custom_summaries: &HashMap<String, String>,
    ) -> serde_json::Value {
        let mut summaries = serde_json::Map::new();

        for (field, summary_type) in property_summaries {
            // Collect values for this field from all candidates
            let values: Vec<&serde_json::Value> = candidates.iter()
                .filter_map(|c| c.get("frontmatter").and_then(|fm| fm.get(field)))
                .collect();

            let result = if let Some(formula) = custom_summaries.get(summary_type) {
                // Custom summary: evaluate formula with `values` array in context
                self.evaluate_custom_summary(formula, field, candidates)
            } else {
                match summary_type.as_str() {
                    "Average" => {
                        let nums: Vec<f64> = values.iter()
                            .filter_map(|v| v.as_f64())
                            .collect();
                        if nums.is_empty() {
                            serde_json::Value::Null
                        } else {
                            let sum: f64 = nums.iter().sum();
                            let avg = sum / nums.len() as f64;
                            // Return integer if it's a whole number
                            if avg == avg.floor() && avg.abs() < i64::MAX as f64 {
                                serde_json::json!(avg as i64)
                            } else {
                                serde_json::json!(avg)
                            }
                        }
                    }
                    "Sum" => {
                        let nums: Vec<f64> = values.iter()
                            .filter_map(|v| v.as_f64())
                            .collect();
                        let has_float = values.iter().any(|v| v.is_f64() && !v.is_i64() && !v.is_u64());
                        if nums.is_empty() {
                            serde_json::json!(0)
                        } else {
                            let sum: f64 = nums.iter().sum();
                            if !has_float && sum == sum.floor() && sum.abs() < i64::MAX as f64 {
                                serde_json::json!(sum as i64)
                            } else {
                                serde_json::json!(sum)
                            }
                        }
                    }
                    "Min" => {
                        let nums: Vec<f64> = values.iter()
                            .filter_map(|v| v.as_f64())
                            .collect();
                        if nums.is_empty() {
                            serde_json::Value::Null
                        } else {
                            let min = nums.iter().cloned().fold(f64::INFINITY, f64::min);
                            if min == min.floor() && min.abs() < i64::MAX as f64 {
                                serde_json::json!(min as i64)
                            } else {
                                serde_json::json!(min)
                            }
                        }
                    }
                    "Max" => {
                        let nums: Vec<f64> = values.iter()
                            .filter_map(|v| v.as_f64())
                            .collect();
                        if nums.is_empty() {
                            serde_json::Value::Null
                        } else {
                            let max = nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                            if max == max.floor() && max.abs() < i64::MAX as f64 {
                                serde_json::json!(max as i64)
                            } else {
                                serde_json::json!(max)
                            }
                        }
                    }
                    "Earliest" => {
                        let strs: Vec<&str> = values.iter()
                            .filter_map(|v| v.as_str())
                            .collect();
                        strs.iter().min().map(|s| serde_json::json!(s)).unwrap_or(serde_json::Value::Null)
                    }
                    "Latest" => {
                        let strs: Vec<&str> = values.iter()
                            .filter_map(|v| v.as_str())
                            .collect();
                        strs.iter().max().map(|s| serde_json::json!(s)).unwrap_or(serde_json::Value::Null)
                    }
                    "Checked" => {
                        let count = values.iter()
                            .filter(|v| v.as_bool() == Some(true))
                            .count();
                        serde_json::json!(count)
                    }
                    "Unchecked" => {
                        let count = values.iter()
                            .filter(|v| v.as_bool() == Some(false))
                            .count();
                        serde_json::json!(count)
                    }
                    "Empty" => {
                        let count = candidates.iter()
                            .filter(|c| {
                                let val = c.get("frontmatter").and_then(|fm| fm.get(field));
                                val.is_none() || val == Some(&serde_json::Value::Null)
                                    || val.and_then(|v| v.as_str()) == Some("")
                            })
                            .count();
                        serde_json::json!(count)
                    }
                    "Filled" => {
                        let count = candidates.iter()
                            .filter(|c| {
                                let val = c.get("frontmatter").and_then(|fm| fm.get(field));
                                val.is_some() && val != Some(&serde_json::Value::Null)
                                    && val.and_then(|v| v.as_str()) != Some("")
                            })
                            .count();
                        serde_json::json!(count)
                    }
                    "Unique" => {
                        let mut unique_vals: Vec<serde_json::Value> = Vec::new();
                        for v in &values {
                            if !unique_vals.contains(v) {
                                unique_vals.push((*v).clone());
                            }
                        }
                        serde_json::json!(unique_vals.len())
                    }
                    _ => serde_json::Value::Null,
                }
            };

            summaries.insert(field.clone(), result);
        }

        serde_json::Value::Object(summaries)
    }

    /// Evaluate a custom summary formula with `values` array in context.
    fn evaluate_custom_summary(
        &self,
        formula: &str,
        field: &str,
        candidates: &[serde_json::Value],
    ) -> serde_json::Value {
        // Collect numeric values for the field
        let values: Vec<serde_json::Value> = candidates.iter()
            .filter_map(|c| c.get("frontmatter").and_then(|fm| fm.get(field)).cloned())
            .collect();

        let values_array = serde_json::Value::Array(values);

        // Build context with `values` as a top-level field
        let ctx = EvalContext {
            frontmatter: serde_json::json!({"values": values_array}),
            raw_frontmatter: None,
            file_path: None,
            body: None,
            file_size: None, file_mtime: None, file_ctime: None,
            this_context: None,
            all_files: None,
            traversal_depth: std::cell::Cell::new(0),
            backlinks_index: None,
        };

        match ExprParser::parse(formula) {
            Ok(parsed) => {
                match eval_expr(&parsed, &ctx) {
                    Ok(val) => val,
                    Err(_) => serde_json::Value::Null,
                }
            }
            Err(_) => serde_json::Value::Null,
        }
    }

    /// Get a value for sorting from a result entry.
    fn get_sort_value(&self, entry: &serde_json::Value, field: &str) -> serde_json::Value {
        // Handle formula.X fields
        if let Some(formula_field) = field.strip_prefix("formula.") {
            return entry.get("formulas")
                .and_then(|f| f.get(formula_field))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
        }
        // Handle file.X fields (including nested like file.embeds.length)
        if let Some(file_field) = field.strip_prefix("file.") {
            if let Some(file_obj) = entry.get("file") {
                // Handle nested properties like embeds.length, tags.length
                if let Some((prop, sub)) = file_field.split_once('.') {
                    let prop_val = file_obj.get(prop).cloned().unwrap_or(serde_json::Value::Null);
                    return match sub {
                        "length" => {
                            if let Some(arr) = prop_val.as_array() {
                                serde_json::json!(arr.len())
                            } else if let Some(s) = prop_val.as_str() {
                                serde_json::json!(s.len())
                            } else {
                                serde_json::json!(0)
                            }
                        }
                        _ => serde_json::Value::Null,
                    };
                }
                return file_obj.get(file_field).cloned().unwrap_or(serde_json::Value::Null);
            }
            return serde_json::Value::Null;
        }
        // Regular frontmatter field
        entry.get("frontmatter")
            .and_then(|fm| fm.get(field))
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }

    /// Compare two sort values with null handling and enum order.
    fn compare_sort_values(
        &self,
        a: &serde_json::Value,
        b: &serde_json::Value,
        field: &str,
        a_entry: &serde_json::Value,
        b_entry: &serde_json::Value,
    ) -> std::cmp::Ordering {
        let a_null = a.is_null();
        let b_null = b.is_null();

        // Null handling: nulls sort last in ascending (we handle reversal in caller)
        if a_null && b_null { return std::cmp::Ordering::Equal; }
        if a_null { return std::cmp::Ordering::Greater; }
        if b_null { return std::cmp::Ordering::Less; }

        // Check if this is an enum field - need to find field def from types
        if !field.starts_with("formula.") && !field.starts_with("file.") {
            if let Some(enum_values) = self.get_enum_values_for_field(field, a_entry, b_entry) {
                let a_str = a.as_str().unwrap_or("");
                let b_str = b.as_str().unwrap_or("");
                let a_idx = enum_values.iter().position(|v| v == a_str).unwrap_or(usize::MAX);
                let b_idx = enum_values.iter().position(|v| v == b_str).unwrap_or(usize::MAX);
                return a_idx.cmp(&b_idx);
            }
        }

        // Standard comparison
        match (a, b) {
            (serde_json::Value::Number(an), serde_json::Value::Number(bn)) => {
                let af = an.as_f64().unwrap_or(0.0);
                let bf = bn.as_f64().unwrap_or(0.0);
                af.partial_cmp(&bf).unwrap_or(std::cmp::Ordering::Equal)
            }
            (serde_json::Value::String(a_s), serde_json::Value::String(b_s)) => a_s.cmp(b_s),
            (serde_json::Value::Bool(ab), serde_json::Value::Bool(bb)) => ab.cmp(bb),
            _ => std::cmp::Ordering::Equal,
        }
    }

    /// Find enum values for a field from the types of result entries.
    fn get_enum_values_for_field(
        &self,
        field: &str,
        a_entry: &serde_json::Value,
        _b_entry: &serde_json::Value,
    ) -> Option<Vec<String>> {
        // Look up field definition from the entry's types
        let type_names = a_entry.get("types")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>())
            .unwrap_or_default();

        for tn in &type_names {
            if let Some(td) = self.types.get(tn) {
                if let Some(fd) = td.fields.get(field) {
                    if fd.field_type == "enum" {
                        if let Some(ref vals) = fd.values {
                            return Some(vals.clone());
                        }
                    }
                }
            }
        }
        None
    }

    /// Evaluate a where clause (string expression or YAML and/or/not structure).
    fn evaluate_where(&self, ctx: &QueryEvalContext, where_val: &serde_json::Value) -> bool {
        match where_val {
            serde_json::Value::String(expr_str) => {
                self.evaluate_where_expr(ctx, expr_str)
            }
            serde_json::Value::Object(map) => {
                if let Some(and_val) = map.get("and") {
                    if let Some(arr) = and_val.as_array() {
                        return arr.iter().all(|clause| self.evaluate_where(ctx, clause));
                    }
                }
                if let Some(or_val) = map.get("or") {
                    if let Some(arr) = or_val.as_array() {
                        return arr.iter().any(|clause| self.evaluate_where(ctx, clause));
                    }
                }
                if let Some(not_val) = map.get("not") {
                    return !self.evaluate_where(ctx, not_val);
                }
                // Unknown structure - treat as false
                false
            }
            _ => false,
        }
    }

    /// Validate a where clause (pre-check before scanning files).
    /// Returns Err with error JSON if the clause has a syntax error.
    fn validate_where_clause(&self, where_val: &serde_json::Value) -> Result<(), serde_json::Value> {
        match where_val {
            serde_json::Value::String(expr_str) => {
                self.validate_single_expr(expr_str)
            }
            serde_json::Value::Object(map) => {
                if let Some(and_val) = map.get("and") {
                    if let Some(arr) = and_val.as_array() {
                        for clause in arr {
                            self.validate_where_clause(clause)?;
                        }
                    }
                }
                if let Some(or_val) = map.get("or") {
                    if let Some(arr) = or_val.as_array() {
                        for clause in arr {
                            self.validate_where_clause(clause)?;
                        }
                    }
                }
                if let Some(not_val) = map.get("not") {
                    self.validate_where_clause(not_val)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Validate a single expression string - check for parse errors, unknown functions, etc.
    fn validate_single_expr(&self, expr_str: &str) -> Result<(), serde_json::Value> {
        match ExprParser::parse(expr_str) {
            Ok(parsed) => {
                // Try evaluating with empty context to catch static errors
                let ctx = EvalContext::empty();
                match eval_expr(&parsed, &ctx) {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        match e.code.as_str() {
                            "wrong_argument_count" | "expression_depth_exceeded" => {
                                Err(serde_json::json!({
                                    "error": { "code": e.code, "message": e.message }
                                }))
                            }
                            "unknown_function" => {
                                // ext:: functions are expected to be unknown — don't abort the query
                                if e.message.contains("ext.") || e.message.contains("ext::") || e.message.contains("extension") {
                                    Ok(())
                                } else {
                                    Err(serde_json::json!({
                                        "error": { "code": e.code, "message": e.message }
                                    }))
                                }
                            }
                            _ => Ok(()),  // Other errors depend on context
                        }
                    }
                }
            }
            Err(msg) => {
                let code = if msg.contains("expression_depth_exceeded") {
                    "expression_depth_exceeded"
                } else {
                    "invalid_expression"
                };
                Err(serde_json::json!({
                    "error": { "code": code, "message": msg }
                }))
            }
        }
    }

    /// Validate formula expressions for syntax errors and circular references.
    fn validate_formulas(&self, formulas: &HashMap<String, String>) -> Result<(), serde_json::Value> {
        // Check for circular references between formulas
        for (name, expr_str) in formulas {
            // Check if formula references itself
            if expr_str.contains(&format!("formula.{}", name)) {
                return Err(serde_json::json!({
                    "error": { "code": "circular_formula", "message": format!("Formula '{}' references itself", name) }
                }));
            }
        }

        // Check for circular chains: A -> B -> A
        for (name, expr_str) in formulas {
            let mut visited = std::collections::HashSet::new();
            visited.insert(name.clone());
            let mut to_check: Vec<String> = Vec::new();
            // Find formula references in this expression
            for (other_name, _) in formulas {
                if other_name != name && expr_str.contains(&format!("formula.{}", other_name)) {
                    to_check.push(other_name.clone());
                }
            }
            while let Some(dep) = to_check.pop() {
                if !visited.insert(dep.clone()) {
                    return Err(serde_json::json!({
                        "error": { "code": "circular_formula", "message": format!("Circular formula reference involving '{}'", name) }
                    }));
                }
                if let Some(dep_expr) = formulas.get(&dep) {
                    for (other_name, _) in formulas {
                        if dep_expr.contains(&format!("formula.{}", other_name)) {
                            to_check.push(other_name.clone());
                        }
                    }
                }
            }
        }

        // Validate each formula expression syntax
        for (name, expr_str) in formulas {
            match ExprParser::parse(expr_str) {
                Ok(parsed) => {
                    // Check for literal division by zero
                    if Self::has_literal_div_by_zero(&parsed) {
                        return Err(serde_json::json!({
                            "error": { "code": "formula_evaluation_error", "message": format!("Formula '{}': Division by zero", name) }
                        }));
                    }
                    // Try evaluating with empty context to catch static errors
                    let ctx = EvalContext::empty();
                    match eval_expr(&parsed, &ctx) {
                        Ok(_) => {}
                        Err(e) => {
                            match e.code.as_str() {
                                "unknown_function" | "wrong_argument_count" => {
                                    return Err(serde_json::json!({
                                        "error": { "code": "formula_evaluation_error", "message": format!("Formula '{}': {}", name, e.message) }
                                    }));
                                }
                                _ => {} // Other errors might depend on per-file context
                            }
                        }
                    }
                }
                Err(msg) => {
                    return Err(serde_json::json!({
                        "error": { "code": "invalid_formula", "message": format!("Formula '{}': {}", name, msg) }
                    }));
                }
            }
        }

        Ok(())
    }

    /// Check if an expression AST contains a literal division by zero (e.g., `x / 0`).
    fn has_literal_div_by_zero(expr: &crate::expressions::ast::Expr) -> bool {
        use crate::expressions::ast::{Expr, BinOp};
        match expr {
            Expr::BinOp(left, BinOp::Div, right) | Expr::BinOp(left, BinOp::Mod, right) => {
                if let Expr::Number(n) = right.as_ref() {
                    if *n == 0.0 {
                        return true;
                    }
                }
                Self::has_literal_div_by_zero(left) || Self::has_literal_div_by_zero(right)
            }
            Expr::BinOp(left, _, right) => {
                Self::has_literal_div_by_zero(left) || Self::has_literal_div_by_zero(right)
            }
            Expr::UnaryOp(_, inner) => Self::has_literal_div_by_zero(inner),
            Expr::NullCoalesce(left, right) => {
                Self::has_literal_div_by_zero(left) || Self::has_literal_div_by_zero(right)
            }
            Expr::Call(base, args) => {
                Self::has_literal_div_by_zero(base) || args.iter().any(|a| Self::has_literal_div_by_zero(a))
            }
            Expr::Conditional(cond, then, else_) => {
                Self::has_literal_div_by_zero(cond) || Self::has_literal_div_by_zero(then) || Self::has_literal_div_by_zero(else_)
            }
            Expr::Dot(inner, _) => Self::has_literal_div_by_zero(inner),
            Expr::Index(left, right) => {
                Self::has_literal_div_by_zero(left) || Self::has_literal_div_by_zero(right)
            }
            Expr::Array(items) => items.iter().any(|i| Self::has_literal_div_by_zero(i)),
            _ => false,
        }
    }

    /// Sort formulas in dependency order (topological sort).
    fn topological_sort_formulas(&self, formulas: &HashMap<String, String>) -> Vec<String> {
        // Build dependency graph
        let mut deps: HashMap<String, Vec<String>> = HashMap::new();
        for (name, expr) in formulas {
            let mut name_deps = Vec::new();
            for (other, _) in formulas {
                if other != name && expr.contains(&format!("formula.{}", other)) {
                    name_deps.push(other.clone());
                }
            }
            deps.insert(name.clone(), name_deps);
        }

        let mut result = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut visiting = std::collections::HashSet::new();

        fn visit(
            name: &str,
            deps: &HashMap<String, Vec<String>>,
            visited: &mut std::collections::HashSet<String>,
            visiting: &mut std::collections::HashSet<String>,
            result: &mut Vec<String>,
        ) {
            if visited.contains(name) { return; }
            if visiting.contains(name) { return; } // circular, handled elsewhere
            visiting.insert(name.to_string());
            if let Some(d) = deps.get(name) {
                for dep in d {
                    visit(dep, deps, visited, visiting, result);
                }
            }
            visiting.remove(name);
            visited.insert(name.to_string());
            result.push(name.to_string());
        }

        // Sort formula names for deterministic ordering
        let mut names: Vec<&String> = formulas.keys().collect();
        names.sort();
        for name in names {
            visit(name, &deps, &mut visited, &mut visiting, &mut result);
        }

        result
    }

    /// Evaluate a single where expression string against file context.
    fn evaluate_where_expr(&self, ctx: &QueryEvalContext, expr_str: &str) -> bool {
        let parsed = match ExprParser::parse(expr_str) {
            Ok(p) => p,
            Err(_) => return false,
        };

        // Build an enriched frontmatter that includes special query namespaces
        let mut enriched_fm = ctx.frontmatter.clone();

        // Add types array to context
        if let serde_json::Value::Object(ref mut map) = enriched_fm {
            let types_arr: Vec<serde_json::Value> = ctx.type_names.iter()
                .map(|t| serde_json::Value::String(t.clone()))
                .collect();
            map.insert("types".to_string(), serde_json::Value::Array(types_arr));
        }

        // Add formula namespace values
        if !ctx.formulas.is_empty() {
            if let serde_json::Value::Object(ref mut map) = enriched_fm {
                map.insert("formula".to_string(), serde_json::Value::Object(ctx.formulas.clone()));
            }
        }

        let eval_ctx = EvalContext {
            frontmatter: enriched_fm,
            raw_frontmatter: Some(ctx.raw_frontmatter.clone()),
            file_path: Some(ctx.file_path.to_string()),
            body: Some(ctx.body.to_string()),
            file_size: Some(ctx.file_size),
            file_mtime: ctx.file_mtime.map(String::from),
            file_ctime: ctx.file_ctime.map(String::from),
            this_context: ctx.this_context.clone(),
            all_files: ctx.all_files.clone(),
            traversal_depth: std::cell::Cell::new(0),
            backlinks_index: ctx.backlinks_index.clone(),
        };

        match eval_expr(&parsed, &eval_ctx) {
            Ok(val) => is_truthy_value(&val),
            Err(_) => false,
        }
    }
}

/// Context for evaluating query where clauses.
struct QueryEvalContext<'a> {
    frontmatter: &'a serde_json::Value,
    raw_frontmatter: &'a serde_json::Value,
    file_path: &'a str,
    body: &'a str,
    type_names: &'a [String],
    formulas: &'a serde_json::Map<String, serde_json::Value>,
    file_size: u64,
    file_mtime: Option<&'a str>,
    file_ctime: Option<&'a str>,
    this_context: Option<Box<EvalContext>>,
    all_files: Option<std::sync::Arc<Vec<crate::expressions::evaluator::ResolvedFileData>>>,
    backlinks_index: Option<std::sync::Arc<HashMap<String, Vec<String>>>>,
}

/// Check if a JSON value is truthy (for where clause evaluation).
fn is_truthy_value(val: &serde_json::Value) -> bool {
    match val {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().map_or(false, |f| f != 0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(_) => true,
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

/// Count leading ../ segments in a path target.
fn count_leading_dotdot(target: &str) -> usize {
    let mut count = 0;
    let mut rest = target;
    while rest.starts_with("../") {
        count += 1;
        rest = &rest[3..];
    }
    if rest == ".." {
        count += 1;
    }
    count
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

/// Compute a relative path from source_dir to target_path.
/// E.g., from "docs" to "archive/detail.md" -> "../archive/detail.md"
/// E.g., from "notes" to "notes/new-target.md" -> "./new-target.md"
fn compute_relative_path(source_dir: &str, target_path: &str) -> String {
    let src_parts: Vec<&str> = if source_dir.is_empty() {
        Vec::new()
    } else {
        source_dir.split('/').filter(|s| !s.is_empty()).collect()
    };

    let target_dir = std::path::Path::new(target_path).parent()
        .map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let target_filename = std::path::Path::new(target_path).file_name()
        .and_then(|s| s.to_str()).unwrap_or(target_path);

    let tgt_parts: Vec<&str> = if target_dir.is_empty() {
        Vec::new()
    } else {
        target_dir.split('/').filter(|s| !s.is_empty()).collect()
    };

    // Find common prefix
    let mut common = 0;
    while common < src_parts.len() && common < tgt_parts.len() && src_parts[common] == tgt_parts[common] {
        common += 1;
    }

    let ups = src_parts.len() - common;
    let mut rel = String::new();
    if ups == 0 && common == tgt_parts.len() {
        // Same directory
        rel.push_str("./");
    } else {
        for _ in 0..ups {
            rel.push_str("../");
        }
        for part in &tgt_parts[common..] {
            rel.push_str(part);
            rel.push('/');
        }
    }
    rel.push_str(target_filename);
    rel
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
