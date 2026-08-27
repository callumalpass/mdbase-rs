use semver::Version;
use serde_json::{Map, Value};

use super::Diagnostic;
use crate::expressions::evaluator::EvaluationClock;
use crate::Collection;

#[derive(Clone, Debug)]
pub(crate) struct ResolvedWriteMembership {
    types: Vec<String>,
    path_type: Option<String>,
    designated_contract_type: Option<String>,
    initially_explicit: bool,
    evaluation_clock: EvaluationClock,
}

impl ResolvedWriteMembership {
    pub(crate) fn types(&self) -> &[String] {
        &self.types
    }

    pub(crate) fn path_type(&self) -> Option<&str> {
        self.path_type
            .as_deref()
            .or_else(|| self.types.first().map(String::as_str))
    }

    pub(crate) fn resolve_create(
        collection: &Collection,
        input: &Value,
        draft: &mut Map<String, Value>,
        path: &str,
    ) -> Result<Self, Vec<Diagnostic>> {
        // Presence is deliberately separate from validity. A malformed declaration
        // must never fall through to implicit matching.
        let (mut explicit, declarations_present) = explicit_membership(collection, draft, path)?;
        let requested = requested_type(collection, input, path)?;
        let designated = resolve_contract(collection, input, requested.as_deref(), path)?;
        let selected = designated.as_ref().or(requested.as_ref());
        let initially_explicit = declarations_present || selected.is_some();

        // Freeze one clock even for explicit authority. If a later stage erases the
        // declaration, the diagnostic classification must not observe a new time.
        let evaluation_clock = capture_clock(collection, path)?;
        let mut types = if declarations_present || selected.is_some() {
            if let Some(name) = selected {
                explicit.push(name.clone());
            }
            canonicalize(&mut explicit);
            explicit
        } else {
            implicit_membership(collection, draft, path, &evaluation_clock)?
        };

        if let Some(name) = selected {
            if collection.settings.explicit_type_keys.is_empty() {
                return Err(vec![persistence_failure(
                    path,
                    &types,
                    &[],
                    "No explicit type keys are configured.",
                )]);
            }
            persist_selected(collection, draft, name, path)?;
            let reopened = match classify(collection, draft, path, Some(&evaluation_clock)) {
                Ok(reopened) => reopened,
                Err(_) if collection.settings.explicit_type_keys.is_empty() => {
                    return Err(vec![persistence_failure(
                        path,
                        &types,
                        &[],
                        "No explicit type keys are configured and implicit reopen classification failed.",
                    )]);
                }
                Err(errors) => return Err(errors),
            };
            if reopened != types {
                return Err(vec![changed(
                    "type_membership_persistence_failed",
                    "Configured type membership cannot be persisted for this record.",
                    path,
                    &types,
                    &reopened,
                )]);
            }
            types = reopened;
        }
        Ok(Self {
            types,
            path_type: selected.cloned(),
            designated_contract_type: designated,
            initially_explicit,
            evaluation_clock,
        })
    }

    pub(crate) fn resolve_update(
        collection: &Collection,
        draft: &Map<String, Value>,
        path: &str,
    ) -> Result<Self, Vec<Diagnostic>> {
        let clock = capture_clock(collection, path)?;
        Ok(Self {
            types: classify(collection, draft, path, Some(&clock))?,
            path_type: None,
            designated_contract_type: None,
            initially_explicit: explicit_membership(collection, draft, path)?.1,
            evaluation_clock: clock,
        })
    }

    pub(crate) fn revalidate(
        &self,
        collection: &Collection,
        raw: &Map<String, Value>,
        path: &str,
    ) -> Result<(), Vec<Diagnostic>> {
        let (explicit_after, declarations_present) =
            match explicit_membership(collection, raw, path) {
                Ok(classified) => classified,
                Err(_) if self.initially_explicit => {
                    return Err(vec![authority_changed(path, &self.types, &[])]);
                }
                Err(errors) => return Err(errors),
            };
        let after = if declarations_present {
            explicit_after
        } else {
            implicit_membership(collection, raw, path, &self.evaluation_clock)?
        };
        if self.initially_explicit && !declarations_present {
            return Err(vec![authority_changed(path, &self.types, &after)]);
        }
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
                return Err(vec![contract_diagnostic(
                    "data_contract_type_mismatch",
                    "The designated contract type is not present in final membership.",
                    path,
                    "type",
                    None,
                    None,
                    Some(designated),
                    &[],
                )]);
            }
        }
        Ok(())
    }
}

