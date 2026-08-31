use std::fs;
use std::sync::{Arc, Barrier};

use mdbase::Collection;
use serde_json::json;

fn generated_collection(field: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.2.0\nsettings:\n  validation: error\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(
        root.path().join("_types/item.md"),
        format!("---\nname: item\nmatch:\n  path_glob: '*.md'\nfields:\n  value: {field}\n---\n"),
    )
    .unwrap();
    root
}

#[test]
fn backfill_batch_reserves_distinct_sequence_values() {
    let root = generated_collection("{ type: integer, generated: sequence }");
    fs::write(root.path().join("zero.md"), "---\nbroken: [yaml\n---\n").unwrap();
    fs::write(
        root.path().join("existing.md"),
        "---\ntype: item\nvalue: 7\n---\n",
    )
    .unwrap();
    for path in ["one.md", "two.md"] {
        fs::write(root.path().join(path), "---\ntype: item\n---\n").unwrap();
    }
    let collection = Collection::open(root.path()).unwrap();
    let result = collection.backfill(&json!({
        "type": "item",
        "apply": {"defaults": false, "generated": true}
    }));
    assert_eq!(result["batch_result"]["succeeded"], 3, "{result:#}");
    assert_eq!(result["batch_result"]["failed"], 1, "{result:#}");
    let one = collection.read(&json!({"path": "one.md"}));
    let two = collection.read(&json!({"path": "two.md"}));
    assert_eq!(one["frontmatter"]["value"], 8);
    assert_eq!(two["frontmatter"]["value"], 9);
}

#[test]
fn generated_schema_failure_writes_nothing() {
    let root = generated_collection("{ type: string, generated: uuid, pattern: '^never$' }");
    let collection = Collection::open(root.path()).unwrap();
    let result = collection.create(&json!({
        "path": "invalid.md",
        "type": "item",
        "frontmatter": {}
    }));
    assert_eq!(result["error"]["code"], "validation_failed", "{result:#}");
    assert!(!root.path().join("invalid.md").exists());
}

#[test]
fn sequence_start_is_a_floor_and_overflow_is_typed() {
    let root = generated_collection("{ type: integer, generated: { sequence: { start: 100 } } }");
    fs::write(
        root.path().join("existing.md"),
        "---\ntype: item\nvalue: 7\n---\n",
    )
    .unwrap();
    let collection = Collection::open(root.path()).unwrap();
    let created = collection.create(&json!({
        "path": "floor.md",
        "type": "item",
        "frontmatter": {}
    }));
    assert_eq!(created["frontmatter"]["value"], 100, "{created:#}");

    fs::write(
        root.path().join("maximum.md"),
        format!("---\ntype: item\nvalue: {}\n---\n", i64::MAX),
    )
    .unwrap();
    let collection = Collection::open(root.path()).unwrap();
    let overflow = collection.create(&json!({
        "path": "overflow.md",
        "type": "item",
        "frontmatter": {}
    }));
    assert_eq!(
        overflow["error"]["code"], "generated_sequence_overflow",
        "{overflow:#}"
    );
    assert!(!root.path().join("overflow.md").exists());
}

#[test]
fn generated_derived_field_can_read_an_unmaterialized_default() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.2.0\nsettings:\n  validation: error\n  write_defaults: false\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(
        root.path().join("_types/item.md"),
        "---\nname: item\nfields:\n  source: { type: string, default: 'Hello World' }\n  slug:\n    type: string\n    generated: { from: source, transform: slugify }\n---\n",
    )
    .unwrap();
    let collection = Collection::open(root.path()).unwrap();
    let result = collection.create(&json!({
        "path": "derived.md",
        "type": "item",
        "frontmatter": {}
    }));
    assert_eq!(result["frontmatter"]["slug"], "hello-world", "{result:#}");
    let persisted = fs::read_to_string(root.path().join("derived.md")).unwrap();
    assert!(persisted.contains("slug: hello-world"), "{persisted}");
    assert!(!persisted.contains("source:"), "{persisted}");
}

