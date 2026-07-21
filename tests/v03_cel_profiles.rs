use std::fs;

use mdbase::Collection;
use serde_json::json;
use tempfile::TempDir;

fn v03_collection(type_file: &str, records: &[(&str, &str)]) -> (TempDir, Collection) {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  timezone: UTC\n  validation: error\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(root.path().join("_types/test.md"), type_file).unwrap();
    for (path, contents) in records {
        let target = root.path().join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, contents).unwrap();
    }
    let collection = Collection::open(root.path()).unwrap();
    (root, collection)
}

#[test]
fn portable_cel_accepts_the_required_depth_and_rejects_excess_depth() {
    let (_root, collection) = v03_collection(
        "---\nkind: mdbase.type\nname: test\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n",
        &[],
    );
    let operations = collection.v03_operations().unwrap();

    let nested = |depth| {
        (0..depth).fold("true".to_string(), |inner, _| {
            format!("if(true, {inner}, false)")
        })
    };
    let supported = operations.evaluate_cel(&json!({"expression": nested(100)}));
    assert!(supported.valid, "{supported:#?}");
    assert_eq!(supported.result["value"], true);

    let rejected = operations.evaluate_cel(&json!({"expression": nested(129)}));
    assert!(!rejected.valid);
    assert_eq!(rejected.diagnostics[0].code, "expression_depth_exceeded");
}

#[test]
fn portable_cel_bounds_source_and_supports_iso8601_durations() {
    let (_root, collection) = v03_collection(
        "---\nkind: mdbase.type\nname: test\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n",
        &[],
    );
    let operations = collection.v03_operations().unwrap();

    let duration = operations.evaluate_cel(&json!({
        "expression": "duration('P1DT2H30M') == 95400000"
    }));
    assert!(duration.valid, "{duration:#?}");
    assert_eq!(duration.result["value"], true);

    let stable_clock = operations.evaluate_cel(&json!({
        "expression": "now() == now() && today() == today()",
        "timezone": "Australia/Melbourne"
    }));
    assert!(stable_clock.valid, "{stable_clock:#?}");
    assert_eq!(stable_clock.result["value"], true);

    let oversized = operations.evaluate_cel(&json!({
        "expression": format!("'{}'", "x".repeat(64 * 1024))
    }));
    assert!(!oversized.valid);
    assert_eq!(
        oversized.diagnostics[0].code,
        "expression_source_limit_exceeded"
    );
}

#[test]
fn match_evaluation_errors_are_reported_and_do_not_match() {
    let (_root, collection) = v03_collection(
        "---\nkind: mdbase.type\nname: test\nmatch:\n  path_glob: records/**/*.md\n  expr:\n    $expr: '\"not-a-number\" - 1 > 1'\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n",
        &[("records/failing.md", "---\ntitle: Failing\n---\n")],
    );
    let operations = collection.v03_operations().unwrap();

    let matched = operations.get_types(&json!({"path": "records/failing.md"}));
    assert!(matched.valid, "{matched:#?}");
    assert_eq!(matched.result["types"], json!([]));
    assert_eq!(matched.diagnostics.len(), 1);
    assert_eq!(matched.diagnostics[0].code, "expression_evaluation_error");
    assert_eq!(matched.diagnostics[0].type_name.as_deref(), Some("test"));
    assert_eq!(
        matched.diagnostics[0]
            .details
            .as_ref()
            .and_then(|details| details.get("context")),
        Some(&json!("match"))
    );

    let read = operations.read(&json!({"path": "records/failing.md"}));
    assert!(read.valid, "{read:#?}");
    assert_eq!(read.result["types"], json!([]));
    assert_eq!(read.diagnostics[0].code, "expression_evaluation_error");
}
