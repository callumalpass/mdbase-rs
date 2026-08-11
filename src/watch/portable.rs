use super::WatchEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ulid::Ulid;

/// A standard Watch profile change kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WatchKind {
    RecordCreated,
    RecordModified,
    RecordDeleted,
    RecordRenamed,
    ConfigChanged,
    TypeChanged,
    ContractChanged,
    SchemaChanged,
    ViewChanged,
    LockChanged,
    /// A collection reload failed. This extension preserves the diagnostic
    /// carried by the transport event without pretending a record changed.
    CollectionInvalidated,
}

/// Portable event shape from the mdbase v0.3 Watch profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortableWatchEvent {
    pub kind: WatchKind,
    pub id: String,
    pub observed_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_fields: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontmatter: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<Value>,
}

impl From<WatchEvent> for PortableWatchEvent {
    fn from(event: WatchEvent) -> Self {
        let payload = &event.payload;
        let kind = kind(&event.event_type);
        let is_record = matches!(
            kind,
            WatchKind::RecordCreated
                | WatchKind::RecordModified
                | WatchKind::RecordDeleted
                | WatchKind::RecordRenamed
        );
        let path = if kind == WatchKind::ConfigChanged {
            Some("mdbase.yaml".to_string())
        } else {
            payload
                .get(if kind == WatchKind::RecordRenamed {
                    "to"
                } else {
                    "path"
                })
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let previous_path = (kind == WatchKind::RecordRenamed)
            .then(|| {
                payload
                    .get("from")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .flatten();
        let changed_fields = is_record.then(|| {
            payload
                .get("changed_fields")
                .and_then(Value::as_array)
                .map(|fields| {
                    fields
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        });
        let frontmatter = match kind {
            WatchKind::RecordCreated | WatchKind::RecordModified | WatchKind::RecordRenamed => {
                payload.get("after").cloned()
            }
            WatchKind::RecordDeleted => payload.get("before").cloned(),
            _ => None,
        };
        let subject = (kind == WatchKind::CollectionInvalidated).then(|| "collection".to_string());

        Self {
            kind,
            id: format!("watch_{}", Ulid::new()),
            observed_at: event.occurred_at,
            path,
            previous_path,
            changed_fields,
            frontmatter,
            subject,
            diagnostic: payload.get("diagnostic").cloned(),
        }
    }
}

fn kind(event_type: &str) -> WatchKind {
    match event_type {
        "mdbase.record.created" => WatchKind::RecordCreated,
        "mdbase.record.modified" => WatchKind::RecordModified,
        "mdbase.record.deleted" => WatchKind::RecordDeleted,
        "mdbase.record.renamed" => WatchKind::RecordRenamed,
        "mdbase.config.changed" => WatchKind::ConfigChanged,
        "mdbase.type.changed" => WatchKind::TypeChanged,
        "mdbase.contract.changed" => WatchKind::ContractChanged,
        "mdbase.schema.changed" => WatchKind::SchemaChanged,
        "mdbase.view.changed" => WatchKind::ViewChanged,
        "mdbase.lock.changed" => WatchKind::LockChanged,
        _ => WatchKind::CollectionInvalidated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn converts_record_transport_events() {
        let portable = WatchEvent {
            event_type: "mdbase.record.renamed".to_string(),
            sequence: 4,
            occurred_at: "2026-07-22T10:00:00Z".to_string(),
            payload: json!({
                "from": "old.md",
                "to": "new.md",
                "after": {"title": "New"}
            }),
        }
        .into_portable();

        assert_eq!(portable.kind, WatchKind::RecordRenamed);
        assert_eq!(portable.path.as_deref(), Some("new.md"));
        assert_eq!(portable.previous_path.as_deref(), Some("old.md"));
        assert_eq!(portable.changed_fields, Some(vec![]));
        assert_eq!(portable.frontmatter, Some(json!({"title": "New"})));
        assert!(portable.id.starts_with("watch_"));
    }

    #[test]
    fn each_portable_observation_has_a_unique_id() {
        let event = WatchEvent {
            event_type: "mdbase.config.changed".to_string(),
            sequence: 1,
            occurred_at: "2026-07-22T10:00:00Z".to_string(),
            payload: json!({}),
        };

        assert_ne!(event.clone().into_portable().id, event.into_portable().id);
    }
}
