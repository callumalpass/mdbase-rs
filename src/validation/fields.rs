//! Per-field validation (§7).

mod complex;
mod link;
mod numeric;
mod scalar;

use crate::errors::*;
use crate::types::schema::*;

/// Validate a single field value against its definition.
pub fn validate_field(
    field_name: &str,
    value: &serde_json::Value,
    field_def: &FieldDef,
    path: &str,
    type_name: &str,
) -> Vec<Issue> {
    let mut issues = Vec::new();

    // Check required (value is null means not satisfied)
    if value.is_null() {
        if field_def.required {
            issues.push(Issue {
                code: MISSING_REQUIRED.to_string(),
                message: format!("Required field '{}' is null", field_name),
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
        return issues;
    }

    // Deprecated field warning
    if field_def.deprecated.is_some() {
        issues.push(Issue {
            code: DEPRECATED_FIELD.to_string(),
            message: format!("Field '{}' is deprecated", field_name),
            path: Some(path.to_string()),
            field: Some(field_name.to_string()),
            severity: Severity::Warning,
            expected: None,
            actual: None,
            type_name: Some(type_name.to_string()),
            line: None,
            column: None,
        });
    }

    // Type-specific validation
    match field_def.field_type.as_str() {
        "string" => {
            scalar::validate_string(field_name, value, field_def, path, type_name, &mut issues)
        }
        "integer" => {
            numeric::validate_integer(field_name, value, field_def, path, type_name, &mut issues)
        }
        "number" => {
            numeric::validate_number(field_name, value, field_def, path, type_name, &mut issues)
        }
        "boolean" => scalar::validate_boolean(field_name, value, path, type_name, &mut issues),
        "enum" => scalar::validate_enum(field_name, value, field_def, path, type_name, &mut issues),
        "date" => scalar::validate_date(field_name, value, path, type_name, &mut issues),
        "datetime" => scalar::validate_datetime(field_name, value, path, type_name, &mut issues),
        "time" => scalar::validate_time(field_name, value, path, type_name, &mut issues),
        "list" => {
            complex::validate_list(field_name, value, field_def, path, type_name, &mut issues)
        }
        "link" => link::validate_link(field_name, value, field_def, path, type_name, &mut issues),
        "object" => {
            complex::validate_object(field_name, value, field_def, path, type_name, &mut issues)
        }
        "any" => {} // No validation
        _ => {}
    }

    issues
}
