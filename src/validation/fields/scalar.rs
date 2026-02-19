use crate::errors::*;
use crate::types::schema::*;

pub(super) fn validate_string(
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
                message: format!(
                    "Field '{}' length {} is less than minimum {}",
                    field_name,
                    s.chars().count(),
                    min
                ),
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
                message: format!(
                    "Field '{}' length {} exceeds maximum {}",
                    field_name,
                    s.chars().count(),
                    max
                ),
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
                    message: format!(
                        "Field '{}' does not match pattern '{}'",
                        field_name, pattern
                    ),
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

pub(super) fn validate_boolean(
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

pub(super) fn validate_enum(
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
                        message: format!(
                            "Field '{}' expected enum value (string), got {:?}",
                            field_name, value
                        ),
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

pub(super) fn validate_date(
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

pub(super) fn validate_datetime(
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

pub(super) fn validate_time(
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