fn requested_type(
    collection: &Collection,
    input: &Value,
    path: &str,
) -> Result<Option<String>, Vec<Diagnostic>> {
    match input.get("type") {
        None => Ok(None),
        Some(Value::String(name)) if !name.is_empty() => {
            known_type(collection, name, path, "type").map(Some)
        }
        Some(_) => Err(vec![diagnostic(
            "invalid_type",
            "type must be a non-empty string.",
            path,
            Some("type"),
            None,
        )]),
    }
}

fn resolve_contract(
    collection: &Collection,
    input: &Value,
    requested: Option<&str>,
    path: &str,
) -> Result<Option<String>, Vec<Diagnostic>> {
    let contract_value = input.get("contract");
    let version_value = input.get("contract_version");
    if contract_value.is_none() && version_value.is_none() {
        return Ok(None);
    }
    if contract_value.is_none() || version_value.is_none() {
        return Err(vec![contract_diagnostic(
            "invalid_contract_envelope",
            "contract and contract_version must be non-empty strings supplied together.",
            path,
            if contract_value.is_none() {
                "contract"
            } else {
                "contract_version"
            },
            contract_value.and_then(Value::as_str),
            version_value.and_then(Value::as_str),
            requested,
            &[],
        )]);
    }
    let contract = non_empty_string(contract_value, "contract", path)?;
    let version = non_empty_string(version_value, "contract_version", path)?;
    let parsed = Version::parse(version).map_err(|_| {
        vec![contract_diagnostic(
            "invalid_contract_version",
            "contract_version must be an exact semantic version.",
            path,
            "contract_version",
            Some(contract),
            Some(version),
            requested,
            &[],
        )]
    })?;
    if parsed.to_string() != version {
        return Err(vec![contract_diagnostic(
            "invalid_contract_version",
            "contract_version must be an exact semantic version.",
            path,
            "contract_version",
            Some(contract),
            Some(version),
            requested,
            &[],
        )]);
    }
    let definition = collection
        .list_data_contracts()
        .into_iter()
        .find(|item| item.id == contract && item.version == version)
        .ok_or_else(|| {
            vec![contract_diagnostic(
                "data_contract_not_found",
                format!("Data contract '{contract}' {version} was not found."),
                path,
                "contract",
                Some(contract),
                Some(version),
                requested,
                &[],
            )]
        })?;
    if definition.contract_type != "record" || definition.record_schema.is_none() {
        return Err(vec![contract_diagnostic(
            "data_contract_record_not_found",
            format!("Data contract '{contract}' {version} has no record contract."),
            path,
            "contract",
            Some(contract),
            Some(version),
            requested,
            &[],
        )]);
    }
    let mut eligible = collection
        .get_data_contract_implementations(contract, version)
        .into_iter()
        .map(|item| item.type_name.to_ascii_lowercase())
        .collect::<Vec<_>>();
    canonicalize(&mut eligible);
    if eligible.is_empty() {
        return Err(vec![contract_diagnostic(
            "data_contract_implementation_not_found",
            format!("Data contract '{contract}' {version} has no record implementation."),
            path,
            "contract",
            Some(contract),
            Some(version),
            requested,
            &eligible,
        )]);
    }
    let selected = match requested {
        Some(name) if eligible.iter().any(|candidate| candidate == name) => name.to_string(),
        Some(name) => return Err(vec![contract_diagnostic(
            "data_contract_type_mismatch", format!("Type '{name}' does not implement data contract '{contract}' {version}."),
            path, "type", Some(contract), Some(version), Some(name), &eligible,
        )]),
        None if eligible.len() == 1 => eligible[0].clone(),
        None => return Err(vec![contract_diagnostic(
            "data_contract_implementation_ambiguous", format!("Data contract '{contract}' {version} has multiple record implementations; type is required."),
            path, "type", Some(contract), Some(version), None, &eligible,
        )]),
    };
    Ok(Some(selected))
}

fn classify(
    collection: &Collection,
    draft: &Map<String, Value>,
    path: &str,
    clock: Option<&EvaluationClock>,
) -> Result<Vec<String>, Vec<Diagnostic>> {
    let (explicit, present) = explicit_membership(collection, draft, path)?;
    if present {
        Ok(explicit)
    } else {
        let captured;
        let clock = match clock {
            Some(clock) => clock,
            None => {
                captured = capture_clock(collection, path)?;
                &captured
            }
        };
        implicit_membership(collection, draft, path, clock)
    }
}

