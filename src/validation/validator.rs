//! Validation orchestrator (§9).

use super::fields::validate_field;
use crate::errors::*;
use crate::types::schema::*;

/// Validate frontmatter against a type definition.
pub fn validate_frontmatter(
    frontmatter: &serde_json::Value,
    type_def: &TypeDef,
    path: &str,
) -> ValidationResult {
    validate_frontmatter_with_config_strict(frontmatter, type_def, path, None)
}

/// Validate frontmatter against a type definition with config default_strict.
pub fn validate_frontmatter_with_config_strict(
    frontmatter: &serde_json::Value,
    type_def: &TypeDef,
    path: &str,
    config_strict: Option<&StrictMode>,
) -> ValidationResult {
    validate_frontmatter_full(frontmatter, type_def, path, config_strict, None)
}

/// Validate frontmatter against a type definition with all options.
pub fn validate_frontmatter_full(
    frontmatter: &serde_json::Value,
    type_def: &TypeDef,
    path: &str,
    config_strict: Option<&StrictMode>,
    explicit_type_keys: Option<&[String]>,
) -> ValidationResult {
    validate_frontmatter_full_multi(
        frontmatter,
        type_def,
        path,
        config_strict,
        explicit_type_keys,
        None,
    )
}

/// Validate frontmatter against a type definition with multi-type union support.
/// `union_fields` provides additional field names from other types that should be
/// considered known for strict mode checks.
pub fn validate_frontmatter_full_multi(
    frontmatter: &serde_json::Value,
    type_def: &TypeDef,
    path: &str,
    config_strict: Option<&StrictMode>,
    explicit_type_keys: Option<&[String]>,
    union_fields: Option<&std::collections::HashSet<String>>,
) -> ValidationResult {
    if let Some(schema) = &type_def.json_schema {
        let issues =
            crate::v03::validate_schema_instance(schema, frontmatter, path, Some(&type_def.name))
                .into_iter()
                .map(|diagnostic| Issue {
                    code: diagnostic.code,
                    message: diagnostic.message,
                    path: diagnostic.path,
                    field: diagnostic.field,
                    severity: match diagnostic.severity.as_str() {
                        "warning" => Severity::Warning,
                        "info" => Severity::Info,
                        _ => Severity::Error,
                    },
                    expected: None,
                    actual: diagnostic.details,
                    type_name: diagnostic.type_name,
                    line: None,
                    column: None,
                })
                .collect::<Vec<_>>();
        return ValidationResult {
            valid: !issues.iter().any(|issue| issue.severity == Severity::Error),
            issues,
        };
    }
    let mut issues = Vec::new();
    let obj = match frontmatter.as_object() {
        Some(o) => o,
        None => {
            return ValidationResult {
                valid: false,
                issues: vec![Issue {
                    code: INVALID_FRONTMATTER.to_string(),
                    message: "Frontmatter must be an object".to_string(),
                    path: Some(path.to_string()),
                    field: None,
                    severity: Severity::Error,
                    expected: None,
                    actual: None,
                    type_name: Some(type_def.name.clone()),
                    line: None,
                    column: None,
                }],
            };
        }
    };

    // Check each defined field
    for (field_name, field_def) in &type_def.fields {
        let value = obj.get(field_name).unwrap_or(&serde_json::Value::Null);

        // For missing required fields (key not present at all)
        if !obj.contains_key(field_name) && field_def.required && field_def.default.is_none() {
            issues.push(Issue {
                code: MISSING_REQUIRED.to_string(),
                message: format!("Required field '{}' is missing", field_name),
                path: Some(path.to_string()),
                field: Some(field_name.clone()),
                severity: Severity::Error,
                expected: None,
                actual: None,
                type_name: Some(type_def.name.clone()),
                line: None,
                column: None,
            });
            continue;
        }

        // Skip validation for missing non-required fields with no value
        if !obj.contains_key(field_name) {
            continue;
        }

        let field_issues = validate_field(field_name, value, field_def, path, &type_def.name);
        issues.extend(field_issues);
    }

    // Check for unknown fields (strict mode)
    let default = StrictMode::Off;
    let strict = type_def
        .strict
        .as_ref()
        .or(config_strict)
        .unwrap_or(&default);
    if *strict != StrictMode::Off {
        // Build implicit keys from explicit_type_keys config
        let default_keys: Vec<String> = vec!["type".to_string(), "types".to_string()];
        let implicit_keys: &[String] = explicit_type_keys.unwrap_or(&default_keys);

        for key in obj.keys() {
            // In multi-type mode, a field known in any type is not unknown
            let in_union = union_fields.is_some_and(|uf| uf.contains(key));
            if !type_def.fields.contains_key(key)
                && !implicit_keys.iter().any(|k| k == key)
                && !in_union
            {
                let severity = if *strict == StrictMode::Error {
                    Severity::Error
                } else {
                    Severity::Warning
                };
                issues.push(Issue {
                    code: UNKNOWN_FIELD.to_string(),
                    message: format!("Unknown field '{}' in type '{}'", key, type_def.name),
                    path: Some(path.to_string()),
                    field: Some(key.clone()),
                    severity,
                    expected: None,
                    actual: None,
                    type_name: Some(type_def.name.clone()),
                    line: None,
                    column: None,
                });
            }
        }
    }

    let has_errors = issues.iter().any(|i| i.severity == Severity::Error);
    ValidationResult {
        valid: !has_errors,
        issues,
    }
}

