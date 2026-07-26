use std::fs;

use mdbase::Collection;

fn write_type_collection(type_file: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.2.0\nsettings:\n  validation: error\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(root.path().join("_types/task.md"), type_file).unwrap();
    root
}

#[test]
fn invalid_computed_expression_rejects_the_registry_at_open() {
    let root = write_type_collection(
        r#"---
name: task
fields:
  title: { type: string }
  broken:
    type: string
    computed: "title +"
---
"#,
    );

    let error = match Collection::open(root.path()) {
        Ok(_) => panic!("invalid computed expression must reject the registry"),
        Err(error) => error,
    };
    assert_eq!(error["error"]["code"], "invalid_type_definition");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Computed field 'broken' is invalid"));
}

#[test]
fn computed_dependencies_come_from_the_ast_and_run_in_compiled_order() {
    let root = write_type_collection(
        r#"---
name: task
fields:
  title: { type: string }
  identity:
    type: string
    computed: "'id'"
  id:
    type: string
    computed: "'identity'"
  first:
    type: string
    computed: "title.upper()"
  second:
    type: string
    computed: "first + '-SECOND'"
---
"#,
    );
    fs::write(
        root.path().join("record.md"),
        "---\ntype: task\ntitle: hello\n---\n",
    )
    .unwrap();

    let collection = Collection::open(root.path()).unwrap();
    let read = collection.read(&serde_json::json!({"path": "record.md"}));
    assert_eq!(read["frontmatter"]["identity"], "id");
    assert_eq!(read["frontmatter"]["id"], "identity");
    assert_eq!(read["frontmatter"]["first"], "HELLO");
    assert_eq!(read["frontmatter"]["second"], "HELLO-SECOND");
}
