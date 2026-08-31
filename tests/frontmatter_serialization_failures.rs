use std::fs;

use mdbase::Collection;
use serde_json::json;

const COMPLEX_TAGGED_ENTRY: &str = "? !key\n  nested: key\n: !value\n  nested: value\n";

fn document(frontmatter: &str, body: &str) -> String {
    format!("---\n{frontmatter}---\n{body}")
}

fn collection() -> (tempfile::TempDir, Collection) {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.2.0\nsettings:\n  write_nulls: omit\n",
    )
    .unwrap();
    let collection = Collection::open(root.path()).unwrap();
    (root, collection)
}

#[test]
fn v03_create_preparation_reports_serialization_failure_without_writing() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: '0.3.0'\nsettings:\n  validation: error\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(
        root.path().join("_types/generated.md"),
        "---\nkind: mdbase.type\nname: generated\nversion: 1\nschema:\n  dialect: json-schema-2020-12\n  value: { type: object }\nlifecycle:\n  on_create:\n    set: { marker: { literal: created } }\n---\n",
    )
    .unwrap();
    let collection = Collection::open(root.path()).unwrap();
    let authored = document(
        "holder:\n  ? !key\n    nested: key\n  : !value\n    nested: value\n",
        "body\n",
    );

    let result = collection.v03_operations().unwrap().create(&json!({
        "type": "generated",
        "path": "created.md",
        "document": authored,
    }));
    assert!(!result.valid, "{result:#?}");
    assert_eq!(
        result.diagnostics[0].code, "frontmatter_serialization_failed",
        "{result:#?}"
    );
    assert!(!root.path().join("created.md").exists());
}

#[test]
fn update_and_batch_report_serialization_failure_without_writing() {
    let (root, collection) = collection();
    let path = root.path().join("record.md");
    let authored = document(COMPLEX_TAGGED_ENTRY, "body\n");
    fs::write(&path, &authored).unwrap();

    let update = collection.update(&json!({"path": "record.md", "body": "changed"}));
    assert_eq!(
        update["error"]["code"], "frontmatter_serialization_failed",
        "{update:#}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), authored);

    let batch_authored = document(
        &format!("{COMPLEX_TAGGED_ENTRY}remove: present\n"),
        "body\n",
    );
    fs::write(&path, &batch_authored).unwrap();
    let dry = collection.batch_update(
        &json!({"where": "true", "fields": {"remove": null}, "dry_run": true}),
        None,
        false,
    );
    assert_eq!(
        dry["batch_result"]["details"][0]["error"]["code"], "frontmatter_serialization_failed",
        "{dry:#}"
    );
    let run = collection.batch_update(
        &json!({"where": "true", "fields": {"remove": null}}),
        None,
        false,
    );
    assert_eq!(dry["batch_result"]["failed"], run["batch_result"]["failed"]);
    assert_eq!(
        run["batch_result"]["details"][0]["error"]["code"], "frontmatter_serialization_failed",
        "{run:#}"
    );
    assert_eq!(fs::read_to_string(path).unwrap(), batch_authored);
}

#[test]
fn rename_keeps_unemittable_referrer_bytes_and_reports_partial_failure() {
    let (root, collection) = collection();
    fs::write(root.path().join("target.md"), "target\n").unwrap();
    let referrer = root.path().join("referrer.md");
    let tagged_ref = "holder:\n  ? !key\n    nested: key\n  : !value\n    nested: '[[target]]'\n";
    let authored = document(tagged_ref, "body\n");
    fs::write(&referrer, &authored).unwrap();

    let result = collection.rename(&json!({
        "from": "target.md",
        "to": "renamed.md",
        "update_refs": true
    }));
    assert_eq!(
        result["error"]["code"], "rename_ref_update_failed",
        "{result:#}"
    );
    assert_eq!(
        result["partial_updates"]["failed"][0]["reason"], "frontmatter_serialization_failed",
        "{result:#}"
    );
    assert!(!root.path().join("target.md").exists());
    assert!(root.path().join("renamed.md").exists());
    assert_eq!(fs::read_to_string(referrer).unwrap(), authored);
}

#[test]
fn backfill_preflights_unemittable_rewrite_and_noop_records_are_not_fatal() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.2.0\nsettings:\n  write_defaults: false\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(
        root.path().join("_types/item.md"),
        "---\nname: item\nmatch:\n  path_glob: 'record.md'\nfields:\n  filled: { type: string, default: yes }\n---\n",
    )
    .unwrap();
    let path = root.path().join("record.md");
    let authored = document(COMPLEX_TAGGED_ENTRY, "body\n");
    fs::write(&path, &authored).unwrap();
    let collection = Collection::open(root.path()).unwrap();

    let dry = collection.backfill(&json!({"type": "item", "dry_run": true}));
    assert_eq!(
        dry["batch_result"]["details"][0]["error"]["code"], "frontmatter_serialization_failed",
        "{dry:#}"
    );
    let run = collection.backfill(&json!({"type": "item"}));
    assert_eq!(
        run["batch_result"]["details"][0]["error"]["code"], "frontmatter_serialization_failed",
        "{run:#}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), authored);

    let noop = collection.backfill(&json!({
        "type": "item",
        "fields": ["not-missing"]
    }));
    assert_eq!(noop["batch_result"]["failed"], 0, "{noop:#}");
    assert_eq!(fs::read_to_string(path).unwrap(), authored);
}
