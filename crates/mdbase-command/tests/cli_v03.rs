use std::fs;

use clap::Parser;
use mdbase_command::{execute_args, DirectArgs};
use serde_json::Value;

fn cli_collection() -> tempfile::TempDir {
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
    root
}

fn run(root: &tempfile::TempDir, arguments: &[&str]) -> (i32, Value) {
    let mut argv = vec![
        "mdbase".to_string(),
        "-C".to_string(),
        root.path().to_string_lossy().into_owned(),
    ];
    argv.extend(arguments.iter().map(|argument| (*argument).to_string()));
    let result = execute_args(DirectArgs::try_parse_from(argv).expect("valid command arguments"));
    (result.exit_code, result.value)
}

#[test]
fn canonical_cli_runs_a_typed_record_lifecycle() {
    let root = cli_collection();

    let (output, created) = run(
        &root,
        &[
            "create",
            "--path",
            r"tasks\first.md",
            "--type",
            "task",
            "--fields",
            r#"{"title":"First"}"#,
        ],
    );
    assert_eq!(output, 0, "{created:#}");
    assert_eq!(created["valid"], true);
    assert_eq!(created["result"]["path"], "tasks/first.md");
    assert!(created["result"]["revision"]
        .as_str()
        .is_some_and(|revision| revision.starts_with("sha256:")));

    let (output, updated) = run(
        &root,
        &[
            "update",
            "tasks/first.md",
            "--fields",
            r#"{"status":"done"}"#,
        ],
    );
    assert_eq!(output, 0, "{updated:#}");
    assert_eq!(updated["valid"], true);
    assert_eq!(updated["result"]["frontmatter"]["status"], "done");

    let (output, queried) = run(
        &root,
        &[
            "query",
            "--types",
            "task",
            "--folder",
            "tasks",
            "--where",
            "status == 'done'",
        ],
    );
    assert_eq!(output, 0, "{queried:#}");
    assert_eq!(queried["valid"], true);
    assert_eq!(queried["result"]["meta"]["total_count"], 1);
    assert_eq!(
        queried["result"]["results"][0]["file"]["path"],
        "tasks/first.md"
    );

    let (output, renamed) = run(
        &root,
        &[
            "rename",
            "tasks/first.md",
            "archive/first.md",
            "--update-refs",
        ],
    );
    assert_eq!(output, 0, "{renamed:#}");
    assert_eq!(renamed["valid"], true);
    assert_eq!(renamed["result"]["to"], "archive/first.md");

    let (output, deleted) = run(&root, &["delete", "archive/first.md"]);
    assert_eq!(output, 0, "{deleted:#}");
    assert_eq!(deleted["valid"], true);
    assert_eq!(deleted["result"]["deleted"], true);

    let (output, invalid) = run(&root, &["read", "../outside.md"]);
    assert_ne!(output, 0, "{invalid:#}");
    assert_eq!(invalid["valid"], false);
    assert_eq!(invalid["diagnostics"][0]["code"], "invalid_path");
}

