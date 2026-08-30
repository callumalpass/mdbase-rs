use std::fs;

use mdbase::api::{
    BatchOperation, BatchRequest, CollectionPath, CreateRequest, DeleteRequest, FrontmatterMode,
    MdbaseError, QueryDirection, QueryRequest, ReadRequest, RenameRequest, Revision, UpdateRequest,
    V02MigrationRequest,
};
use mdbase::{Collection, CompatibilityMode};
use serde_json::{json, Value};

fn diagnostic_values<T: serde::Serialize>(diagnostics: &[T]) -> Value {
    let mut value = serde_json::to_value(diagnostics).unwrap();
    for diagnostic in value.as_array_mut().unwrap() {
        let object = diagnostic.as_object_mut().unwrap();
        if let Some(type_name) = object.remove("type_name") {
            object.insert("type".to_string(), type_name);
        }
        object.retain(|_, value| !value.is_null());
    }
    value
}

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
            CreateRequest::new(original_path.clone())
                .with_frontmatter(json!({"type": "task", "title": "First"}))
                .with_body("Body")
                .with_document(),
        )
        .unwrap();
    assert!(created.diagnostics.is_empty(), "{created:#?}");
    assert_eq!(created.value.path, original_path);
    assert_eq!(created.value.frontmatter["title"], "First");
    assert_eq!(created.value.effective_frontmatter["title"], "First");
    assert_eq!(created.value.body, "Body\n");
    assert!(created
        .value
        .document
        .as_deref()
        .is_some_and(|source| source.contains("title: First")));
    assert_eq!(created.value.file.name, "first.md");
    let initial_revision = created.value.revision;

    let read = api
        .read(ReadRequest::new("tasks/first.md").unwrap().with_document())
        .unwrap();
    assert_eq!(read.value.frontmatter["title"], "First");
    assert_eq!(read.value.body, "Body\n");
    assert_eq!(read.value.document, created.value.document);
    assert_eq!(read.value.revision, initial_revision);

    let mut update = UpdateRequest::new(
        CollectionPath::new("tasks/first.md").unwrap(),
        json!({"status": "done"}),
    );
    update.if_revision = Some(initial_revision.clone());
    update = update.with_document();
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
    rename = rename.with_document();
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
    assert!(renamed.value.document.document.is_some());

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
fn exact_record_documents_round_trip_without_reformatting() {
    let (root, collection) = typed_collection();
    fs::create_dir_all(root.path().join("tasks")).unwrap();
    let original = "\u{feff}---\r\ntype: task\r\ntitle: \"Exact\" # keep this comment\r\ncustom: null\r\n---\r\nBody with CRLF.\r\n";
    fs::write(root.path().join("tasks/exact.md"), original).unwrap();
    let api = collection.typed().unwrap();

    let read = api
        .read(ReadRequest::new("tasks/exact.md").unwrap().with_document())
        .unwrap();
    assert_eq!(read.value.document.as_deref(), Some(original));
    assert_eq!(read.value.frontmatter["custom"], Value::Null);

    let replacement =
        "---\ntype: task\ntitle: 'Replacement' # preserve source\ncustom: null\n---\n\nNew body.\n";
    let mut update = UpdateRequest::replace_document(
        CollectionPath::new("tasks/exact.md").unwrap(),
        replacement,
    );
    update.if_revision = Some(read.value.revision);
    let updated = api.update(update).unwrap();

    assert_eq!(updated.value.document.as_deref(), Some(replacement));
    assert_eq!(updated.value.frontmatter["title"], "Replacement");
    assert_eq!(updated.value.frontmatter["custom"], Value::Null);
    assert_eq!(
        fs::read_to_string(root.path().join("tasks/exact.md")).unwrap(),
        replacement
    );
}

