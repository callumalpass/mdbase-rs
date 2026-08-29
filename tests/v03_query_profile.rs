use std::fs;

use mdbase::api::{CollectionPath, FrontmatterMode, QueryDirection, QueryOrder, QueryRequest};
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
fn cancelled_queries_stop_without_becoming_collection_diagnostics() {
    let (_root, collection) = query_collection();
    let cancellation = mdbase::OperationCancellation::new();
    cancellation.cancel();

    let result = collection.v03_operations().unwrap().query_cancellable(
        &json!({"where": "file.body.lower().contains('body')"}),
        &cancellation,
    );

    assert_eq!(result.unwrap_err(), mdbase::OperationCancelled);
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
fn invocation_timezone_controls_datetime_calendar_conversion() {
    let (root, collection) = query_collection();
    write_record(
        &root,
        "tasks/temporal.md",
        "---\ntype: task\ntitle: Temporal\nscheduled: 2026-08-05T23:30:00Z\n---\n",
    );

    let melbourne = query(
        &collection,
        json!({
            "types": ["task"],
            "timezone": "Australia/Melbourne",
            "where": "date(scheduled) == date('2026-08-06')"
        }),
    );
    assert!(melbourne.valid, "{melbourne:#?}");
    assert_eq!(melbourne.result["meta"]["total_count"], 1);

    let los_angeles = query(
        &collection,
        json!({
            "types": ["task"],
            "timezone": "America/Los_Angeles",
            "where": "date(scheduled) == date('2026-08-05')"
        }),
    );
    assert!(los_angeles.valid, "{los_angeles:#?}");
    assert_eq!(los_angeles.result["meta"]["total_count"], 1);

    let invalid = query(&collection, json!({"timezone": "+10:00", "where": "true"}));
    assert!(!invalid.valid);
    assert_eq!(invalid.diagnostics[0].code, "invalid_timezone");
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

fn diagnostic_signature(
    diagnostic: &mdbase::api::Diagnostic,
) -> (
    &str,
    Option<&str>,
    Option<&str>,
    Option<&str>,
    Option<&Value>,
) {
    (
        diagnostic.code.as_str(),
        diagnostic.path.as_deref(),
        diagnostic.field.as_deref(),
        diagnostic.schema_location.as_deref(),
        diagnostic.details.as_ref(),
    )
}

#[test]
fn hostile_typed_requests_match_wire_schema_failures_without_using_the_wire_path() {
    let (_root, collection) = query_collection();
    let long_type = format!("A{}", "a".repeat(128));
    let mut cases = Vec::<(&str, QueryRequest, Value)>::new();

    for (name, types) in [
        ("duplicate types", vec!["task", "task"]),
        ("empty type", vec![""]),
        ("invalid type", vec!["1task"]),
    ] {
        let request = QueryRequest {
            types: types.into_iter().map(str::to_string).collect(),
            ..QueryRequest::default()
        };
        cases.push((name, request.clone(), request.to_wire()));
    }
    let request = QueryRequest {
        types: vec![long_type.clone()],
        ..QueryRequest::default()
    };
    cases.push(("long type", request, json!({"types": [long_type]})));

    let timezone = QueryRequest {
        timezone: Some(String::new()),
        ..QueryRequest::default()
    };
    cases.push(("empty timezone", timezone, json!({"timezone": ""})));
    let filter = QueryRequest {
        where_expression: Some(String::new()),
        ..QueryRequest::default()
    };
    cases.push(("empty filter", filter, json!({"where": ""})));

    let mut projection_name = QueryRequest::default();
    projection_name
        .projections
        .insert("bad.name".to_string(), "true".to_string());
    cases.push((
        "projection name",
        projection_name,
        json!({"projections": {"bad.name": {"expr": "true"}}}),
    ));
    let mut projection_expression = QueryRequest::default();
    projection_expression
        .projections
        .insert("valid".to_string(), String::new());
    cases.push((
        "projection expression",
        projection_expression,
        json!({"projections": {"valid": {"expr": ""}}}),
    ));

    let empty_select = QueryRequest {
        select: Some(Vec::new()),
        ..QueryRequest::default()
    };
    cases.push(("empty select", empty_select, json!({"select": []})));
    let empty_selection = QueryRequest {
        select: Some(vec![String::new()]),
        ..QueryRequest::default()
    };
    cases.push(("empty selection", empty_selection, json!({"select": [""]})));

    for (name, property) in [
        ("empty order field", "order_by"),
        ("empty group field", "group_by"),
    ] {
        let order = QueryOrder {
            field: String::new(),
            direction: QueryDirection::Asc,
        };
        let mut request = QueryRequest::default();
        if property == "order_by" {
            request.order_by.push(order);
        } else {
            request.group_by.push(order);
        }
        let wire_input = request.to_wire();
        cases.push((name, request, wire_input));
    }

    let mut combined = QueryRequest {
        types: vec!["1bad".to_string(), "1bad".to_string()],
        timezone: Some(String::new()),
        where_expression: Some(String::new()),
        select: Some(vec![String::new()]),
        order_by: vec![QueryOrder {
            field: String::new(),
            direction: QueryDirection::Asc,
        }],
        group_by: vec![QueryOrder {
            field: String::new(),
            direction: QueryDirection::Desc,
        }],
        ..QueryRequest::default()
    };
    combined
        .projections
        .insert("bad.name".to_string(), String::new());
    cases.push((
        "combined diagnostic order",
        combined.clone(),
        combined.to_wire(),
    ));

    for (name, request, wire_input) in cases {
        let typed = collection.typed().unwrap().query(request).unwrap_err();
        let wire = query(&collection, wire_input);
        assert!(!wire.valid, "{name}: {wire:#?}");
        let wire_diagnostics = wire
            .diagnostics
            .into_iter()
            .map(mdbase::api::Diagnostic::from)
            .collect::<Vec<_>>();
        assert_eq!(
            typed
                .diagnostics()
                .iter()
                .map(diagnostic_signature)
                .collect::<Vec<_>>(),
            wire_diagnostics
                .iter()
                .map(diagnostic_signature)
                .collect::<Vec<_>>(),
            "{name}"
        );
    }
}

#[test]
fn typed_query_projects_directly_to_the_unchanged_wire_result() {
    let (_root, collection) = query_collection();
    let mut request = QueryRequest::builder()
        .type_name("task")
        .where_expression("estimate != 'many'")
        .order_by("projection.adjusted", QueryDirection::Desc)
        .offset(1)
        .limit(1);
    request.context = Some(CollectionPath::new("projects/alpha.md").unwrap());
    request
        .projections
        .insert("adjusted".to_string(), "estimate + 1".to_string());
    request.select = Some(vec!["title".to_string(), "projection.adjusted".to_string()]);
    request.group_by = vec![QueryOrder {
        field: "project".to_string(),
        direction: QueryDirection::Asc,
    }];
    // Typed requests intentionally have no summary or selection-expression
    // fields; every supported field is projected here without serde defaults.
    request.include_body = true;
    request.frontmatter_mode = FrontmatterMode::Both;

    let wire = query(&collection, request.to_wire());
    let typed = collection.typed().unwrap().query(request).unwrap();
    assert!(wire.valid, "{wire:#?}");
    assert_eq!(json!(typed.value.records), wire.result["results"]);
    assert_eq!(typed.value.meta, wire.result["meta"]);
    let wire_diagnostics = wire
        .diagnostics
        .clone()
        .into_iter()
        .map(mdbase::api::Diagnostic::from)
        .collect::<Vec<_>>();
    assert_eq!(typed.diagnostics, wire_diagnostics);
    assert!(v03::validate_query_result(&wire.result).is_empty());
    let envelope = serde_json::to_value(&wire).unwrap();
    let envelope_schema: Value =
        serde_json::from_str(include_str!("../schemas/v0.3/operation-result.schema.json")).unwrap();
    let diagnostic_schema =
        serde_json::from_str(include_str!("../schemas/v0.3/diagnostic.schema.json")).unwrap();
    let envelope_validator = jsonschema::JSONSchema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .with_document(
            "https://mdbase.dev/schemas/v0.3/diagnostic.schema.json".to_string(),
            diagnostic_schema,
        )
        .compile(&envelope_schema)
        .unwrap();
    let envelope_errors = envelope_validator
        .validate(&envelope)
        .err()
        .into_iter()
        .flatten()
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(envelope_errors.is_empty(), "{envelope_errors:#?}");

    let invalid_request = QueryRequest::builder().where_expression("(");
    let invalid_wire = query(&collection, invalid_request.to_wire());
    let invalid_typed = collection
        .typed()
        .unwrap()
        .query(invalid_request)
        .unwrap_err();
    let invalid_wire_diagnostics = invalid_wire
        .diagnostics
        .into_iter()
        .map(mdbase::api::Diagnostic::from)
        .collect::<Vec<_>>();
    assert_eq!(invalid_typed.diagnostics(), invalid_wire_diagnostics);
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

    let (typed_page, typed_page_profile) = operations.query_profiled(&json!({
        "types": ["task"],
        "order_by": [{"field": "file.path", "direction": "asc"}],
        "limit": 1,
    }));
    assert!(typed_page.valid, "{typed_page:#?}");
    assert_eq!(typed_page.result["meta"]["total_count"], 3);
    assert_eq!(
        typed_page.result["results"][0]["file"]["path"],
        "tasks/a.md"
    );
    assert_eq!(typed_page_profile.records_loaded, 1);
    assert_eq!(typed_page_profile.candidates, 3);
    assert_eq!(typed_page_profile.results, 1);

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
fn invalid_utf8_is_a_canonical_stub_while_legacy_query_envelope_is_unchanged() {
    let (root, collection) = query_collection();
    assert_eq!(collection.cache_rebuild()["success"], true);
    let unreadable = root.path().join("tasks/unreadable.md");
    fs::write(&unreadable, [0xff, 0xfe]).unwrap();

    let canonical = query(&collection, json!({"types": ["task"]}));
    assert!(canonical.valid, "{canonical:#?}");
    assert_eq!(canonical.result["meta"]["total_count"], 3);
    assert!(canonical.diagnostics.is_empty());

    let untyped = query(&collection, json!({}));
    assert!(untyped.valid, "{untyped:#?}");
    assert_eq!(untyped.result["meta"]["total_count"], 5);
    assert_eq!(untyped.diagnostics.len(), 1);
    assert_eq!(untyped.diagnostics[0].code, "invalid_frontmatter");
    assert_eq!(
        untyped.diagnostics[0].path.as_deref(),
        Some("tasks/unreadable.md")
    );
    assert_eq!(
        untyped.diagnostics[0].details.as_ref().unwrap()["reason"],
        "invalid_utf8"
    );

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
