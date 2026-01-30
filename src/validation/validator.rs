//! Validation orchestrator (§9).

use crate::errors::*;
use crate::types::schema::*;
use super::fields::validate_field;

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
    validate_frontmatter_full_multi(frontmatter, type_def, path, config_strict, explicit_type_keys, None)
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
    let strict = type_def.strict.as_ref()
        .or(config_strict)
        .unwrap_or(&default);
    if *strict != StrictMode::Off {
        // Build implicit keys from explicit_type_keys config
        let default_keys: Vec<String> = vec!["type".to_string(), "types".to_string()];
        let implicit_keys: &[String] = explicit_type_keys.unwrap_or(&default_keys);

        for key in obj.keys() {
            // In multi-type mode, a field known in any type is not unknown
            let in_union = union_fields.map_or(false, |uf| uf.contains(key));
            if !type_def.fields.contains_key(key) && !implicit_keys.iter().any(|k| k == key) && !in_union {
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
