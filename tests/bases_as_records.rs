//! Obsidian Bases stored as YAML document records and claimed through the
//! `obsidian.base` contract (spec Obsidian Bases adapter, "Bases as records").

use std::fs;
use std::path::Path;

use mdbase::runtime::FilesystemProvider;
use mdbase::Collection;
use serde_json::json;

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn collection() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(
        root,
        "mdbase.yaml",
        "spec_version: 0.3.0\nsettings:\n  record_extensions: [md, base]\n",
    );
    write(
        root,
        "_contracts/obsidian.base.md",
        "---\nkind: mdbase.contract\ncontract_type: record\nid: obsidian.base\nversion: 1.0.0\nrecord_schema: {dialect: json-schema-2020-12, value: {type: object, required: [views], properties: {filters: {}, views: {type: array}}}}\n---\n",
    );
    write(
        root,
        "_types/obsidian-base.md",
        "---\nkind: mdbase.type\nname: obsidian_base\nversion: 1\nmatch: {path_glob: 'views/**/*.base'}\nschema: {dialect: json-schema-2020-12, value: {type: object, properties: {filters: {}, views: {type: array}}}}\nimplements: [{contract: obsidian.base, version: 1.0.0, fields: {filters: filters, views: views}}]\n---\n",
    );
    write(
        root,
        "views/open.base",
        "filters:\n  and:\n    - 'status == \"open\"'\nviews:\n  - type: table\n    name: Open\n    order: [file.name]\n",
    );
    write(
        root,
        "other/unclaimed.base",
        "views:\n  - type: table\n    name: Unclaimed\n",
    );
    write(root, "notes/a.md", "---\nstatus: open\n---\nA\n");
    write(root, "notes/b.md", "---\nstatus: done\n---\nB\n");
    directory
}

#[test]
fn a_base_record_implementing_the_contract_is_listed_and_executed() {
    let directory = collection();
    let collection = Collection::open(directory.path()).unwrap();
    let operations = collection.v03_operations().unwrap();

    let listed = operations.list_views(&json!({}));
    assert!(listed.valid, "{listed:?}");
    let sources = listed.result["views"].as_array().unwrap();
    assert_eq!(sources.len(), 1, "{sources:#?}");
    assert_eq!(sources[0]["source"]["path"], "views/open.base");
    assert_eq!(sources[0]["source"]["format"], "obsidian.base");

    let executed = operations.execute_view(&json!({"path": "views/open.base", "view": "open"}));
    assert!(executed.valid, "{executed:?}");
    let paths = executed.result["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(paths, ["notes/a.md"]);

    let unclaimed =
        operations.execute_view(&json!({"path": "other/unclaimed.base", "view": "unclaimed"}));
    assert!(!unclaimed.valid);
    assert_eq!(unclaimed.diagnostics[0].code, "view_not_found");
}

#[test]
fn base_records_are_snapshot_records_not_view_resources() {
    let directory = collection();
    let snapshot = FilesystemProvider::open(directory.path())
        .unwrap()
        .snapshot()
        .unwrap();
    let record = snapshot
        .records
        .iter()
        .find(|record| record.path == "views/open.base")
        .expect("the Base is a record");
    assert_eq!(record.types, ["obsidian_base"]);
    assert_eq!(record.body, "");
    assert!(!snapshot
        .resources
        .iter()
        .any(|resource| resource.path.ends_with(".base")));
}

#[test]
fn creating_a_yaml_document_record_with_a_body_is_rejected_before_writing() {
    let directory = collection();
    let collection = Collection::open(directory.path()).unwrap();
    let created = collection.v03_operations().unwrap().create(&json!({
        "path": "views/new.base",
        "frontmatter": {"views": [{"type": "table", "name": "New"}]},
        "body": "not allowed",
    }));
    assert!(!created.valid);
    assert_eq!(created.diagnostics[0].code, "invalid_request");
    assert!(!directory.path().join("views/new.base").exists());
}
