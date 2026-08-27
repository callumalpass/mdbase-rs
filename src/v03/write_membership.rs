use semver::Version;
use serde_json::{Map, Value};

use super::Diagnostic;
use crate::Collection;

#[derive(Clone, Debug)]
pub(crate) struct ResolvedWriteMembership {
    types: Vec<String>,
    designated_contract_type: Option<String>,
}

impl ResolvedWriteMembership {
    pub(crate) fn types(&self) -> &[String] {
        &self.types
    }

    pub(crate) fn resolve_create(
        collection: &Collection,
        input: &Value,
        draft: &mut Map<String, Value>,
        path: &str,
    ) -> Result<Self, Vec<Diagnostic>> {
        let mut types = classify(collection, draft, path)?;
        let requested_type = match input.get("type") {
            None => None,
            Some(Value::String(name)) if !name.is_empty() => {
                Some(known_type(collection, name, path)?)
            }
            Some(_) => {
                return Err(vec![diagnostic(
                    "invalid_type",
                    "type must be a non-empty string.",
                    path,
                    Some("type"),
                    None,
                )])
            }
        };
        if let Some(name) = &requested_type {
            if !types.contains(name) {
                types.push(name.clone());
            }
        }

        let contract_present = input.get("contract").is_some();
        let version_present = input.get("contract_version").is_some();
        if contract_present != version_present {
            return Err(vec![diagnostic(
                "invalid_contract_envelope",
                "contract and contract_version must be non-empty strings supplied together.",
                path,
                Some("contract"),
                None,
            )]);
        }
        let mut designated = None;
        if contract_present {
            let contract = non_empty_string(input.get("contract"), "contract", path)?;
            let version =
                non_empty_string(input.get("contract_version"), "contract_version", path)?;
            let parsed = Version::parse(version).map_err(|_| {
                vec![diagnostic(
                    "invalid_contract_version",
                    "contract_version must be an exact semantic version.",
                    path,
                    Some("contract_version"),
                    None,
                )]
            })?;
            if parsed.to_string() != version {
                return Err(vec![diagnostic(
                    "invalid_contract_version",
                    "contract_version must be an exact semantic version.",
                    path,
                    Some("contract_version"),
                    None,
                )]);
            }
            if !collection
                .list_data_contracts()
                .iter()
                .any(|item| item.id == contract && item.version == version)
            {
                return Err(vec![diagnostic(
                    "data_contract_not_found",
                    format!("Data contract '{contract}' {version} was not found."),
                    path,
                    Some("contract"),
                    None,
                )]);
            }
            let mut implementations = collection
                .get_data_contract_implementations(contract, version)
                .into_iter()
                .map(|item| item.type_name.to_ascii_lowercase())
                .collect::<Vec<_>>();
            implementations.sort();
            implementations.dedup();
            if implementations.is_empty() {
                return Err(vec![diagnostic(
                    "data_contract_implementation_not_found",
                    format!("Data contract '{contract}' {version} has no record implementation."),
                    path,
                    Some("contract"),
                    None,
                )]);
            }
            let selected = if let Some(name) = &requested_type {
                if !implementations.contains(name) {
                    return Err(vec![diagnostic("data_contract_type_mismatch", format!("Type '{name}' does not implement data contract '{contract}' {version}."), path, Some("type"), Some(name))]);
                }
                name.clone()
            } else if implementations.len() == 1 {
                implementations[0].clone()
            } else {
                return Err(vec![diagnostic("data_contract_implementation_ambiguous", format!("Data contract '{contract}' {version} has multiple record implementations; type is required."), path, Some("type"), None)]);
            };
            if !types.contains(&selected) {
                types.push(selected.clone());
            }
            designated = Some(selected);
        }
        canonicalize(&mut types);
        if requested_type.is_some() || designated.is_some() {
            persist(collection, draft, &types);
        }
        let reopened = classify(collection, draft, path)?;
        if reopened != types {
            return Err(vec![changed(
                "type_membership_persistence_failed",
                "Configured type membership cannot be persisted for this record.",
                path,
                &types,
                &reopened,
            )]);
        }
        Ok(Self {
            types,
            designated_contract_type: designated,
        })
    }

    pub(crate) fn resolve_update(
        collection: &Collection,
        draft: &Map<String, Value>,
        path: &str,
    ) -> Result<Self, Vec<Diagnostic>> {
        Ok(Self {
            types: classify(collection, draft, path)?,
            designated_contract_type: None,
        })
    }

    pub(crate) fn revalidate(
        &self,
        collection: &Collection,
        raw: &Map<String, Value>,
        path: &str,
    ) -> Result<(), Vec<Diagnostic>> {
        let after = classify(collection, raw, path)?;
        if after != self.types {
            return Err(vec![changed(
                "type_membership_changed",
                "Lifecycle or generated values changed the record's type membership.",
                path,
                &self.types,
                &after,
            )]);
        }
        if let Some(designated) = &self.designated_contract_type {
            if !after.contains(designated) {
                return Err(vec![diagnostic(
                    "data_contract_type_mismatch",
                    "The designated contract type is not present in final membership.",
                    path,
                    Some("type"),
                    Some(designated),
                )]);
            }
        }
        Ok(())
    }
}