fn explicit_membership(
    collection: &Collection,
    draft: &Map<String, Value>,
    path: &str,
) -> Result<(Vec<String>, bool), Vec<Diagnostic>> {
    let mut explicit = Vec::new();
    let mut errors = Vec::new();
    let mut present = false;
    for key in &collection.settings.explicit_type_keys {
        let Some(value) = draft.get(key) else {
            continue;
        };
        present = true;
        match value {
            Value::String(name) if !name.is_empty() => match known_type(collection, name, path, key) {
                Ok(name) => explicit.push(name), Err(mut e) => errors.append(&mut e),
            },
            Value::Array(values) if !values.is_empty() => for (index, value) in values.iter().enumerate() {
                match value {
                    Value::String(name) if !name.is_empty() => match known_type(collection, name, path, key) {
                        Ok(name) => explicit.push(name), Err(mut e) => errors.append(&mut e),
                    },
                    _ => {
                        let mut d = diagnostic("invalid_type_declaration", format!("Explicit type key '{key}' element {index} must be a non-empty string."), path, Some(key), None);
                        d.details = Some(serde_json::json!({"key":key,"element":index}));
                        errors.push(d);
                    }
                }
            },
            _ => errors.push(diagnostic("invalid_type_declaration", format!("Explicit type key '{key}' must be a non-empty string or non-empty string list."), path, Some(key), None)),
        }
    }
    errors.sort_by(|a, b| {
        (&a.field, &a.type_name, &a.message).cmp(&(&b.field, &b.type_name, &b.message))
    });
    if !errors.is_empty() {
        return Err(errors);
    }
    canonicalize(&mut explicit);
    Ok((explicit, present))
}

fn implicit_membership(
    collection: &Collection,
    draft: &Map<String, Value>,
    path: &str,
    clock: &EvaluationClock,
) -> Result<Vec<String>, Vec<Diagnostic>> {
    let (mut types, failures) = collection.determine_types_for_path_checked_with_clock(
        &Value::Object(draft.clone()),
        Some(path),
        clock,
    );
    let mut errors = failures
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
    errors.sort_by(|a, b| a.type_name.cmp(&b.type_name));
    if !errors.is_empty() {
        return Err(errors);
    }
    canonicalize(&mut types);
    Ok(types)
}

#[cfg(test)]
thread_local! {
    static TEST_CLOCKS: std::cell::RefCell<Vec<EvaluationClock>> = const { std::cell::RefCell::new(Vec::new()) };
    static TEST_CLOCK_CAPTURES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
struct TestClockGuard;

#[cfg(test)]
impl TestClockGuard {
    fn install(clocks: Vec<EvaluationClock>) -> Self {
        TEST_CLOCKS.with(|queued| *queued.borrow_mut() = clocks);
        TEST_CLOCK_CAPTURES.with(|captures| captures.set(0));
        Self
    }
}

#[cfg(test)]
impl Drop for TestClockGuard {
    fn drop(&mut self) {
        TEST_CLOCKS.with(|clocks| clocks.borrow_mut().clear());
        TEST_CLOCK_CAPTURES.with(|captures| captures.set(0));
    }
}

fn capture_clock(collection: &Collection, path: &str) -> Result<EvaluationClock, Vec<Diagnostic>> {
    #[cfg(test)]
    if let Some(clock) = TEST_CLOCKS.with(|clocks| {
        let mut clocks = clocks.borrow_mut();
        (!clocks.is_empty()).then(|| clocks.remove(0))
    }) {
        TEST_CLOCK_CAPTURES.with(|captures| captures.set(captures.get() + 1));
        return Ok(clock);
    }
    super::cel::operation_clock(collection.settings.timezone.as_deref()).map_err(|failure| {
        vec![diagnostic(
            failure.code,
            failure.message,
            path,
            Some("match.expr"),
            None,
        )]
    })
}

fn persist_selected(
    collection: &Collection,
    draft: &mut Map<String, Value>,
    selected: &str,
    path: &str,
) -> Result<(), Vec<Diagnostic>> {
    let keys = &collection.settings.explicit_type_keys;
    for key in keys {
        match draft.get(key) {
            Some(Value::String(name)) if name.eq_ignore_ascii_case(selected) => return Ok(()),
            Some(Value::Array(values))
                if values
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|name| name.eq_ignore_ascii_case(selected)) =>
            {
                return Ok(())
            }
            _ => {}
        }
    }
    if let Some(key) = keys
        .iter()
        .find(|key| matches!(draft.get(*key), Some(Value::Array(_))))
    {
        if let Some(Value::Array(values)) = draft.get_mut(key) {
            values.push(Value::String(selected.to_string()));
        }
        return Ok(());
    }
    if let Some(key) = keys.iter().find(|key| !draft.contains_key(*key)) {
        draft.insert(key.clone(), Value::String(selected.to_string()));
        return Ok(());
    }
    if keys.is_empty() {
        return Ok(());
    }
    let mut d = diagnostic("type_membership_persistence_failed", "Selected type membership cannot be represented without changing an occupied scalar declaration.", path, Some("type"), Some(selected));
    d.details = Some(
        serde_json::json!({"selected":selected,"configured_keys":keys,"occupied_scalar_keys":keys}),
    );
    Err(vec![d])
}

