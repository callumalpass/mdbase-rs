use std::fs;
use std::process::{Command, Output};

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

fn run(root: &tempfile::TempDir, arguments: &[&str]) -> (Output, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_mdb"))
        .arg("-C")
        .arg(root.path())
        .args(arguments)
        .output()
        .unwrap();
    let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "CLI output was not JSON: {error}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output, value)
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
    assert!(output.status.success(), "{created:#}");
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
    assert!(output.status.success(), "{updated:#}");
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
    assert!(output.status.success(), "{queried:#}");
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
    assert!(output.status.success(), "{renamed:#}");
    assert_eq!(renamed["valid"], true);
    assert_eq!(renamed["result"]["to"], "archive/first.md");

    let (output, deleted) = run(&root, &["delete", "archive/first.md"]);
    assert!(output.status.success(), "{deleted:#}");
    assert_eq!(deleted["valid"], true);
    assert_eq!(deleted["result"]["deleted"], true);

    let (output, invalid) = run(&root, &["read", "../outside.md"]);
    assert!(!output.status.success(), "{invalid:#}");
    assert_eq!(invalid["valid"], false);
    assert_eq!(invalid["diagnostics"][0]["code"], "invalid_path");
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
    assert!(output.status.success(), "{read:#}");
    assert_eq!(read["valid"], true);
    assert_eq!(read["result"]["frontmatter"]["title"], "Legacy");

    let (output, rejected) = run(
        &root,
        &["update", "legacy.md", "--fields", r#"{"title":"Changed"}"#],
    );
    assert!(!output.status.success(), "{rejected:#}");
    assert_eq!(rejected["diagnostics"][0]["code"], "migration_required");
    assert_eq!(
        fs::read_to_string(root.path().join("legacy.md")).unwrap(),
        record
    );

    let (output, plan) = run(&root, &["migrate-v02", "--dry-run"]);
    assert!(output.status.success(), "{plan:#}");
    assert_eq!(plan["result"]["applied"], false);
    assert_eq!(plan["result"]["verified_records"], 1);
    assert!(fs::read_to_string(root.path().join("mdbase.yaml"))
        .unwrap()
        .contains("0.2.0"));

    let (output, applied) = run(&root, &["migrate-v02"]);
    assert!(output.status.success(), "{applied:#}");
    assert_eq!(applied["result"]["applied"], true);
    assert!(fs::read_to_string(root.path().join("mdbase.yaml"))
        .unwrap()
        .contains("0.3.0"));
}
