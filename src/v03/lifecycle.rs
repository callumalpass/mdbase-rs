//! Deterministic v0.3 lifecycle policy evaluation.

use std::collections::{BTreeSet, HashMap};

use serde_json::{json, Map, Value};

use super::{
    cel::{enrich_record_bindings, evaluate_compiled, operation_clock},
    Diagnostic,
};
use crate::expressions::ast::Expr;
use crate::expressions::evaluator::{EvalContext, EvaluationClock, NoteNamespaceSource};
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

#[derive(Debug, Clone)]
struct AppliedAssignment {
    provider: Value,
    type_name: String,
    lifecycle_path: String,
}

impl Collection {
    /// Apply the lifecycle policy for the already-frozen type membership.
    ///
    /// The returned map is an in-memory draft. Callers re-evaluate membership
    /// and validate it before any bytes are written.
    pub(crate) fn apply_v03_lifecycle(
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
        let mut assignments: HashMap<String, AppliedAssignment> = HashMap::new();
        let mut ordered_types = type_names.to_vec();
        ordered_types.sort();
        ordered_types.dedup();
        let known_fields = ordered_types
            .iter()
            .filter_map(|type_name| self.types.get(type_name))
            .flat_map(|definition| definition.fields.keys().cloned())
            .collect::<BTreeSet<_>>();

        for type_name in &ordered_types {
            let Some(type_definition) = self.types.get(type_name) else {
                continue;
            };
            let Some(policy) = type_definition
                .lifecycle
                .as_ref()
                .and_then(|lifecycle| lifecycle.get(event.key()))
            else {
                continue;
            };
            let actions: Vec<&Value> = match policy {
                Value::Array(actions) => actions.iter().collect(),
                action => vec![action],
            };

            for (action_index, action) in actions.into_iter().enumerate() {
                if let Some(source) = action.get("if").and_then(Value::as_str) {
                    let Some(expression) = self
                        .type_plans
                        .get(type_name)
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
                            diagnostic.type_name = Some(type_name.clone());
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
                for (field, provider) in set {
                    let lifecycle_path = format!(
                        "types/{}/lifecycle/{}/{}/set/{}",
                        type_name,
                        event.key(),
                        action_index,
                        field
                    );
                    if let Some(previous) = assignments.get(field) {
                        if previous.type_name != *type_name && previous.provider != *provider {
                            let mut diagnostic = Diagnostic::error(
                                "type_conflict",
                                format!(
                                    "Types '{}' and '{}' assign different lifecycle values to '{}'.",
                                    previous.type_name, type_name, field
                                ),
                                Some(path.to_string()),
                            );
                            diagnostic.field = Some(field.clone());
                            diagnostic.details = Some(json!({
                                "event": event.key(),
                                "types": [previous.type_name, type_name],
                                "lifecycle_paths": [previous.lifecycle_path, lifecycle_path],
                            }));
                            return Err(vec![diagnostic]);
                        }
                        if previous.type_name != *type_name && previous.provider == *provider {
                            continue;
                        }
                    }

                    let value = resolve_provider(provider, &draft, &now_value, &today_value);
                    set_path(&mut draft, field, value);
                    assignments.insert(
                        field.clone(),
                        AppliedAssignment {
                            provider: provider.clone(),
                            type_name: type_name.clone(),
                            lifecycle_path,
                        },
                    );
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

fn resolve_provider(
    provider: &Value,
    draft: &Map<String, Value>,
    now: &Value,
    today: &Value,
) -> Value {
    if provider.get("now") == Some(&Value::Bool(true)) {
        return now.clone();
    }
    if provider.get("today") == Some(&Value::Bool(true)) {
        return today.clone();
    }
    if provider.get("uuid") == Some(&Value::Bool(true)) {
        return Value::String(uuid::Uuid::new_v4().to_string());
    }
    if provider.get("ulid") == Some(&Value::Bool(true)) {
        return Value::String(ulid::Ulid::new().to_string());
    }
    if let Some(path) = provider.get("slugify").and_then(Value::as_str) {
        return value_at_path(draft, path)
            .map(|value| match value {
                Value::String(value) => Value::String(slugify(value)),
                value => Value::String(slugify(&value.to_string())),
            })
            .unwrap_or(Value::Null);
    }
    if let Some(path) = provider.get("copy").and_then(Value::as_str) {
        return value_at_path(draft, path).cloned().unwrap_or(Value::Null);
    }
    provider.get("literal").cloned().unwrap_or(Value::Null)
}

fn value_at_path<'a>(object: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let mut segments = path.split('.');
    let first = segments.next()?;
    let mut value = object.get(first)?;
    for segment in segments {
        value = value.as_object()?.get(segment)?;
    }
    Some(value)
}

fn set_path(object: &mut Map<String, Value>, path: &str, value: Value) {
    let segments = path.split('.').collect::<Vec<_>>();
    let Some((last, parents)) = segments.split_last() else {
        return;
    };
    let mut current = object;
    for segment in parents {
        let entry = current
            .entry((*segment).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        current = entry.as_object_mut().expect("object inserted above");
    }
    current.insert((*last).to_string(), value);
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
            value_at_path(&object, "source.name"),
            Some(&json!("Hello World"))
        );
        set_path(&mut object, "metadata.slug", json!("hello-world"));
        assert_eq!(object["metadata"]["slug"], "hello-world");
    }

    #[test]
    fn guards_receive_effective_note_presence_old_and_operation_bindings() {
        let draft =
            serde_json::from_value::<Map<String, Value>>(json!({"status": "done"})).unwrap();
        let old = serde_json::from_value::<Map<String, Value>>(json!({"status": "open"})).unwrap();
        let known = BTreeSet::from(["status".to_string(), "missing".to_string()]);
        let clock = EvaluationClock::capture(Some("UTC")).unwrap();
        let expression = super::super::cel::compile(
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
