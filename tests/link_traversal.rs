//! `asFile()` follows links the way link resolution, backlinks and validation do.

use std::fs;

use mdbase::Collection;
use serde_json::{json, Value};
use tempfile::TempDir;

fn write(root: &TempDir, path: &str, content: &str) {
    let path = root.path().join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn record_type(name: &str, folder: &str, links: &str) -> String {
    format!(
        "---\nkind: mdbase.type\nname: {name}\nmatch:\n  path_glob: \"{folder}/**/*.md\"\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    properties:\n      id: {{ type: string }}\n      title: {{ type: string }}\n      source: {{ type: string }}\n{links}---\n"
    )
}

/// `[[alpha]]` names one source by ID and a note by filename; `[[gamma]]` names a source and a
/// note only by filename, and the link field accepts only sources.
fn collection() -> (TempDir, Collection) {
    let root = tempfile::tempdir().unwrap();
    write(
        &root,
        "mdbase.yaml",
        "spec_version: 0.3.0\nsettings:\n  timezone: UTC\n  validation: warn\n",
    );
    write(
        &root,
        "_types/source.md",
        &record_type("source", "sources", ""),
    );
    write(&root, "_types/note.md", &record_type("note", "notes", ""));
    write(
        &root,
        "_types/annotation.md",
        &record_type(
            "annotation",
            "annotations",
            "collection:\n  links:\n    source:\n      target_type: source\n",
        ),
    );
    write(
        &root,
        "sources/a.md",
        "---\nid: alpha\ntitle: Alpha source\n---\n",
    );
    write(
        &root,
        "notes/alpha.md",
        "---\nid: note-alpha\ntitle: Alpha note\n---\n",
    );
    write(
        &root,
        "sources/gamma.md",
        "---\nid: source-gamma\ntitle: Gamma source\n---\n",
    );
    write(
        &root,
        "notes/gamma.md",
        "---\nid: note-gamma\ntitle: Gamma note\n---\n",
    );
    write(
        &root,
        "annotations/1.md",
        "---\nid: ann-1\nsource: '[[alpha]]'\n---\n",
    );
    write(
        &root,
        "annotations/2.md",
        "---\nid: ann-2\nsource: '[[gamma]]'\n---\n",
    );
    let collection = Collection::open(root.path()).unwrap();
    (root, collection)
}

/// Runs a query on a fresh collection twice: first with the link graph built from records, then
/// with it read from the rebuilt cache. Both must agree.
fn query(input: Value) -> Vec<Value> {
    let (_root, collection) = collection();
    let operations = collection.v03_operations().unwrap();
    let run = |cached: bool| {
        let (result, profile) = operations.query_profiled(&input);
        assert!(result.valid, "{result:#?}");
        assert_eq!(profile.cache_used, cached);
        result.result["results"].as_array().unwrap().clone()
    };
    let built = run(false);
    assert_eq!(collection.cache_rebuild()["success"], true);
    assert_eq!(
        built,
        run(true),
        "the built and cached link graphs disagree"
    );
    built
}

fn targets(expression: &str) -> Vec<(String, Value)> {
    query(json!({
        "types": ["annotation"],
        "select": [{"name": "target", "expr": expression}],
        "order_by": [{"field": "id", "direction": "asc"}],
    }))
    .into_iter()
    .map(|row| {
        (
            row["file"]["path"].as_str().unwrap().to_string(),
            row["values"]["target"].clone(),
        )
    })
    .collect()
}

#[test]
fn as_file_prefers_a_configured_id_to_a_filename_and_honours_target_types() {
    assert_eq!(
        targets("source.asFile().file.path"),
        vec![
            ("annotations/1.md".to_string(), json!("sources/a.md")),
            ("annotations/2.md".to_string(), json!("sources/gamma.md")),
        ]
    );
    assert_eq!(
        targets("source.asFile().title"),
        vec![
            ("annotations/1.md".to_string(), json!("Alpha source")),
            ("annotations/2.md".to_string(), json!("Gamma source")),
        ]
    );
}

#[test]
fn as_file_agrees_with_backlinks() {
    let linked = query(json!({
        "where": "file.backlinks.size() > 0",
        "order_by": [{"field": "file.path", "direction": "asc"}],
    }))
    .into_iter()
    .map(|row| row["file"]["path"].as_str().unwrap().to_string())
    .collect::<Vec<_>>();
    assert_eq!(linked, vec!["sources/a.md", "sources/gamma.md"]);
}

#[test]
fn links_built_in_an_expression_use_the_same_resolution() {
    // Not a stored link of the candidate, so it resolves without a declared target type.
    assert_eq!(
        targets("\"[[alpha]]\".asFile().file.path"),
        vec![
            ("annotations/1.md".to_string(), json!("sources/a.md")),
            ("annotations/2.md".to_string(), json!("sources/a.md")),
        ]
    );
    assert_eq!(
        targets("\"[[missing]]\".asFile()"),
        vec![
            ("annotations/1.md".to_string(), Value::Null),
            ("annotations/2.md".to_string(), Value::Null),
        ]
    );
}