#[test]
fn body_only_records_are_created_queried_and_updated_without_synthetic_frontmatter() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  validation: error\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(
        root.path().join("_types/note.md"),
        r#"---
kind: mdbase.type
name: note
match:
  path_glob: "notes/**/*.md"
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    properties:
      title: { type: string }
collection:
  read_defaults:
    title: Untitled
---
"#,
    )
    .unwrap();
    let collection = Collection::open(root.path()).unwrap();
    let api = collection.typed().unwrap();
    let path = CollectionPath::new("notes/plain.md").unwrap();
    let body = "---\nThis is body text, not a complete frontmatter block.";

    let created = api
        .create(
            CreateRequest::new(path.clone())
                .with_body(body)
                .with_document(),
        )
        .unwrap();
    assert_eq!(created.value.frontmatter, json!({}));
    assert_eq!(created.value.effective_frontmatter["title"], "Untitled");
    assert_eq!(created.value.types, ["note"]);
    assert_eq!(created.value.body, body);
    assert_eq!(created.value.document.as_deref(), Some(body));
    assert_eq!(
        fs::read_to_string(root.path().join("notes/plain.md")).unwrap(),
        body
    );

    let mut query_request = QueryRequest::builder().type_name("note");
    query_request.frontmatter_mode = FrontmatterMode::Both;
    let query = api.query(query_request).unwrap();
    assert_eq!(query.value.total_count, 1);
    assert_eq!(query.value.records[0]["frontmatter"], json!({}));

    let mut body_update = UpdateRequest::new(path.clone(), json!({}));
    body_update.body = Some("# Revised\n".to_string());
    body_update = body_update.with_document();
    let updated = api.update(body_update).unwrap();
    assert_eq!(updated.value.frontmatter, json!({}));
    assert_eq!(updated.value.document.as_deref(), Some("# Revised\n"));

    let structured = api
        .update(UpdateRequest::new(path.clone(), json!({"title": "Named"})))
        .unwrap();
    assert_eq!(structured.value.frontmatter["title"], "Named");
    assert!(fs::read_to_string(root.path().join("notes/plain.md"))
        .unwrap()
        .starts_with("---\ntitle: Named\n---\n"));

    let replacement = "# Plain again";
    let plain = api
        .update(UpdateRequest::replace_document(path, replacement))
        .unwrap();
    assert_eq!(plain.value.frontmatter, json!({}));
    assert_eq!(plain.value.document.as_deref(), Some(replacement));
    assert_eq!(
        fs::read_to_string(root.path().join("notes/plain.md")).unwrap(),
        replacement
    );

    let deserialized: CreateRequest = serde_json::from_value(json!({
        "path": "notes/from-json.md",
        "body": "No frontmatter"
    }))
    .unwrap();
    assert_eq!(deserialized.frontmatter, json!({}));
}

#[test]
fn document_updates_reject_invalid_or_ambiguous_candidates_without_writing() {
    let (root, collection) = typed_collection();
    fs::create_dir_all(root.path().join("tasks")).unwrap();
    let original = "---\ntype: task\ntitle: Original\n---\nBody\n";
    fs::write(root.path().join("tasks/exact.md"), original).unwrap();
    let operations = collection.v03_operations().unwrap();

    let invalid = operations.update(&json!({
        "path": "tasks/exact.md",
        "document": "---\n- not\n- a\n- mapping\n---\nBody\n",
    }));
    assert!(!invalid.valid);
    assert_eq!(invalid.diagnostics[0].code, "invalid_frontmatter");
    assert_eq!(
        fs::read_to_string(root.path().join("tasks/exact.md")).unwrap(),
        original
    );

    let ambiguous = operations.update(&json!({
        "path": "tasks/exact.md",
        "patch": {"title": "Patched"},
        "document": "---\ntype: task\ntitle: Replacement\n---\nBody\n",
    }));
    assert!(!ambiguous.valid);
    assert_eq!(ambiguous.diagnostics[0].code, "invalid_request");
    assert_eq!(
        fs::read_to_string(root.path().join("tasks/exact.md")).unwrap(),
        original
    );

    let wrong_type = operations.update(&json!({
        "path": "tasks/exact.md",
        "document": {"frontmatter": "is not source"},
    }));
    assert!(!wrong_type.valid);
    assert_eq!(wrong_type.diagnostics[0].code, "invalid_request");
}

