use std::fs;
use std::path::Path;

use mdbase::v03;
use mdbase::{Collection, CollectionResources, SpecProfile};
use tempfile::TempDir;

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("test file parent")).expect("create test directory");
    fs::write(path, content).expect("write test file");
}

fn v03_collection() -> TempDir {
    let directory = tempfile::tempdir().expect("temp collection");
    write(
        directory.path(),
        "mdbase.yaml",
        r#"spec_version: "0.3.0"
settings:
  validation: error
  types_folder: _types
"#,
    );
    write(
        directory.path(),
        "_types/task.md",
        r#"---
kind: mdbase.type
name: task
version: 1
match:
  path_glob: "tasks/**/*.md"
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    required: [type, title]
    additionalProperties: false
    properties:
      type: { const: task }
      title: { type: string, minLength: 1 }
      status: { enum: [open, done], default: open }
collection:
  read_defaults:
    status: open
---
"#,
    );
    directory
}

#[test]
fn canonical_v03_schemas_compile() {
    v03::validate_canonical_schemas().expect("canonical schemas compile");
}

#[test]
fn resource_open_does_not_read_records_or_create_runtime_state() {
    let directory = v03_collection();
    for index in 0..1_000 {
        write(
            directory.path(),
            &format!("tasks/{index}.md"),
            "This is deliberately not a valid typed Markdown record.",
        );
    }

    let configuration = v03::inspect_configuration(directory.path())
        .expect("configuration inspection ignores types and records");
    let collection = CollectionResources::open(directory.path())
        .expect("resource catalog ignores ordinary records");

    let inspected = v03::inspect_collection(directory.path());
    assert_eq!(configuration["spec_version"], "0.3.0");
    assert_eq!(collection.spec_profile(), SpecProfile::V03);
    let task = collection.types().get("task").expect("task resource");
    assert_eq!(
        task.source_revision.as_ref(),
        Some(&inspected.types[0].revision),
        "loaded definitions retain the digest of the exact parsed source"
    );
    assert!(
        !directory.path().join(".mdbase").exists(),
        "resource loading must not create runtime indexes or feeds"
    );
}

#[test]
fn canonical_view_type_validates_an_ordinary_markdown_record() {
    let directory = v03_collection();
    write(
        directory.path(),
        "schemas/v0.3/view.schema.json",
        include_str!("../schemas/v0.3/view.schema.json"),
    );
    write(
        directory.path(),
        "_types/view.md",
        r#"---
kind: mdbase.type
name: view
version: 1
match:
  where: { type: view }
schema:
  dialect: json-schema-2020-12
  ref: "../schemas/v0.3/view.schema.json"
---
"#,
    );
    write(
        directory.path(),
        "views/tasks.md",
        r#"---
type: view
id: tasks.views
version: 1
name: Task views
query:
  types: [task]
views:
  - id: all
    name: All tasks
---
"#,
    );

    let collection = Collection::open(directory.path()).expect("open collection with view type");
    let validation = collection.validate_op(&serde_json::json!({
        "path": "views/tasks.md"
    }));
    assert_eq!(validation.get("valid"), Some(&serde_json::json!(true)));
    let read = collection.read(&serde_json::json!({ "path": "views/tasks.md" }));
    assert_eq!(read.get("types"), Some(&serde_json::json!(["view"])));
}

#[test]
fn v03_allows_disabling_explicit_type_keys() {
    let directory = tempfile::tempdir().expect("temp collection");
    write(
        directory.path(),
        "mdbase.yaml",
        r#"spec_version: "0.3.0"
settings:
  explicit_type_keys: []
"#,
    );

    let collection = Collection::open(directory.path()).expect("open collection");
    assert!(collection.settings().explicit_type_keys.is_empty());
}

#[test]
fn init_defaults_to_a_minimal_v03_collection() {
    let directory = tempfile::tempdir().expect("temp collection");
    let result = mdbase::init::init_collection(
        directory.path(),
        &serde_json::json!({
            "config": {
                "name": "Example",
                "settings": { "types_folder": "schemas/types" },
                "x-owner": "tests"
            }
        }),
    );

    assert_eq!(
        result,
        serde_json::json!({
            "config_path": "mdbase.yaml",
            "types_folder": "schemas/types",
            "contracts_folder": "_contracts"
        })
    );
    assert!(directory.path().join("schemas/types").is_dir());
    assert!(directory.path().join("_contracts").is_dir());
    assert!(!directory.path().join("schemas/types/meta.md").exists());
    let config = fs::read_to_string(directory.path().join("mdbase.yaml")).expect("read config");
    assert!(config.contains("spec_version: 0.3.0"));
    assert!(config.contains("name: Example"));
    assert!(config.contains("x-owner: tests"));
    assert_eq!(
        Collection::open(directory.path())
            .expect("open initialized collection")
            .spec_profile(),
        SpecProfile::V03
    );
}

