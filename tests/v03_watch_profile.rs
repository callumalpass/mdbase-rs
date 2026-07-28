use std::fs;
use std::time::Duration;

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
