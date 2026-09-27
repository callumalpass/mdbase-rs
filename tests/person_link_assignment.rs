//! Link-valued person references resolve through the engine's own link rules,
//! so applications can find "assigned to me" without reimplementing resolution.
use std::fs;

use mdbase::Collection;
use serde_json::json;
use tempfile::TempDir;

fn write(root: &TempDir, path: &str, contents: &str) {
    let target = root.path().join(path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, contents).unwrap();
}

fn collection() -> (TempDir, Collection) {
    let root = tempfile::tempdir().unwrap();
    write(&root, "mdbase.yaml", "spec_version: 0.3.0\nsettings:\n  timezone: UTC\n");
    write(
        &root,
        "_types/task.md",
        "---\nkind: mdbase.type\nname: task\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    properties:\n      type: { const: task }\n      assignees: { type: array, items: { type: string } }\n      owners: { type: array, items: { type: string } }\ncollection:\n  links:\n    assignees[]: {}\n---\n",
    );
    write(
        &root,
        "_types/person.md",
        "---\nkind: mdbase.type\nname: person\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    properties:\n      type: { const: person }\n      name: { type: string }\n---\n",
    );
    write(&root, "people/Alice Smith.md", "---\ntype: person\nname: Alice\n---\n");
    write(&root, "people/bob.md", "---\ntype: person\nname: Bob\n---\n");
    write(&root, "tasks/a.md", "---\ntype: task\nassignees: [\"[[Alice Smith]]\"]\nowners: [\"[[Alice Smith]]\"]\n---\n");
    write(&root, "tasks/b.md", "---\ntype: task\nassignees: [\"[[people/bob]]\", \"[[Nobody]]\"]\n---\n");
    write(&root, "tasks/c.md", "---\ntype: task\nassignees: [\"[[people/Alice Smith|Al]]\"]\n---\n");
    let opened = Collection::open(root.path()).unwrap();
    (root, opened)
}

fn paths(result: &mdbase::v03::OperationResult) -> Vec<String> {
    assert!(result.valid, "{result:#?}");
    result.result["results"].as_array().unwrap().iter()
        .map(|record| record["path"].as_str().unwrap().to_owned()).collect()
}

#[test]
fn declared_link_lists_filter_by_resolved_person_path() {
    let (_root, collection) = collection();
    let result = collection.v03_operations().unwrap().query(&json!({
        "types": ["task"],
        "where": "assignees.exists(a, a.asFile() != null && a.asFile().file.path == 'people/Alice Smith.md')",
        "order_by": [{ "field": "file.path" }]
    }));
    assert_eq!(paths(&result), ["tasks/a.md", "tasks/c.md"]);
}

#[test]
fn record_index_form_works_for_any_field_name() {
    // Applications build filters from each type's local field name, which may
    // not be a CEL identifier, so they index `record` instead.
    let (_root, collection) = collection();
    let result = collection.v03_operations().unwrap().query(&json!({
        "types": ["task"],
        "where": "\"assignees\" in record && record[\"assignees\"].exists(a, a.asFile() != null && a.asFile().file.path == \"people/Alice Smith.md\")",
        "order_by": [{ "field": "file.path" }]
    }));
    assert_eq!(paths(&result), ["tasks/a.md", "tasks/c.md"]);
}

#[test]
fn projections_return_resolved_targets_and_null_for_unresolved_links() {
    let (_root, collection) = collection();
    let result = collection.v03_operations().unwrap().query(&json!({
        "types": ["task"],
        "where": "file.path == 'tasks/b.md'",
        "projections": {
            "links": { "expr": "\"assignees\" in raw ? raw[\"assignees\"] : []" },
            "targets": { "expr": "\"assignees\" in record ? record[\"assignees\"].map(a, a.asFile() == null ? null : a.asFile().file.path) : []" }
        },
        "select": ["projection.links", "projection.targets"]
    }));
    assert!(result.valid, "{result:#?}");
    // Raw link text and resolved targets line up by index.
    assert_eq!(result.result["results"][0]["values"]["links"], json!(["[[people/bob]]", "[[Nobody]]"]));
    assert_eq!(result.result["results"][0]["values"]["targets"], json!(["people/bob.md", null]));
}