#[test]
fn init_retains_explicit_v02_meta_type_generation() {
    let directory = tempfile::tempdir().expect("temp collection");
    let result = mdbase::init::init_collection(
        directory.path(),
        &serde_json::json!({ "config": { "spec_version": "0.2.1" } }),
    );

    assert_eq!(
        result.get("meta_type_path"),
        Some(&serde_json::json!("_types/meta.md"))
    );
    assert!(directory.path().join("_types/meta.md").is_file());
    assert_eq!(
        Collection::open(directory.path())
            .expect("open legacy collection")
            .spec_profile(),
        SpecProfile::V02
    );
}

#[test]
fn init_rejects_unsafe_type_paths_before_writing() {
    let directory = tempfile::tempdir().expect("temp collection");
    let result = mdbase::init::init_collection(
        directory.path(),
        &serde_json::json!({
            "config": { "settings": { "types_folder": "../types" } }
        }),
    );

    assert_eq!(
        result.pointer("/error/code"),
        Some(&serde_json::json!("path_traversal"))
    );
    assert!(!directory.path().join("mdbase.yaml").exists());
}

#[test]
fn v03_asserts_the_required_rfc3339_formats() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "date": {"type": "string", "format": "date"},
            "time": {"type": "string", "format": "time"},
            "date_time": {"type": "string", "format": "date-time"}
        }
    });
    let valid = serde_json::json!({
        "date": "2026-07-16",
        "time": "12:34:56+10:00",
        "date_time": "2026-07-16T12:34:56+10:00"
    });
    assert!(v03::validate_schema_instance(&schema, &valid, "valid.md", Some("formats")).is_empty());

    for (field, value) in [
        ("date", "16/07/2026"),
        ("time", "12:34:56"),
        ("date_time", "2026-07-16T12:34:56"),
    ] {
        let diagnostics = v03::validate_schema_instance(
            &schema,
            &serde_json::json!({(field): value}),
            "invalid.md",
            Some("formats"),
        );
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "format_invalid" && diagnostic.field.as_deref() == Some(field)
        }));
    }
}

#[test]
fn inspects_the_canonical_canvas_collection() {
    let fixture = Path::new("../mdbase-spec/examples/v0.3/canvas-runtime");
    if !fixture.exists() {
        return;
    }
    let report = v03::inspect_collection(fixture);
    assert!(report.valid, "{:#?}", report.diagnostics);
    assert!(report
        .types
        .iter()
        .any(|type_file| type_file.name == "task"));
    assert_eq!(
        report.types.len(),
        1,
        "runtime contracts are first-class artifacts, not local record types"
    );
}

#[test]
fn opens_v03_and_keeps_read_defaults_out_of_required_validation() {
    let directory = v03_collection();
    write(
        directory.path(),
        "tasks/valid.md",
        "---\ntype: task\ntitle: Valid\n---\n",
    );
    write(
        directory.path(),
        "tasks/missing-title.md",
        "---\ntype: task\n---\n",
    );

    let collection = Collection::open(directory.path()).expect("open v0.3 collection");
    assert_eq!(collection.spec_profile(), SpecProfile::V03);

    let read = collection.read(&serde_json::json!({ "path": "tasks/valid.md" }));
    assert_eq!(
        read.pointer("/effective_frontmatter/status"),
        Some(&serde_json::json!("open"))
    );
    assert_eq!(read.pointer("/frontmatter/status"), None);

    let validation = collection.validate_op(&serde_json::json!({
        "path": "tasks/missing-title.md"
    }));
    assert_eq!(validation.get("valid"), Some(&serde_json::json!(false)));
    assert!(validation["issues"]
        .as_array()
        .is_some_and(|issues| issues.iter().any(|issue| {
            issue.get("code") == Some(&serde_json::json!("schema_required"))
                && issue.get("field") == Some(&serde_json::json!("title"))
        })));
}

#[test]
fn reports_canonical_type_wrapper_diagnostics() {
    let directory = v03_collection();
    write(
        directory.path(),
        "_types/broken.md",
        r#"---
kind: mdbase.type
name: broken
schema:
  dialect: json-schema-2020-12
  value: { type: object }
collecton: {}
---
"#,
    );

    let report = v03::inspect_collection(directory.path());
    assert!(!report.valid);
    assert!(report.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "schema_unevaluated_properties"
            && diagnostic.field.as_deref() == Some("collecton")
    }));
}

