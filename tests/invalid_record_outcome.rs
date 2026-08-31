#![cfg(feature = "legacy-collection-mutation")]

use std::fs;

use mdbase::Collection;
use serde_json::{json, Value};
use tempfile::TempDir;

fn collection() -> (TempDir, Collection) {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  timezone: UTC\n",
    )
    .unwrap();
    fs::write(
        root.path().join("healthy.md"),
        "---\ntitle: Healthy\n---\nBody\n",
    )
    .unwrap();
    fs::write(
        root.path().join("broken.md"),
        "---\ntitle: [broken\n---\nBody\n",
    )
    .unwrap();
    let opened = Collection::open(root.path()).unwrap();
    (root, opened)
}

fn query(collection: &Collection) -> mdbase::v03::OperationResult {
    collection
        .v03_operations()
        .unwrap()
        .query(&json!({"frontmatter_mode": "persisted"}))
}

fn scoped_collection() -> (TempDir, Collection) {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("_types")).unwrap();
    fs::create_dir_all(root.path().join("records")).unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  timezone: UTC\n",
    )
    .unwrap();
    fs::write(
        root.path().join("_types/path-scope.md"),
        "---\nkind: mdbase.type\nname: path-scope\nmatch:\n  path_glob: records/**/*.md\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    properties:\n      private:\n        type: string\n        default: should-never-leak\n---\n",
    )
    .unwrap();
    fs::write(
        root.path().join("_types/frontmatter-scope.md"),
        "---\nkind: mdbase.type\nname: frontmatter-scope\nmatch:\n  fields_present: [secret]\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n",
    )
    .unwrap();
    fs::write(
        root.path().join("records/healthy.md"),
        "---\nsecret: visible\ngroup: one\nscore: 3\n---\nPublic body\n",
    )
    .unwrap();
    fs::write(
        root.path().join("records/broken.md"),
        "---\nsecret: [private\ngroup: hidden\nscore: 999\n---\nPrivate body\n",
    )
    .unwrap();
    let collection = Collection::open(root.path()).unwrap();
    (root, collection)
}

fn normalized(result: &mdbase::v03::OperationResult) -> Value {
    json!({
        "results": result.result["results"],
        "diagnostics": result.result["diagnostics"],
    })
}

#[test]
fn legacy_batch_and_backfill_never_rewrite_opaque_records() {
    let (batch_root, batch_collection) = scoped_collection();
    let broken_before = fs::read(batch_root.path().join("records/broken.md")).unwrap();
    let preview = batch_collection.batch_update(
        &json!({"query": {"types": ["path-scope"]}, "fields": {"updated": true}, "dry_run": true}),
        None,
        false,
    );
    assert_eq!(preview["batch_result"]["failed"], 1, "{preview:#}");
    assert_eq!(
        fs::read(batch_root.path().join("records/broken.md")).unwrap(),
        broken_before
    );
    let batch = batch_collection.batch_update(
        &json!({"query": {"types": ["path-scope"]}, "fields": {"updated": true}}),
        None,
        false,
    );
    assert_eq!(batch["batch_result"]["total"], 2, "{batch:#}");
    assert_eq!(batch["batch_result"]["succeeded"], 1, "{batch:#}");
    assert_eq!(batch["batch_result"]["failed"], 1, "{batch:#}");
    assert_eq!(
        fs::read(batch_root.path().join("records/broken.md")).unwrap(),
        broken_before
    );
    assert!(
        fs::read_to_string(batch_root.path().join("records/healthy.md"))
            .unwrap()
            .contains("updated: true")
    );

    let (backfill_root, backfill_collection) = scoped_collection();
    let broken_before = fs::read(backfill_root.path().join("records/broken.md")).unwrap();
    let preview = backfill_collection.backfill(&json!({"type": "path-scope", "dry_run": true}));
    assert_eq!(preview["batch_result"]["failed"], 1, "{preview:#}");
    assert_eq!(
        fs::read(backfill_root.path().join("records/broken.md")).unwrap(),
        broken_before
    );
    let backfill = backfill_collection.backfill(&json!({"type": "path-scope"}));
    assert_eq!(backfill["batch_result"]["total"], 2, "{backfill:#}");
    assert_eq!(backfill["batch_result"]["failed"], 1, "{backfill:#}");
    assert_eq!(
        fs::read(backfill_root.path().join("records/broken.md")).unwrap(),
        broken_before
    );
    assert!(backfill["batch_result"]["details"]
        .as_array()
        .unwrap()
        .iter()
        .any(|detail| detail["path"] == "records/healthy.md" && detail["status"] == "success"));
}

