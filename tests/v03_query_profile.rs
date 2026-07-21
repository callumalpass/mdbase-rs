use std::fs;

use mdbase::{v03, Collection};
use serde_json::{json, Value};
use tempfile::TempDir;

fn query_collection() -> (TempDir, Collection) {
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
    properties:
      type: { const: task }
      title: { type: string }
      status: { type: string }
      project: { type: string }
      estimate: {}
      optional: {}
collection:
  read_defaults:
    status: open
---
"#,
    )
    .unwrap();
    fs::write(
        root.path().join("_types/project.md"),
        r#"---
kind: mdbase.type
name: project
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    properties:
      type: { const: project }
      title: { type: string }
      project: { type: string }
---
"#,
    )
    .unwrap();
    write_record(
        &root,
        "projects/alpha.md",
        "---\ntype: project\ntitle: Alpha\nproject: alpha\n---\n",
    );
    write_record(
        &root,
        "tasks/a.md",
        "---\ntype: task\ntitle: A\nproject: alpha\nestimate: 2\n---\nBody #alpha",
    );
    write_record(
        &root,
        "tasks/b.md",
        "---\ntype: task\ntitle: B\nstatus: done\nproject: beta\nestimate: 5\noptional:\n---\n",
    );
    write_record(
        &root,
        "tasks/bad.md",
        "---\ntype: task\ntitle: Bad\nproject: alpha\nestimate: many\n---\n",
    );
    let collection = Collection::open(root.path()).unwrap();
    (root, collection)
}

fn write_record(root: &TempDir, path: &str, contents: &str) {
    let target = root.path().join(path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, contents).unwrap();
}

fn query(collection: &Collection, input: Value) -> v03::OperationResult {
    collection.v03_operations().unwrap().query(&input)
}

#[test]
fn schema_and_semantic_preflight_fail_before_scanning() {
    let (_root, collection) = query_collection();

    let unknown = query(&collection, json!({"folder": "tasks"}));
    assert!(!unknown.valid);
    assert_eq!(unknown.diagnostics[0].code, "invalid_query");

    let cycle = query(
        &collection,
        json!({
            "projections": {
                "first": {"expr": "projection.second"},
                "second": {"expr": "projection.first"}
            }
        }),
    );
    assert!(!cycle.valid);
    assert!(cycle.diagnostics.iter().any(
        |diagnostic| diagnostic.code == "invalid_query" && diagnostic.message.contains("cycle")
    ));

    let duplicate = query(&collection, json!({"select": ["title", "file.title"]}));
    assert!(!duplicate.valid);
    assert!(duplicate.diagnostics[0].message.contains("duplicated"));

    let unavailable = query(
        &collection,
        json!({"where": "steps.patch.status == 'done'"}),
    );
    assert!(!unavailable.valid);
    assert!(unavailable.diagnostics[0]
        .message
        .contains("System binding 'steps' is unavailable"));
}

#[test]
fn context_projections_selection_and_record_errors_follow_portable_semantics() {
    let (_root, collection) = query_collection();
    let result = query(
        &collection,
        json!({
            "types": ["task"],
            "context": {"this": {"path": "projects/alpha.md"}},
            "projections": {
                "adjusted": {"expr": "estimate + 1"},
                "selected": {"expr": "projection.adjusted >= 3 && this.project == project"}
            },
            "where": "projection.selected",
            "select": [
                "title",
                "projection.adjusted",
                {"name": "display", "expr": "title + '!'"}
            ],
            "order_by": [{"field": "projection.adjusted", "direction": "desc"}]
        }),
    );

    assert!(result.valid, "{result:#?}");
    assert_eq!(
        result.result["meta"]["context"]["path"],
        "projects/alpha.md"
    );
    assert_eq!(result.result["meta"]["total_count"], 1);
    assert_eq!(result.result["results"][0]["file"]["path"], "tasks/a.md");
    assert_eq!(result.result["results"][0]["values"]["adjusted"], 3);
    assert_eq!(result.result["results"][0]["values"]["display"], "A!");
    assert!(result
        .diagnostics
        .iter()
        .any(
            |diagnostic| diagnostic.path.as_deref() == Some("tasks/bad.md")
                && diagnostic.field.as_deref() == Some("projections.adjusted")
        ));
    let result_shape = v03::validate_query_result(&result.result);
    assert!(result_shape.is_empty(), "{result_shape:#?}\n{result:#?}");
}

#[test]
fn ordering_pagination_frontmatter_and_body_modes_are_deterministic() {
    let (_root, collection) = query_collection();
    let result = query(
        &collection,
        json!({
            "types": ["task"],
            "where": "estimate != 'many'",
            "order_by": [{"field": "estimate", "direction": "desc"}],
            "offset": 1,
            "limit": 1,
            "frontmatter": "both",
            "include_body": true
        }),
    );

    assert!(result.valid, "{result:#?}");
    assert_eq!(result.result["meta"]["total_count"], 2);
    assert!(!result.result["meta"]["has_more"].as_bool().unwrap());
    assert_eq!(result.result["results"][0]["file"]["path"], "tasks/a.md");
    assert_eq!(result.result["results"][0]["frontmatter"]["status"], "open");
    assert!(result.result["results"][0]["raw_frontmatter"]
        .get("status")
        .is_none());
    assert!(result.result["results"][0]["body"]
        .as_str()
        .unwrap()
        .contains("#alpha"));

    let empty_page = query(&collection, json!({"types": ["task"], "limit": 0}));
    assert!(empty_page.valid);
    assert_eq!(empty_page.result["results"], json!([]));
    assert_eq!(empty_page.result["meta"]["total_count"], 3);
    assert_eq!(empty_page.result["meta"]["has_more"], true);
}

#[test]
fn grouping_and_builtin_and_custom_summaries_describe_the_full_result_set() {
    let (_root, collection) = query_collection();
    let result = query(
        &collection,
        json!({
            "types": ["task"],
            "where": "estimate != 'many'",
            "group_by": [{"field": "status", "direction": "asc"}],
            "summary_functions": {
                "double_count": {"expr": "values.size() * 2"}
            },
            "summaries": [
                {"field": "estimate", "function": "sum", "name": "total"},
                {"field": "estimate", "function": "double_count", "name": "weighted"}
            ],
            "limit": 1
        }),
    );

    assert!(result.valid, "{result:#?}");
    assert_eq!(result.result["meta"]["total_count"], 2);
    assert_eq!(result.result["meta"]["has_more"], true);
    let groups = result.result["meta"]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["values"]["status"], "done");
    assert_eq!(groups[0]["summaries"]["total"], 5);
    assert_eq!(groups[0]["summaries"]["weighted"], 2);
    assert_eq!(groups[1]["values"]["status"], "open");
    assert_eq!(groups[1]["summaries"]["total"], 2);
}

#[test]
fn unresolved_context_and_null_this_have_distinct_results() {
    let (_root, collection) = query_collection();
    let missing = query(
        &collection,
        json!({"context": {"this": {"path": "missing.md"}}}),
    );
    assert!(!missing.valid);
    assert_eq!(missing.diagnostics[0].code, "context_not_found");

    let no_context = query(
        &collection,
        json!({"types": ["project"], "where": "this == null"}),
    );
    assert!(no_context.valid, "{no_context:#?}");
    assert_eq!(no_context.result["meta"]["total_count"], 1);
    assert!(no_context.result["meta"].get("context").is_none());
}