#[test]
fn rejects_forbidden_and_unsupported_embedded_schema_refs_without_resolution() {
    let directory = v03_collection();
    write(
        directory.path(),
        "_types/remote.md",
        r#"---
kind: mdbase.type
name: remote
schema:
  dialect: json-schema-2020-12
  value:
    $ref: https://example.invalid/schema.json
---
"#,
    );
    write(
        directory.path(),
        "_types/nested.md",
        r#"---
kind: mdbase.type
name: nested
schema:
  dialect: json-schema-2020-12
  value:
    $ref: ./nested.schema.json
---
"#,
    );

    let report = v03::inspect_collection(directory.path());
    assert!(report
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "schema_ref_forbidden"));
    assert!(report
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "unsupported_profile"));
}

#[cfg(unix)]
#[test]
fn rejects_schema_ref_symlink_escapes() {
    use std::os::unix::fs::symlink;

    let directory = v03_collection();
    let outside = tempfile::tempdir().expect("outside directory");
    write(outside.path(), "outside.json", r#"{"type":"object"}"#);
    symlink(
        outside.path().join("outside.json"),
        directory.path().join("linked.json"),
    )
    .expect("create schema symlink");
    write(
        directory.path(),
        "_types/linked.md",
        r#"---
kind: mdbase.type
name: linked
schema:
  dialect: json-schema-2020-12
  ref: ../linked.json
---
"#,
    );

    let report = v03::inspect_collection(directory.path());
    assert!(report
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "schema_ref_forbidden"));
}

#[test]
fn legacy_v02_collections_still_open() {
    let directory = tempfile::tempdir().expect("temp collection");
    write(directory.path(), "mdbase.yaml", "spec_version: \"0.2.1\"\n");
    write(
        directory.path(),
        "_types/task.md",
        "---\nname: task\nfields:\n  title:\n    type: string\n    required: true\n---\n",
    );
    let collection = Collection::open(directory.path()).expect("open v0.2 collection");
    assert_eq!(collection.spec_profile(), SpecProfile::V02);
    assert!(collection.types().contains_key("task"));
    assert_eq!(
        collection
            .v03_operations()
            .err()
            .expect("v0.2 collection must reject v0.3 facade")
            .code,
        "unsupported_profile"
    );
}

#[test]
fn alpha_v03_collections_still_open() {
    let directory = tempfile::tempdir().expect("temp collection");
    write(
        directory.path(),
        "mdbase.yaml",
        "spec_version: \"0.3.0-alpha.1\"\n",
    );

    let loaded = mdbase::config::load_config(directory.path());
    assert_eq!(loaded["valid"], true);
    assert_eq!(loaded["config"]["spec_profile"], "v0.3");
    assert!(loaded["warnings"]
        .as_array()
        .is_some_and(|warnings| warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|message| message.contains("compatible v0.3 prerelease")))));

    let collection = Collection::open(directory.path()).expect("open alpha v0.3 collection");
    assert_eq!(collection.spec_profile(), SpecProfile::V03);
}

#[test]
fn v03_operation_facade_returns_canonical_envelopes_and_revisions() {
    let directory = v03_collection();
    write(
        directory.path(),
        "tasks/valid.md",
        "---\ntype: task\ntitle: Valid\n---\n",
    );

    let collection = Collection::open(directory.path()).expect("open v0.3 collection");
    let operations = collection.v03_operations().expect("v0.3 operations");
    let read = operations.read(&serde_json::json!({"path": "tasks/valid.md"}));
    assert!(read.valid, "{:#?}", read.diagnostics);
    assert!(read.diagnostics.is_empty());
    assert_eq!(
        read.result.pointer("/effective_frontmatter/status"),
        Some(&serde_json::json!("open"))
    );
    assert_eq!(
        read.result.pointer("/frontmatter/title"),
        Some(&serde_json::json!("Valid"))
    );
    assert_eq!(read.result.pointer("/frontmatter/status"), None);
    assert!(read
        .result
        .get("revision")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|revision| revision.starts_with("sha256:") && revision.len() == 71));

    let traversal = operations.create(&serde_json::json!({
        "type": "task",
        "path": "../escape.md",
        "frontmatter": {"type": "task", "title": "Escape"}
    }));
    assert!(!traversal.valid);
    assert_eq!(traversal.result, serde_json::json!({}));
    assert!(traversal
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "path_traversal"));
}

