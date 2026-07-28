use std::fs;
use std::path::Path;

use mdbase::Collection;
use serde_json::json;

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn contract() -> &'static str {
    r#"---
kind: mdbase.contract
id: example.note
version: 1.0.0
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    required: [title]
    additionalProperties: false
    properties:
      title: { type: string, minLength: 1 }
---
"#
}

fn implementing_type(name: &str, field: &str, version: u64) -> String {
    format!(
        r#"---
kind: mdbase.type
name: {name}
version: {version}
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    required: [type, {field}]
    properties:
      type: {{ const: {name} }}
      {field}: {{ type: string }}
implements:
  - contract: example.note
    version: 1.0.0
    fields: {{ title: {field} }}
---
"#
    )
}

#[test]
fn explicit_union_and_contract_views_are_stable_and_validated() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "mdbase.yaml",
        "spec_version: \"0.3.0\"\nsettings:\n  contracts_folder: contracts\n",
    );
    write(root.path(), "contracts/example.note.md", contract());
    write(
        root.path(),
        "_types/personal_note.md",
        &implementing_type("personal_note", "title", 1),
    );
    write(
        root.path(),
        "_types/work_note.md",
        &implementing_type("work_note", "summary", 2),
    );
    write(
        root.path(),
        "work.md",
        "---\ntype: work_note\nsummary: Work note\n---\n",
    );
    write(
        root.path(),
        "invalid.md",
        "---\ntype: work_note\nsummary: ''\n---\n",
    );

    let collection = Collection::open(root.path()).unwrap();
    let definitions = collection.list_data_contracts();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].id, "example.note");
    assert!(definitions[0].digest.starts_with("sha256:"));

    let implementations = collection.get_data_contract_implementations("example.note", "1.0.0");
    assert_eq!(
        implementations
            .iter()
            .map(|implementation| implementation.type_name.as_str())
            .collect::<Vec<_>>(),
        vec!["personal_note", "work_note"]
    );
    assert!(implementations
        .iter()
        .all(|implementation| implementation.implementation_digest.starts_with("sha256:")));

    let view = collection.get_contract_view("work.md", "example.note", "1.0.0", None);
    assert!(view.valid);
    assert_eq!(view.type_name, "work_note");
    assert_eq!(view.view, json!({"title": "Work note"}));

    let validation = collection
        .v03_operations()
        .unwrap()
        .validate(&json!({"path": "invalid.md"}));
    assert!(!validation.valid);
    assert!(validation
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "data_contract_record_invalid"));

    let query = collection.v03_operations().unwrap().query(&json!({}));
    let paths = query.result["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|record| record["path"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(paths, vec!["invalid.md", "work.md"]);
}

#[test]
fn missing_exact_contract_fails_collection_open() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "mdbase.yaml", "spec_version: \"0.3.0\"\n");
    write(
        root.path(),
        "_types/note.md",
        &implementing_type("note", "title", 1).replace("1.0.0", "2.0.0"),
    );
    let error = match Collection::open(root.path()) {
        Ok(_) => panic!("missing contract must fail closed"),
        Err(error) => error,
    };
    assert_eq!(
        error.pointer("/error/code"),
        Some(&json!("data_contract_not_found"))
    );
}

#[test]
fn conflicting_exact_contract_identity_fails_collection_open() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "mdbase.yaml", "spec_version: \"0.3.0\"\n");
    write(root.path(), "_contracts/a.md", contract());
    write(
        root.path(),
        "_contracts/b.md",
        &contract().replace("minLength: 1", "minLength: 2"),
    );
    let error = match Collection::open(root.path()) {
        Ok(_) => panic!("conflicting contracts must fail closed"),
        Err(error) => error,
    };
    assert_eq!(
        error.pointer("/error/code"),
        Some(&json!("data_contract_conflict"))
    );
}

#[test]
fn canonical_tasknotes_digests_match_the_spec_fixture() {
    let spec_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mdbase-spec");
    let collection_root = spec_root.join("examples/v0.3/tasknotes-migration/v0.3");
    if !collection_root.exists() {
        return;
    }
    let collection = Collection::open(&collection_root).unwrap();
    let definitions = collection.list_data_contracts();
    assert_eq!(
        definitions[0].digest,
        "sha256:7174b83651f68fd061b37f7fdf0c90f3a5e54ec87ed7c5e709fd7e3b9415c5c7"
    );
    let implementations = collection.get_data_contract_implementations("tasknotes.task", "0.2.0");
    assert_eq!(
        implementations[0].implementation_digest,
        "sha256:69e4f95ca2785c59756ab80bd4cc2f1f1498449221ed3d18de37f43f6972fcc8"
    );
}
