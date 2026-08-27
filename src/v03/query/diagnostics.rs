use serde_json::{json, Value};

use crate::v03::{cel, Diagnostic, OperationResult};

pub(super) fn invalid_schema(mut diagnostic: Diagnostic) -> Diagnostic {
    let original_code = diagnostic.code;
    diagnostic.code = "invalid_query".to_string();
    diagnostic.path = None;
    let mut details = diagnostic
        .details
        .take()
        .and_then(|details| details.as_object().cloned())
        .unwrap_or_default();
    details.insert("schema_code".to_string(), Value::String(original_code));
    diagnostic.details = Some(Value::Object(details));
    diagnostic
}

pub(crate) fn evaluation(
    path: &str,
    field: &str,
    context: &str,
    failure: cel::CelFailure,
    type_name: Option<String>,
) -> Diagnostic {
    Diagnostic {
        severity: "warning".to_string(),
        code: "expression_evaluation_error".to_string(),
        message: failure.message,
        path: Some(path.to_string()),
        field: Some(field.to_string()),
        type_name,
        schema_location: None,
        details: Some(json!({
            "context": context,
            "evaluator_code": failure.code,
        })),
    }
}

pub(crate) fn invalid_record(path: &str, reason: &str) -> Diagnostic {
    Diagnostic {
        severity: "warning".to_string(),
        code: "invalid_frontmatter".to_string(),
        message: format!("Record could not be loaded: {reason}"),
        path: Some(path.to_string()),
        field: None,
        type_name: None,
        schema_location: None,
        details: Some(json!({"reason": reason})),
    }
}

pub(super) fn failed(diagnostics: Vec<Diagnostic>) -> OperationResult {
    OperationResult {
        valid: false,
        result: json!({}),
        diagnostics,
    }
}