#[test]
fn cached_and_forced_disk_queries_keep_siblings_and_invalid_stubs_in_parity() {
    let (_root, collection) = collection();
    let disk = query(&collection);
    assert!(disk.valid, "{disk:#?}");
    assert_eq!(disk.result["results"].as_array().unwrap().len(), 2);
    let stub = disk.result["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["path"] == "broken.md")
        .unwrap();
    assert!(stub.get("frontmatter").is_none());
    assert!(stub.get("effective_frontmatter").is_none());
    assert!(stub["file"]["revision"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(stub.get("body").is_none());
    assert_eq!(disk.diagnostics.len(), 1);
    assert_eq!(disk.diagnostics[0].code, "invalid_frontmatter");
    assert_eq!(disk.diagnostics[0].path.as_deref(), Some("broken.md"));
    assert_eq!(
        disk.diagnostics[0].details.as_ref().unwrap()["reason"],
        "invalid_yaml"
    );

    assert_eq!(collection.cache_rebuild()["success"], true);
    let cached = query(&collection);
    assert!(cached.valid, "{cached:#?}");
    assert_eq!(normalized(&cached), normalized(&disk));
}

#[test]
fn invalid_stub_scope_privacy_and_totals_are_bounded() {
    let (_root, collection) = scoped_collection();
    let operations = collection.v03_operations().unwrap();

    let simple = operations.query(&json!({"frontmatter_mode": "persisted"}));
    assert!(simple.valid, "{simple:#?}");
    assert_eq!(simple.result["meta"]["total_count"], 2);
    let stub = simple.result["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["path"] == "records/broken.md")
        .unwrap();
    assert_eq!(stub["types"], json!(["path-scope"]));
    assert!(stub.get("frontmatter").is_none());
    assert!(stub.get("effective_frontmatter").is_none());
    assert!(stub.get("body").is_none());
    assert!(stub.get("reason").is_none());
    assert!(!stub.to_string().contains("private"));
    assert!(!stub.to_string().contains("999"));

    let path_scoped = operations.query(&json!({
        "types": ["path-scope"],
        "frontmatter_mode": "persisted"
    }));
    assert!(path_scoped.valid, "{path_scoped:#?}");
    assert_eq!(path_scoped.result["meta"]["total_count"], 2);
    assert_eq!(path_scoped.diagnostics.len(), 1);

    let frontmatter_scoped = operations.query(&json!({
        "types": ["frontmatter-scope"],
        "frontmatter_mode": "persisted"
    }));
    assert!(frontmatter_scoped.valid, "{frontmatter_scoped:#?}");
    assert_eq!(frontmatter_scoped.result["meta"]["total_count"], 1);
    assert!(frontmatter_scoped.diagnostics.is_empty());

    for input in [
        json!({"where": "secret == 'visible'", "frontmatter_mode": "persisted"}),
        json!({"projections": {"copy": {"expr": "secret"}}}),
        json!({
            "group_by": [{"field": "group"}],
            "summaries": [{"field": "score", "function": "sum", "name": "total"}]
        }),
        json!({"where": "file.body.contains('Public')", "include_body": true}),
    ] {
        let result = operations.query(&input);
        assert!(result.valid, "{input:#}: {result:#?}");
        assert_eq!(result.result["meta"]["total_count"], 1, "{input:#}");
        assert!(result.diagnostics.is_empty(), "{input:#}: {result:#?}");
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("Private body"));
        assert!(!serialized.contains("999"));
    }

    assert_eq!(collection.cache_rebuild()["success"], true);
    let cached = operations.query(&json!({"types": ["path-scope"]}));
    assert_eq!(cached.result["meta"]["total_count"], 2);
    assert_eq!(cached.diagnostics.len(), 1);
}

#[test]
fn full_validation_reports_yaml_and_binary_without_aborting_siblings() {
    let (root, collection) = collection();
    fs::write(root.path().join("binary.md"), b"---\ntitle: \xff\n---\n").unwrap();
    let result = collection.v03_operations().unwrap().validate(&json!({}));
    assert!(!result.valid, "{result:#?}");
    assert!(result.diagnostics.iter().any(|diagnostic| {
        diagnostic.path.as_deref() == Some("broken.md")
            && diagnostic.details.as_ref().unwrap()["reason"] == "invalid_yaml"
    }));
    assert!(result.diagnostics.iter().any(|diagnostic| {
        diagnostic.path.as_deref() == Some("binary.md")
            && diagnostic.details.as_ref().unwrap()["reason"] == "invalid_utf8"
    }));
    assert!(!result
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.path.as_deref() == Some("healthy.md")));
}

#[test]
fn targeted_validation_preserves_file_read_failed_outcome() {
    let (root, collection) = collection();
    fs::create_dir(root.path().join("unreadable.md")).unwrap();

    let result = collection.validate_op(&json!({"path": "unreadable.md"}));
    assert_eq!(result["valid"], false, "{result:#}");
    assert_eq!(result["path"], "unreadable.md");
    assert_eq!(result["issues"][0]["code"], "file_read_failed");
    assert_ne!(result["issues"][0]["code"], "invalid_frontmatter");
}

#[test]
fn cache_commits_invalid_rows_and_repair_converges_to_parsed_state() {
    let (root, collection) = collection();
    assert_eq!(collection.cache_rebuild()["success"], true);
    fs::write(
        root.path().join("broken.md"),
        "---\ntitle: Fixed\n---\nBody\n",
    )
    .unwrap();
    let repaired = query(&collection);
    assert!(repaired.valid, "{repaired:#?}");
    assert!(repaired.diagnostics.is_empty());
    let fixed = repaired.result["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["path"] == "broken.md")
        .unwrap();
    assert_eq!(fixed["frontmatter"]["title"], "Fixed");
    assert!(fixed["file"].get("revision").is_none());
}

#[test]
fn valid_raw_document_create_preserves_exact_source_bytes() {
    let (root, collection) = collection();
    let source = "\u{feff}---\r\ntitle: 'Exact' # preserve comment\r\ncustom: null\r\n---\r\nBody with CRLF.\r\n";
    let created = collection
        .v03_operations()
        .unwrap()
        .create(&json!({"path": "exact-create.md", "document": source}));
    assert!(created.valid, "{created:#?}");
    assert_eq!(
        fs::read(root.path().join("exact-create.md")).unwrap(),
        source.as_bytes()
    );
}

#[test]
fn malformed_raw_document_create_and_update_are_atomic() {
    let (root, collection) = collection();
    let operations = collection.v03_operations().unwrap();
    let invalid = [
        "---\ntitle: [broken\n---\nBody\n",
        "---\ntitle: one\ntitle: two\n---\nBody\n",
        "---\n- not\n- a mapping\n---\nBody\n",
    ];
    for (index, document) in invalid.into_iter().enumerate() {
        let path = format!("new-{index}.md");
        let created = operations.create(&json!({"path": path, "document": document}));
        assert!(!created.valid);
        assert_eq!(created.diagnostics[0].code, "invalid_frontmatter");
        assert!(!root.path().join(path).exists());

        let before = fs::read(root.path().join("healthy.md")).unwrap();
        let before_revision =
            operations.read(&json!({"path": "healthy.md"})).result["revision"].clone();
        let updated = operations.update(&json!({"path": "healthy.md", "document": document}));
        assert!(!updated.valid);
        assert_eq!(updated.diagnostics[0].code, "invalid_frontmatter");
        assert_eq!(fs::read(root.path().join("healthy.md")).unwrap(), before);
        assert_eq!(
            operations.read(&json!({"path": "healthy.md"})).result["revision"],
            before_revision
        );
    }
}

#[test]
fn full_replacement_repairs_malformed_record_but_patch_cannot() {
    let (root, collection) = collection();
    let operations = collection.v03_operations().unwrap();
    let path = root.path().join("repair.md");
    let malformed = b"---\ntitle: [broken\n---\nOpaque\n";
    fs::write(&path, malformed).unwrap();

    let patched = operations.update(&json!({"path": "repair.md", "patch": {"title": "No"}}));
    assert!(!patched.valid);
    assert_eq!(patched.diagnostics[0].code, "invalid_frontmatter");
    assert_eq!(fs::read(&path).unwrap(), malformed);

    let replacement = "---\ntitle: Repaired\n---\nVisible\n";
    let repaired = operations.update(&json!({
        "path": "repair.md",
        "document": replacement,
        "include_document": true
    }));
    assert!(repaired.valid, "{repaired:#?}");
    assert_eq!(fs::read(&path).unwrap(), replacement.as_bytes());
}

#[test]
fn full_replacement_repairs_nonmapping_and_invalid_utf8_records() {
    let (root, collection) = collection();
    let operations = collection.v03_operations().unwrap();
    for (path, original) in [
        (
            "nonmapping.md",
            b"---\n- one\n- two\n---\nOpaque\n".as_slice(),
        ),
        ("binary.md", b"---\ntitle: \xff\n---\nOpaque\n".as_slice()),
    ] {
        fs::write(root.path().join(path), original).unwrap();
        let patched = operations.update(&json!({"path": path, "patch": {"title": "No"}}));
        assert!(!patched.valid);
        assert_eq!(patched.diagnostics[0].code, "invalid_frontmatter");
        assert_eq!(fs::read(root.path().join(path)).unwrap(), original);

        let replacement = format!("---\ntitle: Repaired {path}\n---\nVisible\n");
        let repaired = operations.update(&json!({
            "path": path,
            "document": replacement,
            "include_document": true
        }));
        assert!(repaired.valid, "{path}: {repaired:#?}");
        assert_eq!(
            fs::read(root.path().join(path)).unwrap(),
            replacement.as_bytes()
        );
    }
}

#[test]
fn invalid_parse_is_reclassified_without_mtime_change_after_cache_rebuild() {
    let (_root, collection) = collection();
    assert_eq!(collection.cache_rebuild()["success"], true);
    let result = query(&collection);
    assert!(result.valid, "{result:#?}");
    let validation = collection.validate_op(&json!({"path": "broken.md"}));
    assert_eq!(validation["valid"], false);
    assert_eq!(validation["issues"][0]["code"], "invalid_frontmatter");
}
