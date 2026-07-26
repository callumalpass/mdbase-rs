use std::time::Duration;

use mdbase::runtime_contracts::RuntimeRegistry;
use mdbase::watch::WatchEvent;
use serde_json::{json, Value};
use ulid::Ulid;

use crate::{DeliveryOutcome, Runtime, RuntimeError, RuntimeResult};

/// Configuration for a durable workflow that reacts to a record entering one
/// status and invokes an application-owned action after a quiet period.
///
/// mdbase owns event admission, debounce, replacement, and recovery. The
/// embedding application owns the action semantics, such as applying its
/// portable archive representation or moving the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusTransitionActivity {
    pub id: String,
    pub name: String,
    pub record_type: String,
    pub status_field: String,
    pub status_value: String,
    pub action: String,
    pub delay: Duration,
}

impl StatusTransitionActivity {
    /// Build a canonical Runtime Contracts workflow value.
    pub fn workflow_contract(&self) -> RuntimeResult<Value> {
        for (label, value) in [
            ("activity id", self.id.as_str()),
            ("record type", self.record_type.as_str()),
            ("action", self.action.as_str()),
        ] {
            if !runtime_identifier(value) {
                return Err(RuntimeError::diagnostic(
                    "invalid_status_activity",
                    format!("{label} {value:?} is not a runtime identifier."),
                ));
            }
        }
        if self.name.trim().is_empty() {
            return Err(RuntimeError::diagnostic(
                "invalid_status_activity",
                "Activity name cannot be empty.",
            ));
        }
        if self.status_field.trim().is_empty() {
            return Err(RuntimeError::diagnostic(
                "invalid_status_activity",
                "Status field cannot be empty.",
            ));
        }
        let delay_ms = u64::try_from(self.delay.as_millis()).map_err(|_| {
            RuntimeError::diagnostic(
                "duration_out_of_range",
                "Activity delay exceeds the supported range.",
            )
        })?;
        let record_type = serde_json::to_string(&self.record_type)
            .expect("serializing a string literal cannot fail");
        let status_field = serde_json::to_string(&self.status_field)
            .expect("serializing a string literal cannot fail");
        let status_value = serde_json::to_string(&self.status_value)
            .expect("serializing a string literal cannot fail");
        let condition = format!(
            "event.payload.types.exists(record_type, record_type == {record_type}) \
             && event.payload.after[{status_field}] == {status_value}"
        );
        let trigger = |id: &str, event: &str| {
            json!({
                "id": id,
                "event": event,
                "if": {"$expr": condition},
                "debounce": format!("{delay_ms}ms")
            })
        };

        Ok(json!({
            "type": "workflow",
            "id": self.id,
            "version": 1,
            "name": self.name,
            "description": "Run an application action after a record remains in a configured status.",
            "enabled": true,
            "triggers": [
                trigger("created", "mdbase.record.created"),
                trigger("modified", "mdbase.record.modified"),
                trigger("renamed", "mdbase.record.renamed")
            ],
            "steps": [{
                "id": "apply",
                "action": self.action,
                "input": {
                    "path": {"$expr": "event.payload.path"},
                    "if_revision": {"$expr": "event.payload.revision"},
                    "status_field": self.status_field,
                    "status_value": self.status_value
                }
            }],
            "run": {
                "execution": {"mode": "single_executor"},
                "concurrency": {
                    "group": {"$expr": "event.payload.path"},
                    "policy": "replace"
                },
                "on_error": "stop"
            }
        }))
    }
}

/// Convert one mdbase Watch event into a Runtime event envelope without
/// discarding its before/after values, revision, types, or changed fields.
///
/// Rename events receive the destination as `payload.path` so record-oriented
/// workflows can use one expression for create, modify, and rename events.
pub fn watch_event_envelope(
    event: WatchEvent,
    source_runtime: &str,
    collection: Option<&str>,
) -> RuntimeResult<Value> {
    if !runtime_identifier(source_runtime) {
        return Err(RuntimeError::diagnostic(
            "invalid_runtime_event",
            format!("Source runtime {source_runtime:?} is not a runtime identifier."),
        ));
    }
    let mut payload = event.payload;
    if event.event_type == "mdbase.record.renamed" && payload.get("path").is_none() {
        if let Some(path) = payload.get("to").cloned() {
            payload["path"] = path;
        }
    }
    let mut source = json!({
        "runtime": source_runtime,
        "provider": "mdbase.watch"
    });
    if let Some(collection) = collection {
        source["collection"] = Value::String(collection.to_string());
    }
    Ok(json!({
        "type": event.event_type,
        "contract_version": 1,
        "id": format!("watch_{}", Ulid::new()),
        "occurred_at": event.occurred_at,
        "source": source,
        "payload": payload
    }))
}

impl Runtime {
    /// Admit one Watch event directly into the durable workflow runtime.
    pub async fn deliver_watch_event(
        &self,
        registry: &RuntimeRegistry,
        event: WatchEvent,
        collection: Option<&str>,
    ) -> RuntimeResult<DeliveryOutcome> {
        let envelope = watch_event_envelope(event, &self.config().runtime_id, collection)?;
        self.deliver_event(registry, envelope).await
    }
}

fn runtime_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | ':' | '-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_rename_events_for_record_activities() {
        let envelope = watch_event_envelope(
            WatchEvent {
                event_type: "mdbase.record.renamed".to_string(),
                sequence: 2,
                occurred_at: "2026-07-26T10:00:00Z".to_string(),
                payload: json!({
                    "from": "tasks/old.md",
                    "to": "tasks/new.md",
                    "revision": "rev-new",
                    "after": {"status": "done"},
                    "types": ["task"]
                }),
            },
            "local",
            Some("collection-one"),
        )
        .unwrap();

        assert_eq!(envelope["payload"]["path"], "tasks/new.md");
        assert_eq!(envelope["source"]["collection"], "collection-one");
        assert!(envelope["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("watch_")));
    }

    #[test]
    fn rejects_invalid_runtime_identifiers() {
        let error = watch_event_envelope(
            WatchEvent {
                event_type: "mdbase.record.modified".to_string(),
                sequence: 1,
                occurred_at: "2026-07-26T10:00:00Z".to_string(),
                payload: json!({}),
            },
            "not valid",
            None,
        )
        .unwrap_err();
        assert_eq!(error.code(), "invalid_runtime_event");
    }
}
