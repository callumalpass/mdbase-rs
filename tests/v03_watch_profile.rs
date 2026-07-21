use std::fs;
use std::time::Duration;

use mdbase::runtime_contracts::{ContractDocument, ContractSource, LoadOptions};
use mdbase::watch::{CollectionWatcher, WatchKind};
use mdbase::Collection;
use serde_json::json;

fn setup() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
    )
    .unwrap();
    root
}

#[test]
fn portable_watcher_observes_core_writes_external_changes_and_renames() {
    let root = setup();
    let collection = Collection::open(root.path()).unwrap();
    let operations = collection.v03_operations().unwrap();
    let watcher = CollectionWatcher::open(root.path(), Duration::from_millis(25)).unwrap();

    let created = operations.create(&json!({
        "path": "notes/example.md",
        "frontmatter": {"title": "Core write", "status": "open"},
        "body": "First body\n"
    }));
    assert!(created.valid, "{created:#?}");
    watcher.rescan().unwrap();

    let event = watcher
        .recv_portable_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("created notification");
    assert_eq!(event.kind, WatchKind::RecordCreated);
    assert_eq!(event.path.as_deref(), Some("notes/example.md"));
    assert_eq!(event.frontmatter.as_ref().unwrap()["title"], "Core write");
    assert_eq!(event.changed_fields.unwrap().len(), 2);
    assert!(event.id.starts_with("watch_"));

    // Multiple host writes before a rescan collapse into the final snapshot.
    fs::write(
        root.path().join("notes/example.md"),
        "---\ntitle: Intermediate\nstatus: open\n---\nSecond body\n",
    )
    .unwrap();
    fs::write(
        root.path().join("notes/example.md"),
        "---\ntitle: Final\nstatus: open\n---\nThird body\n",
    )
    .unwrap();
    watcher.rescan().unwrap();
    let event = watcher.recv_portable().unwrap();
    assert_eq!(event.kind, WatchKind::RecordModified);
    assert_eq!(event.changed_fields, Some(vec!["title".to_string()]));
    assert_eq!(event.frontmatter.unwrap()["title"], "Final");

    // A body-only change is observable but has no changed frontmatter fields.
    fs::write(
        root.path().join("notes/example.md"),
        "---\ntitle: Final\nstatus: open\n---\nFourth body\n",
    )
    .unwrap();
    watcher.rescan().unwrap();
    let event = watcher.recv_portable().unwrap();
    assert_eq!(event.kind, WatchKind::RecordModified);
    assert_eq!(event.changed_fields, Some(vec![]));

    fs::rename(
        root.path().join("notes/example.md"),
        root.path().join("notes/renamed.md"),
    )
    .unwrap();
    watcher.rescan().unwrap();
    let event = watcher.recv_portable().unwrap();
    assert_eq!(event.kind, WatchKind::RecordRenamed);
    assert_eq!(event.path.as_deref(), Some("notes/renamed.md"));
    assert_eq!(event.previous_path.as_deref(), Some("notes/example.md"));
    assert_eq!(event.frontmatter.unwrap()["title"], "Final");
}

#[test]
fn portable_watcher_tracks_resources_and_honors_collection_scope() {
    let root = setup();
    let watcher = CollectionWatcher::open(root.path(), Duration::from_millis(25)).unwrap();

    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(
        root.path().join("_types/task.md"),
        concat!(
            "---\nkind: mdbase.type\nname: task\n",
            "schema:\n  dialect: json-schema-2020-12\n",
            "  value:\n    type: object\n---\n"
        ),
    )
    .unwrap();
    watcher.rescan().unwrap();
    let event = watcher.recv_portable().unwrap();
    assert_eq!(event.kind, WatchKind::TypeChanged);
    assert_eq!(event.path.as_deref(), Some("_types/task.md"));

    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  validation: error\n",
    )
    .unwrap();
    watcher.rescan().unwrap();
    let event = watcher.recv_portable().unwrap();
    assert_eq!(event.kind, WatchKind::ConfigChanged);
    assert_eq!(event.path.as_deref(), Some("mdbase.yaml"));

    fs::create_dir(root.path().join(".git")).unwrap();
    fs::write(
        root.path().join(".git/ignored.md"),
        "---\ntitle: Ignored\n---\n",
    )
    .unwrap();
    watcher.rescan().unwrap();
    assert!(watcher
        .recv_portable_timeout(Duration::from_millis(100))
        .unwrap()
        .is_none());

    fs::create_dir(root.path().join("nested")).unwrap();
    fs::write(
        root.path().join("nested/mdbase.yaml"),
        "spec_version: 0.3.0\n",
    )
    .unwrap();
    fs::write(
        root.path().join("nested/hidden.md"),
        "---\ntitle: Nested collection\n---\n",
    )
    .unwrap();
    watcher.rescan().unwrap();
    assert!(watcher
        .recv_portable_timeout(Duration::from_millis(100))
        .unwrap()
        .is_none());
}

#[test]
fn runtime_aware_watcher_reports_effective_registry_recomposition() {
    let root = setup();
    let provider = ContractDocument::virtual_contract(json!({
        "type": "provider",
        "id": "local",
        "version": 1,
        "provider_version": "1.0.0",
        "name": "Local test provider",
        "contracts": {"events": ["local.changed"]}
    }));
    let watcher = CollectionWatcher::open_with_runtime_contracts(
        root.path(),
        Duration::from_millis(25),
        vec![ContractSource::built_in(vec![provider])],
        LoadOptions::default(),
    )
    .unwrap();

    let event = json!({
        "type": "event",
        "id": "local.changed",
        "version": 1,
        "provider": "local",
        "name": "Local change",
        "schemas": {
            "dialect": "json-schema-2020-12",
            "payload": {"type": "object"}
        }
    });
    fs::write(
        root.path().join("local-event.md"),
        format!("---\n{}---\n", serde_yaml::to_string(&event).unwrap()),
    )
    .unwrap();
    watcher.rescan().unwrap();

    let events = (0..2)
        .map(|_| {
            watcher
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .expect("record and registry notifications")
        })
        .collect::<Vec<_>>();
    assert_eq!(events[0].event_type, "mdbase.record.created");
    let changed = &events[1];
    assert_eq!(changed.event_type, "mdbase.runtime.registry.changed");
    assert_eq!(changed.payload["identity"], "effective_registry");
    assert_eq!(
        changed.payload["valid"], true,
        "unexpected runtime state: {}",
        changed.payload
    );
    assert!(changed.payload["revision"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(changed.payload.get("path").is_none());
    assert!(!changed
        .payload
        .to_string()
        .contains(root.path().to_string_lossy().as_ref()));

    let portable = changed.clone().into_portable();
    assert_eq!(portable.kind, WatchKind::RuntimeRegistryChanged);
    assert_eq!(portable.subject.as_deref(), Some("effective_registry"));
    assert!(portable.path.is_none());

    // A normal record changes the collection snapshot but leaves the runtime
    // registry alone, so no spurious registry notification is emitted.
    fs::write(root.path().join("ordinary.md"), "---\ntitle: Note\n---\n").unwrap();
    watcher.rescan().unwrap();
    assert_eq!(watcher.recv().unwrap().event_type, "mdbase.record.created");
    assert!(watcher
        .recv_timeout(Duration::from_millis(100))
        .unwrap()
        .is_none());
}
