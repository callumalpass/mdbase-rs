use serde_json::{Map, Value};

use super::diff::{canonical_field_changes, canonical_present_fields};
use super::{
    CanonicalChange, CanonicalTypeSet, ChangeBatch, ChangeSet, ProviderError, RebuildReason,
    RecordChange, RecordChangeKind, ResourceChange, ResourceChangeKind,
};
use crate::api::{CollectionPath, Revision};
use crate::watch::WatchEvent;

pub(crate) fn normalize(event: &WatchEvent) -> Result<ChangeSet, ProviderError> {
    let change = match event.event_type.as_str() {
        "mdbase.record.created" => record_created(&event.payload)?,
        "mdbase.record.modified" => record_modified(&event.payload)?,
        "mdbase.record.deleted" => record_deleted(&event.payload)?,
        "mdbase.record.renamed" => record_renamed(&event.payload)?,
        "mdbase.config.changed" => resource(
            &event.payload,
            ResourceChangeKind::Configuration,
            Some("mdbase.yaml"),
        )?,
        "mdbase.type.changed" => {
            resource(&event.payload, ResourceChangeKind::TypeDefinition, None)?
        }
        "mdbase.contract.changed" => resource(&event.payload, ResourceChangeKind::Contract, None)?,
        "mdbase.view.changed" => resource(&event.payload, ResourceChangeKind::ViewSource, None)?,
        "mdbase.schema.changed" | "mdbase.lock.changed" => {
            resource(&event.payload, ResourceChangeKind::File, None)?
        }
        _ => {
            return Ok(ChangeSet::CollectionWide {
                reason: RebuildReason::ExternalChangeUncertain,
            })
        }
    };
    Ok(ChangeSet::Exact(ChangeBatch::new(vec![change])?))
}

pub(crate) fn matches_known(event: &WatchEvent, known: &ChangeBatch) -> bool {
    let Ok(ChangeSet::Exact(observed)) = normalize(event) else {
        return false;
    };
    observed
        .items()
        .iter()
        .all(|item| known.items().contains(item))
}

fn record_created(payload: &Value) -> Result<CanonicalChange, ProviderError> {
    let after = object(payload, "raw_after")?;
    Ok(CanonicalChange::Record(RecordChange {
        kind: RecordChangeKind::Created,
        path: path(payload, "path")?,
        from: None,
        before_revision: None,
        after_revision: revision(payload.get("revision"))?,
        before_types: CanonicalTypeSet::default(),
        after_types: types(payload.get("types")),
        changed_fields: canonical_present_fields(after)?,
        body_changed: payload
            .get("body_changed")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    }))
}

fn record_modified(payload: &Value) -> Result<CanonicalChange, ProviderError> {
    let before = object(payload, "raw_before")?;
    let after = object(payload, "raw_after")?;
    Ok(CanonicalChange::Record(RecordChange {
        kind: RecordChangeKind::Updated,
        path: path(payload, "path")?,
        from: None,
        before_revision: revision(payload.get("previous_revision"))?,
        after_revision: revision(payload.get("revision"))?,
        before_types: types(payload.get("previous_types")),
        after_types: types(payload.get("types")),
        changed_fields: canonical_field_changes(before, after)?,
        body_changed: payload
            .get("body_changed")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    }))
}

fn record_deleted(payload: &Value) -> Result<CanonicalChange, ProviderError> {
    let before = object(payload, "raw_before")?;
    Ok(CanonicalChange::Record(RecordChange {
        kind: RecordChangeKind::Deleted,
        path: path(payload, "path")?,
        from: None,
        before_revision: revision(payload.get("previous_revision"))?,
        after_revision: None,
        before_types: types(payload.get("types")),
        after_types: CanonicalTypeSet::default(),
        changed_fields: canonical_present_fields(before)?,
        body_changed: payload
            .get("body_changed")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    }))
}

fn record_renamed(payload: &Value) -> Result<CanonicalChange, ProviderError> {
    let before = object(payload, "raw_before")?;
    let after = object(payload, "raw_after")?;
    Ok(CanonicalChange::Record(RecordChange {
        kind: RecordChangeKind::Renamed,
        path: path(payload, "to")?,
        from: Some(path(payload, "from")?),
        before_revision: revision(payload.get("previous_revision"))?,
        after_revision: revision(payload.get("revision"))?,
        before_types: types(payload.get("previous_types")),
        after_types: types(payload.get("types")),
        changed_fields: canonical_field_changes(before, after)?,
        body_changed: payload
            .get("body_changed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }))
}

fn resource(
    payload: &Value,
    kind: ResourceChangeKind,
    fixed_path: Option<&str>,
) -> Result<CanonicalChange, ProviderError> {
    let path = match fixed_path {
        Some(path) => path,
        None => payload
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| malformed("resource event path is missing"))?,
    };
    Ok(CanonicalChange::Resource(ResourceChange {
        kind,
        path: CollectionPath::new(path).map_err(|error| malformed(&error.to_string()))?,
        before_revision: revision(payload.get("previous_revision"))?,
        after_revision: revision(payload.get("revision"))?,
    }))
}

fn object<'a>(payload: &'a Value, field: &str) -> Result<&'a Map<String, Value>, ProviderError> {
    payload
        .get(field)
        .and_then(Value::as_object)
        .ok_or_else(|| malformed(&format!("record event {field} is missing")))
}

fn path(payload: &Value, field: &str) -> Result<CollectionPath, ProviderError> {
    let value = payload
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| malformed(&format!("record event {field} is missing")))?;
    CollectionPath::new(value).map_err(|error| malformed(&error.to_string()))
}

fn revision(value: Option<&Value>) -> Result<Option<Revision>, ProviderError> {
    value
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| malformed("event revision is not a string"))
                .and_then(|value| {
                    Revision::parse(value).map_err(|error| malformed(&error.to_string()))
                })
        })
        .transpose()
}

fn types(value: Option<&Value>) -> CanonicalTypeSet {
    CanonicalTypeSet::new(
        value
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string),
    )
}

fn malformed(message: &str) -> ProviderError {
    ProviderError::InvalidChangeSet(format!("filesystem observation is malformed: {message}"))
}