#[test]
fn canonical_cli_supports_revision_dry_run_and_typed_json_requests() {
    let root = cli_collection();
    let (_, created) = run(
        &root,
        &[
            "create",
            "--path",
            "tasks/base.md",
            "--type",
            "task",
            "--fields",
            r#"{"title":"Base","status":"open"}"#,
        ],
    );
    let revision = created["result"]["revision"].as_str().unwrap();

    let (output, preview) = run(
        &root,
        &[
            "update",
            "tasks/base.md",
            "--fields",
            r#"{"status":"preview"}"#,
            "--if-revision",
            revision,
            "--dry-run",
        ],
    );
    assert_eq!(output, 0, "{preview:#}");
    assert!(!fs::read_to_string(root.path().join("tasks/base.md"))
        .unwrap()
        .contains("preview"));

    let batch_path = root.path().join("batch.json");
    fs::write(
        &batch_path,
        r#"{
  "operations": [
    {
      "kind": "update",
      "input": {
        "path": "tasks/base.md",
        "patch": {"status": "done"}
      }
    },
    {
      "kind": "create",
      "input": {
        "path": "tasks/second.md",
        "type": "task",
        "frontmatter": {"title": "Second", "status": "done"}
      }
    }
  ]
}"#,
    )
    .unwrap();
    let batch_path = batch_path.to_string_lossy();
    let (output, batch) = run(&root, &["batch", "--request", &batch_path]);
    assert_eq!(output, 0, "{batch:#}");
    assert_eq!(batch["result"]["succeeded"], 2);
    assert_eq!(batch["result"]["failed"], 0);

    let query_path = root.path().join("query.json");
    fs::write(
        &query_path,
        r#"{
  "types": ["task"],
  "where": "status == 'done'",
  "order_by": [{"field": "file.path", "direction": "asc"}]
}"#,
    )
    .unwrap();
    let query_path = query_path.to_string_lossy();
    let (output, query) = run(&root, &["query", "--request", &query_path]);
    assert_eq!(output, 0, "{query:#}");
    assert_eq!(query["result"]["meta"]["total_count"], 2);

    let (output, stale) = run(
        &root,
        &["delete", "tasks/base.md", "--if-revision", "sha256:stale"],
    );
    assert_ne!(output, 0, "{stale:#}");
    assert_eq!(stale["diagnostics"][0]["code"], "concurrent_modification");
}

#[test]
fn legacy_cli_is_read_only_and_exposes_verified_migration() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.2.0\n").unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(
        root.path().join("_types/task.md"),
        "---\nname: task\nfields:\n  title: { type: string, required: true }\n---\n",
    )
    .unwrap();
    let record = "---\ntype: task\ntitle: Legacy\n---\n";
    fs::write(root.path().join("legacy.md"), record).unwrap();

    let (output, read) = run(&root, &["read", "legacy.md"]);
    assert_eq!(output, 0, "{read:#}");
    assert_eq!(read["valid"], true);
    assert_eq!(read["result"]["frontmatter"]["title"], "Legacy");

    let (output, rejected) = run(
        &root,
        &["update", "legacy.md", "--fields", r#"{"title":"Changed"}"#],
    );
    assert_ne!(output, 0, "{rejected:#}");
    assert_eq!(rejected["diagnostics"][0]["code"], "migration_required");
    assert_eq!(
        fs::read_to_string(root.path().join("legacy.md")).unwrap(),
        record
    );

    let (output, plan) = run(&root, &["migrate-v02", "--dry-run"]);
    assert_eq!(output, 0, "{plan:#}");
    assert_eq!(plan["result"]["applied"], false);
    assert_eq!(plan["result"]["verified_records"], 1);
    assert!(fs::read_to_string(root.path().join("mdbase.yaml"))
        .unwrap()
        .contains("0.2.0"));

    let (output, applied) = run(&root, &["migrate-v02"]);
    assert_eq!(output, 0, "{applied:#}");
    assert_eq!(applied["result"]["applied"], true);
    assert!(fs::read_to_string(root.path().join("mdbase.yaml"))
        .unwrap()
        .contains("0.3.0"));
}

#[test]
fn update_refs_flag_overrides_the_collection_default() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  rename_update_refs: false\n",
    )
    .unwrap();
    fs::write(root.path().join("a.md"), "filename-resolved target\n").unwrap();
    fs::write(root.path().join("r.md"), "---\nref: \"[[a]]\"\n---\n").unwrap();

    let (status, result) = run(&root, &["rename", "a.md", "b.md", "--update-refs"]);
    assert_eq!(status, 0, "{result:#}");
    assert!(fs::read_to_string(root.path().join("r.md"))
        .unwrap()
        .contains("[[b]]"));
}

#[test]
fn cache_clear_uses_the_configured_cache_folder() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.2.0\nsettings:\n  cache_folder: custom-cache\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("custom-cache")).unwrap();
    fs::write(root.path().join("custom-cache/cache.db"), "x").unwrap();

    let (status, result) = run(&root, &["cache", "clear"]);
    assert_eq!(status, 0, "{result:#}");
    assert!(!root.path().join("custom-cache/cache.db").exists());
}
