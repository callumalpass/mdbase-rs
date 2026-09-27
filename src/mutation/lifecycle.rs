//! Deterministic v0.3 lifecycle policy evaluation.

use std::collections::{BTreeSet, HashMap};

use serde_json::{json, Map, Value};

use crate::cel::{enrich_record_bindings, evaluate_compiled, operation_clock};
use crate::diagnostic::Diagnostic;
use crate::expressions::ast::Expr;
use crate::expressions::evaluator::{EvalContext, EvaluationClock, NoteNamespaceSource};
use crate::field_references;
use crate::generated::slugify;
use crate::Collection;

#[derive(Debug, Clone, Copy)]
pub(crate) enum LifecycleEvent {
    Create,
    Update,
}

impl LifecycleEvent {
    fn key(self) -> &'static str {
        match self {
            Self::Create => "on_create",
            Self::Update => "on_update",
        }
    }

    fn operation_name(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
        }
    }
}

impl Collection {
    /// Apply the lifecycle policy for the already-frozen type membership.
    ///
    /// The returned map is an in-memory draft. Callers re-evaluate membership
    /// and validate it before any bytes are written.
    pub(crate) fn apply_mutation_lifecycle(
        &self,
        event: LifecycleEvent,
        type_names: &[String],
        mut draft: Map<String, Value>,
        old: Option<&Map<String, Value>>,
        path: &str,
    ) -> Result<Map<String, Value>, Vec<Diagnostic>> {
        let clock = operation_clock(self.settings.timezone.as_deref()).map_err(|error| {
            vec![Diagnostic::error(
                error.code,
                error.message,
                Some(path.to_string()),
            )]
        })?;
        let now_value = Value::String(clock.now().to_string());
        let today_value = Value::String(clock.today().to_string());
        let mut ordered_types = type_names.to_vec();
        ordered_types.sort();
        ordered_types.dedup();
        let known_fields = ordered_types
            .iter()
            .filter_map(|type_name| self.types.get(type_name))
            .flat_map(|definition| definition.fields.keys().cloned())
            .collect::<BTreeSet<_>>();
        let policies = ordered_types
            .iter()
            .filter_map(|type_name| {
                let policy = self
                    .types
                    .get(type_name)?
                    .lifecycle
                    .as_ref()?
                    .get(event.key())?;
                let actions: Vec<&Value> = match policy {
                    Value::Array(actions) => actions.iter().collect(),
                    action => vec![action],
                };
                Some((type_name, actions))
            })
            .collect::<Vec<_>>();
        let shared = cross_type_assignments(&policies, event, path)?;

        for (type_name, actions) in &policies {
            for (action_index, action) in actions.iter().enumerate() {
                if let Some(source) = action.get("if").and_then(Value::as_str) {
                    let Some(expression) = self
                        .type_plans
                        .get(*type_name)
                        .and_then(|plan| plan.lifecycle_guard(event.key(), action_index))
                    else {
                        return Err(vec![Diagnostic::error(
                            "invalid_type_definition",
                            format!("Compiled lifecycle guard is missing for type '{type_name}'."),
                            Some(path.to_string()),
                        )]);
                    };
                    match evaluate_guard_compiled(
                        expression,
                        &draft,
                        old,
                        &known_fields,
                        path,
                        event,
                        &clock,
                    ) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(message) => {
                            let mut diagnostic = Diagnostic::error(
                                "lifecycle_expression_error",
                                message,
                                Some(path.to_string()),
                            );
                            diagnostic.type_name = Some((*type_name).clone());
                            diagnostic.details = Some(json!({
                                "event": event.key(),
                                "action": action_index,
                                "source": source,
                            }));
                            return Err(vec![diagnostic]);
                        }
                    }
                }

                let Some(set) = action.get("set").and_then(Value::as_object) else {
                    continue;
                };
                // Every provider in one `set` reads the draft as it was before
                // this action, so YAML key order never changes the result.
                let snapshot = draft.clone();
                for (field, provider) in set {
                    if shared.get(field).is_some_and(|owner| owner != *type_name) {
                        // An identical assignment from an earlier type already ran.
                        continue;
                    }
                    let result =
                        match resolve_provider(provider, &snapshot, &now_value, &today_value) {
                            Some(value) => {
                                field_references::set_object_value(&mut draft, field, value)
                            }
                            None => field_references::remove_object_value(&mut draft, field),
                        };
                    if let Err(message) = result {
                        let mut diagnostic = Diagnostic::error(
                            "invalid_lifecycle_path",
                            message,
                            Some(path.to_string()),
                        );
                        diagnostic.field = Some(field.clone());
                        diagnostic.type_name = Some((*type_name).clone());
                        return Err(vec![diagnostic]);
                    }
                }
            }
        }

        Ok(draft)
    }
}

