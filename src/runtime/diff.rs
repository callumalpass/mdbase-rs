use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use super::{
    CanonicalChange, CanonicalFieldChangeSet, CanonicalTypeSet, ChangeBatch, CollectionSnapshot,
    CollectionSnapshotRecord, CollectionSnapshotResourceKind, OperationKind, OperationRequest,
    ProviderError, RecordChange, RecordChangeKind, ResourceChange, ResourceChangeKind,
};
use crate::api::{CollectionPath, Revision};

pub(crate) fn canonical_changes(
    before: &CollectionSnapshot,
    after: &CollectionSnapshot,
    request: Option<&OperationRequest>,
) -> Result<ChangeBatch, ProviderError> {
    let mut before_records = before
        .records
        .iter()
        .map(|record| (record.path.as_str(), record))
        .collect::<BTreeMap<_, _>>();
    let mut after_records = after
        .records
        .iter()
        .map(|record| (record.path.as_str(), record))
        .collect::<BTreeMap<_, _>>();
    let mut changes = Vec::new();

    for (from, to) in proven_requested_renames(request, &before_records, &after_records) {
        let before = before_records
            .remove(from.as_str())
            .expect("a proven rename source exists");
        let after = after_records
            .remove(to.as_str())
            .expect("a proven rename destination exists");
        changes.push(CanonicalChange::Record(record_change(
            RecordChangeKind::Renamed,
            after.path.as_str(),
            Some(from),
            Some(before),
            Some(after),
        )?));
    }

    for path in before_records
        .keys()
        .chain(after_records.keys())
        .copied()
        .collect::<BTreeSet<_>>()
    {
        match (before_records.get(path), after_records.get(path)) {
            (Some(before), Some(after)) if before.revision != after.revision => {
                changes.push(CanonicalChange::Record(record_change(
                    RecordChangeKind::Updated,
                    path,
                    None,
                    Some(before),
                    Some(after),
                )?));
            }
            (Some(before), None) => changes.push(CanonicalChange::Record(record_change(
                RecordChangeKind::Deleted,
                path,
                None,
                Some(before),
                None,
            )?)),
            (None, Some(after)) => changes.push(CanonicalChange::Record(record_change(
                RecordChangeKind::Created,
                path,
                None,
                None,
                Some(after),
            )?)),
            _ => {}
        }
    }

    let before_resources = before
        .resources
        .iter()
        .map(|resource| (resource.path.as_str(), resource))
        .collect::<BTreeMap<_, _>>();
    let after_resources = after
        .resources
        .iter()
        .map(|resource| (resource.path.as_str(), resource))
        .collect::<BTreeMap<_, _>>();
    for path in before_resources
        .keys()
        .chain(after_resources.keys())
        .copied()
        .collect::<BTreeSet<_>>()
    {
        let before = before_resources.get(path).copied();
        let after = after_resources.get(path).copied();
        if before.map(|resource| resource.revision.as_str())
            == after.map(|resource| resource.revision.as_str())
        {
            continue;
        }
        let kind = after
            .or(before)
            .expect("a resource exists on one side")
            .kind;
        changes.push(CanonicalChange::Resource(ResourceChange {
            kind: resource_kind(kind),
            path: collection_path(path)?,
            before_revision: before
                .map(|resource| revision(&resource.revision))
                .transpose()?,
            after_revision: after
                .map(|resource| revision(&resource.revision))
                .transpose()?,
        }));
    }

    ChangeBatch::new(changes)
}

