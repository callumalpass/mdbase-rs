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

fn add_throwing_type(root: &Path, name: &str) {
    write(
        root,
        &format!("_types/{name}.md"),
        &format!(
            "---\nkind: mdbase.type\nname: {name}\nmatch:\n  expr:\n    $expr: '\"not-a-number\" - 1 > 1'\nschema:\n  dialect: json-schema-2020-12\n  value: {{type: object}}\n---\n"
        ),
    );
}

#[test]
fn selected_membership_never_evaluates_unrelated_throwing_rules() {
    for contract in [false, true] {
        let root = fixture("kind");
        add_throwing_type(root.path(), "throwing");
        let collection = Collection::open(root.path()).unwrap();
        let mut request = json!({"path":"selected.md","type":"note","frontmatter":{"title":"ok"}});
        if contract {
            request["contract"] = json!("example.note");
            request["contract_version"] = json!("1.0.0");
        }
        let result = collection.v03_operations().unwrap().create(&request);
        assert!(result.valid, "{result:#?}");
        assert!(result
            .diagnostics
            .iter()
            .all(|d| d.code != "expression_evaluation_error"));
    }
}

#[test]
fn malformed_explicit_declarations_are_complete_sorted_errors() {
    for declaration in [
        json!(""),
        json!([]),
        json!(["", 7, "missing"]),
        json!("missing"),
    ] {
        let root = fixture("kind");
        add_throwing_type(root.path(), "throwing");
        let collection = Collection::open(root.path()).unwrap();
        let result = collection.v03_operations().unwrap().create(&json!({
            "path":"bad.md", "frontmatter":{"kind":declaration,"title":"bad"}
        }));
        assert!(!result.valid);
        assert!(!root.path().join("bad.md").exists());
        assert!(result
            .diagnostics
            .iter()
            .all(|d| d.code == "invalid_type_declaration" || d.code == "unknown_type"));
        let order = result
            .diagnostics
            .iter()
            .map(|d| (&d.field, &d.type_name, &d.message))
            .collect::<Vec<_>>();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(order, sorted);
    }
}

#[test]
fn implicit_throwing_errors_are_complete_sorted_and_explicit_repairs_skip_them() {
    let root = fixture("kind");
    add_throwing_type(root.path(), "z_throw");
    add_throwing_type(root.path(), "a_throw");
    let collection = Collection::open(root.path()).unwrap();
    let failed = collection.v03_operations().unwrap().create(&json!({
        "path":"implicit.md", "frontmatter":{"title":"bad"}
    }));
    assert!(!failed.valid);
    assert_eq!(
        failed
            .diagnostics
            .iter()
            .map(|d| d.type_name.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("a_throw"), Some("z_throw")]
    );
    assert!(!root.path().join("implicit.md").exists());

    write(
        root.path(),
        "repair.md",
        "---\nkind: missing\ntitle: old\n---\n",
    );
    let repaired = collection.v03_operations().unwrap().update(&json!({
        "path":"repair.md", "patch":{"kind":"note","title":"fixed"}
    }));
    assert!(repaired.valid, "{repaired:#?}");
}

#[test]
fn persistence_prefers_secondary_key_over_scalar_shape_change() {
    let root = fixture("kind, types");
    write(root.path(), "_types/aux.md", "---\nkind: mdbase.type\nname: aux\nschema:\n  dialect: json-schema-2020-12\n  value: {type: object}\n---\n");
    write(root.path(), "_types/note.md", "---\nkind: mdbase.type\nname: note\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    required: [title]\nimplements:\n  - contract: example.note\n    version: 1.0.0\n    fields: {}\n---\n");
    let collection = Collection::open(root.path()).unwrap();
    let result = collection.v03_operations().unwrap().create(&json!({
        "path":"multi.md", "type":"note", "frontmatter":{"kind":"aux","title":"ok"}
    }));
    assert!(result.valid, "{result:#?}");
    assert_eq!(result.result["frontmatter"]["kind"], "aux");
    assert_eq!(result.result["frontmatter"]["types"], "note");
    assert_eq!(result.result["types"], json!(["aux", "note"]));
}

