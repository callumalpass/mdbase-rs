//! Link resolution algorithm (§8.4).

pub(crate) fn allowed_target_types(field: &crate::types::schema::FieldDef) -> Vec<String> {
    if field.target_types.is_empty() {
        field.target.iter().cloned().collect()
    } else {
        field.target_types.clone()
    }
}

fn normalize_collection_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn relative_path_crosses_root(target: &str, source_dir: &str) -> bool {
    let mut depth = source_dir
        .split('/')
        .filter(|segment| !segment.is_empty())
        .count();
    for segment in target.split('/') {
        match segment {
            ".." if depth == 0 => return true,
            ".." => depth -= 1,
            "." | "" => {}
            _ => depth += 1,
        }
    }
    false
}

#[cfg(test)]
mod path_tests {
    use super::{normalize_collection_path, relative_path_crosses_root};

    #[test]
    fn collection_paths_are_platform_independent() {
        let path = normalize_collection_path(r"projects\active\note.md");

        assert_eq!(path, "projects/active/note.md");
    }

    #[test]
    fn root_crossing_is_detected_after_intermediate_segments() {
        assert!(relative_path_crosses_root("a/../../inside", ""));
        assert!(!relative_path_crosses_root("../../inside", "a/b"));
        assert!(relative_path_crosses_root("../../../inside", "a/b"));
    }
}