fn proven_requested_renames(
    request: Option<&OperationRequest>,
    before: &BTreeMap<&str, &CollectionSnapshotRecord>,
    after: &BTreeMap<&str, &CollectionSnapshotRecord>,
) -> Vec<(CollectionPath, CollectionPath)> {
    let Some(request) = request else {
        return Vec::new();
    };
    let candidates = match request.operation {
        OperationKind::Rename => vec![&request.input],
        OperationKind::Batch => request
            .input
            .get("operations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|item| item.get("kind").and_then(Value::as_str) == Some("rename"))
            .filter_map(|item| item.get("input"))
            .collect(),
        _ => return Vec::new(),
    };
    candidates
        .into_iter()
        .filter_map(|input| {
            let from = input.get("from").or_else(|| input.get("path"))?.as_str()?;
            let to = input
                .get("to")
                .or_else(|| input.get("new_path"))?
                .as_str()?;
            let from = CollectionPath::new(from).ok()?;
            let to = CollectionPath::new(to).ok()?;
            (before.contains_key(from.as_str())
                && !after.contains_key(from.as_str())
                && !before.contains_key(to.as_str())
                && after.contains_key(to.as_str()))
            .then_some((from, to))
        })
        .collect()
}

fn record_change(
    kind: RecordChangeKind,
    path: &str,
    from: Option<CollectionPath>,
    before: Option<&CollectionSnapshotRecord>,
    after: Option<&CollectionSnapshotRecord>,
) -> Result<RecordChange, ProviderError> {
    let before_fields = before.map(record_fields).unwrap_or_default();
    let after_fields = after.map(record_fields).unwrap_or_default();
    let changed_fields = match (before, after) {
        (Some(before), Some(after)) => changed_frontmatter(&before.frontmatter, &after.frontmatter),
        (Some(_), None) => before_fields,
        (None, Some(_)) => after_fields,
        (None, None) => BTreeSet::new(),
    };
    Ok(RecordChange {
        kind,
        path: collection_path(path)?,
        from,
        before_revision: before
            .map(|record| revision(&record.revision))
            .transpose()?,
        after_revision: after.map(|record| revision(&record.revision)).transpose()?,
        before_types: CanonicalTypeSet::new(
            before
                .into_iter()
                .flat_map(|record| record.types.iter().cloned()),
        ),
        after_types: CanonicalTypeSet::new(
            after
                .into_iter()
                .flat_map(|record| record.types.iter().cloned()),
        ),
        changed_fields: CanonicalFieldChangeSet::new(changed_fields)?,
        body_changed: before.map(|record| record.body.as_str())
            != after.map(|record| record.body.as_str()),
    })
}

fn record_fields(record: &CollectionSnapshotRecord) -> BTreeSet<String> {
    let mut fields = BTreeSet::new();
    collect_visible_fields(&Value::Object(record.frontmatter.clone()), "", &mut fields);
    fields
}

fn changed_frontmatter(
    before: &Map<String, Value>,
    after: &Map<String, Value>,
) -> BTreeSet<String> {
    let mut changed = BTreeSet::new();
    collect_changed_fields(
        Some(&Value::Object(before.clone())),
        Some(&Value::Object(after.clone())),
        "",
        &mut changed,
    );
    changed
}

pub(crate) fn canonical_field_changes(
    before: &Map<String, Value>,
    after: &Map<String, Value>,
) -> Result<CanonicalFieldChangeSet, ProviderError> {
    CanonicalFieldChangeSet::new(changed_frontmatter(before, after))
}

pub(crate) fn canonical_present_fields(
    value: &Map<String, Value>,
) -> Result<CanonicalFieldChangeSet, ProviderError> {
    let mut fields = BTreeSet::new();
    collect_visible_fields(&Value::Object(value.clone()), "", &mut fields);
    CanonicalFieldChangeSet::new(fields)
}

fn collect_visible_fields(value: &Value, pointer: &str, fields: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) if !object.is_empty() => {
            for (key, value) in object {
                let child = child_pointer(pointer, key);
                collect_visible_fields(value, &child, fields);
            }
        }
        _ if !pointer.is_empty() => {
            fields.insert(pointer.to_string());
        }
        _ => {}
    }
}

fn collect_changed_fields(
    before: Option<&Value>,
    after: Option<&Value>,
    pointer: &str,
    changed: &mut BTreeSet<String>,
) {
    if before == after {
        return;
    }
    match (before, after) {
        (Some(Value::Object(before)), Some(Value::Object(after))) => {
            for key in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
                let child = child_pointer(pointer, key);
                collect_changed_fields(before.get(key), after.get(key), &child, changed);
            }
        }
        _ if !pointer.is_empty() => {
            changed.insert(pointer.to_string());
        }
        _ => {
            let value = after.or(before).expect("different roots have a value");
            collect_visible_fields(value, pointer, changed);
        }
    }
}

fn child_pointer(parent: &str, key: &str) -> String {
    let escaped = key.replace('~', "~0").replace('/', "~1");
    format!("{parent}/{escaped}")
}