#[test]
fn occupied_scalar_without_a_secondary_key_fails_before_write() {
    let root = fixture("kind");
    write(root.path(), "_types/aux.md", "---\nkind: mdbase.type\nname: aux\nschema:\n  dialect: json-schema-2020-12\n  value: {type: object}\n---\n");
    let collection = Collection::open(root.path()).unwrap();
    let result = collection.v03_operations().unwrap().create(&json!({
        "path":"blocked.md", "type":"note", "frontmatter":{"kind":"aux","title":"ok"}
    }));
    assert!(!result.valid);
    assert_eq!(
        result.diagnostics[0].code,
        "type_membership_persistence_failed"
    );
    assert!(!root.path().join("blocked.md").exists());
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

#[test]
fn selected_membership_without_keys_never_reopens_implicitly() {
    let root = fixture("kind");
    write(root.path(), "mdbase.yaml", "spec_version: \"0.3.0\"\nsettings:\n  contracts_folder: contracts\n  explicit_type_keys: []\n");
    let collection = Collection::open(root.path()).unwrap();
    let result = collection.v03_operations().unwrap().create(&json!({
        "path":"no.md", "type":"note", "frontmatter":{"kind":"note","title":"matches"}
    }));
    assert!(!result.valid);
    assert_eq!(
        result.diagnostics[0].code,
        "type_membership_persistence_failed"
    );
    assert!(!root.path().join("no.md").exists());
    drop(collection);
    let reopened = Collection::open(root.path()).unwrap();
    assert!(
        !reopened
            .v03_operations()
            .unwrap()
            .read(&json!({"path":"no.md"}))
            .valid
    );
}

#[test]
fn selected_type_owns_derived_path_over_sorted_auxiliary() {
    let root = fixture("types");
    write(
        root.path(),
        "_types/note.md",
        r#"---
kind: mdbase.type
name: note
collection:
  path:
    pattern: notes/{title}.md
schema:
  dialect: json-schema-2020-12
  value: {type: object}
implements:
  - contract: example.note
    version: 1.0.0
    fields: {}
---
"#,
    );
    write(
        root.path(),
        "_types/aux.md",
        r#"---
kind: mdbase.type
name: aux
collection:
  path:
    pattern: wrong/{title}.md
schema:
  dialect: json-schema-2020-12
  value: {type: object}
---
"#,
    );
    for contract in [false, true] {
        let collection = Collection::open(root.path()).unwrap();
        let title = if contract { "contract" } else { "top" };
        let mut request = json!({"type":"note","frontmatter":{"types":["aux"],"title":title}});
        if contract {
            request["contract"] = json!("example.note");
            request["contract_version"] = json!("1.0.0");
        }
        let result = collection.v03_operations().unwrap().create(&request);
        assert!(result.valid, "{result:#?}");
        assert!(root.path().join(format!("notes/{title}.md")).exists());
        assert!(!root.path().join(format!("wrong/{title}.md")).exists());
    }
}

#[test]
fn update_implicit_matching_receives_only_canonical_path() {
    let root = fixture("kind");
    write(root.path(), "mdbase.yaml", "spec_version: \"0.3.0\"\nsettings:\n  contracts_folder: contracts\n  explicit_type_keys: []\n");
    write(
        root.path(),
        "_types/note.md",
        r#"---
kind: mdbase.type
name: note
match:
  expr:
    $expr: 'file.path == "notes/a.md"'
schema:
  dialect: json-schema-2020-12
  value: {type: object}
---
"#,
    );
    write(root.path(), "notes/a.md", "---\ntitle: old\n---\nbody\n");
    let collection = Collection::open(root.path()).unwrap();
    for path in ["notes/a.md", "notes\\a.md"] {
        let result = collection
            .v03_operations()
            .unwrap()
            .update(&json!({"path":path,"fields":{"title":"new"}}));
        assert!(result.valid, "{path}: {result:#?}");
    }
    let bytes = fs::read(root.path().join("notes/a.md")).unwrap();
    let rejected = collection
        .v03_operations()
        .unwrap()
        .update(&json!({"path":"../notes/a.md","fields":{"title":"bad"}}));
    assert!(!rejected.valid);
    assert_eq!(fs::read(root.path().join("notes/a.md")).unwrap(), bytes);
}
