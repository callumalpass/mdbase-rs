use std::fs;

use mdbase::api::{
    BatchOperation, BatchRequest, CollectionPath, CreateRequest, DeleteRequest, MdbaseError,
    QueryDirection, QueryRequest, ReadRequest, RenameRequest, Revision, UpdateRequest,
    V02MigrationRequest,
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
    assert_eq!(created.value.frontmatter["title"], "First");
    assert_eq!(created.value.effective_frontmatter["title"], "First");
    assert_eq!(created.value.body, "Body\n");
    assert_eq!(created.value.file.name, "first.md");
    let initial_revision = created.value.revision;

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
    assert_eq!(updated.value.frontmatter["status"], "done");
    assert_eq!(updated.value.effective_frontmatter["status"], "done");
    let updated_revision = updated.value.revision;
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
    let rename_preview = api.preflight_rename(rename.clone()).unwrap();
    assert!(rename_preview.value.would_rename);
    assert_eq!(rename_preview.value.to, renamed_path);
    assert!(api
        .read(ReadRequest::new("tasks/first.md").unwrap())
        .is_ok());
    let renamed = api.rename(rename).unwrap();
    assert_eq!(renamed.value.to, renamed_path);
    assert_eq!(renamed.value.document.path, renamed_path);
    assert_eq!(renamed.value.document.frontmatter["status"], "done");
    assert_eq!(renamed.value.document.file.name, "first.md");

    let delete_request = DeleteRequest::new(renamed_path.clone());
    let delete_preview = api.preflight_delete(delete_request.clone()).unwrap();
    assert!(delete_preview.value.would_delete);
    assert!(api
        .read(ReadRequest::new("archive/first.md").unwrap())
        .is_ok());
    let deleted = api.delete(delete_request).unwrap();
    assert!(deleted.value.deleted);
}

#[test]
fn typed_non_partial_batch_commits_all_mutations_together() {
    let (root, collection) = typed_collection();
    fs::create_dir_all(root.path().join("tasks")).unwrap();
    fs::write(
        root.path().join("tasks/existing.md"),
        "---\ntype: task\ntitle: Existing\nstatus: open\n---\n",
    )
    .unwrap();
    let api = collection.typed().unwrap();
    let request = BatchRequest::new(vec![
        BatchOperation::Update(UpdateRequest::new(
            CollectionPath::new("tasks/existing.md").unwrap(),
            json!({"status": "done"}),
        )),
        BatchOperation::Create(CreateRequest::new(
            CollectionPath::new("tasks/created.md").unwrap(),
            json!({"type": "task", "title": "Created"}),
        )),
    ])
    .unwrap();

    let outcome = api.batch(request).unwrap();
    assert_eq!(outcome.value.succeeded, 2);
    assert_eq!(outcome.value.failed, 0);
    assert!(!outcome.value.preflight);
    assert!(root.path().join("tasks/created.md").is_file());
    assert!(fs::read_to_string(root.path().join("tasks/existing.md"))
        .unwrap()
        .contains("status: done"));
}

#[test]
fn typed_v02_adapter_is_read_only_until_migration() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.2.0\nsettings:\n  default_validation: error\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(
        root.path().join("_types/task.md"),
        "---\nname: task\nfields:\n  title: { type: string, required: true }\n  status: { type: string, default: open }\n---\n",
    )
    .unwrap();
    fs::write(
        root.path().join("legacy.md"),
        "---\ntype: task\ntitle: Legacy\n---\nBody\n",
    )
    .unwrap();
    let collection = Collection::open(root.path()).unwrap();
    assert_eq!(
        collection.compatibility_mode(),
        CompatibilityMode::V02ReadOnly
    );
    let api = collection.typed().unwrap();

    let read = api.read(ReadRequest::new("legacy.md").unwrap()).unwrap();
    assert!(read.value.frontmatter.get("status").is_none());
    assert_eq!(read.value.effective_frontmatter["status"], "open");
    let query = api
        .query(QueryRequest::builder().type_name("task"))
        .unwrap();
    assert_eq!(query.value.total_count, 1);

    let error = api
        .update(UpdateRequest::new(
            CollectionPath::new("legacy.md").unwrap(),
            json!({"status": "done"}),
        ))
        .unwrap_err();
    assert!(matches!(
        error,
        MdbaseError::MigrationRequired {
            operation: "update"
        }
    ));
    assert!(!fs::read_to_string(root.path().join("legacy.md"))
        .unwrap()
        .contains("status: done"));
}

