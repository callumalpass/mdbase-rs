//! Single-record mutations stage only definitions; schemas they reference must
//! come along wherever they live.

use std::fs;
use std::path::Path;
use std::time::Duration;

use mdbase::runtime::{FilesystemRuntime, OperationKind, OperationRequest};
use serde_json::json;

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn referenced_schemas_outside_the_schemas_folder_reach_the_mutation_stage() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(root, "mdbase.yaml", "spec_version: 0.3.0\n");
    write(
        root,
        "schemas/example.note/1.0.0.schema.json",
        r#"{"type": "object", "properties": {"title": {"type": "string"}}}"#,
    );
    write(
        root,
        "_contracts/example.note/1.0.0.md",
        "---\nkind: mdbase.contract\ncontract_type: record\nid: example.note\nversion: 1.0.0\nrecord_schema:\n  dialect: json-schema-2020-12\n  ref: ../../schemas/example.note/1.0.0.schema.json\n---\n",
    );
    write(
        root,
        "_types/note.md",
        "---\nkind: mdbase.type\nname: note\nversion: 1\nmatch: {where: {type: note}}\nschema: {dialect: json-schema-2020-12, value: {type: object, properties: {title: {type: string}}}}\nimplements: [{contract: example.note, version: 1.0.0, fields: {title: title}}]\n---\n",
    );
    let runtime = FilesystemRuntime::open(root, Duration::from_millis(5)).unwrap();

    let created = runtime
        .execute(&OperationRequest::new(
            OperationKind::Create,
            json!({"path": "notes/a.md", "frontmatter": {"type": "note", "title": "A"}}),
        ))
        .unwrap();

    assert!(created.valid, "{created:?}");
}