fn evaluate_guard_compiled(
    expression: &Expr,
    draft: &Map<String, Value>,
    old: Option<&Map<String, Value>>,
    known_fields: &BTreeSet<String>,
    path: &str,
    event: LifecycleEvent,
    clock: &EvaluationClock,
) -> Result<bool, String> {
    let draft_value = Value::Object(draft.clone());
    let old_value = old.cloned().map(Value::Object).unwrap_or(Value::Null);
    let mut bindings = enrich_record_bindings(&draft_value, &draft_value, known_fields.iter())
        .as_object()
        .cloned()
        .expect("record bindings are always an object");
    bindings.insert("old".to_string(), old_value);
    bindings.insert(
        "operation".to_string(),
        json!({"name": event.operation_name()}),
    );
    let mut context = EvalContext::empty();
    context.frontmatter = Value::Object(bindings);
    context.raw_frontmatter = Some(Value::Object(draft.clone()));
    context.file_path = Some(path.to_string());
    context.note_namespace_source = NoteNamespaceSource::Effective;
    context.string_concat = false;

    let result = evaluate_compiled(expression, &context, clock)
        .map_err(|error| format!("Lifecycle guard evaluation failed: {}", error.message))?;
    Ok(result == Value::Bool(true))
}

/// Map each field assigned by more than one matched type to the first type
/// that assigns it. Different providers for one field are a `type_conflict`
/// whether or not their guards would run (Chapter 09).
fn cross_type_assignments(
    policies: &[(&String, Vec<&Value>)],
    event: LifecycleEvent,
    path: &str,
) -> Result<HashMap<String, String>, Vec<Diagnostic>> {
    let mut first: HashMap<&String, (&String, &Value, String)> = HashMap::new();
    let mut shared = HashMap::new();
    for (type_name, actions) in policies {
        for (action_index, action) in actions.iter().enumerate() {
            let Some(set) = action.get("set").and_then(Value::as_object) else {
                continue;
            };
            for (field, provider) in set {
                let lifecycle_path = format!(
                    "types/{type_name}/lifecycle/{}/{action_index}/set/{field}",
                    event.key()
                );
                match first.get(field) {
                    None => {
                        first.insert(field, (type_name, provider, lifecycle_path));
                    }
                    Some((owner, _, _)) if owner == type_name => {}
                    Some((owner, existing, _)) if *existing == provider => {
                        shared.insert(field.clone(), (*owner).clone());
                    }
                    Some((owner, _, owner_path)) => {
                        let mut diagnostic = Diagnostic::error(
                            "type_conflict",
                            format!(
                                "Types '{owner}' and '{type_name}' assign different lifecycle values to '{field}'."
                            ),
                            Some(path.to_string()),
                        );
                        diagnostic.field = Some(field.clone());
                        diagnostic.details = Some(json!({
                            "event": event.key(),
                            "types": [owner, type_name],
                            "lifecycle_paths": [owner_path, lifecycle_path],
                        }));
                        return Err(vec![diagnostic]);
                    }
                }
            }
        }
    }
    Ok(shared)
}

/// The value a provider assigns, or `None` when the target key is removed.
fn resolve_provider(
    provider: &Value,
    draft: &Map<String, Value>,
    now: &Value,
    today: &Value,
) -> Option<Value> {
    if provider.get("now") == Some(&Value::Bool(true)) {
        return Some(now.clone());
    }
    if provider.get("today") == Some(&Value::Bool(true)) {
        return Some(today.clone());
    }
    if provider.get("uuid") == Some(&Value::Bool(true)) {
        return Some(Value::String(uuid::Uuid::new_v4().to_string()));
    }
    if provider.get("ulid") == Some(&Value::Bool(true)) {
        return Some(Value::String(ulid::Ulid::new().to_string()));
    }
    if let Some(path) = provider.get("slugify").and_then(Value::as_str) {
        return Some(
            field_references::get_value_from_object(draft, path)
                .and_then(Value::as_str)
                .map_or(Value::Null, |value| Value::String(slugify(value))),
        );
    }
    if let Some(path) = provider.get("copy").and_then(Value::as_str) {
        return field_references::get_value_from_object(draft, path).cloned();
    }
    Some(provider.get("literal").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_paths_can_be_read_and_written() {
        let mut object = serde_json::from_value::<Map<String, Value>>(json!({
            "source": {"name": "Hello World"}
        }))
        .unwrap();
        assert_eq!(
            field_references::get_value_from_object(&object, "source.name"),
            Some(&json!("Hello World"))
        );
        field_references::set_object_value(&mut object, "metadata.slug", json!("hello-world"))
            .unwrap();
        assert_eq!(object["metadata"]["slug"], "hello-world");
        field_references::set_object_value(&mut object, "/@type", json!("Contact")).unwrap();
        assert_eq!(object["@type"], "Contact");
    }

    #[test]
    fn guards_receive_effective_note_presence_old_and_operation_bindings() {
        let draft =
            serde_json::from_value::<Map<String, Value>>(json!({"status": "done"})).unwrap();
        let old = serde_json::from_value::<Map<String, Value>>(json!({"status": "open"})).unwrap();
        let known = BTreeSet::from(["status".to_string(), "missing".to_string()]);
        let clock = EvaluationClock::capture(Some("UTC")).unwrap();
        let expression = crate::cel::compile(
            "note.status == 'done' && record.status == 'done' && !present.raw.missing && old.status == 'open' && operation.name == 'update'",
        )
        .unwrap();
        assert!(evaluate_guard_compiled(
            &expression,
            &draft,
            Some(&old),
            &known,
            "task.md",
            LifecycleEvent::Update,
            &clock,
        )
        .unwrap());
    }
}
