use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::api::{
    Diagnostic, DiagnosticCode, MdbaseError, MdbaseResult, OperationOutcome, QueryRequest,
    QueryResult, ReadRequest, RecordDocument, Severity,
};
use crate::Collection;

pub(crate) fn read(
    collection: &Collection,
    request: ReadRequest,
) -> MdbaseResult<OperationOutcome<RecordDocument>> {
    let result = collection.read(&json!({
        "path": request.path,
        "include_document": request.include_document,
    }));
    decode_legacy(result, Some(request.path.as_str()))
}

pub(crate) fn query(
    collection: &Collection,
    request: QueryRequest,
) -> MdbaseResult<OperationOutcome<QueryResult>> {
    let result = collection.query(&request.to_wire());
    let diagnostics = legacy_diagnostics(&result, None);
    if result.get("error").is_some() {
        return Err(MdbaseError::Operation { diagnostics });
    }
    let records = result
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| MdbaseError::InvalidResult {
            message: "legacy query result does not contain a results array".to_string(),
        })?;
    let meta = result.get("meta").cloned().unwrap_or_else(|| json!({}));
    let total_count = meta
        .get("total_count")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| MdbaseError::InvalidResult {
            message: "legacy query result does not contain a valid total_count".to_string(),
        })?;
    let has_more = meta
        .get("has_more")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(OperationOutcome {
        value: QueryResult {
            records,
            total_count,
            has_more,
            meta,
        },
        diagnostics,
    })
}

fn decode_legacy<T: DeserializeOwned>(
    result: Value,
    path: Option<&str>,
) -> MdbaseResult<OperationOutcome<T>> {
    let diagnostics = legacy_diagnostics(&result, path);
    if result.get("error").is_some() {
        return Err(MdbaseError::Operation { diagnostics });
    }
    let value = serde_json::from_value(result).map_err(|error| MdbaseError::InvalidResult {
        message: error.to_string(),
    })?;
    Ok(OperationOutcome { value, diagnostics })
}

fn legacy_diagnostics(result: &Value, fallback_path: Option<&str>) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    if let Some(error) = result.get("error") {
        diagnostics.push(diagnostic(error, Severity::Error, fallback_path));
    }
    if let Some(warnings) = result.get("warnings").and_then(Value::as_array) {
        diagnostics.extend(
            warnings
                .iter()
                .map(|warning| diagnostic(warning, Severity::Warning, fallback_path)),
        );
    }
    if let Some(issues) = result
        .pointer("/validation/issues")
        .and_then(Value::as_array)
    {
        diagnostics.extend(issues.iter().map(|issue| {
            let severity = match issue.get("severity").and_then(Value::as_str) {
                Some("warning") => Severity::Warning,
                Some("info") => Severity::Info,
                _ => Severity::Error,
            };
            diagnostic(issue, severity, fallback_path)
        }));
    }
    diagnostics
}

fn diagnostic(value: &Value, severity: Severity, fallback_path: Option<&str>) -> Diagnostic {
    Diagnostic {
        severity,
        code: DiagnosticCode::new(
            value
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("legacy_diagnostic"),
        ),
        message: value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Legacy operation diagnostic.")
            .to_string(),
        path: value
            .get("path")
            .and_then(Value::as_str)
            .or(fallback_path)
            .map(str::to_string),
        field: value
            .get("field")
            .and_then(Value::as_str)
            .map(str::to_string),
        type_name: value
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string),
        schema_location: None,
        details: None,
    }
}
