use std::fs;

use mdbase::Collection;
use serde_json::json;
use tempfile::tempdir;

fn collection() -> (tempfile::TempDir, Collection) {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("TaskNotes/Views")).unwrap();
    fs::create_dir_all(root.path().join("tasks")).unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        r#"spec_version: 0.3.0
settings:
  explicit_type_keys: []
x-obsidian:
  bases:
    include: ["TaskNotes/Views/**/*.base"]
"#,
    )
    .unwrap();
    fs::write(
        root.path().join("TaskNotes/Views/tasks.base"),
        r#"filters:
  and:
    - 'file.hasTag("task")'
formulas:
  urgency: 'if(priority == "high", 2, 1)'
views:
  - type: tasknotesTaskList
    name: Open tasks
    filters:
      and:
        - 'status != "done"'
    order: [status, formula.urgency, file.name]
    sort:
      - property: formula.urgency
        direction: DESC
  - type: tasknotesKanban
    name: Board
    order: [status, file.name]
    groupBy:
      property: status
      direction: ASC
"#,
    )
    .unwrap();
    fs::write(
        root.path().join("tasks/low.md"),
        "---\nstatus: todo\npriority: low\ntags: [task]\n---\nLow\n",
    )
    .unwrap();
    fs::write(
        root.path().join("tasks/high.md"),
        "---\nstatus: todo\npriority: high\ntags: [task]\n---\nHigh\n",
    )
    .unwrap();
    fs::write(
        root.path().join("tasks/done.md"),
        "---\nstatus: done\npriority: high\ntags: [task]\n---\nDone\n",
    )
    .unwrap();
    let opened = Collection::open(root.path()).unwrap();
    (root, opened)
}

#[test]
fn discovers_and_executes_configured_obsidian_bases() {
    let (_root, collection) = collection();
    let operations = collection.v03_operations().unwrap();
    let listed = operations.list_views(&json!({}));
    assert!(listed.valid, "{:?}", listed.diagnostics);
    assert_eq!(listed.result["meta"]["total_count"], 1);
    assert_eq!(
        listed.result["views"][0]["source"]["format"],
        "obsidian.base"
    );
    assert_eq!(listed.result["views"][0]["source"]["writable"], false);
    assert_eq!(
        listed.result["views"][0]["views"][1]["presentation"]["type"],
        "tasknotes.kanban"
    );

    let executed = operations.execute_view(&json!({
        "path": "TaskNotes/Views/tasks.base",
        "view": "open-tasks"
    }));
    assert!(executed.valid, "{:?}", executed.diagnostics);
    assert_eq!(executed.result["meta"]["total_count"], 2);
    assert_eq!(executed.result["results"][0]["path"], "tasks/high.md");
    assert_eq!(
        executed.result["results"][0]["values"]["formula.urgency"],
        2
    );
}

#[test]
fn configured_sources_are_not_ordinary_records() {
    let (_root, collection) = collection();
    let read = collection.v03_operations().unwrap().read(&json!({
        "path": "TaskNotes/Views/tasks.base"
    }));
    assert!(!read.valid);
    assert!(read
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "file_not_found"));
}
