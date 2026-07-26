use std::fs;

use mdbase::api::{
    CollectionPath, CreateRequest, DeleteRequest, MdbaseError, QueryDirection, QueryRequest,
    ReadRequest, RenameRequest, Revision, UpdateRequest,
};
use mdbase::{Collection, CompatibilityMode};
use serde_json::json;

fn typed_collection() -> (tempfile::TempDir, Collection) {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  timezone: UTC\n  validation: error\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(
        root.path().join("_types/task.md"),
        r#"---
kind: mdbase.type
name: task
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    required: [type, title]
    properties:
      type: { const: task }
      title: { type: string }
      status: { type: string }
---
"#,
    )
    .unwrap();
    let collection = Collection::open(root.path()).unwrap();
    (root, collection)
}

#[test]
fn typed_crud_query_and_revision_failures_are_structured() {
    let (_root, collection) = typed_collection();
    assert_eq!(
        collection.compatibility_mode(),
        CompatibilityMode::Canonical
    );
    assert_eq!(collection.root(), _root.path());
    assert!(collection.types().contains_key("task"));
    let api = collection.typed().unwrap();
    let original_path = CollectionPath::new(r"tasks\first.md").unwrap();

    let created = api
        .create(
            CreateRequest::new(
                original_path.clone(),
                json!({"type": "task", "title": "First"}),
            )
            .with_body("Body"),
        )
        .unwrap();
    assert!(created.diagnostics.is_empty(), "{created:#?}");
    assert_eq!(created.value.path, original_path);
    let initial_revision = created.value.revision.unwrap();

    let read = api
        .read(ReadRequest::new("tasks/first.md").unwrap())
        .unwrap();
    assert_eq!(read.value.frontmatter["title"], "First");
    assert_eq!(read.value.body, "Body\n");
    assert_eq!(read.value.revision, initial_revision);

    let mut update = UpdateRequest::new(
        CollectionPath::new("tasks/first.md").unwrap(),
        json!({"status": "done"}),
    );
    update.if_revision = Some(initial_revision.clone());
    let updated = api.update(update).unwrap();
    let updated_revision = updated.value.revision.unwrap();
    assert_ne!(updated_revision, initial_revision);

    let query = api
        .query(
            QueryRequest::builder()
                .type_name("task")
                .where_expression("status == 'done'")
                .order_by("file.path", QueryDirection::Asc)
                .limit(10),
        )
        .unwrap();
    assert_eq!(query.value.total_count, 1);
    assert_eq!(query.value.records[0]["file"]["path"], "tasks/first.md");

    let mut stale = UpdateRequest::new(
        CollectionPath::new("tasks/first.md").unwrap(),
        json!({"status": "blocked"}),
    );
    stale.if_revision = Some(Revision::parse("sha256:stale").unwrap());
    let error = api.update(stale).unwrap_err();
    assert!(matches!(error, MdbaseError::Operation { .. }));
    assert_eq!(
        error.diagnostics()[0].code.as_str(),
        "concurrent_modification"
    );

    let renamed_path = CollectionPath::new("archive/first.md").unwrap();
    let mut rename = RenameRequest::new(original_path, renamed_path.clone());
    rename.if_revision = Some(updated_revision);
    let renamed = api.rename(rename).unwrap();
    assert_eq!(renamed.value.to, renamed_path);

    let deleted = api.delete(DeleteRequest::new(renamed_path)).unwrap();
    assert!(deleted.value.deleted);
}
