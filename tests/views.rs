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
  timezone: Australia/Melbourne
x-obsidian:
  bases:
    include: ["TaskNotes/Views/**/*.base"]
    create_folder: TaskNotes/Views
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
  localEpoch: 'number(date("1970-01-02"))'
properties:
  formula.urgency:
    displayName: Urgency
    description: Relative urgency score
    format: number
    hidden: false
views:
  - type: tasknotesTaskList
    name: Open tasks
    filters:
      and:
        - 'status != "done"'
    order: [status, formula.urgency, formula.localEpoch, file.name]
    sort:
      - property: tags
        direction: DESC
      - property: formula.urgency
        direction: DESC
  - type: tasknotesKanban
    name: Board
    order: [file.name]
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
    assert_eq!(listed.result["views"][0]["source"]["writable"], true);
    assert_eq!(
        listed.result["views"][0]["views"][1]["presentation"]["type"],
        "tasknotesKanban"
    );
    assert_eq!(
        listed.result["views"][0]["views"][0]["properties"][1],
        json!({
            "key": "formula.urgency",
            "label": "Urgency",
            "description": "Relative urgency score",
            "format": "number",
            "hidden": false
        })
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
    assert_eq!(
        executed.result["results"][0]["values"]["formula.localEpoch"],
        50_400_000
    );
    assert!(!executed.result["results"][0]["values"]
        .as_object()
        .unwrap()
        .contains_key("tags"));

    let page = operations.execute_view(&json!({
        "path": "TaskNotes/Views/tasks.base",
        "view": "open-tasks",
        "offset": 1,
        "limit": 1
    }));
    assert!(page.valid, "{:?}", page.diagnostics);
    assert_eq!(page.result["meta"]["total_count"], 2);
    assert_eq!(page.result["meta"]["has_more"], false);
    assert_eq!(page.result["results"].as_array().unwrap().len(), 1);
    assert_eq!(page.result["results"][0]["path"], "tasks/low.md");

    let board = operations.execute_view(&json!({
        "path": "TaskNotes/Views/tasks.base",
        "view": "board"
    }));
    assert!(board.valid, "{:?}", board.diagnostics);
    assert_eq!(board.result["results"][0]["values"]["status"], "done");
}

#[test]
fn project_relationship_view_filters_records_through_task_project_backlinks() {
    let (root, collection) = collection();
    fs::create_dir_all(root.path().join("Projects")).unwrap();
    fs::write(
        root.path().join("Projects/mobile.md"),
        "---\ntitle: Mobile roadmap\n---\nProject notes\n",
    )
    .unwrap();
    fs::write(
        root.path().join("Projects/unlinked.md"),
        "---\ntitle: Unlinked\n---\nNo active tasks\n",
    )
    .unwrap();
    fs::write(
        root.path().join("tasks/project-task.md"),
        "---\nstatus: todo\npriority: high\ntags: [task]\nprojects: ['[[Projects/mobile]]']\n---\nShip mobile\n",
    )
    .unwrap();
    fs::write(
        root.path().join("TaskNotes/Views/projects.base"),
        r##"views:
  - type: tasknotesProjects
    name: Projects
    filters:
      and:
        - 'file.backlinks.filter((value.asFile().properties["status"].isEmpty() == false) && (value.asFile().properties["status"] != "done") && (value.asFile().hasTag("archived") != true) && (list(value.asFile().properties["projects"]).map(file(value.replace(/^\[[^\]]+\]\((.*)\)$/, "$1").replace("[[", "").replace("]]", "").split("|")[0].split("#")[0].replace(/%20/g, " ")).asLink()).contains(file.asLink()))).length > 0'
    order: [file.name, file.folder]
"##,
    )
    .unwrap();

    let reopened = Collection::open(root.path()).unwrap();
    let executed = reopened.v03_operations().unwrap().execute_view(&json!({
        "path": "TaskNotes/Views/projects.base",
        "view": "projects"
    }));

    assert!(executed.valid, "{:?}", executed.diagnostics);
    assert_eq!(executed.result["meta"]["total_count"], 1);
    assert_eq!(executed.result["results"][0]["path"], "Projects/mobile.md");
    assert_eq!(
        executed.result["results"][0]["effective_frontmatter"]["title"],
        "Mobile roadmap"
    );
    drop(collection);
}

#[test]
fn saved_view_sources_are_revision_safe_resources() {
    let (root, collection) = collection();
    let operations = collection.v03_operations().unwrap();
    let read = operations.read_view_source(&json!({
        "path": "TaskNotes/Views/tasks.base"
    }));
    assert!(read.valid, "{:?}", read.diagnostics);
    assert_eq!(read.result["format"], "obsidian.base");
    let revision = read.result["revision"].as_str().unwrap();
    let changed = read.result["document"]
        .as_str()
        .unwrap()
        .replace("Open tasks", "Focused tasks");

    let stale = operations.update_view_source(&json!({
        "path": "TaskNotes/Views/tasks.base",
        "if_revision": "sha256:stale",
        "document": changed.clone(),
    }));
    assert!(!stale.valid);
    assert_eq!(stale.diagnostics[0].code, "concurrent_modification");

    let updated = operations.update_view_source(&json!({
        "path": "TaskNotes/Views/tasks.base",
        "if_revision": revision,
        "document": changed,
    }));
    assert!(updated.valid, "{:?}", updated.diagnostics);
    assert_ne!(updated.result["revision"], revision);
    let listed = Collection::open(root.path())
        .unwrap()
        .v03_operations()
        .unwrap()
        .list_views(&json!({}));
    assert_eq!(
        listed.result["views"][0]["views"][0]["name"],
        "Focused tasks"
    );

    let deleted = collection
        .v03_operations()
        .unwrap()
        .delete_view_source(&json!({
            "path": "TaskNotes/Views/tasks.base",
            "if_revision": updated.result["revision"],
        }));
    assert!(deleted.valid, "{:?}", deleted.diagnostics);
    assert!(!root.path().join("TaskNotes/Views/tasks.base").exists());
}