fn collection_path(path: &str) -> Result<CollectionPath, ProviderError> {
    CollectionPath::new(path).map_err(|error| ProviderError::InvalidChangeSet(error.to_string()))
}

fn revision(value: &str) -> Result<Revision, ProviderError> {
    Revision::parse(value).map_err(|error| ProviderError::InvalidChangeSet(error.to_string()))
}

fn resource_kind(kind: CollectionSnapshotResourceKind) -> ResourceChangeKind {
    match kind {
        CollectionSnapshotResourceKind::Configuration => ResourceChangeKind::Configuration,
        CollectionSnapshotResourceKind::Type => ResourceChangeKind::TypeDefinition,
        CollectionSnapshotResourceKind::Contract => ResourceChangeKind::Contract,
        CollectionSnapshotResourceKind::View => ResourceChangeKind::ViewSource,
        CollectionSnapshotResourceKind::Lock | CollectionSnapshotResourceKind::Schema => {
            ResourceChangeKind::File
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot(records: Vec<CollectionSnapshotRecord>) -> CollectionSnapshot {
        CollectionSnapshot {
            revision: "snapshot".to_string(),
            resource_revision: "resources".to_string(),
            spec_version: "0.3.0".to_string(),
            resources: vec![],
            records,
        }
    }

    fn record(
        path: &str,
        revision: &str,
        frontmatter: Value,
        body: &str,
    ) -> CollectionSnapshotRecord {
        CollectionSnapshotRecord {
            path: path.to_string(),
            revision: revision.to_string(),
            frontmatter: frontmatter.as_object().cloned().unwrap(),
            body: body.to_string(),
            types: vec!["task".to_string()],
            document: String::new(),
            frontmatter_error: None,
        }
    }

    #[test]
    fn exact_record_diff_carries_fields_types_body_and_revisions() {
        let before = snapshot(vec![record(
            "task.md",
            "sha256:before",
            json!({"status": "open", "nested": {"old": true}}),
            "Before",
        )]);
        let after = snapshot(vec![record(
            "task.md",
            "sha256:after",
            json!({"status": "done", "nested": {"new": true}}),
            "After",
        )]);

        let batch = canonical_changes(&before, &after, None).unwrap();
        let page = batch
            .page(
                None,
                std::num::NonZeroUsize::new(10).unwrap(),
                std::num::NonZeroUsize::new(10).unwrap(),
            )
            .unwrap();
        let CanonicalChange::Record(change) = &page.items[0] else {
            panic!("expected record change");
        };
        assert_eq!(change.kind, RecordChangeKind::Updated);
        assert!(change.body_changed);
        assert_eq!(
            change.changed_fields.iter().collect::<Vec<_>>(),
            vec!["/nested/new", "/nested/old", "/status"]
        );
        assert_eq!(change.before_types.iter().collect::<Vec<_>>(), vec!["task"]);
        assert_eq!(
            change.before_revision.as_ref().unwrap().as_str(),
            "sha256:before"
        );
        assert_eq!(
            change.after_revision.as_ref().unwrap().as_str(),
            "sha256:after"
        );
    }

    #[test]
    fn requested_rename_is_one_proven_change_plus_reference_updates() {
        let before = snapshot(vec![
            record("old.md", "sha256:old", json!({"title": "Old"}), "Body"),
            record("link.md", "sha256:link1", json!({"link": "old.md"}), ""),
        ]);
        let after = snapshot(vec![
            record("new.md", "sha256:new", json!({"title": "Old"}), "Body"),
            record("link.md", "sha256:link2", json!({"link": "new.md"}), ""),
        ]);
        let request = OperationRequest::new(
            OperationKind::Rename,
            json!({"from": "old.md", "to": "new.md", "update_references": true}),
        );

        let batch = canonical_changes(&before, &after, Some(&request)).unwrap();
        let page = batch
            .page(
                None,
                std::num::NonZeroUsize::new(10).unwrap(),
                std::num::NonZeroUsize::new(10).unwrap(),
            )
            .unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(page.items.iter().any(|change| matches!(
            change,
            CanonicalChange::Record(RecordChange {
                kind: RecordChangeKind::Renamed,
                from: Some(from),
                path,
                ..
            }) if from.as_str() == "old.md" && path.as_str() == "new.md"
        )));
    }
}