#[test]
fn canonical_record_mutations_cannot_reach_control_or_executable_paths() {
    let (root, collection) = typed_collection();
    fs::create_dir_all(root.path().join(".git/hooks")).unwrap();
    fs::write(root.path().join("payload.bat"), b"original executable").unwrap();
    fs::write(
        root.path().join(".git/hooks/post-checkout.md"),
        b"original hook",
    )
    .unwrap();
    let config_before = fs::read(root.path().join("mdbase.yaml")).unwrap();
    let type_before = fs::read(root.path().join("_types/task.md")).unwrap();
    let executable_before = fs::read(root.path().join("payload.bat")).unwrap();
    let hook_before = fs::read(root.path().join(".git/hooks/post-checkout.md")).unwrap();
    let operations = collection.v03_operations().unwrap();

    for (operation, input) in [
        (
            "create",
            json!({"path": "created.exe", "document": "malware"}),
        ),
        (
            "update",
            json!({"path": "mdbase.yaml", "document": "spec_version: 0.2.0\n"}),
        ),
        (
            "update",
            json!({"path": "_types/task.md", "document": "replaced"}),
        ),
        ("delete", json!({"path": "payload.bat"})),
        (
            "rename",
            json!({"from": ".git/hooks/post-checkout.md", "to": "hook.md"}),
        ),
    ] {
        let result = match operation {
            "create" => operations.create(&input),
            "update" => operations.update(&input),
            "delete" => operations.delete(&input),
            "rename" => operations.rename(&input),
            _ => unreachable!(),
        };
        assert!(
            !result.valid,
            "{operation} unexpectedly succeeded: {result:#?}"
        );
    }

    assert!(!root.path().join("created.exe").exists());
    assert_eq!(
        fs::read(root.path().join("mdbase.yaml")).unwrap(),
        config_before
    );
    assert_eq!(
        fs::read(root.path().join("_types/task.md")).unwrap(),
        type_before
    );
    assert_eq!(
        fs::read(root.path().join("payload.bat")).unwrap(),
        executable_before
    );
    assert_eq!(
        fs::read(root.path().join(".git/hooks/post-checkout.md")).unwrap(),
        hook_before
    );
    assert!(!root.path().join("hook.md").exists());
}