// --- impl Collection methods for validation ---

use crate::generated::derive_path;
use crate::validation::merge::detect_type_conflicts;
use crate::Collection;
use std::collections::{HashMap, HashSet};

fn invalid_record_issue(path: &str, reason: &str) -> Issue {
    Issue {
        code: INVALID_FRONTMATTER.to_string(),
        message: format!("Invalid frontmatter: {reason}"),
        path: Some(path.to_string()),
        field: None,
        severity: Severity::Error,
        expected: None,
        actual: Some(serde_json::json!({"reason": reason})),
        type_name: None,
        line: None,
        column: None,
    }
}

fn invalid_record_validation(path: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({
        "valid": false,
        "path": path,
        "issues": [crate::errors::issue_to_json(&invalid_record_issue(path, reason))],
    })
}

fn file_read_validation(path: &str) -> serde_json::Value {
    let issue = Issue {
        code: "file_read_failed".to_string(),
        message: "Collection record could not be read".to_string(),
        path: Some(path.to_string()),
        field: None,
        severity: Severity::Error,
        expected: None,
        actual: None,
        type_name: None,
        line: None,
        column: None,
    };
    serde_json::json!({
        "valid": false,
        "path": path,
        "issues": [crate::errors::issue_to_json(&issue)],
    })
}

