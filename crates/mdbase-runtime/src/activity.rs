use std::time::Duration;

use mdbase::watch::WatchEvent;
use mdbase_interop::{ExactContractReference, ImplementationIdentity};
use serde_json::{json, Value};
use ulid::Ulid;

use crate::{AdmissionCatalog, DeliveryOutcome, Runtime, RuntimeError, RuntimeResult};

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
    /// Build a canonical `runtime_workflow` record value.
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
            "event.data.types.exists(record_type, record_type == {record_type}) \
             && event.data.after[{status_field}] == {status_value}"
        );
        let trigger = |id: &str, event: &str| {
            json!({
                "id": id,
                "event": {"id": event, "version": "^1.0.0"},
                "if": {"$expr": condition},
                "debounce": format!("{delay_ms}ms")
            })
        };

        Ok(json!({
            "type": "runtime_workflow",
            "id": self.id,
            "version": "1.0.0",
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
                "action": {"id": self.action, "version": "^1.0.0"},
                "input": {
                    "path": {"$expr": "event.data.path"},
                    "if_revision": {"$expr": "event.data.revision"},
                    "status_field": self.status_field,
                    "status_value": self.status_value
                }
            }],
            "run": {
                "concurrency": {
                    "group": {"$expr": "event.data.path"},
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
/// Rename events receive the destination as `data.path` so record-oriented
/// workflows can use one expression for create, modify, and rename events.
pub fn watch_event_envelope(
    event: WatchEvent,
    contract: &ExactContractReference,
    source: &ImplementationIdentity,
    source_uri: &str,
    subject: Option<&str>,
) -> RuntimeResult<Value> {
    if !source_uri.contains(':') {
        return Err(RuntimeError::diagnostic(
            "invalid_runtime_event",
            format!("CloudEvent source {source_uri:?} must be an absolute URI-reference."),
        ));
    }
    if event.event_type != contract.id {
        return Err(RuntimeError::diagnostic(
            "event_contract_mismatch",
            format!(
                "Watch event {} does not match contract {}.",
                event.event_type, contract.id
            ),
        ));
    }
    let mut data = event.payload;
    if event.event_type == "mdbase.record.renamed" && data.get("path").is_none() {
        if let Some(path) = data.get("to").cloned() {
            data["path"] = path;
        }
    }
    let mut envelope = json!({
        "specversion": "1.0",
        "id": format!("watch_{}", Ulid::new()),
        "source": source_uri,
        "type": event.event_type,
        "time": event.occurred_at,
        "datacontenttype": "application/json",
        "dataschema": format!(
            "urn:mdbase:contract:{}:{}:{}",
            contract.id, contract.version, contract.digest
        ),
        "data": data,
        "mdbaseprofile": "0.1",
        "mdbasecontractversion": contract.version,
        "mdbasecontractdigest": contract.digest,
        "mdbaseapplication": source.application,
        "mdbaseimplementation": source.implementation,
        "mdbaseimplementationversion": source.version,
    });
    if let Some(instance_id) = &source.instance_id {
        envelope["mdbaseinstanceid"] = Value::String(instance_id.clone());
    }
    if let Some(subject) = subject {
        envelope["subject"] = Value::String(subject.to_string());
    }
    Ok(envelope)
}

impl Runtime {
    /// Admit one Watch event directly into the durable workflow runtime.
    pub async fn deliver_watch_event(
        &self,
        catalog: &AdmissionCatalog,
        event: WatchEvent,
        contract: &ExactContractReference,
        source: &ImplementationIdentity,
        source_uri: &str,
        subject: Option<&str>,
    ) -> RuntimeResult<DeliveryOutcome> {
        let envelope = watch_event_envelope(event, contract, source, source_uri, subject)?;
        self.deliver_event(catalog, envelope).await
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
            &ExactContractReference {
                id: "mdbase.record.renamed".to_string(),
                version: "1.0.0".to_string(),
                digest: format!("sha256:{}", "a".repeat(64)),
            },
            &ImplementationIdentity {
                application: "mdbase".to_string(),
                implementation: "mdbase-rs".to_string(),
                version: "0.4.0".to_string(),
                instance_id: None,
            },
            "urn:mdbase:watch:collection-one",
            Some("collection-one"),
        )
        .unwrap();

        assert_eq!(envelope["data"]["path"], "tasks/new.md");
        assert_eq!(envelope["subject"], "collection-one");
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
            &ExactContractReference {
                id: "mdbase.record.modified".to_string(),
                version: "1.0.0".to_string(),
                digest: format!("sha256:{}", "a".repeat(64)),
            },
            &ImplementationIdentity {
                application: "mdbase".to_string(),
                implementation: "mdbase-rs".to_string(),
                version: "0.4.0".to_string(),
                instance_id: None,
            },
            "not valid",
            None,
        )
        .unwrap_err();
        assert_eq!(error.code(), "invalid_runtime_event");
    }

    #[test]
    fn produces_a_standard_runtime_workflow_record() {
        let activity = StatusTransitionActivity {
            id: "tasknotes.archive".to_string(),
            name: "Archive completed tasks".to_string(),
            record_type: "task".to_string(),
            status_field: "status".to_string(),
            status_value: "done".to_string(),
            action: "tasknotes.task.archive".to_string(),
            delay: Duration::from_secs(60),
        };

        crate::validate_runtime_record(&activity.workflow_contract().unwrap()).unwrap();
    }
}