#[test]
fn canonical_rename_rejects_internal_concurrency_test_fields() {
    let (root, collection) = typed_collection();
    fs::create_dir_all(root.path().join("tasks")).unwrap();
    fs::write(
        root.path().join("tasks/source.md"),
        "---\ntype: task\ntitle: Source\n---\n",
    )
    .unwrap();
    let result = collection.v03_operations().unwrap().rename(&json!({
        "from": "tasks/source.md",
        "to": "tasks/destination.md",
        "simulate_before_ref_update": [{
            "path": ".git/hooks/post-checkout.md",
            "content": "malware"
        }]
    }));

    assert!(!result.valid);
    assert_eq!(result.diagnostics[0].code, "invalid_request");
    assert!(root.path().join("tasks/source.md").exists());
    assert!(!root.path().join("tasks/destination.md").exists());
    assert!(!root.path().join(".git/hooks/post-checkout.md").exists());
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
        BatchOperation::Create(
            CreateRequest::new(CollectionPath::new("tasks/created.md").unwrap())
                .with_frontmatter(json!({"type": "task", "title": "Created"})),
        ),
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
    let body_only = "# Body-only legacy note\n";
    fs::write(root.path().join("plain.md"), body_only).unwrap();
    let collection = Collection::open(root.path()).unwrap();
    let api = collection.typed().unwrap();

    let plan = api
        .migrate_v02(V02MigrationRequest {
            dry_run: true,
            allow_lossy: false,
        })
        .unwrap();
    assert!(!plan.applied);
    assert_eq!(plan.verified_records, 2);
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
    assert_eq!(
        fs::read_to_string(root.path().join("plain.md")).unwrap(),
        body_only
    );

    drop(collection);
    let canonical = Collection::open(root.path()).unwrap();
    assert_eq!(canonical.compatibility_mode(), CompatibilityMode::Canonical);
    let api = canonical.typed().unwrap();
    let read = api.read(ReadRequest::new("legacy.md").unwrap()).unwrap();
    assert!(read.value.frontmatter.get("status").is_none());
    assert_eq!(read.value.effective_frontmatter["status"], "open");
    assert_eq!(read.value.effective_frontmatter["label"], "Legacy!");
    let plain = api
        .read(ReadRequest::new("plain.md").unwrap().with_document())
        .unwrap();
    assert_eq!(plain.value.frontmatter, json!({}));
    assert_eq!(plain.value.document.as_deref(), Some(body_only));

    let created = api
        .create(
            CreateRequest::new(CollectionPath::new("new.md").unwrap())
                .with_frontmatter(json!({"type": "task", "title": "New"})),
        )
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

#[test]
fn typed_and_wire_create_update_outcomes_and_diagnostic_order_match() {
    let (_typed_root, typed_records) = typed_collection();
    let (_wire_root, wire_collection) = typed_collection();
    let path = CollectionPath::new("tasks/differential.md").unwrap();
    let frontmatter = json!({"type": "task", "title": "Differential"});

    let invalid_path = CollectionPath::new("tasks/create-invalid.md").unwrap();
    let typed_create_error = typed_records
        .typed()
        .unwrap()
        .create(
            CreateRequest::new(invalid_path.clone()).with_frontmatter(json!({
                "type": "task", "title": 7, "status": 9
            })),
        )
        .unwrap_err();
    let wire_create_error = wire_collection.v03_operations().unwrap().create(&json!({
        "path": invalid_path.as_str(),
        "frontmatter": {"type": "task", "title": 7, "status": 9}
    }));
    assert!(!wire_create_error.valid);
    assert!(wire_create_error.result == json!({}));
    assert_eq!(
        diagnostic_values(typed_create_error.diagnostics()),
        diagnostic_values(wire_create_error.diagnostics.as_slice())
    );
    assert!(typed_create_error.diagnostics().len() >= 2);

    let typed_create = typed_records
        .typed()
        .unwrap()
        .create(
            CreateRequest::new(path.clone())
                .with_frontmatter(frontmatter.clone())
                .with_body("Body")
                .with_document(),
        )
        .unwrap();
    let wire_create = wire_collection.v03_operations().unwrap().create(&json!({
        "path": path.as_str(),
        "frontmatter": frontmatter,
        "body": "Body",
        "include_document": true,
    }));
    assert!(wire_create.valid, "{:?}", wire_create.diagnostics);
    let typed_value = serde_json::to_value(&typed_create.value).unwrap();
    assert_eq!(typed_value, wire_create.result);

    let replacement = "\u{feff}---\ntype: task\ntitle: Replaced\n---\r\nBody\r\n";
    let typed_update = typed_records
        .typed()
        .unwrap()
        .update(UpdateRequest::replace_document(path.clone(), replacement))
        .unwrap();
    let wire_update = wire_collection.v03_operations().unwrap().update(&json!({
        "path": path.as_str(),
        "document": replacement,
    }));
    assert!(wire_update.valid, "{:?}", wire_update.diagnostics);
    assert_eq!(typed_update.value.document.as_deref(), Some(replacement));
    assert_eq!(wire_update.result["document"], replacement);

    let stale = Revision::parse("sha256:stale").unwrap();
    let mut typed_request = UpdateRequest::new(path.clone(), json!({"title": "stale"}));
    typed_request.if_revision = Some(stale);
    let typed_error = typed_records
        .typed()
        .unwrap()
        .update(typed_request)
        .unwrap_err();
    let wire_error = wire_collection.v03_operations().unwrap().update(&json!({
        "path": path.as_str(),
        "patch": {"title": "stale"},
        "if_revision": "sha256:stale",
    }));
    assert!(!wire_error.valid);
    assert!(wire_error.result == json!({}));
    assert_eq!(
        diagnostic_values(typed_error.diagnostics()),
        diagnostic_values(wire_error.diagnostics.as_slice())
    );

    let typed_membership = typed_records
        .typed()
        .unwrap()
        .update(UpdateRequest::new(path.clone(), json!({"type": "missing"})))
        .unwrap_err();
    let wire_membership = wire_collection.v03_operations().unwrap().update(&json!({
        "path": path.as_str(), "patch": {"type": "missing"}
    }));
    assert!(!wire_membership.valid);
    assert!(wire_membership.result == json!({}));
    assert_eq!(
        diagnostic_values(typed_membership.diagnostics()),
        diagnostic_values(wire_membership.diagnostics.as_slice())
    );

    let typed_validation = typed_records
        .typed()
        .unwrap()
        .update(UpdateRequest::new(path.clone(), json!({"title": 7})))
        .unwrap_err();
    let wire_validation = wire_collection.v03_operations().unwrap().update(&json!({
        "path": path.as_str(), "patch": {"title": 7}
    }));
    assert!(!wire_validation.valid);
    assert!(wire_validation.result == json!({}));
    assert_eq!(
        diagnostic_values(typed_validation.diagnostics()),
        diagnostic_values(wire_validation.diagnostics.as_slice())
    );
}