fn known_type(
    collection: &Collection,
    name: &str,
    path: &str,
    field: &str,
) -> Result<String, Vec<Diagnostic>> {
    let canonical = name.to_ascii_lowercase();
    if !collection.types().contains_key(&canonical) {
        Err(vec![diagnostic(
            "unknown_type",
            format!("Unknown type '{name}'."),
            path,
            Some(field),
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
            vec![contract_diagnostic(
                "invalid_contract_envelope",
                format!("{field} must be a non-empty string."),
                path,
                field,
                None,
                None,
                None,
                &[],
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
#[allow(clippy::too_many_arguments)] // Wire diagnostics require all four contract-selection detail fields.
fn contract_diagnostic(
    code: impl Into<String>,
    message: impl Into<String>,
    path: &str,
    field: &str,
    contract: Option<&str>,
    version: Option<&str>,
    selected: Option<&str>,
    eligible: &[String],
) -> Diagnostic {
    let mut d = diagnostic(code, message, path, Some(field), selected);
    d.details = Some(
        serde_json::json!({"contract":contract,"version":version,"selected":selected,"eligible":eligible}),
    );
    d
}
fn authority_changed(path: &str, before: &[String], after: &[String]) -> Diagnostic {
    let mut diagnostic = changed(
        "type_membership_authority_changed",
        "Lifecycle or generated values removed authoritative explicit type membership.",
        path,
        before,
        after,
    );
    if let Some(Value::Object(details)) = &mut diagnostic.details {
        details.insert(
            "before_authority".to_string(),
            Value::String("explicit".to_string()),
        );
        details.insert(
            "after_authority".to_string(),
            Value::String("implicit".to_string()),
        );
    }
    diagnostic
}

fn persistence_failure(
    path: &str,
    before: &[String],
    after: &[String],
    reason: &str,
) -> Diagnostic {
    let mut d = changed(
        "type_membership_persistence_failed",
        "Configured type membership cannot be persisted for this record.",
        path,
        before,
        after,
    );
    if let Some(Value::Object(details)) = &mut d.details {
        details.insert("reason".to_string(), Value::String(reason.to_string()));
    }
    d
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use std::fs;

    #[test]
    fn explicit_authority_fallback_reuses_the_single_injected_operation_clock() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  explicit_type_keys: [kind]\n  timezone: UTC\n",
        )
        .unwrap();
        fs::create_dir(root.path().join("_types")).unwrap();
        fs::write(
            root.path().join("_types/note.md"),
            "---\nkind: mdbase.type\nname: note\nmatch:\n  expr:\n    $expr: 'today() == \"2020-01-01\"'\nschema:\n  dialect: json-schema-2020-12\n  value: {type: object}\nlifecycle:\n  on_create:\n    set: { kind: { literal: null } }\n---\n",
        )
        .unwrap();
        let _clock_guard = TestClockGuard::install(vec![
            EvaluationClock::from_utc(
                Utc.with_ymd_and_hms(2020, 1, 1, 12, 0, 0).unwrap(),
                Some("UTC"),
            )
            .unwrap(),
            EvaluationClock::from_utc(
                Utc.with_ymd_and_hms(2020, 1, 2, 12, 0, 0).unwrap(),
                Some("UTC"),
            )
            .unwrap(),
        ]);

        let mut collection = Collection::open(root.path()).unwrap();
        // v0.3 currently fixes null serialization to explicit. Override only this
        // in-memory policy so the public staged-operation boundary reaches the
        // omission path that final revalidation must guard if it becomes configurable.
        collection.settings.write_nulls = "omit".to_string();
        let result = collection
            .v03_operations()
            .unwrap()
            .execute_staged_mutation(
                "create",
                &json!({"path":"clock.md", "type":"note", "frontmatter":{}}),
            );
        assert!(!result.valid, "{result:#?}");
        assert_eq!(
            result.diagnostics[0].code,
            "type_membership_authority_changed"
        );
        assert_eq!(
            result.diagnostics[0].details.as_ref().unwrap()["after"],
            json!(["note"])
        );
        assert_eq!(TEST_CLOCK_CAPTURES.with(|captures| captures.get()), 1);
        assert_eq!(TEST_CLOCKS.with(|clocks| clocks.borrow().len()), 1);
        assert!(!root.path().join("clock.md").exists());
    }
}