#[test]
fn backfill_unique_defaults_fail_before_any_write() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.2.0\nsettings:\n  validation: error\n  write_defaults: true\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(
        root.path().join("_types/item.md"),
        "---\nname: item\nmatch: { path_glob: '*.md' }\nfields:\n  key: { type: string, default: same, unique: true }\n---\n",
    )
    .unwrap();
    for path in ["one.md", "two.md"] {
        fs::write(root.path().join(path), "---\ntype: item\n---\n").unwrap();
    }
    let before = fs::read(root.path().join("one.md")).unwrap();
    let collection = Collection::open(root.path()).unwrap();
    let result = collection.backfill(&json!({"type": "item"}));
    assert_eq!(result["error"]["code"], "validation_failed", "{result:#}");
    assert_eq!(fs::read(root.path().join("one.md")).unwrap(), before);
    assert!(!fs::read_to_string(root.path().join("two.md"))
        .unwrap()
        .contains("key:"));
}

#[test]
fn batch_uniqueness_uses_effective_defaults_and_coercions() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.2.0\nsettings:\n  validation: error\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(
        root.path().join("_types/item.md"),
        "---\nname: item\nfields:\n  number: { type: integer, unique: true }\n  bucket: { type: string, default: common, unique: true }\n---\n",
    )
    .unwrap();
    fs::write(
        root.path().join("one.md"),
        "---\ntype: item\nnumber: '7'\n---\n",
    )
    .unwrap();
    fs::write(root.path().join("two.md"), "---\ntype: item\n---\n").unwrap();
    let collection = Collection::open(root.path()).unwrap();
    let result = collection.batch_update(
        &json!({"updates": [{"path": "two.md", "fields": {"number": 7}}]}),
        None,
        false,
    );
    assert_eq!(result["error"]["code"], "validation_failed", "{result:#}");
    assert!(!fs::read_to_string(root.path().join("two.md"))
        .unwrap()
        .contains("number:"));
}

#[test]
fn explicit_path_infers_generated_type_and_create_checks_effective_uniqueness() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.2.0\nsettings:\n  validation: error\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::create_dir(root.path().join("records")).unwrap();
    fs::write(
        root.path().join("_types/item.md"),
        "---\nname: item\nmatch: { path_glob: 'records/*.md' }\nfields:\n  uid: { type: string, generated: uuid }\n  bucket: { type: string, default: common, unique: true }\n---\n",
    )
    .unwrap();
    let collection = Collection::open(root.path()).unwrap();
    let first = collection.create(&json!({"path": "records/one.md", "frontmatter": {}}));
    assert_eq!(first["types"][0], "item", "{first:#}");
    assert!(first["frontmatter"]["uid"].is_string(), "{first:#}");
    let second = collection.create(&json!({"path": "records/two.md", "frontmatter": {}}));
    assert_eq!(second["error"]["code"], "validation_failed", "{second:#}");
    assert!(!root.path().join("records/two.md").exists());
}

#[test]
fn concurrent_creates_retain_the_pre_phase3_sequence_race_contract() {
    let root = generated_collection("{ type: integer, generated: sequence }");
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for path in ["left.md", "right.md"] {
        let root_path = root.path().to_path_buf();
        let barrier = Arc::clone(&barrier);
        workers.push(std::thread::spawn(move || {
            let collection = Collection::open(&root_path).unwrap();
            barrier.wait();
            collection.create(&json!({
                "path": path,
                "type": "item",
                "frontmatter": {}
            }))
        }));
    }
    barrier.wait();
    for worker in workers {
        let result = worker.join().unwrap();
        assert!(result.get("error").is_none(), "{result:#}");
    }
    let collection = Collection::open(root.path()).unwrap();
    for path in ["left.md", "right.md"] {
        assert!(collection.read(&json!({"path": path}))["frontmatter"]["value"].is_i64());
    }
    // Sequence allocation is snapshot-based before the collection write lock.
    // Atomic uniqueness between concurrent creates remains a Phase 3 concern.
}
