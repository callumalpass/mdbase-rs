//! Per-field validation (§7).

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
        "string" => validate_string(field_name, value, field_def, path, type_name, &mut issues),
        "integer" => validate_integer(field_name, value, field_def, path, type_name, &mut issues),
        "number" => validate_number(field_name, value, field_def, path, type_name, &mut issues),
        "boolean" => validate_boolean(field_name, value, path, type_name, &mut issues),
        "enum" => validate_enum(field_name, value, field_def, path, type_name, &mut issues),
        "date" => validate_date(field_name, value, path, type_name, &mut issues),
        "datetime" => validate_datetime(field_name, value, path, type_name, &mut issues),
        "time" => validate_time(field_name, value, path, type_name, &mut issues),
        "list" => validate_list(field_name, value, field_def, path, type_name, &mut issues),
        "link" => validate_link(field_name, value, field_def, path, type_name, &mut issues),
        "object" => validate_object(field_name, value, field_def, path, type_name, &mut issues),
        "any" => {} // No validation
        _ => {}
    }

    issues
}

fn validate_string(
    field_name: &str,
    value: &serde_json::Value,
    def: &FieldDef,
    path: &str,
    type_name: &str,
    issues: &mut Vec<Issue>,
) {
    // Coerce to string if possible
    let s = match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => {
            issues.push(Issue {
                code: TYPE_MISMATCH.to_string(),
                message: format!("Field '{}' expected string, got {:?}", field_name, value),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!("string")),
                actual: Some(value.clone()),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
            return;
        }
    };

    if let Some(min) = def.min_length {
        if s.chars().count() < min {
            issues.push(Issue {
                code: STRING_TOO_SHORT.to_string(),
                message: format!("Field '{}' length {} is less than minimum {}", field_name, s.chars().count(), min),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!({"min_length": min})),
                actual: Some(serde_json::json!(s.len())),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
        }
    }

    if let Some(max) = def.max_length {
        if s.chars().count() > max {
            issues.push(Issue {
                code: STRING_TOO_LONG.to_string(),
                message: format!("Field '{}' length {} exceeds maximum {}", field_name, s.chars().count(), max),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!({"max_length": max})),
                actual: Some(serde_json::json!(s.len())),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
        }
    }

    if let Some(pattern) = &def.pattern {
        if let Ok(re) = fancy_regex::Regex::new(pattern) {
            if !re.is_match(&s).unwrap_or(false) {
                issues.push(Issue {
                    code: PATTERN_MISMATCH.to_string(),
                    message: format!("Field '{}' does not match pattern '{}'", field_name, pattern),
                    path: Some(path.to_string()),
                    field: Some(field_name.to_string()),
                    severity: Severity::Error,
                    expected: Some(serde_json::json!({"pattern": pattern})),
                    actual: Some(serde_json::json!(s)),
                    type_name: Some(type_name.to_string()),
                    line: None,
                    column: None,
                });
            }
        }
    }
}

fn validate_integer(
    field_name: &str,
    value: &serde_json::Value,
    def: &FieldDef,
    path: &str,
    type_name: &str,
    issues: &mut Vec<Issue>,
) {
    // Check if it's a number with a fractional part (not_integer)
    let float_val = match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    };
    if let Some(n) = float_val {
        if n.fract() != 0.0 && !n.is_nan() && !n.is_infinite() {
            issues.push(Issue {
                code: NOT_INTEGER.to_string(),
                message: format!("Field '{}' expected integer, got float {}", field_name, n),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!("integer")),
                actual: Some(value.clone()),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
            return;
        }
    }

    let n = match coerce_to_integer(value) {
        Some(n) => n,
        None => {
            issues.push(Issue {
                code: TYPE_MISMATCH.to_string(),
                message: format!("Field '{}' expected integer, got {:?}", field_name, value),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!("integer")),
                actual: Some(value.clone()),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
            return;
        }
    };

    check_number_bounds(field_name, n as f64, def, path, type_name, issues);
}

fn validate_number(
    field_name: &str,
    value: &serde_json::Value,
    def: &FieldDef,
    path: &str,
    type_name: &str,
    issues: &mut Vec<Issue>,
) {
    // Special handling for strings that represent special float values
    if let Some(s) = value.as_str() {
        match s.to_lowercase().as_str() {
            ".inf" | "inf" | "infinity" => {
                // Positive infinity: always exceeds max
                if def.max.is_some() {
                    issues.push(Issue {
                        code: NUMBER_TOO_LARGE.to_string(),
                        message: format!("Field '{}' value Infinity exceeds maximum", field_name),
                        path: Some(path.to_string()),
                        field: Some(field_name.to_string()),
                        severity: Severity::Error,
                        expected: None,
                        actual: Some(value.clone()),
                        type_name: Some(type_name.to_string()),
                        line: None,
                        column: None,
                    });
                }
                return;
            }
            "-.inf" | "-inf" | "-infinity" => {
                if def.min.is_some() {
                    issues.push(Issue {
                        code: NUMBER_TOO_SMALL.to_string(),
                        message: format!("Field '{}' value -Infinity below minimum", field_name),
                        path: Some(path.to_string()),
                        field: Some(field_name.to_string()),
                        severity: Severity::Error,
                        expected: None,
                        actual: Some(value.clone()),
                        type_name: Some(type_name.to_string()),
                        line: None,
                        column: None,
                    });
                }
                return;
            }
            ".nan" | "nan" => {
                if def.min.is_some() || def.max.is_some() {
                    issues.push(Issue {
                        code: CONSTRAINT_VIOLATION.to_string(),
                        message: format!("Field '{}' is NaN, which violates constraints", field_name),
                        path: Some(path.to_string()),
                        field: Some(field_name.to_string()),
                        severity: Severity::Error,
                        expected: None,
                        actual: Some(value.clone()),
                        type_name: Some(type_name.to_string()),
                        line: None,
                        column: None,
                    });
                }
                return;
            }
            _ => {}
        }
    }

    // Also handle null for NaN/Infinity (serde_yaml may convert these to null)
    if value.is_null() {
        if def.min.is_some() || def.max.is_some() {
            issues.push(Issue {
                code: CONSTRAINT_VIOLATION.to_string(),
                message: format!("Field '{}' is NaN/null, which violates constraints", field_name),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: None,
                actual: Some(value.clone()),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
        }
        return;
    }

    let n = match coerce_to_number(value) {
        Some(n) => n,
        None => {
            issues.push(Issue {
                code: TYPE_MISMATCH.to_string(),
                message: format!("Field '{}' expected number, got {:?}", field_name, value),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!("number")),
                actual: Some(value.clone()),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
            return;
        }
    };

    check_number_bounds(field_name, n, def, path, type_name, issues);
}

fn validate_boolean(
    field_name: &str,
    value: &serde_json::Value,
    path: &str,
    type_name: &str,
    issues: &mut Vec<Issue>,
) {
    match value {
        serde_json::Value::Bool(_) => {}
        serde_json::Value::String(s) => match s.to_lowercase().as_str() {
            "true" | "false" | "yes" | "no" | "on" | "off" => {}
            _ => {
                issues.push(Issue {
                    code: TYPE_MISMATCH.to_string(),
                    message: format!("Field '{}' expected boolean, got {:?}", field_name, value),
                    path: Some(path.to_string()),
                    field: Some(field_name.to_string()),
                    severity: Severity::Error,
                    expected: Some(serde_json::json!("boolean")),
                    actual: Some(value.clone()),
                    type_name: Some(type_name.to_string()),
                    line: None,
                    column: None,
                });
            }
        },
        _ => {
            issues.push(Issue {
                code: TYPE_MISMATCH.to_string(),
                message: format!("Field '{}' expected boolean, got {:?}", field_name, value),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!("boolean")),
                actual: Some(value.clone()),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
        }
    }
}

fn validate_enum(
    field_name: &str,
    value: &serde_json::Value,
    def: &FieldDef,
    path: &str,
    type_name: &str,
    issues: &mut Vec<Issue>,
) {
    let s = match value.as_str() {
        Some(s) => s.to_string(),
        None => {
            // Try coercing number/bool to string for enum check
            match value {
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                _ => {
                    issues.push(Issue {
                        code: TYPE_MISMATCH.to_string(),
                        message: format!("Field '{}' expected enum value (string), got {:?}", field_name, value),
                        path: Some(path.to_string()),
                        field: Some(field_name.to_string()),
                        severity: Severity::Error,
                        expected: Some(serde_json::json!("enum")),
                        actual: Some(value.clone()),
                        type_name: Some(type_name.to_string()),
                        line: None,
                        column: None,
                    });
                    return;
                }
            }
        }
    };

    if let Some(values) = &def.values {
        if !values.contains(&s) {
            issues.push(Issue {
                code: INVALID_ENUM.to_string(),
                message: format!(
                    "Field '{}' value '{}' is not one of: {:?}",
                    field_name, s, values
                ),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!({"values": values})),
                actual: Some(serde_json::json!(s)),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
        }
    }
}

fn validate_date(
    field_name: &str,
    value: &serde_json::Value,
    path: &str,
    type_name: &str,
    issues: &mut Vec<Issue>,
) {
    let s = match value.as_str() {
        Some(s) => s,
        None => {
            issues.push(Issue {
                code: TYPE_MISMATCH.to_string(),
                message: format!("Field '{}' expected date string", field_name),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!("date")),
                actual: Some(value.clone()),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
            return;
        }
    };

    if chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_err() {
        issues.push(Issue {
            code: INVALID_DATE.to_string(),
            message: format!("Field '{}' has invalid date: '{}'", field_name, s),
            path: Some(path.to_string()),
            field: Some(field_name.to_string()),
            severity: Severity::Error,
            expected: Some(serde_json::json!("YYYY-MM-DD")),
            actual: Some(serde_json::json!(s)),
            type_name: Some(type_name.to_string()),
            line: None,
            column: None,
        });
    }
}

fn validate_datetime(
    field_name: &str,
    value: &serde_json::Value,
    path: &str,
    type_name: &str,
    issues: &mut Vec<Issue>,
) {
    let s = match value.as_str() {
        Some(s) => s,
        None => return, // serde_yaml may give us a string already
    };

    // Try various ISO 8601 formats
    let valid = chrono::DateTime::parse_from_rfc3339(s).is_ok()
        || chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").is_ok()
        || chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").is_ok();

    if !valid {
        issues.push(Issue {
            code: INVALID_DATETIME.to_string(),
            message: format!("Field '{}' has invalid datetime: '{}'", field_name, s),
            path: Some(path.to_string()),
            field: Some(field_name.to_string()),
            severity: Severity::Error,
            expected: Some(serde_json::json!("ISO 8601 datetime")),
            actual: Some(serde_json::json!(s)),
            type_name: Some(type_name.to_string()),
            line: None,
            column: None,
        });
    }
}

fn validate_time(
    field_name: &str,
    value: &serde_json::Value,
    path: &str,
    type_name: &str,
    issues: &mut Vec<Issue>,
) {
    let s = match value.as_str() {
        Some(s) => s,
        None => {
            issues.push(Issue {
                code: TYPE_MISMATCH.to_string(),
                message: format!("Field '{}' expected time string", field_name),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!("time")),
                actual: Some(value.clone()),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
            return;
        }
    };

    // Require two-digit hours (e.g., "09:30" not "9:30")
    let valid = (chrono::NaiveTime::parse_from_str(s, "%H:%M:%S").is_ok()
        || chrono::NaiveTime::parse_from_str(s, "%H:%M").is_ok())
        && s.len() >= 5
        && s.chars().nth(2) == Some(':');

    if !valid {
        issues.push(Issue {
            code: INVALID_TIME.to_string(),
            message: format!("Field '{}' has invalid time: '{}'", field_name, s),
            path: Some(path.to_string()),
            field: Some(field_name.to_string()),
            severity: Severity::Error,
            expected: Some(serde_json::json!("HH:MM or HH:MM:SS")),
            actual: Some(serde_json::json!(s)),
            type_name: Some(type_name.to_string()),
            line: None,
            column: None,
        });
    }
}

fn validate_list(
    field_name: &str,
    value: &serde_json::Value,
    def: &FieldDef,
    path: &str,
    type_name: &str,
    issues: &mut Vec<Issue>,
) {
    let arr = match value.as_array() {
        Some(a) => a,
        None => {
            issues.push(Issue {
                code: TYPE_MISMATCH.to_string(),
                message: format!("Field '{}' expected list, got {:?}", field_name, value),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!("list")),
                actual: Some(value.clone()),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
            return;
        }
    };

    if let Some(min) = def.min_items {
        if arr.len() < min {
            issues.push(Issue {
                code: LIST_TOO_SHORT.to_string(),
                message: format!("Field '{}' has {} items, minimum is {}", field_name, arr.len(), min),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!({"min_items": min})),
                actual: Some(serde_json::json!(arr.len())),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
        }
    }

    if let Some(max) = def.max_items {
        if arr.len() > max {
            issues.push(Issue {
                code: LIST_TOO_LONG.to_string(),
                message: format!("Field '{}' has {} items, maximum is {}", field_name, arr.len(), max),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!({"max_items": max})),
                actual: Some(serde_json::json!(arr.len())),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
        }
    }

    // Item validation
    if let Some(item_def) = &def.items {
        for (i, item) in arr.iter().enumerate() {
            let item_issues = validate_field(
                &format!("{}[{}]", field_name, i),
                item,
                item_def,
                path,
                type_name,
            );
            // Wrap item issues in list_item_invalid
            for issue in item_issues {
                if issue.severity == Severity::Error {
                    issues.push(Issue {
                        code: LIST_ITEM_INVALID.to_string(),
                        message: format!("List item {}: {}", i, issue.message),
                        path: Some(path.to_string()),
                        field: Some(field_name.to_string()),
                        severity: Severity::Error,
                        expected: issue.expected,
                        actual: issue.actual,
                        type_name: Some(type_name.to_string()),
                        line: None,
                        column: None,
                    });
                } else {
                    issues.push(issue);
                }
            }
        }
    }

    // Unique check
    if def.list_unique {
        let mut seen = std::collections::HashSet::new();
        for item in arr {
            let key = item.to_string();
            if !seen.insert(key) {
                issues.push(Issue {
                    code: LIST_DUPLICATE.to_string(),
                    message: format!("Field '{}' contains duplicate item: {:?}", field_name, item),
                    path: Some(path.to_string()),
                    field: Some(field_name.to_string()),
                    severity: Severity::Error,
                    expected: None,
                    actual: Some(item.clone()),
                    type_name: Some(type_name.to_string()),
                    line: None,
                    column: None,
                });
            }
        }
    }
}

fn validate_object(
    field_name: &str,
    value: &serde_json::Value,
    def: &FieldDef,
    path: &str,
    type_name: &str,
    issues: &mut Vec<Issue>,
) {
    let obj = match value.as_object() {
        Some(o) => o,
        None => {
            issues.push(Issue {
                code: TYPE_MISMATCH.to_string(),
                message: format!("Field '{}' expected object, got {:?}", field_name, value),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!("object")),
                actual: Some(value.clone()),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
            return;
        }
    };

    // Validate nested fields if defined
    if let Some(ref nested_fields) = def.fields {
        for (nested_name, nested_def) in nested_fields {
            let nested_value = obj.get(nested_name).unwrap_or(&serde_json::Value::Null);
            let qualified_name = format!("{}.{}", field_name, nested_name);

            // Check required
            if !obj.contains_key(nested_name) && nested_def.required && nested_def.default.is_none() {
                issues.push(Issue {
                    code: MISSING_REQUIRED.to_string(),
                    message: format!("Required field '{}' is missing", qualified_name),
                    path: Some(path.to_string()),
                    field: Some(qualified_name.clone()),
                    severity: Severity::Error,
                    expected: None,
                    actual: None,
                    type_name: Some(type_name.to_string()),
                    line: None,
                    column: None,
                });
                continue;
            }

            if !obj.contains_key(nested_name) {
                continue;
            }

            let field_issues = validate_field(&qualified_name, nested_value, nested_def, path, type_name);
            issues.extend(field_issues);
        }
    }
}

fn check_number_bounds(
    field_name: &str,
    n: f64,
    def: &FieldDef,
    path: &str,
    type_name: &str,
    issues: &mut Vec<Issue>,
) {
    // Check NaN - NaN always fails constraints
    if n.is_nan() {
        if def.min.is_some() || def.max.is_some() {
            issues.push(Issue {
                code: CONSTRAINT_VIOLATION.to_string(),
                message: format!("Field '{}' is NaN, which violates constraints", field_name),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: None,
                actual: Some(serde_json::Value::Null),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
        }
        return;
    }

    if let Some(min) = def.min {
        if n < min {
            issues.push(Issue {
                code: NUMBER_TOO_SMALL.to_string(),
                message: format!("Field '{}' value {} is less than minimum {}", field_name, n, min),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!({"min": min})),
                actual: Some(serde_json::json!(n)),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
        }
    }

    if let Some(max) = def.max {
        if n > max {
            issues.push(Issue {
                code: NUMBER_TOO_LARGE.to_string(),
                message: format!("Field '{}' value {} exceeds maximum {}", field_name, n, max),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!({"max": max})),
                actual: Some(serde_json::json!(n)),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
        }
    }
}

/// Try to coerce a JSON value to an integer.
fn coerce_to_integer(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i)
            } else if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 {
                    Some(f as i64)
                } else {
                    None
                }
            } else {
                None
            }
        }
        serde_json::Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

/// Try to coerce a JSON value to a number.
fn coerce_to_number(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

/// Validate a link field value.
fn validate_link(
    field_name: &str,
    value: &serde_json::Value,
    _def: &FieldDef,
    path: &str,
    type_name: &str,
    issues: &mut Vec<Issue>,
) {
    // Link field must be a string
    let link_str = match value {
        serde_json::Value::String(s) => s,
        serde_json::Value::Null => return, // Null is ok for non-required
        _ => {
            issues.push(Issue {
                code: TYPE_MISMATCH.to_string(),
                message: format!("Field '{}' must be a string (link), got {}", field_name, value_type_name(value)),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: Some(serde_json::json!("link")),
                actual: Some(serde_json::json!(value_type_name(value))),
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
            return;
        }
    };

    // Validate wiki-link format
    if link_str.starts_with("[[") {
        // Should end with ]]
        if !link_str.ends_with("]]") {
            issues.push(Issue {
                code: "invalid_link".to_string(),
                message: format!("Field '{}' has unclosed wikilink", field_name),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: None,
                actual: None,
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
            return;
        }

        let inner = &link_str[2..link_str.len()-2];

        // Empty wikilink
        if inner.is_empty() {
            issues.push(Issue {
                code: "invalid_link".to_string(),
                message: format!("Field '{}' has empty wikilink target", field_name),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: None,
                actual: None,
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
            return;
        }

        // Get target (before | display text)
        let target = inner.split('|').next().unwrap_or(inner);

        // Whitespace-only target
        if target.trim().is_empty() {
            issues.push(Issue {
                code: "invalid_link".to_string(),
                message: format!("Field '{}' has whitespace-only wikilink target", field_name),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: None,
                actual: None,
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
            return;
        }

        // Embedded newline
        if target.contains('\n') {
            issues.push(Issue {
                code: "invalid_link".to_string(),
                message: format!("Field '{}' has embedded newline in wikilink", field_name),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: None,
                actual: None,
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
            return;
        }

        // Only pipe separator (no actual target)
        if inner == "|" || inner.starts_with('|') {
            issues.push(Issue {
                code: "invalid_link".to_string(),
                message: format!("Field '{}' has empty wikilink target (only pipe separator)", field_name),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: None,
                actual: None,
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
            return;
        }

        // Only hash (no target, just heading)
        if target == "#" || target.trim() == "#" {
            issues.push(Issue {
                code: "invalid_link".to_string(),
                message: format!("Field '{}' has wikilink with only hash (no target)", field_name),
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
    } else if link_str.starts_with('[') {
        // Markdown link format: [text](url)
        // Check for malformed markdown links
        if !link_str.contains("](") || !link_str.ends_with(')') {
            issues.push(Issue {
                code: "invalid_link".to_string(),
                message: format!("Field '{}' has malformed markdown link", field_name),
                path: Some(path.to_string()),
                field: Some(field_name.to_string()),
                severity: Severity::Error,
                expected: None,
                actual: None,
                type_name: Some(type_name.to_string()),
                line: None,
                column: None,
            });
            return;
        }

        // Extract target from markdown link
        if let Some(paren_start) = link_str.rfind("](") {
            let target = &link_str[paren_start+2..link_str.len()-1];
            if target.is_empty() {
                issues.push(Issue {
                    code: "invalid_link".to_string(),
                    message: format!("Field '{}' has markdown link with empty target", field_name),
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
    }
}

fn value_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}