#[test]
fn v02_migration_is_verified_dry_runnable_and_enables_canonical_writes() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.2.0\nname: Legacy\nsettings:\n  default_validation: error\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(
        root.path().join("_types/task.yaml"),
        r#"---
name: task
fields:
  title: { type: string, required: true }
  status: { type: string, default: open }
  label: { type: string, computed: "title + '!'"}
  uid: { type: string, generated: uuid }
---
"#,
    )
    .unwrap();
    let record = "---\ntype: task\ntitle: Legacy\n---\nBody\n";
    fs::write(root.path().join("legacy.md"), record).unwrap();
    let collection = Collection::open(root.path()).unwrap();
    let api = collection.typed().unwrap();

    let plan = api
        .migrate_v02(V02MigrationRequest {
            dry_run: true,
            allow_lossy: false,
        })
        .unwrap();
    assert!(!plan.applied);
    assert_eq!(plan.verified_records, 1);
    assert!(plan.changes.iter().any(|change| {
        change.path == "_types/task.yaml"
            && change.before_revision.is_some()
            && change.after_revision.is_none()
    }));
    assert_eq!(
        fs::read_to_string(root.path().join("mdbase.yaml"))
            .unwrap()
            .lines()
            .next(),
        Some("spec_version: 0.2.0")
    );
    assert!(!root.path().join(&plan.manifest_path).exists());

    let applied = api
        .migrate_v02(V02MigrationRequest {
            dry_run: false,
            allow_lossy: false,
        })
        .unwrap();
    assert!(applied.applied);
    assert!(root.path().join(&applied.manifest_path).is_file());
    assert!(!root.path().join("_types/task.yaml").exists());
    assert_eq!(
        fs::read_to_string(root.path().join("legacy.md")).unwrap(),
        record
    );

    drop(collection);
    let canonical = Collection::open(root.path()).unwrap();
    assert_eq!(canonical.compatibility_mode(), CompatibilityMode::Canonical);
    let api = canonical.typed().unwrap();
    let read = api.read(ReadRequest::new("legacy.md").unwrap()).unwrap();
    assert!(read.value.frontmatter.get("status").is_none());
    assert_eq!(read.value.effective_frontmatter["status"], "open");
    assert_eq!(read.value.effective_frontmatter["label"], "Legacy!");

    let created = api
        .create(CreateRequest::new(
            CollectionPath::new("new.md").unwrap(),
            json!({"type": "task", "title": "New"}),
        ))
        .unwrap();
    assert!(created.value.frontmatter["uid"]
        .as_str()
        .is_some_and(|value| uuid::Uuid::parse_str(value).is_ok()));
}

#[test]
fn lossy_v02_migration_requires_explicit_apply_opt_in() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.2.0\n").unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(
        root.path().join("_types/item.md"),
        "---\nname: item\nfields:\n  sequence: { type: integer, generated: sequence }\n---\n",
    )
    .unwrap();
    fs::write(root.path().join("item.md"), "---\ntype: item\n---\n").unwrap();
    let collection = Collection::open(root.path()).unwrap();
    let api = collection.typed().unwrap();

    let plan = api
        .migrate_v02(V02MigrationRequest {
            dry_run: true,
            allow_lossy: false,
        })
        .unwrap();
    assert!(plan
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code.as_str() == "migration_lossy"));

    let error = api
        .migrate_v02(V02MigrationRequest {
            dry_run: false,
            allow_lossy: false,
        })
        .unwrap_err();
    assert!(matches!(error, MdbaseError::LossyMigration { .. }));
    assert!(fs::read_to_string(root.path().join("mdbase.yaml"))
        .unwrap()
        .contains("0.2.0"));

    let applied = api
        .migrate_v02(V02MigrationRequest {
            dry_run: false,
            allow_lossy: true,
        })
        .unwrap();
    assert!(applied.applied);
}
