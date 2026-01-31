//! Link resolution algorithm (§8.4).

/// Compute a relative path from source_dir to target_path.
/// E.g., from "docs" to "archive/detail.md" -> "../archive/detail.md"
/// E.g., from "notes" to "notes/new-target.md" -> "./new-target.md"
pub(crate) fn compute_relative_path(source_dir: &str, target_path: &str) -> String {
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

// --- impl Collection methods for link resolution ---

use std::path::{Path, PathBuf};
use crate::frontmatter::parser::{parse_document, yaml_mapping_to_json};
use crate::links::parser::{count_leading_dotdot, normalize_link_path};
use crate::errors::*;
use crate::types::schema::FieldDef;
use crate::Collection;

impl Collection {
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
    pub(crate) fn resolve_relative_link(&self, target: &str, source_dir: &str) -> Option<String> {
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
    pub(crate) fn resolve_simple_name(&self, name: &str, source_dir: &str, target_type: Option<&str>) -> Option<String> {
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
    pub(crate) fn get_field_target_type(&self, source_path: &str, field_name: &str) -> Option<String> {
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

    /// Resolve a link target string to a file path.
    pub(crate) fn resolve_link_target(&self, target: &str, source_path: &str, known_paths: &[&str]) -> Option<String> {
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

    /// Check link fields with validate_exists: true.
    /// Verifies that wiki-link targets actually exist in the collection.
    pub(crate) fn check_link_exists(&self, frontmatter: &serde_json::Value, type_names: &[String], path: &str) -> Vec<Issue> {
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
    pub(crate) fn validate_single_link(
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
    pub(crate) fn resolve_link_matches(&self, normalized: &str, original: &str) -> Vec<String> {
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
    pub(crate) fn get_file_types(&self, rel_path: &str) -> Vec<String> {
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
}