#[test]
fn creates_valid_sources_without_clobbering_or_escaping_configuration() {
    let (root, collection) = collection();
    let operations = collection.v03_operations().unwrap();
    let document = "views:\n  - type: tasknotesTaskList\n    name: Inbox\n";
    let created = operations.create_view_source(&json!({
        "format": "obsidian.base",
        "name": "Inbox",
        "document": document,
    }));
    assert!(created.valid, "{:?}", created.diagnostics);
    assert_eq!(created.result["path"], "TaskNotes/Views/inbox.base");

    let conflict = operations.create_view_source(&json!({
        "path": "TaskNotes/Views/inbox.base",
        "document": document,
    }));
    assert!(!conflict.valid);
    assert_eq!(conflict.diagnostics[0].code, "path_conflict");

    let outside = operations.create_view_source(&json!({
        "path": "elsewhere/inbox.base",
        "document": document,
    }));
    assert!(!outside.valid);
    assert_eq!(outside.diagnostics[0].code, "invalid_view_path");

    let invalid = operations.create_view_source(&json!({
        "path": "TaskNotes/Views/broken.base",
        "document": "views: [",
    }));
    assert!(!invalid.valid);
    assert_eq!(invalid.diagnostics[0].code, "invalid_view");
    assert!(!root.path().join("TaskNotes/Views/broken.base").exists());
}

#[test]
fn creates_obsidian_sources_in_a_neutral_default_folder() {
    let root = tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        r#"spec_version: 0.3.0
x-obsidian:
  bases:
    include: ["views/**/*.base"]
"#,
    )
    .unwrap();
    let collection = Collection::open(root.path()).unwrap();

    let created = collection
        .v03_operations()
        .unwrap()
        .create_view_source(&json!({
            "format": "obsidian.base",
            "name": "Inbox",
            "document": "views:\n  - type: table\n    name: Inbox\n"
        }));

    assert!(created.valid, "{:?}", created.diagnostics);
    assert_eq!(created.result["path"], "views/inbox.base");
    assert!(root.path().join("views/inbox.base").exists());
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

#[test]
fn invalid_collection_timezone_is_rejected_deterministically() {
    let root = tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  timezone: Australia/Atlantis\n",
    )
    .unwrap();
    let loaded = mdbase::config::load_config(root.path());
    assert_eq!(loaded["valid"], false);
    assert_eq!(loaded["error"]["code"], "invalid_config");
    assert!(loaded["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Unknown IANA timezone"));
}

#[test]
fn reports_invalid_missing_and_unsupported_view_requests() {
    let (root, collection) = collection();
    fs::write(
        root.path().join("TaskNotes/Views/invalid.base"),
        "views: [not: valid",
    )
    .unwrap();
    let operations = collection.v03_operations().unwrap();

    let listed = operations.list_views(&json!({}));
    assert!(listed.valid, "{:?}", listed.diagnostics);
    assert!(listed
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "invalid_view" && diagnostic.severity == "warning"));

    for (input, code) in [
        (
            json!({"path": "TaskNotes/Views/invalid.base", "view": "anything"}),
            "invalid_view",
        ),
        (
            json!({"path": "TaskNotes/Views/tasks.base", "view": "missing"}),
            "view_not_found",
        ),
        (
            json!({"path": "TaskNotes/Views/missing.base", "view": "missing"}),
            "view_not_found",
        ),
        (
            json!({
                "path": "TaskNotes/Views/tasks.base",
                "view": "open-tasks",
                "render": true
            }),
            "unsupported_presentation",
        ),
        (
            json!({"path": "../outside.base", "view": "anything"}),
            "invalid_path",
        ),
    ] {
        let result = operations.execute_view(&input);
        assert!(!result.valid, "{input}: {:?}", result.diagnostics);
        assert_eq!(result.diagnostics[0].code, code, "{input}");
    }
}

#[test]
fn rejects_invalid_expressions_before_scanning_records() {
    let (root, collection) = collection();
    fs::write(
        root.path().join("TaskNotes/Views/bad-expression.base"),
        "views:\n  - type: table\n    name: Bad\n    filters: 'status == '\n    order: [file.name]\n",
    )
    .unwrap();
    let result = collection.v03_operations().unwrap().execute_view(&json!({
        "path": "TaskNotes/Views/bad-expression.base",
        "view": "bad"
    }));
    assert!(!result.valid);
    assert!(result
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "invalid_view"
            && diagnostic.field.as_deref() == Some("views.filters")));
}

#[cfg(unix)]
#[test]
fn rejects_view_paths_through_symbolic_links() {
    use std::os::unix::fs::symlink;

    let (root, collection) = collection();
    let outside = tempdir().unwrap();
    fs::write(
        outside.path().join("outside.base"),
        "views:\n  - type: table\n    name: Outside\n",
    )
    .unwrap();
    symlink(outside.path(), root.path().join("TaskNotes/Views/link")).unwrap();
    let result = collection.v03_operations().unwrap().execute_view(&json!({
        "path": "TaskNotes/Views/link/outside.base",
        "view": "outside"
    }));
    assert!(!result.valid);
    assert_eq!(result.diagnostics[0].code, "path_traversal");
}