#[test]
fn v03_mutation_preflights_return_canonical_envelopes_without_persisting() {
    let directory = v03_collection();
    write(
        directory.path(),
        "tasks/source.md",
        "---\ntype: task\ntitle: Source\n---\n",
    );
    write(
        directory.path(),
        "tasks/backlink.md",
        "---\ntype: task\ntitle: Backlink\n---\n[[source]]\n",
    );

    let collection = Collection::open(directory.path()).expect("open v0.3 collection");
    let operations = collection.v03_operations().expect("v0.3 operations");

    let rename = operations.rename(&serde_json::json!({
        "from": "tasks/source.md",
        "to": "archive/source.md",
        "update_refs": true,
        "dry_run": true,
    }));
    assert!(rename.valid, "{:#?}", rename.diagnostics);
    assert_eq!(rename.result["dry_run"], true);
    assert_eq!(rename.result["would_rename"], true);
    assert!(!directory.path().join("archive/source.md").exists());

    let delete = operations.delete(&serde_json::json!({
        "path": "tasks/source.md",
        "dry_run": true,
    }));
    assert!(delete.valid, "{:#?}", delete.diagnostics);
    assert_eq!(delete.result["dry_run"], true);
    assert_eq!(delete.result["would_delete"], true);
    assert!(directory.path().join("tasks/source.md").exists());
    assert!(directory.path().join("tasks/backlink.md").exists());
}

#[test]
fn v03_mutations_enforce_opaque_revision_preconditions() {
    let directory = v03_collection();
    write(
        directory.path(),
        "tasks/conditional.md",
        "---\ntype: task\ntitle: Original\n---\nBody\n",
    );

    let collection = Collection::open(directory.path()).expect("open v0.3 collection");
    let operations = collection.v03_operations().expect("v0.3 operations");
    let read = operations.read(&serde_json::json!({"path": "tasks/conditional.md"}));
    let original_revision = read.result["revision"]
        .as_str()
        .expect("read revision")
        .to_string();

    let updated = operations.update(&serde_json::json!({
        "path": "tasks/conditional.md",
        "fields": {"title": "Updated"},
        "if_revision": original_revision,
    }));
    assert!(updated.valid, "{:#?}", updated.diagnostics);
    let updated_revision = updated.result["revision"]
        .as_str()
        .expect("updated revision")
        .to_string();
    assert_ne!(updated_revision, original_revision);

    write(
        directory.path(),
        "tasks/conditional.md",
        "---\ntype: task\ntitle: External\n---\nBody\n",
    );

    for conflict in [
        operations.update(&serde_json::json!({
            "path": "tasks/conditional.md",
            "fields": {"title": "Lost update"},
            "if_revision": updated_revision,
        })),
        operations.delete(&serde_json::json!({
            "path": "tasks/conditional.md",
            "if_revision": updated_revision,
        })),
        operations.rename(&serde_json::json!({
            "from": "tasks/conditional.md",
            "to": "tasks/renamed.md",
            "if_revision": updated_revision,
        })),
    ] {
        assert!(!conflict.valid);
        assert!(conflict
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "concurrent_modification"));
    }

    let persisted = fs::read_to_string(directory.path().join("tasks/conditional.md"))
        .expect("conflicting writes preserve current file");
    assert!(persisted.contains("title: External"));
    assert!(!directory.path().join("tasks/renamed.md").exists());

    let invalid_token = operations.update(&serde_json::json!({
        "path": "tasks/conditional.md",
        "fields": {"title": "Unsafe"},
        "if_revision": 42,
    }));
    assert!(!invalid_token.valid);
    assert_eq!(invalid_token.diagnostics[0].code, "invalid_request");
}

#[test]
fn v03_update_accepts_the_canonical_patch_and_keeps_legacy_fields_compatible() {
    let directory = v03_collection();
    write(
        directory.path(),
        "tasks/update-shapes.md",
        "---\ntype: task\ntitle: Original\nstatus: open\n---\nBody\n",
    );

    let collection = Collection::open(directory.path()).expect("open v0.3 collection");
    let operations = collection.v03_operations().expect("v0.3 operations");
    let patched = operations.update(&serde_json::json!({
        "path": "tasks/update-shapes.md",
        "patch": {"title": "Canonical", "status": "done"},
        "fields": {"title": "Legacy must not win"}
    }));
    assert!(patched.valid, "{:#?}", patched.diagnostics);
    assert_eq!(patched.result["frontmatter"]["title"], "Canonical");
    assert_eq!(patched.result["frontmatter"]["status"], "done");

    let legacy = operations.update(&serde_json::json!({
        "path": "tasks/update-shapes.md",
        "fields": {"title": "Legacy still works"}
    }));
    assert!(legacy.valid, "{:#?}", legacy.diagnostics);
    assert_eq!(legacy.result["frontmatter"]["title"], "Legacy still works");
}

#[test]
fn v03_query_uses_the_canonical_operation_envelope() {
    let directory = v03_collection();
    write(
        directory.path(),
        "tasks/query.md",
        "---\ntype: task\ntitle: Query me\n---\n",
    );
    let collection = Collection::open(directory.path()).expect("open v0.3 collection");
    let query = collection
        .v03_operations()
        .expect("v0.3 operations")
        .query(&serde_json::json!({"types": ["task"]}));

    assert!(query.valid, "{:#?}", query.diagnostics);
    assert_eq!(query.result["results"][0]["path"], "tasks/query.md");
}
