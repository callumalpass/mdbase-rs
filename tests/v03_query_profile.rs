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
            "frontmatter_mode": "both",
            "include_body": true
        }),
    );

    assert!(result.valid, "{result:#?}");
    assert_eq!(result.result["meta"]["total_count"], 2);
    assert!(!result.result["meta"]["has_more"].as_bool().unwrap());
    assert_eq!(result.result["results"][0]["file"]["path"], "tasks/a.md");
    assert_eq!(
        result.result["results"][0]["effective_frontmatter"]["status"],
        "open"
    );
    assert!(result.result["results"][0]["frontmatter"]
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

#[test]
fn profiling_exposes_lazy_query_plans_without_payloads() {
    let (_root, collection) = query_collection();
    assert_eq!(collection.cache_rebuild()["success"], true);
    let operations = collection.v03_operations().unwrap();

    let (metadata, metadata_profile) = operations.query_profiled(&json!({
        "order_by": [{"field": "file.mtime", "direction": "desc"}],
        "limit": 1,
    }));
    assert!(metadata.valid, "{metadata:#?}");
    assert_eq!(metadata_profile.records_loaded, 1);
    assert_eq!(metadata_profile.candidates, 4);
    assert_eq!(metadata_profile.results, 1);
    assert!(!metadata_profile.link_graph_built);
    let serialized = serde_json::to_string(&metadata_profile).unwrap();
    assert!(!serialized.contains("tasks/a.md"));
    assert!(!serialized.contains("Alpha"));

    let (traversal, traversal_profile) = operations.query_profiled(&json!({
        "where": "project.asFile().title == 'Alpha'",
    }));
    assert!(traversal.valid, "{traversal:#?}");
    assert!(traversal_profile.link_graph_built);

    let (body_metadata, body_profile) = operations.query_profiled(&json!({
        "select": ["file.tags"],
        "where": "file.tags.size() > 0",
    }));
    assert!(body_metadata.valid, "{body_metadata:#?}");
    assert!(!body_profile.link_graph_built);
    assert_eq!(
        body_metadata.result["results"][0]["values"]["tags"],
        json!(["alpha"])
    );
}

#[test]
fn query_pagination_refreshes_against_the_current_collection() {
    let (root, collection) = query_collection();
    assert_eq!(collection.cache_rebuild()["success"], true);
    let operations = collection.v03_operations().unwrap();
    let input = json!({
        "order_by": [{"field": "file.path", "direction": "asc"}],
        "limit": 1,
    });
    let first = operations.query(&input);
    assert!(first.valid, "{first:#?}");
    assert!(first.result["meta"].get("snapshot").is_none());

    write_record(
        &root,
        "tasks/added.md",
        "---\ntype: task\ntitle: Added\n---\n",
    );
    let continued = operations.query(&json!({
        "order_by": [{"field": "file.path", "direction": "asc"}],
        "offset": 1,
        "limit": 1,
    }));
    assert!(continued.valid, "{continued:#?}");
    assert_eq!(continued.result["meta"]["total_count"], 5);

    let refreshed = operations.query(&input);
    assert!(refreshed.valid, "{refreshed:#?}");
    assert_eq!(refreshed.result["meta"]["total_count"], 5);

    let legacy = operations.query(&json!({"snapshot": "legacy-token"}));
    assert!(!legacy.valid);
    assert_eq!(legacy.diagnostics[0].code, "invalid_query");
}

#[test]
fn sqlite_metadata_pagination_preserves_portable_ordering() {
    let (_root, collection) = query_collection();
    let operations = collection.v03_operations().unwrap();
    let input = json!({
        "order_by": [{"field": "file.mtime", "direction": "desc"}],
        "offset": 1,
        "limit": 2,
    });
    let uncached = operations.query(&input);
    assert!(uncached.valid, "{uncached:#?}");
    assert_eq!(collection.cache_rebuild()["success"], true);
    let cached = operations.query(&input);
    assert!(cached.valid, "{cached:#?}");
    let paths = |result: &v03::OperationResult| {
        result.result["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| record["file"]["path"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(paths(&cached), paths(&uncached));
    assert_eq!(
        cached.result["meta"]["total_count"],
        uncached.result["meta"]["total_count"]
    );
}

#[test]
fn corrupt_cache_rows_fall_back_to_authoritative_markdown() {
    let (root, collection) = query_collection();
    assert_eq!(collection.cache_rebuild()["success"], true);
    let database = root.path().join(".mdbase/cache.db");
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .execute(
            "UPDATE files SET frontmatter_json = 'not-json' WHERE path = 'tasks/a.md'",
            [],
        )
        .unwrap();
    drop(connection);

    let (result, performance) = collection
        .v03_operations()
        .unwrap()
        .query_profiled(&json!({"types": ["task"]}));

    assert!(result.valid, "{result:#?}");
    assert_eq!(result.result["meta"]["total_count"], 3);
    assert!(performance.cache_fallback);
    assert!(!performance.cache_used);
    assert!(result.result["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|record| record["effective_frontmatter"]["title"] == "A"
            && record["file"]["path"] == "tasks/a.md"));
}

#[test]
fn incompatible_cache_schema_falls_back_and_rebuild_reports_failure() {
    let (root, collection) = query_collection();
    assert_eq!(collection.cache_rebuild()["success"], true);
    let database = root.path().join(".mdbase/cache.db");
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .execute_batch(
            "
        DROP TABLE files;
        CREATE TABLE files (
            path TEXT PRIMARY KEY,
            ctime_ns INTEGER
        );
        ",
        )
        .unwrap();
    drop(connection);

    let (result, performance) = collection
        .v03_operations()
        .unwrap()
        .query_profiled(&json!({"types": ["task"]}));
    assert!(result.valid, "{result:#?}");
    assert_eq!(result.result["meta"]["total_count"], 3);
    assert!(performance.cache_fallback);
    assert!(!performance.cache_used);

    let rebuild = collection.cache_rebuild();
    assert_eq!(rebuild["success"], false);
    assert_eq!(rebuild["error"]["code"], "cache_rebuild_failed");
}

#[test]
fn unreadable_markdown_fails_the_snapshot_instead_of_disappearing() {
    let (root, collection) = query_collection();
    assert_eq!(collection.cache_rebuild()["success"], true);
    let unreadable = root.path().join("tasks/unreadable.md");
    fs::write(&unreadable, [0xff, 0xfe]).unwrap();

    let canonical = query(&collection, json!({"types": ["task"]}));
    assert!(!canonical.valid);
    assert_eq!(canonical.diagnostics.len(), 1);
    assert_eq!(canonical.diagnostics[0].code, "collection_snapshot_failed");
    assert_eq!(
        canonical.diagnostics[0].path.as_deref(),
        Some("tasks/unreadable.md")
    );
    assert!(canonical.diagnostics[0]
        .message
        .contains("failed to read collection file"));

    let legacy = collection.query(&json!({"types": ["task"]}));
    assert_eq!(
        legacy["error"]["code"], "collection_snapshot_failed",
        "{legacy:#}"
    );

    fs::remove_file(unreadable).unwrap();
    let recovered = query(&collection, json!({"types": ["task"]}));
    assert!(recovered.valid, "{recovered:#?}");
    assert_eq!(recovered.result["meta"]["total_count"], 3);
}