fn classify(
    collection: &Collection,
    draft: &Map<String, Value>,
    path: &str,
) -> Result<Vec<String>, Vec<Diagnostic>> {
    let mut explicit = Vec::new();
    let mut errors = Vec::new();
    let mut effective = false;
    for key in &collection.settings.explicit_type_keys {
        let Some(value) = draft.get(key) else {
            continue;
        };
        match value {
            Value::String(name) if !name.is_empty() => { effective = true; match known_type(collection, name, path) { Ok(name) => explicit.push(name), Err(mut e) => errors.append(&mut e) } }
            Value::Array(values) if !values.is_empty() => for value in values {
                match value {
                    Value::String(name) if !name.is_empty() => { effective = true; match known_type(collection, name, path) { Ok(name) => explicit.push(name), Err(mut e) => errors.append(&mut e) } }
                    _ => errors.push(diagnostic("invalid_type_declaration", format!("Explicit type key '{key}' must contain only non-empty strings."), path, Some(key), None)),
                }
            },
            _ => errors.push(diagnostic("invalid_type_declaration", format!("Explicit type key '{key}' must be a non-empty string or non-empty string list."), path, Some(key), None)),
        }
    }
    if effective {
        if !errors.is_empty() {
            return Err(errors);
        }
        canonicalize(&mut explicit);
        return Ok(explicit);
    }
    let (mut types, failures) =
        collection.determine_types_for_path_checked(&Value::Object(draft.clone()), Some(path));
    let mut expression_errors = failures
        .into_iter()
        .map(|(name, failure)| {
            let mut d = diagnostic(
                "expression_evaluation_error",
                format!("Type '{name}' match expression failed: {}", failure.message),
                path,
                Some("match.expr"),
                Some(&name),
            );
            d.details = Some(serde_json::json!({"context":"match","evaluator_code":failure.code}));
            d
        })
        .collect::<Vec<_>>();
    if !expression_errors.is_empty() || !errors.is_empty() {
        expression_errors.extend(errors);
        return Err(expression_errors);
    }
    canonicalize(&mut types);
    Ok(types)
}

fn persist(collection: &Collection, draft: &mut Map<String, Value>, types: &[String]) {
    if collection.settings.explicit_type_keys.is_empty() {
        return;
    }
    // `classify` may return implicit matches; persistence specifically needs
    // names already represented by configured explicit declarations.
    let mut represented = Vec::new();
    for key in &collection.settings.explicit_type_keys {
        match draft.get(key) {
            Some(Value::String(name)) if !name.is_empty() => {
                represented.push(name.to_ascii_lowercase())
            }
            Some(Value::Array(values)) => represented.extend(
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_ascii_lowercase),
            ),
            _ => {}
        }
    }
    canonicalize(&mut represented);
    let missing = types
        .iter()
        .filter(|name| !represented.contains(name))
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return;
    }
    let key = &collection.settings.explicit_type_keys[0];
    match draft.get_mut(key) {
        None => {
            draft.insert(
                key.clone(),
                if missing.len() == 1 {
                    Value::String(missing[0].clone())
                } else {
                    Value::Array(missing.into_iter().map(Value::String).collect())
                },
            );
        }
        Some(Value::String(existing)) => {
            let mut values = vec![Value::String(existing.clone())];
            values.extend(missing.into_iter().map(Value::String));
            draft.insert(key.clone(), Value::Array(values));
        }
        Some(Value::Array(values)) => values.extend(missing.into_iter().map(Value::String)),
        Some(_) => unreachable!("explicit declarations were validated"),
    }
}

fn known_type(collection: &Collection, name: &str, path: &str) -> Result<String, Vec<Diagnostic>> {
    let canonical = name.to_ascii_lowercase();
    if name.is_empty() || !collection.types().contains_key(&canonical) {
        Err(vec![diagnostic(
            "unknown_type",
            format!("Unknown type '{name}'."),
            path,
            Some("type"),
            Some(name),
        )])
    } else {
        Ok(canonical)
    }
}
fn non_empty_string<'a>(
    value: Option<&'a Value>,
    field: &str,
    path: &str,
) -> Result<&'a str, Vec<Diagnostic>> {
    value
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            vec![diagnostic(
                "invalid_contract_envelope",
                format!("{field} must be a non-empty string."),
                path,
                Some(field),
                None,
            )]
        })
}
fn canonicalize(types: &mut Vec<String>) {
    types.sort();
    types.dedup();
}
fn diagnostic(
    code: impl Into<String>,
    message: impl Into<String>,
    path: &str,
    field: Option<&str>,
    type_name: Option<&str>,
) -> Diagnostic {
    Diagnostic {
        severity: "error".into(),
        code: code.into(),
        message: message.into(),
        path: Some(path.into()),
        field: field.map(str::to_string),
        type_name: type_name.map(str::to_string),
        schema_location: None,
        details: None,
    }
}
fn changed(
    code: &str,
    message: &str,
    path: &str,
    before: &[String],
    after: &[String],
) -> Diagnostic {
    let mut d = diagnostic(code, message, path, Some("type"), None);
    d.details = Some(serde_json::json!({"before":before,"after":after}));
    d
}

pub(crate) fn diagnostics_error(diagnostics: Vec<Diagnostic>) -> Value {
    let code = diagnostics
        .first()
        .map(|d| d.code.as_str())
        .unwrap_or("operation_failed");
    let message = diagnostics
        .first()
        .map(|d| d.message.as_str())
        .unwrap_or("Operation failed.");
    serde_json::json!({"error":{"code":code,"message":message,"issues":diagnostics}})
}