impl Collection {
    /// Validate frontmatter against matched types.
    pub(crate) fn validate(
        &self,
        frontmatter: &serde_json::Value,
        type_names: &[String],
        path: &str,
    ) -> ValidationResult {
        let mut all_issues = Vec::new();
        let config_strict = self.config_strict_mode();

        // Detect multi-type conflicts
        if type_names.len() > 1 {
            let type_defs: Vec<&crate::types::schema::TypeDef> = type_names
                .iter()
                .filter_map(|tn| self.types.get(tn))
                .collect();
            let conflict_issues = detect_type_conflicts(&type_defs, path);
            all_issues.extend(conflict_issues);
        }

        // Build union of all field names for multi-type strict mode
        let union_fields: std::collections::HashSet<String> = if type_names.len() > 1 {
            type_names
                .iter()
                .filter_map(|tn| self.types.get(tn))
                .flat_map(|td| td.fields.keys().cloned())
                .collect()
        } else {
            std::collections::HashSet::new()
        };
        let union_ref = if type_names.len() > 1 {
            Some(&union_fields)
        } else {
            None
        };

        for type_name in type_names {
            if let Some(type_def) = self.types.get(type_name) {
                let result = validate_frontmatter_full_multi(
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
        let effective = self.apply_defaults(frontmatter, type_names);
        let effective = self.coerce_types(&effective, type_names);
        all_issues.extend(self.data_contract_issues(type_names, &effective, path));

        let has_errors = all_issues.iter().any(|i| i.severity == Severity::Error);
        ValidationResult {
            valid: !has_errors,
            issues: all_issues,
        }
    }

    /// Check cross-file uniqueness for a file being created or updated.
    /// Returns issues for duplicate id_field and unique field values.
    /// `exclude_path` is the relative path of the file being updated (to exclude self from checks).
    pub(crate) fn check_uniqueness(
        &self,
        frontmatter: &serde_json::Value,
        type_names: &[String],
        exclude_path: &str,
        snapshot: &crate::snapshot::AuthoritativeCollectionSnapshot,
    ) -> Vec<Issue> {
        let corpus = snapshot
            .entries()
            .iter()
            .filter_map(|entry| {
                entry
                    .effective_frontmatter()
                    .map(|frontmatter| (entry.relative_path().to_string(), frontmatter.clone()))
            })
            .collect::<Vec<_>>();
        self.check_uniqueness_in_corpus(frontmatter, type_names, exclude_path, &corpus)
    }

    pub(crate) fn check_uniqueness_in_corpus(
        &self,
        frontmatter: &serde_json::Value,
        type_names: &[String],
        exclude_path: &str,
        corpus: &[(String, serde_json::Value)],
    ) -> Vec<Issue> {
        let mut issues = Vec::new();
        let exclude_normalized = exclude_path.replace('\\', "/");

        for type_name in type_names {
            let type_def = match self.types.get(type_name) {
                Some(td) => td,
                None => continue,
            };

            let unique_checks: Vec<(String, String)> = unique_field_references(type_def)
                .into_iter()
                .filter_map(|field_reference| {
                    crate::field_references::get_value(frontmatter, &field_reference).and_then(
                        |val| {
                            if val.is_null() {
                                return None;
                            }
                            let val_str = match val.as_str() {
                                Some(s) => s.to_string(),
                                None => val.to_string(),
                            };
                            Some((field_reference, val_str))
                        },
                    )
                })
                .collect();

            // Check id_field
            let id_field = &self.settings.id_field;
            let id_value = frontmatter.get(id_field).and_then(|v| {
                if v.is_null() {
                    None
                } else {
                    Some(match v.as_str() {
                        Some(s) => s.to_string(),
                        None => v.to_string(),
                    })
                }
            });

            // Check against all other files using the preloaded frontmatter snapshot.
            for (rel_path, other_fm) in corpus {
                if rel_path == &exclude_normalized {
                    continue;
                }

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
                                    message: format!(
                                        "Duplicate {} value '{}' (also in {})",
                                        id_field, our_id, rel_path
                                    ),
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
                    if let Some(other_val) =
                        crate::field_references::get_value(other_fm, field_name)
                    {
                        if !other_val.is_null() {
                            let other_str = match other_val.as_str() {
                                Some(s) => s.to_string(),
                                None => other_val.to_string(),
                            };
                            if &other_str == our_val {
                                issues.push(Issue {
                                    code: "duplicate_value".to_string(),
                                    message: format!(
                                        "Duplicate unique value '{}' for field '{}' (also in {})",
                                        our_val, field_name, rel_path
                                    ),
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

    /// Check uniqueness against the coordinated runtime index. The runtime
    /// binds this index to its readable generation before preparing a mutation,
    /// so validation never walks or reopens unrelated collection records.
    pub(crate) fn check_uniqueness_indexed(
        &self,
        frontmatter: &serde_json::Value,
        type_names: &[String],
        exclude_path: &str,
    ) -> Result<Vec<Issue>, crate::cache::CacheError> {
        let conflicts = crate::cache::runtime::uniqueness_conflicts(
            self,
            frontmatter,
            type_names,
            &exclude_path.replace('\\', "/"),
        )?;
        Ok(conflicts
            .into_iter()
            .map(|conflict| match conflict.kind {
                crate::cache::runtime::UniqueConflictKind::Identity => Issue {
                    code: "duplicate_id".to_string(),
                    message: format!(
                        "Duplicate {} value '{}' (also in {})",
                        self.settings.id_field, conflict.value, conflict.path
                    ),
                    path: Some(exclude_path.to_string()),
                    field: Some(self.settings.id_field.clone()),
                    severity: Severity::Error,
                    expected: None,
                    actual: None,
                    type_name: type_names.first().cloned(),
                    line: None,
                    column: None,
                },
                crate::cache::runtime::UniqueConflictKind::Field {
                    type_name,
                    field_name,
                } => Issue {
                    code: "duplicate_value".to_string(),
                    message: format!(
                        "Duplicate unique value '{}' for field '{}' (also in {})",
                        conflict.value, field_name, conflict.path
                    ),
                    path: Some(exclude_path.to_string()),
                    field: Some(field_name),
                    severity: Severity::Error,
                    expected: None,
                    actual: None,
                    type_name: Some(type_name),
                    line: None,
                    column: None,
                },
            })
            .collect())
    }

    /// Validate files (§9).
    pub fn validate_op(&self, input: &serde_json::Value) -> serde_json::Value {
        let path = input.get("path").and_then(|v| v.as_str());
        let _type_filter = input.get("type").and_then(|v| v.as_str());
        let collection_only = input
            .get("collection_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // collection_only mode: just check that collection is valid
        if collection_only {
            return serde_json::json!({"valid": true, "issues": []});
        }

        if let Some(path) = path {
            if let Err(error) =
                crate::operations::ensure_safe_relative_path(path, self.spec_profile)
            {
                return error;
            }
            if let Err(error) =
                crate::operations::ensure_no_symlink_components(&self.root, path, self.spec_profile)
            {
                return error;
            }
            if input.get("frontmatter").is_none() {
                let root = match self.root_capability() {
                    Ok(root) => root,
                    Err(error) => {
                        return crate::errors::op_error(
                            "collection_snapshot_failed",
                            &error.to_string(),
                        )
                    }
                };
                match root.symlink_metadata(path) {
                    Ok(metadata) if !metadata.is_file() => return file_read_validation(path),
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return crate::errors::op_error(
                            FILE_NOT_FOUND,
                            &format!("File not found: {path}"),
                        )
                    }
                    Err(_) => return file_read_validation(path),
                }
            }
        }

        let collection_snapshot = match self.capture_collection_snapshot_current() {
            Ok(snapshot) => snapshot,
            Err(error)
                if path
                    .is_some_and(|target| error.is_record_load_failure_for(&self.root, target)) =>
            {
                return file_read_validation(path.expect("matched targeted path"))
            }
            Err(error) => {
                return crate::errors::op_error("collection_snapshot_failed", &error.to_string())
            }
        };
        let resolution_index = collection_snapshot.link_resolution_index(self);

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
                match collection_snapshot.entry(path) {
                    Some(entry) => match entry.outcome() {
                        crate::record_load::RecordLoadOutcome::Parsed {
                            raw_frontmatter, ..
                        } => raw_frontmatter.clone(),
                        crate::record_load::RecordLoadOutcome::Invalid { state, .. } => {
                            return invalid_record_validation(path, state.reason().as_str());
                        }
                    },
                    None => {
                        return crate::errors::op_error(
                            FILE_NOT_FOUND,
                            &format!("File not found: {}", path),
                        )
                    }
                }
            };

            let type_names = self.determine_types_for_path(&raw_frontmatter, Some(path));
            let effective = self.apply_defaults(&raw_frontmatter, &type_names);
            let effective = self.coerce_types(&effective, &type_names);
            let validation_frontmatter = if self.spec_profile == crate::SpecProfile::V03 {
                &raw_frontmatter
            } else {
                &effective
            };

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
                let type_defs: Vec<&crate::types::schema::TypeDef> = type_names
                    .iter()
                    .filter_map(|tn| self.types.get(tn))
                    .collect();
                let conflict_issues = detect_type_conflicts(&type_defs, path);
                all_issues.extend(conflict_issues);
            }

            // Validate against types
            // Build union of all field names across all types for multi-type strict mode
            let config_strict = self.config_strict_mode();
            let union_fields: std::collections::HashSet<String> = if type_names.len() > 1 {
                type_names
                    .iter()
                    .filter_map(|tn| self.types.get(tn))
                    .flat_map(|td| td.fields.keys().cloned())
                    .collect()
            } else {
                std::collections::HashSet::new()
            };
            let union_ref = if type_names.len() > 1 {
                Some(&union_fields)
            } else {
                None
            };

            for type_name in &type_names {
                if let Some(type_def) = self.types.get(type_name) {
                    let result = validate_frontmatter_full_multi(
                        validation_frontmatter,
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
            all_issues.extend(self.data_contract_issues(&type_names, &effective, path));

            // Cross-file uniqueness checking
            let uniqueness_issues =
                self.check_uniqueness(&effective, &type_names, path, &collection_snapshot);
            all_issues.extend(uniqueness_issues);

            // Link validate_exists checking
            let link_issues =
                self.check_link_exists(&effective, &type_names, path, &resolution_index);
            all_issues.extend(link_issues);

            let has_errors = all_issues.iter().any(|i| i.severity == Severity::Error);
            let issues_json: Vec<serde_json::Value> = all_issues
                .iter()
                .map(crate::errors::issue_to_json)
                .collect();

            return serde_json::json!({
                "valid": !has_errors,
                "path": path,
                "types": type_names,
                "issues": issues_json,
            });
        }

        // Validate all files from the operation's authoritative capture.
        let mut all_issues = Vec::new();

        // Track unique values per (type, field) and id values per type
        let mut unique_values: HashMap<(String, String), HashMap<String, Vec<String>>> =
            HashMap::new();
        let mut id_values: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();

        for entry in collection_snapshot.entries() {
            let rel_path = entry.relative_path().to_string();
            let (effective, type_names) = match entry.outcome() {
                crate::record_load::RecordLoadOutcome::Parsed {
                    effective_frontmatter,
                    type_names,
                    ..
                } => (effective_frontmatter.clone(), type_names.clone()),
                crate::record_load::RecordLoadOutcome::Invalid { state, .. } => {
                    all_issues.push(invalid_record_issue(&rel_path, state.reason().as_str()));
                    continue;
                }
            };

            // Detect multi-type conflicts
            if type_names.len() > 1 {
                let type_defs_coll: Vec<&crate::types::schema::TypeDef> = type_names
                    .iter()
                    .filter_map(|tn| self.types.get(tn))
                    .collect();
                let conflict_issues = detect_type_conflicts(&type_defs_coll, &rel_path);
                all_issues.extend(conflict_issues);
            }

            // Validate individual file
            let config_strict = self.config_strict_mode();
            let union_fields_coll: std::collections::HashSet<String> = if type_names.len() > 1 {
                type_names
                    .iter()
                    .filter_map(|tn| self.types.get(tn))
                    .flat_map(|td| td.fields.keys().cloned())
                    .collect()
            } else {
                std::collections::HashSet::new()
            };
            let union_ref_coll = if type_names.len() > 1 {
                Some(&union_fields_coll)
            } else {
                None
            };

            for tn in &type_names {
                if let Some(type_def) = self.types.get(tn) {
                    let result = validate_frontmatter_full_multi(
                        &effective,
                        type_def,
                        &rel_path,
                        Some(&config_strict),
                        Some(&self.settings.explicit_type_keys),
                        union_ref_coll,
                    );
                    all_issues.extend(result.issues);

                    // Track unique fields.
                    for field_reference in unique_field_references(type_def) {
                        if let Some(val) =
                            crate::field_references::get_value(&effective, &field_reference)
                        {
                            if val.is_null() {
                                continue;
                            }
                            let key = (tn.clone(), field_reference);
                            let val_str = match val.as_str() {
                                Some(s) => s.to_string(),
                                None => val.to_string(),
                            };
                            unique_values
                                .entry(key)
                                .or_default()
                                .entry(val_str)
                                .or_default()
                                .push(rel_path.clone());
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
                            id_values
                                .entry(tn.clone())
                                .or_default()
                                .entry(val_str)
                                .or_default()
                                .push(rel_path.clone());
                        }
                    }
                }
            }
            all_issues.extend(self.data_contract_issues(&type_names, &effective, &rel_path));
        }

        // Check for duplicate unique values
        for ((type_name, field_name), values) in &unique_values {
            for (val, paths) in values {
                if paths.len() > 1 {
                    for p in paths {
                        all_issues.push(Issue {
                            code: DUPLICATE_VALUE.to_string(),
                            message: format!(
                                "Duplicate value '{}' for unique field '{}' in type '{}'",
                                val, field_name, type_name
                            ),
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
        let issues_json: Vec<serde_json::Value> = all_issues
            .iter()
            .map(crate::errors::issue_to_json)
            .collect();

        serde_json::json!({
            "valid": !has_errors,
            "issues": issues_json,
        })
    }
}

pub(crate) fn unique_field_references(type_def: &TypeDef) -> Vec<String> {
    let mut references = type_def
        .fields
        .iter()
        .filter(|(_, field)| field.unique)
        .map(|(name, _)| name.clone())
        .collect::<HashSet<_>>();
    references.extend(
        type_def
            .v03_frontmatter
            .as_ref()
            .and_then(|value| value.pointer("/collection/unique"))
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|rule| rule.get("field"))
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string),
    );
    let mut references = references.into_iter().collect::<Vec<_>>();
    references.sort();
    references
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn targeted_links_and_uniqueness_share_one_capture() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        std::fs::write(root.path().join("target.md"), "target\n").unwrap();
        std::fs::write(
            root.path().join("source.md"),
            "---\nrelated: '[[target]]'\n---\n",
        )
        .unwrap();
        let collection = Collection::open(root.path()).unwrap();
        crate::reset_snapshot_scan_calls_for_test();
        crate::record_load::reset_snapshot_record_loads_for_test();

        let validated = collection.validate_op(&serde_json::json!({"path": "source.md"}));
        assert_eq!(validated["valid"], true, "{validated:#}");
        assert_eq!(crate::snapshot_scan_calls_for_test(), 1);
        assert_eq!(crate::record_load::snapshot_record_loads_for_test(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn targeted_disappearance_is_file_read_failed_without_a_second_read() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        std::fs::write(root.path().join("target.md"), "target\n").unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let collection = Collection::open(root.path()).unwrap();
        crate::operations::replace_record_with_symlink_on_open_for_test(
            root.path(),
            "target.md",
            outside.path(),
        );
        crate::record_load::reset_snapshot_record_loads_for_test();

        let result = collection.validate_op(&serde_json::json!({"path": "target.md"}));
        assert_eq!(result["valid"], false, "{result:#}");
        assert_eq!(result["path"], "target.md", "{result:#}");
        assert_eq!(
            result["issues"][0]["code"], "file_read_failed",
            "{result:#}"
        );
        assert_eq!(crate::record_load::snapshot_record_loads_for_test(), 1);
    }
}
