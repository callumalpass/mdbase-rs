use serde_json::Value;

use super::operations::collect_diagnostics;
use super::Diagnostic;

pub(super) fn parse_optional_revision(
    input: &Value,
    path: Option<&str>,
) -> Result<Option<crate::api::Revision>, Vec<Diagnostic>> {
    match input.get("if_revision") {
        None => Ok(None),
        Some(Value::String(revision)) => crate::api::Revision::parse(revision.clone())
            .map(Some)
            .map_err(|error| {
                vec![Diagnostic::error(
                    "invalid_request",
                    error.to_string(),
                    path.map(str::to_string),
                )]
            }),
        Some(_) => Err(vec![Diagnostic::error(
            "invalid_request",
            "if_revision must be an opaque string token.",
            path.map(str::to_string),
        )]),
    }
}

pub(super) fn decode_create(
    input: &Value,
) -> Result<
    (
        crate::api::CreateRequest,
        crate::mutation::PreparationOptions,
    ),
    Vec<Diagnostic>,
> {
    #[cfg(test)]
    crate::mutation::probe_wire_decode();
    let path = input
        .get("path")
        .and_then(Value::as_str)
        .map(|path| {
            crate::operations::ensure_safe_relative_path(path, crate::SpecProfile::V03)
                .map_err(|error| collect_diagnostics(&error, Some(path), "error"))?;
            crate::api::CollectionPath::new(path)
                .map_err(|error| vec![Diagnostic::error("invalid_path", error.to_string(), None)])
        })
        .transpose()?;
    let if_revision = parse_optional_revision(input, input.get("path").and_then(Value::as_str))?;
    Ok((
        crate::api::CreateRequest {
            path,
            type_name: input
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string),
            contract: input
                .get("contract")
                .and_then(Value::as_str)
                .map(str::to_string),
            contract_version: input
                .get("contract_version")
                .and_then(Value::as_str)
                .map(str::to_string),
            frontmatter: input
                .get("frontmatter")
                .or_else(|| input.get("fields"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
            body: input
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            if_revision,
            include_document: input
                .get("include_document")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        crate::mutation::PreparationOptions {
            create_document: input
                .get("document")
                .and_then(Value::as_str)
                .map(str::to_string),
            dry_run: input
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
    ))
}

pub(super) fn decode_delete(
    input: &Value,
) -> Result<
    (
        crate::api::DeleteRequest,
        crate::mutation::PreparationOptions,
    ),
    Vec<Diagnostic>,
> {
    #[cfg(test)]
    crate::mutation::probe_wire_decode();
    let raw_path = input
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| vec![Diagnostic::error("invalid_path", "path is required", None)])?;
    let path = match crate::api::CollectionPath::new(raw_path) {
        Ok(path) => path,
        Err(error) => {
            crate::operations::ensure_safe_relative_path(raw_path, crate::SpecProfile::V03)
                .map_err(|error| collect_diagnostics(&error, Some(raw_path), "error"))?;
            return Err(vec![Diagnostic::error(
                "invalid_path",
                error.to_string(),
                Some(raw_path.to_string()),
            )]);
        }
    };
    let if_revision = parse_optional_revision(input, Some(raw_path))?;
    Ok((
        crate::api::DeleteRequest {
            path,
            check_backlinks: input
                .get("check_backlinks")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            if_revision,
        },
        crate::mutation::PreparationOptions {
            create_document: None,
            dry_run: input
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
    ))
}

pub(super) fn decode_update(
    input: &Value,
) -> Result<
    (
        crate::api::UpdateRequest,
        crate::mutation::PreparationOptions,
    ),
    Vec<Diagnostic>,
> {
    #[cfg(test)]
    crate::mutation::probe_wire_decode();
    let raw_path = input
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| vec![Diagnostic::error("invalid_path", "path is required", None)])?;
    crate::operations::ensure_safe_relative_path(raw_path, crate::SpecProfile::V03)
        .map_err(|error| collect_diagnostics(&error, Some(raw_path), "error"))?;
    let path = crate::api::CollectionPath::new(raw_path).map_err(|error| {
        vec![Diagnostic::error(
            "invalid_path",
            error.to_string(),
            Some(raw_path.to_string()),
        )]
    })?;
    let has_patch = input.get("patch").is_some()
        || input.get("fields").is_some()
        || input.get("frontmatter").is_some();
    let body = input
        .get("body")
        .and_then(Value::as_str)
        .map(str::to_string);
    let document = match input.get("document") {
        Some(Value::String(document)) => Some(document.clone()),
        Some(_) => {
            return Err(vec![Diagnostic::error(
                "invalid_request",
                "document must be a string when provided",
                Some(raw_path.to_string()),
            )])
        }
        None => None,
    };
    if document.is_some() && (has_patch || body.is_some()) {
        return Err(vec![Diagnostic::error(
            "invalid_request",
            "document cannot be combined with patch, fields, frontmatter, or body",
            Some(raw_path.to_string()),
        )]);
    }
    let if_revision = parse_optional_revision(input, Some(raw_path))?;
    Ok((
        crate::api::UpdateRequest {
            path,
            patch: input
                .get("patch")
                .or_else(|| input.get("fields"))
                .or_else(|| input.get("frontmatter"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
            document,
            body,
            if_revision,
            include_document: input
                .get("include_document")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        crate::mutation::PreparationOptions {
            create_document: None,
            dry_run: input
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
    ))
}
