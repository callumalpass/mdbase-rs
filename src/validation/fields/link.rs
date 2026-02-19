use crate::errors::*;
use crate::types::schema::*;

pub(super) fn validate_link(
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
                message: format!(
                    "Field '{}' must be a string (link), got {}",
                    field_name,
                    value_type_name(value)
                ),
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

        let inner = &link_str[2..link_str.len() - 2];

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
                message: format!(
                    "Field '{}' has empty wikilink target (only pipe separator)",
                    field_name
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
            return;
        }

        // Only hash (no target, just heading)
        if target == "#" || target.trim() == "#" {
            issues.push(Issue {
                code: "invalid_link".to_string(),
                message: format!(
                    "Field '{}' has wikilink with only hash (no target)",
                    field_name
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
            let target = &link_str[paren_start + 2..link_str.len() - 1];
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
