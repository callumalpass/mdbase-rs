//! Strict writes capture a collection snapshot only when a write can conflict.

fn strict_collection(type_fields: &str) -> (tempfile::TempDir, crate::Collection) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.2.0\nsettings:\n  validation: error\n",
    )
    .unwrap();
    std::fs::create_dir(root.path().join("_types")).unwrap();
    std::fs::write(
        root.path().join("_types/item.md"),
        format!("---\nname: item\nfields:\n{type_fields}---\n"),
    )
    .unwrap();
    std::fs::write(
        root.path().join("one.md"),
        "---\ntype: item\ntitle: One\nkey: taken\nid: first\n---\n",
    )
    .unwrap();
    let collection = crate::Collection::open(root.path()).unwrap();
    (root, collection)
}

fn captures() -> usize {
    crate::snapshot::SNAPSHOT_CAPTURES.with(std::cell::Cell::get)
}

#[test]
fn strict_writes_skip_the_collection_snapshot_when_nothing_can_conflict() {
    let (_root, collection) = strict_collection("  title: { type: string }\n");
    let before = captures();
    let created = collection.create(&serde_json::json!({
        "path": "two.md", "type": "item", "fields": {"title": "Two"}
    }));
    assert_eq!(created["valid"], true, "{created:#}");
    let updated =
        collection.update(&serde_json::json!({"path": "two.md", "fields": {"title": "Second"}}));
    assert!(updated.get("error").is_none(), "{updated:#}");
    assert_eq!(updated["frontmatter"]["title"], "Second");
    assert_eq!(captures(), before);
}

#[test]
fn strict_writes_still_reject_duplicate_unique_fields() {
    let (_root, collection) = strict_collection("  key: { type: string, unique: true }\n");
    let created = collection.create(&serde_json::json!({
        "path": "two.md", "type": "item", "fields": {"key": "taken"}
    }));
    assert_eq!(created["error"]["code"], "validation_failed", "{created:#}");
    let fresh = collection.create(&serde_json::json!({
        "path": "two.md", "type": "item", "fields": {"key": "free"}
    }));
    assert_eq!(fresh["valid"], true, "{fresh:#}");
    let updated =
        collection.update(&serde_json::json!({"path": "two.md", "fields": {"key": "taken"}}));
    assert_eq!(updated["error"]["code"], "validation_failed", "{updated:#}");
}

#[test]
fn strict_writes_still_reject_duplicate_ids_without_unique_fields() {
    let (_root, collection) = strict_collection("  title: { type: string }\n");
    let created = collection.create(&serde_json::json!({
        "path": "two.md", "type": "item", "fields": {"id": "first"}
    }));
    assert_eq!(created["error"]["code"], "validation_failed", "{created:#}");
}

#[test]
fn strict_writes_check_ids_supplied_by_defaults() {
    let (_root, collection) = strict_collection("  id: { type: string, default: first }\n");
    let created = collection.create(&serde_json::json!({
        "path": "two.md", "type": "item", "fields": {}
    }));
    assert_eq!(created["error"]["code"], "validation_failed", "{created:#}");
}