/// Compute a relative path from source_dir to target_path.
/// E.g., from "docs" to "archive/detail.md" -> "../archive/detail.md"
/// E.g., from "notes" to "notes/new-target.md" -> "./new-target.md"
pub(crate) fn compute_relative_path(source_dir: &str, target_path: &str) -> String {
    let src_parts: Vec<&str> = if source_dir.is_empty() {
        Vec::new()
    } else {
        source_dir.split('/').filter(|s| !s.is_empty()).collect()
    };

    let target_dir = std::path::Path::new(target_path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let target_filename = std::path::Path::new(target_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(target_path);

    let tgt_parts: Vec<&str> = if target_dir.is_empty() {
        Vec::new()
    } else {
        target_dir.split('/').filter(|s| !s.is_empty()).collect()
    };

    // Find common prefix
    let mut common = 0;
    while common < src_parts.len()
        && common < tgt_parts.len()
        && src_parts[common] == tgt_parts[common]
    {
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

use crate::errors::*;
use crate::links::parser::{count_leading_dotdot, normalize_link_path};
use crate::runtime::{
    select_resolution_candidate, RankedResolution, RankedResolutionCandidate,
    RecordResolutionKeyKind,
};
use crate::types::schema::FieldDef;
use crate::Collection;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub(crate) struct LinkResolutionIndex {
    pub known_paths: HashSet<String>,
    pub basename_lower_to_paths: HashMap<String, Vec<String>>,
    pub id_lower_to_paths: HashMap<String, Vec<String>>,
    pub title_lower_to_paths: HashMap<String, Vec<String>>,
    pub types_by_path: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkResolution {
    Missing,
    Resolved(String),
    Ambiguous(Vec<String>),
}

fn insert_resolution_key(index: &mut HashMap<String, Vec<String>>, key: String, path: &str) {
    let paths = index.entry(key).or_default();
    if !paths.iter().any(|candidate| candidate == path) {
        paths.push(path.to_string());
        paths.sort();
    }
}

fn select_local_resolution(
    source_path: &str,
    kind: RecordResolutionKeyKind,
    paths: impl IntoIterator<Item = String>,
) -> LinkResolution {
    let candidates = paths.into_iter().map(|path| RankedResolutionCandidate {
        record_id: path.clone(),
        path,
    });
    match select_resolution_candidate(source_path, kind, candidates) {
        Ok(RankedResolution::Missing) | Err(_) => LinkResolution::Missing,
        Ok(RankedResolution::Resolved { path, .. }) => LinkResolution::Resolved(path),
        Ok(RankedResolution::Ambiguous { paths }) => LinkResolution::Ambiguous(paths),
    }
}

impl Collection {
    pub(crate) fn build_link_resolution_index(
        &self,
        all_files: &[crate::expressions::evaluator::ResolvedFileData],
    ) -> LinkResolutionIndex {
        let mut index = LinkResolutionIndex::default();

        for file_data in all_files {
            let path = file_data.path.clone();
            if crate::api::CollectionPath::new(&path).is_err() {
                continue;
            }
            index.known_paths.insert(path.clone());
            index.types_by_path.insert(
                path.clone(),
                self.determine_types_for_path(&file_data.frontmatter, Some(&path)),
            );

            if let Some(basename) = std::path::Path::new(&path)
                .file_stem()
                .and_then(|s| s.to_str())
            {
                insert_resolution_key(
                    &mut index.basename_lower_to_paths,
                    basename.to_lowercase(),
                    &path,
                );
            }

            if let Some(id) = file_data
                .frontmatter
                .get(&self.settings.id_field)
                .and_then(|v| v.as_str())
            {
                insert_resolution_key(&mut index.id_lower_to_paths, id.to_lowercase(), &path);
            }

            if let Some(title) = file_data.frontmatter.get("title").and_then(|v| v.as_str()) {
                insert_resolution_key(&mut index.title_lower_to_paths, title.to_lowercase(), &path);
            }
        }

        index
    }

    /// Resolve a link field from one authoritative collection capture.
    pub fn resolve_link(&self, input: &serde_json::Value) -> serde_json::Value {
        let source_path = match input.get("path").and_then(|v| v.as_str()) {
            Some(v) => v,
            None => {
                return serde_json::json!({"error": {"code": "invalid_input", "message": "resolve_link requires 'path' field"}})
            }
        };
        let field_name = match input.get("field").and_then(|v| v.as_str()) {
            Some(v) => v,
            None => {
                return serde_json::json!({"error": {"code": "invalid_input", "message": "resolve_link requires 'field' field"}})
            }
        };
        if crate::api::CollectionPath::new(source_path).is_err() {
            return serde_json::json!({"error": {"code": "path_traversal", "message": "Link source path is unsafe"}});
        }

        let snapshot = match self.capture_collection_snapshot_current() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return crate::errors::op_error("collection_snapshot_failed", &error.to_string())
            }
        };
        let source = match snapshot.entry(source_path) {
            Some(entry) => entry,
            None => {
                return serde_json::json!({"error": {"code": "file_not_found", "message": format!("Cannot read {}", source_path)}})
            }
        };
        let fm = match source.effective_frontmatter() {
            Some(frontmatter) => frontmatter,
            None => {
                return serde_json::json!({"error": {"code": "file_not_found", "message": format!("Cannot read {}", source_path)}})
            }
        };
        let field_val = match fm.get(field_name).and_then(|value| value.as_str()) {
            Some(value) => value,
            None => return serde_json::json!({"resolved_path": serde_json::Value::Null}),
        };
        let parsed = self.parse_link(&serde_json::json!({"value": field_val}));
        let Some(target) = parsed
            .pointer("/link/target")
            .and_then(serde_json::Value::as_str)
        else {
            return serde_json::json!({"resolved_path": serde_json::Value::Null});
        };
        let is_relative = parsed
            .pointer("/link/is_relative")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let format = parsed
            .pointer("/link/format")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("wikilink");
        let source_dir = Path::new(source_path)
            .parent()
            .and_then(|path| path.to_str())
            .unwrap_or("");
        let resolves_from_source =
            !target.starts_with('/') && (is_relative || format == "markdown" || format == "path");
        if resolves_from_source && relative_path_crosses_root(target, source_dir) {
            return serde_json::json!({
                "error": {"code": "path_traversal", "message": "Link escapes collection root"},
                "issues": [{"code": "path_traversal", "field": field_name, "severity": "error"}]
            });
        }

        let target_types =
            self.get_field_target_types_from_frontmatter(source_path, field_name, fm);
        let index = snapshot.link_resolution_index(self);
        let resolved = if target.starts_with('/') {
            Some(target.trim_start_matches('/').to_string())
        } else if resolves_from_source {
            self.resolve_relative_link(target, source_dir)
        } else if target.contains('/') {
            Some(target.to_string())
        } else {
            match self.resolve_simple_name(&index, target, source_path, &target_types) {
                LinkResolution::Resolved(path) => Some(path),
                LinkResolution::Missing | LinkResolution::Ambiguous(_) => None,
            }
        }
        .map(|path| normalize_collection_path(&path));

        let Some(path) = resolved else {
            return serde_json::json!({"resolved_path": serde_json::Value::Null});
        };
        if crate::api::CollectionPath::new(&path).is_err()
            || path.starts_with("../")
            || path.contains("/../")
        {
            return serde_json::json!({
                "error": {"code": "path_traversal", "message": "Link escapes collection root"},
                "issues": [{"code": "path_traversal", "field": field_name, "severity": "error"}]
            });
        }
        let mut candidates = vec![path.clone(), format!("{path}.md")];
        candidates.extend(
            self.settings
                .extensions
                .iter()
                .map(|ext| format!("{path}.{ext}")),
        );
        if let Some(found) = candidates.into_iter().find(|candidate| {
            index.known_paths.contains(candidate)
                && !self
                    .eligible_resolution_paths(
                        &index,
                        std::slice::from_ref(candidate),
                        &target_types,
                    )
                    .is_empty()
        }) {
            return serde_json::json!({"resolved_path": found});
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
                ".." => {
                    segments.pop()?;
                }
                s if !s.is_empty() => {
                    segments.push(s);
                }
                _ => {}
            }
        }
        Some(segments.join("/"))
    }

    /// Resolve a simple name (no path separators) via id_field, then filename.
    pub(crate) fn resolve_simple_name(
        &self,
        index: &LinkResolutionIndex,
        name: &str,
        source_path: &str,
        target_types: &[String],
    ) -> LinkResolution {
        let name_lower = name.to_lowercase();
        for (kind, candidates) in [
            (
                RecordResolutionKeyKind::Id,
                index.id_lower_to_paths.get(&name_lower),
            ),
            (
                RecordResolutionKeyKind::Basename,
                index.basename_lower_to_paths.get(&name_lower),
            ),
            (
                RecordResolutionKeyKind::Title,
                index.title_lower_to_paths.get(&name_lower),
            ),
        ] {
            let Some(candidates) = candidates else {
                continue;
            };
            let eligible = self.eligible_resolution_paths(index, candidates, target_types);
            if !eligible.is_empty() {
                return select_local_resolution(source_path, kind, eligible);
            }
        }
        LinkResolution::Missing
    }

    pub(crate) fn get_field_target_types_from_frontmatter(
        &self,
        source_path: &str,
        field_name: &str,
        frontmatter: &serde_json::Value,
    ) -> Vec<String> {
        let file_types = self.determine_types_for_path(frontmatter, Some(source_path));
        for type_name in &file_types {
            if let Some(type_def) = self.types.get(&type_name.to_lowercase()) {
                if let Some(field_def) = type_def.fields.get(field_name) {
                    return allowed_target_types(field_def);
                }
            }
        }
        Vec::new()
    }

    fn eligible_resolution_paths(
        &self,
        index: &LinkResolutionIndex,
        paths: &[String],
        target_types: &[String],
    ) -> Vec<String> {
        if target_types.is_empty() {
            return paths.to_vec();
        }
        paths
            .iter()
            .filter(|path| {
                index.types_by_path.get(*path).is_some_and(|actual_types| {
                    actual_types.iter().any(|actual| {
                        target_types
                            .iter()
                            .any(|expected| actual.eq_ignore_ascii_case(expected))
                    })
                })
            })
            .cloned()
            .collect()
    }

    /// Resolve a link target string to a file path.
    pub(crate) fn resolve_link_target(
        &self,
        target: &str,
        source_path: &str,
        target_types: &[String],
        resolution_index: &LinkResolutionIndex,
    ) -> Option<String> {
        // Strip wikilink syntax
        let target = if target.starts_with("[[") && target.ends_with("]]") {
            let inner = &target[2..target.len() - 2];
            inner
                .split('|')
                .next()
                .unwrap_or(inner)
                .split('#')
                .next()
                .unwrap_or(inner)
                .trim()
        } else {
            // Strip anchor from markdown links
            target.split('#').next().unwrap_or(target).trim()
        };

        if target.is_empty() {
            return None;
        }

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
                    std::path::Component::ParentDir => {
                        components.pop()?;
                    }
                    std::path::Component::CurDir => {}
                    _ => {
                        components.push(c);
                    }
                }
            }
            let normalized: PathBuf = components.iter().collect();
            normalized.to_string_lossy().to_string().replace('\\', "/")
        } else {
            target.to_string()
        };

        // Simple-name lookup follows the same priority and ranking as hosted
        // resolution. A populated ambiguous ID class never falls through.
        let simple_name = !resolved_target.contains('/')
            && std::path::Path::new(&resolved_target).extension().is_none();
        if simple_name {
            let target_lower = resolved_target.to_lowercase();
            if let Some(paths) = resolution_index.id_lower_to_paths.get(&target_lower) {
                let eligible =
                    self.eligible_resolution_paths(resolution_index, paths, target_types);
                if !eligible.is_empty() {
                    return match select_local_resolution(
                        source_path,
                        RecordResolutionKeyKind::Id,
                        eligible,
                    ) {
                        LinkResolution::Resolved(path) => Some(path),
                        LinkResolution::Missing | LinkResolution::Ambiguous(_) => None,
                    };
                }
            }
            if let Some(paths) = resolution_index.basename_lower_to_paths.get(&target_lower) {
                let eligible =
                    self.eligible_resolution_paths(resolution_index, paths, target_types);
                if !eligible.is_empty() {
                    return match select_local_resolution(
                        source_path,
                        RecordResolutionKeyKind::Basename,
                        eligible,
                    ) {
                        LinkResolution::Resolved(path) => Some(path),
                        LinkResolution::Missing | LinkResolution::Ambiguous(_) => None,
                    };
                }
            }
            if let Some(paths) = resolution_index.title_lower_to_paths.get(&target_lower) {
                let eligible =
                    self.eligible_resolution_paths(resolution_index, paths, target_types);
                if !eligible.is_empty() {
                    return match select_local_resolution(
                        source_path,
                        RecordResolutionKeyKind::Title,
                        eligible,
                    ) {
                        LinkResolution::Resolved(path) => Some(path),
                        LinkResolution::Missing | LinkResolution::Ambiguous(_) => None,
                    };
                }
            }
            return None;
        }

        // Explicit path targets retain exact path and extension behavior.
        if resolution_index.known_paths.contains(&resolved_target) {
            return self
                .eligible_resolution_paths(
                    resolution_index,
                    std::slice::from_ref(&resolved_target),
                    target_types,
                )
                .into_iter()
                .next();
        }
        if !resolved_target.ends_with(".md") && !resolved_target.ends_with(".mdx") {
            let with_md = format!("{}.md", resolved_target);
            if resolution_index.known_paths.contains(&with_md) {
                return self
                    .eligible_resolution_paths(
                        resolution_index,
                        std::slice::from_ref(&with_md),
                        target_types,
                    )
                    .into_iter()
                    .next();
            }
        }

        None
    }

    /// Check link fields with validate_exists: true.
    /// Verifies that wiki-link targets actually exist in the collection.
    pub(crate) fn check_link_exists(
        &self,
        frontmatter: &serde_json::Value,
        type_names: &[String],
        path: &str,
        resolution_index: &LinkResolutionIndex,
    ) -> Vec<Issue> {
        let mut issues = Vec::new();

        for (field_name, field, type_name, link) in
            self.validation_link_checks(frontmatter, type_names)
        {
            issues.extend(self.validate_single_link(
                &link,
                &field_name,
                &field,
                &type_name,
                path,
                resolution_index,
            ));
        }

        issues
    }

    pub(crate) fn validation_resolution_targets(
        &self,
        frontmatter: &serde_json::Value,
        type_names: &[String],
        path: &str,
    ) -> Vec<String> {
        let source_dir = std::path::Path::new(path)
            .parent()
            .unwrap_or(std::path::Path::new(""))
            .to_string_lossy()
            .to_string();
        let mut targets = self
            .validation_link_checks(frontmatter, type_names)
            .into_iter()
            .filter(|(_, field, _, _)| {
                field.validate_exists == Some(true) || !allowed_target_types(field).is_empty()
            })
            .filter_map(|(_, _, _, link)| {
                self.parse_link(&serde_json::json!({"value": link}))
                    .pointer("/link/target")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|target| !target.is_empty())
                    .map(|target| normalize_link_path(target, &source_dir))
            })
            .collect::<Vec<_>>();
        targets.sort();
        targets.dedup();
        targets
    }

    fn validation_link_checks(
        &self,
        frontmatter: &serde_json::Value,
        type_names: &[String],
    ) -> Vec<(String, FieldDef, String, String)> {
        let mut checks = Vec::new();

        for type_name in type_names {
            let type_def = match self.types.get(type_name) {
                Some(td) => td,
                None => continue,
            };

            for (field_name, field_def) in &type_def.fields {
                // v0.3 collection.links annotates a JSON Schema string or array; v0.2
                // represents links directly in the field type.
                let link_field = if field_def.validate_exists.is_some()
                    || field_def.target.is_some()
                    || !field_def.target_types.is_empty()
                    || field_def.field_type == "link"
                {
                    Some(field_def)
                } else if field_def.field_type == "list" {
                    field_def.items.as_ref().and_then(|item| {
                        if item.field_type == "link" {
                            Some(item.as_ref())
                        } else {
                            None
                        }
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
                    continue;
                };

                for link_str in link_values {
                    checks.push((
                        field_name.clone(),
                        link_field.clone(),
                        type_name.clone(),
                        link_str.to_string(),
                    ));
                }
            }

            let link_rules = type_def
                .v03_frontmatter
                .as_ref()
                .and_then(|value| value.pointer("/collection/links"))
                .and_then(serde_json::Value::as_object);
            for (field_reference, rule) in link_rules.into_iter().flatten() {
                if type_def.fields.iter().any(|(field_name, field)| {
                    crate::field_references::targets_top_level(field_reference, field_name)
                        && (field.validate_exists.is_some()
                            || field.target.is_some()
                            || !field.target_types.is_empty())
                }) {
                    continue;
                }

                let mut link_field = FieldDef {
                    field_type: "link".to_string(),
                    validate_exists: rule
                        .get("validate_exists")
                        .and_then(serde_json::Value::as_bool),
                    ..FieldDef::default()
                };
                match rule.get("target_type") {
                    Some(serde_json::Value::String(target)) if target != "any" => {
                        link_field.target = Some(target.clone());
                        link_field.target_types = vec![target.clone()];
                    }
                    Some(serde_json::Value::Array(targets)) => {
                        link_field.target_types = targets
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_string)
                            .collect();
                    }
                    _ => {}
                }

                for selected in crate::field_references::get_values(frontmatter, field_reference) {
                    let values: Vec<&serde_json::Value> = if field_reference.starts_with('/') {
                        selected
                            .as_array()
                            .map(|array| array.iter().collect())
                            .unwrap_or_else(|| vec![selected])
                    } else {
                        vec![selected]
                    };
                    for value in values {
                        let Some(link_str) = value.as_str() else {
                            continue;
                        };
                        checks.push((
                            field_reference.clone(),
                            link_field.clone(),
                            type_name.clone(),
                            link_str.to_string(),
                        ));
                    }
                }
            }
        }

        checks
    }

    /// Validate a single link value.
    pub(crate) fn validate_single_link(
        &self,
        link_str: &str,
        field_name: &str,
        field_def: &FieldDef,
        type_name: &str,
        path: &str,
        resolution_index: &LinkResolutionIndex,
    ) -> Vec<Issue> {
        let mut issues = Vec::new();

        // Keep validation aligned with the public parser so every supported
        // representation resolves the same target. In particular, anchors are
        // not part of a filename and Markdown links resolve their URL rather
        // than their complete display syntax.
        let parsed = self.parse_link(&serde_json::json!({"value": link_str}));
        let target = parsed
            .get("link")
            .and_then(|link| link.get("target"))
            .and_then(|target| target.as_str())
            .unwrap_or("")
            .trim();

        if target.is_empty() {
            return issues;
        }

        // Normalize path (resolve ./ and ../)
        let source_dir = std::path::Path::new(path)
            .parent()
            .unwrap_or(std::path::Path::new(""));
        let source_dir_str = source_dir.to_string_lossy();
        let normalized = normalize_link_path(target, &source_dir_str);

        // Check for path traversal (escaping collection root)
        // Per spec §8.13: count leading ../ segments in the raw target.
        // If the target has >= 2 leading ../ segments AND those segments would reach or
        // exceed the collection root boundary, flag as path_traversal.
        let leading_dotdot_count = count_leading_dotdot(target);
        let source_depth = if source_dir_str.is_empty() {
            0
        } else {
            source_dir_str.split('/').filter(|s| !s.is_empty()).count()
        };
        let reaches_root = leading_dotdot_count >= source_depth && leading_dotdot_count >= 2;
        if reaches_root
            || normalized.starts_with("../")
            || normalized.starts_with("..\\")
            || normalized == ".."
        {
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
            let expected = allowed_target_types(field_def);
            let matches =
                self.resolve_link_matches(resolution_index, &normalized, target, path, &expected);

            if matches.is_empty() {
                let unfiltered = if expected.is_empty() {
                    Vec::new()
                } else {
                    self.resolve_link_matches(resolution_index, &normalized, target, path, &[])
                };
                if unfiltered.len() == 1 {
                    let target_types = resolution_index
                        .types_by_path
                        .get(&unfiltered[0])
                        .cloned()
                        .unwrap_or_default();
                    issues.push(Issue {
                        code: "link_wrong_type".to_string(),
                        message: format!(
                            "Link target '{}' is type {:?}, expected one of {:?}",
                            target, target_types, expected
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
                } else {
                    issues.push(Issue {
                        code: "link_not_found".to_string(),
                        message: format!(
                            "Link target '{}' not found for field '{}'",
                            target, field_name
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
                    message: format!(
                        "Link '{}' matches multiple files for field '{}'",
                        target, field_name
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
            } else if !allowed_target_types(field_def).is_empty() {
                // Check target type constraint
                let matched_path = &matches[0];
                let target_types = resolution_index
                    .types_by_path
                    .get(matched_path)
                    .cloned()
                    .unwrap_or_default();
                let expected = allowed_target_types(field_def);
                if !target_types.iter().any(|actual| {
                    expected
                        .iter()
                        .any(|target| actual.eq_ignore_ascii_case(target))
                }) {
                    issues.push(Issue {
                        code: "link_wrong_type".to_string(),
                        message: format!(
                            "Link target '{}' is type {:?}, expected one of {:?}",
                            target, target_types, expected
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
        } else if !allowed_target_types(field_def).is_empty() {
            // Even without validate_exists, check target type if we can resolve
            let expected = allowed_target_types(field_def);
            let matches =
                self.resolve_link_matches(resolution_index, &normalized, target, path, &expected);
            if matches.len() == 1 {
                let matched_path = &matches[0];
                let target_types = resolution_index
                    .types_by_path
                    .get(matched_path)
                    .cloned()
                    .unwrap_or_default();
                let expected = allowed_target_types(field_def);
                if !target_types.iter().any(|actual| {
                    expected
                        .iter()
                        .any(|target| actual.eq_ignore_ascii_case(target))
                }) {
                    issues.push(Issue {
                        code: "link_wrong_type".to_string(),
                        message: format!(
                            "Link target '{}' is type {:?}, expected one of {:?}",
                            target, target_types, expected
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
                    message: format!(
                        "Link '{}' matches multiple files for field '{}'",
                        target, field_name
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

        issues
    }

    /// Resolve a link target to matching file paths.
    pub(crate) fn resolve_link_matches(
        &self,
        resolution_index: &LinkResolutionIndex,
        normalized: &str,
        original: &str,
        source_path: &str,
        target_types: &[String],
    ) -> Vec<String> {
        let simple_name =
            !original.contains('/') && std::path::Path::new(original).extension().is_none();
        if simple_name {
            return match self.resolve_simple_name(
                resolution_index,
                original,
                source_path,
                target_types,
            ) {
                LinkResolution::Missing => Vec::new(),
                LinkResolution::Resolved(path) => vec![path],
                LinkResolution::Ambiguous(paths) => paths,
            };
        }

        let candidates = if !normalized.ends_with(".md") && !normalized.ends_with(".mdx") {
            vec![normalized.to_string(), format!("{normalized}.md")]
        } else {
            vec![normalized.to_string()]
        };
        self.eligible_resolution_paths(
            resolution_index,
            &candidates
                .into_iter()
                .filter(|path| resolution_index.known_paths.contains(path))
                .collect::<Vec<_>>(),
            target_types,
        )
    }
}

#[cfg(test)]
mod snapshot_resolution_tests {
    use super::*;

    #[test]
    fn public_resolve_scans_once_and_loads_each_discovered_record_once() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        std::fs::write(
            root.path().join("source.md"),
            "---\nref: '[[target]]'\n---\n",
        )
        .unwrap();
        std::fs::write(root.path().join("target.md"), "---\ntitle: Target\n---\n").unwrap();
        std::fs::write(root.path().join("malformed.md"), "---\na: [broken\n---\n").unwrap();
        for index in 0..64 {
            std::fs::write(root.path().join(format!("candidate-{index}.md")), "body\n").unwrap();
        }
        let collection = Collection::open(root.path()).unwrap();
        crate::reset_snapshot_scan_calls_for_test();
        crate::record_load::reset_snapshot_record_loads_for_test();

        assert_eq!(
            collection.resolve_link(&serde_json::json!({"path": "source.md", "field": "ref"})),
            serde_json::json!({"resolved_path": "target.md"})
        );
        assert_eq!(crate::snapshot_scan_calls_for_test(), 1);
        assert_eq!(crate::record_load::snapshot_record_loads_for_test(), 67);

        crate::cancel_scan_after_entries_for_test(Some(1));
        let failed =
            collection.resolve_link(&serde_json::json!({"path": "source.md", "field": "ref"}));
        assert_eq!(failed["error"]["code"], "collection_snapshot_failed");
    }
}
