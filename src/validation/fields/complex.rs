use crate::errors::*;
use crate::types::schema::*;

pub(super) fn validate_list(
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
                message: format!(
                    "Field '{}' has {} items, minimum is {}",
                    field_name,
                    arr.len(),
                    min
                ),
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
                message: format!(
                    "Field '{}' has {} items, maximum is {}",
                    field_name,
                    arr.len(),
                    max
                ),
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
            let item_issues = super::validate_field(
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

pub(super) fn validate_object(
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
            if !obj.contains_key(nested_name) && nested_def.required && nested_def.default.is_none()
            {
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

            let field_issues =
                super::validate_field(&qualified_name, nested_value, nested_def, path, type_name);
            issues.extend(field_issues);
        }
    }
}
