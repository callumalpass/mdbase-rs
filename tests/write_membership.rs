use mdbase::Collection;
use serde_json::json;
use std::{fs, path::Path};

fn write(root: &Path, path: &str, text: &str) {
    let target = root.join(path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(target, text).unwrap();
}
fn fixture(explicit: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    write(root.path(),"mdbase.yaml",&format!("spec_version: \"0.3.0\"\nsettings:\n  contracts_folder: contracts\n  explicit_type_keys: [{explicit}]\n"));
    write(
        root.path(),
        "contracts/note.md",
        r#"---
kind: mdbase.contract
contract_type: record
id: example.note
version: 1.0.0
record_schema:
  dialect: json-schema-2020-12
  value: {type: object}
---
"#,
    );
    write(
        root.path(),
        "_types/note.md",
        r#"---
kind: mdbase.type
name: note
version: 1
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    required: [kind, title]
    properties:
      kind: {const: note}
      title: {type: string}
implements:
  - contract: example.note
    version: 1.0.0
    fields: {}
---
"#,
    );
    root
}

#[test]
fn exact_contract_create_persists_custom_membership_and_reopens() {
    let root = fixture("kind");
    let collection = Collection::open(root.path()).unwrap();
    let result=collection.v03_operations().unwrap().create(&json!({"path":"note.md","contract":"example.note","contract_version":"1.0.0","frontmatter":{"title":"Hello"}}));
    assert!(result.valid, "{:?}", result.diagnostics);
    assert_eq!(result.result["frontmatter"]["kind"], "note");
    let reopened = Collection::open(root.path()).unwrap();
    let read = reopened
        .v03_operations()
        .unwrap()
        .read(&json!({"path":"note.md"}));
    assert!(read.valid);
    assert_eq!(read.result["types"], json!(["note"]));
}

#[test]
fn malformed_contract_envelopes_never_create_a_file() {
    for request in [
        json!({"contract":"example.note"}),
        json!({"contract":null,"contract_version":"1.0.0"}),
        json!({"contract":"example.note","contract_version":"^1"}),
    ] {
        let root = fixture("kind");
        let collection = Collection::open(root.path()).unwrap();
        let mut request = request;
        request["path"] = json!("bad.md");
        request["frontmatter"] = json!({"title":"bad"});
        let result = collection.v03_operations().unwrap().create(&request);
        assert!(!result.valid);
        assert!(!root.path().join("bad.md").exists());
    }
}

#[test]
fn unknown_explicit_membership_fails_without_writing() {
    let root = fixture("kind");
    let collection = Collection::open(root.path()).unwrap();
    let result = collection
        .v03_operations()
        .unwrap()
        .create(&json!({"path":"bad.md","frontmatter":{"kind":"missing","title":"bad"}}));
    assert!(!result.valid);
    assert_eq!(result.diagnostics[0].code, "unknown_type");
    assert!(!root.path().join("bad.md").exists());
}
