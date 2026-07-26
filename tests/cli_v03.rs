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
