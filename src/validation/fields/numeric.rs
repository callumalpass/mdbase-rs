use crate::errors::*;
use crate::types::schema::*;

pub(super) fn validate_integer(
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

pub(super) fn validate_number(
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
                        message: format!(
                            "Field '{}' is NaN, which violates constraints",
                            field_name
                        ),
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
                message: format!(
                    "Field '{}' is NaN/null, which violates constraints",
                    field_name
                ),
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
                message: format!(
                    "Field '{}' value {} is less than minimum {}",
                    field_name, n, min
                ),
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
