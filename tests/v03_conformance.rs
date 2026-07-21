use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use mdbase::Collection;
use serde::Deserialize;
use serde_json::{Map, Value};
use tempfile::TempDir;

#[derive(Debug, Deserialize)]
struct Suite {
    fixture_set: String,
    groups: Vec<Group>,
}

#[derive(Debug, Deserialize)]
struct Group {
    name: String,
    setup: Setup,
    tests: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Setup {
    #[serde(default = "default_config")]
    config: String,
    #[serde(default)]
    types: HashMap<String, String>,
    #[serde(default)]
    files: HashMap<String, String>,
    #[serde(default)]
    event: Option<serde_yaml::Value>,
    #[serde(default)]
    steps: Option<serde_yaml::Value>,
}

fn default_config() -> String {
    "spec_version: \"0.3.0\"\n".to_string()
}

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    operation: String,
    #[serde(default)]
    input: serde_yaml::Value,
    #[serde(default)]
    expect: serde_yaml::Value,
}

fn fixture_path(relative_path: &str) -> PathBuf {
    std::env::var_os("MDBASE_SPEC_REPO_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../mdbase-spec"))
        .join("tests/v0.3")
        .join(relative_path)
}

fn materialize(setup: &Setup) -> TempDir {
    let directory = tempfile::tempdir().expect("create fixture collection");
    fs::write(directory.path().join("mdbase.yaml"), &setup.config).expect("write config");

    let types_directory = directory.path().join("_types");
    fs::create_dir_all(&types_directory).expect("create types directory");
    for (relative_path, content) in &setup.types {
        write(&types_directory, relative_path, content);
    }
    for (relative_path, content) in &setup.files {
        write(directory.path(), relative_path, content);
    }
    directory
}

fn write(root: &Path, relative_path: &str, content: &str) {
    let path = root.join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture directory");
    }
    fs::write(path, content).expect("write fixture file");
}

fn yaml_to_json(value: &serde_yaml::Value) -> Value {
    serde_json::to_value(value).expect("convert fixture YAML to JSON")
}

fn execute(collection: &Collection, setup: &Setup, case: &Case, expected: &Value) -> Value {
    let input = yaml_to_json(&case.input);
    let operations = collection
        .v03_operations()
        .expect("shared v0.3 fixture requires v0.3 operations");
    match case.operation.as_str() {
        "validate" => {
            let envelope = operations.validate(&input);
            let mut result = flatten_envelope(envelope);
            result["issues"] = result["diagnostics"].clone();
            if let Some(fields) = expected.get("resolved_links").and_then(Value::as_object) {
                let path = input
                    .get("path")
                    .and_then(Value::as_str)
                    .expect("resolved link assertion requires input.path");
                let resolved = fields
                    .keys()
                    .map(|field| {
                        let resolution = collection.resolve_link(&serde_json::json!({
                            "path": path,
                            "field": field,
                        }));
                        (
                            field.clone(),
                            resolution
                                .get("resolved_path")
                                .cloned()
                                .unwrap_or(Value::Null),
                        )
                    })
                    .collect::<Map<String, Value>>();
                result["resolved_links"] = Value::Object(resolved);
            }
            result
        }
        "read" => {
            let mut result = flatten_envelope(operations.read(&input));
            if input.get("effective") == Some(&Value::Bool(false)) {
                result["frontmatter"] = result["raw_frontmatter"].clone();
            }
            result
        }
        "query" => {
            let mut result = flatten_envelope(operations.query(&input));
            let body_returned = result
                .get("results")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|record| record.get("body").is_some());
            result["body_returned"] = Value::Bool(body_returned);
            result
        }
        "evaluate_cel" => {
            let mut input = input;
            if input.get("context").and_then(Value::as_str) == Some("workflow") {
                let mut bindings = Map::new();
                if let Some(event) = &setup.event {
                    bindings.insert("event".to_string(), yaml_to_json(event));
                }
                if let Some(steps) = &setup.steps {
                    bindings.insert("steps".to_string(), yaml_to_json(steps));
                }
                input["bindings"] = Value::Object(bindings);
            }
            flatten_envelope(operations.evaluate_cel(&input))
        }
        "evaluate_workflow_input" => {
            let mut input = input;
            let mut bindings = Map::new();
            if let Some(event) = &setup.event {
                bindings.insert("event".to_string(), yaml_to_json(event));
            }
            if let Some(steps) = &setup.steps {
                bindings.insert("steps".to_string(), yaml_to_json(steps));
            }
            input["bindings"] = Value::Object(bindings);
            flatten_envelope(operations.evaluate_workflow_input(&input))
        }
        "get_types" => {
            let path = input
                .get("path")
                .and_then(Value::as_str)
                .expect("get_types requires input.path");
            let read = collection.read(&serde_json::json!({"path": path}));
            serde_json::json!({
                "valid": read.get("error").is_none(),
                "types": read.get("types").cloned().unwrap_or_else(|| serde_json::json!([])),
            })
        }
        "get_type" => {
            let name = input
                .get("name")
                .and_then(Value::as_str)
                .expect("get_type requires input.name");
            match collection.types.get(name) {
                Some(type_definition) => serde_json::json!({
                    "valid": true,
                    "type": {
                        "name": type_definition.name,
                        "collection": {
                            "display": {
                                "name_field": type_definition.display_name_key,
                            }
                        }
                    }
                }),
                None => serde_json::json!({
                    "valid": false,
                    "error": {"code": "unknown_type", "message": name},
                }),
            }
        }
        "create" => {
            let mut result = flatten_envelope(operations.create(&input));
            expose_operation_issues(&mut result);
            result
        }
        "update" => {
            let path = input
                .get("path")
                .and_then(Value::as_str)
                .expect("update requires input.path");
            let before = operations.read(&serde_json::json!({"path": path}));
            let before = before
                .result
                .get("raw_frontmatter")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let mut result = flatten_envelope(operations.update(&input));
            expose_operation_issues(&mut result);
            if let Some(after) = result.get("frontmatter").and_then(Value::as_object) {
                let mut changed = before
                    .keys()
                    .chain(after.keys())
                    .filter(|field| before.get(*field) != after.get(*field))
                    .cloned()
                    .collect::<Vec<_>>();
                changed.sort();
                changed.dedup();
                result["frontmatter_changed"] =
                    Value::Array(changed.into_iter().map(Value::String).collect());
            }
            result
        }
        operation => panic!("unsupported v0.3 fixture operation: {operation}"),
    }
}

fn expose_operation_issues(result: &mut Value) {
    result["issues"] = result["diagnostics"].clone();
    if result.get("valid") == Some(&Value::Bool(false)) {
        if let Some(first) = result
            .get("diagnostics")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
        {
            result["error"] = first.clone();
        }
    }
}

fn flatten_envelope(envelope: mdbase::v03::OperationResult) -> Value {
    let mut result = envelope.result.as_object().cloned().unwrap_or_default();
    result.insert("valid".to_string(), Value::Bool(envelope.valid));
    result.insert(
        "diagnostics".to_string(),
        serde_json::to_value(envelope.diagnostics).expect("serialize diagnostics"),
    );
    Value::Object(result)
}

fn assert_expectation(actual: &Value, expected: &Value, case_name: &str) {
    let expected_object = expected.as_object().expect("expect must be a mapping");
    for (key, expected_value) in expected_object {
        match key.as_str() {
            "issues" => assert_array_contains(
                actual.get("issues").and_then(Value::as_array),
                expected_value.as_array(),
                case_name,
                "issues",
            ),
            "frontmatter_not_contains" => {
                let frontmatter = actual
                    .get("frontmatter")
                    .and_then(Value::as_object)
                    .unwrap_or_else(|| panic!("{case_name}: missing frontmatter"));
                for field in expected_value
                    .as_array()
                    .expect("frontmatter_not_contains must be an array")
                {
                    let field = field.as_str().expect("field name must be a string");
                    assert!(
                        !frontmatter.contains_key(field),
                        "{case_name}: frontmatter unexpectedly contains {field}: {actual:#}"
                    );
                }
            }
            "frontmatter_contains" => {
                let frontmatter = actual
                    .get("frontmatter")
                    .and_then(Value::as_object)
                    .unwrap_or_else(|| panic!("{case_name}: missing frontmatter"));
                for (field, constraint) in expected_value
                    .as_object()
                    .expect("frontmatter_contains must be an object")
                {
                    let value = frontmatter.get(field).unwrap_or_else(|| {
                        panic!("{case_name}: frontmatter is missing {field}: {actual:#}")
                    });
                    assert_value_constraint(value, constraint, case_name, field);
                }
            }
            _ => {
                let actual_value = actual
                    .get(key)
                    .unwrap_or_else(|| panic!("{case_name}: missing result key {key}: {actual:#}"));
                assert_subset(actual_value, expected_value, case_name, key);
            }
        }
    }
}

fn assert_value_constraint(actual: &Value, expected: &Value, case_name: &str, path: &str) {
    if let Some(pattern) = expected.get("matches").and_then(Value::as_str) {
        let value = actual
            .as_str()
            .unwrap_or_else(|| panic!("{case_name}: {path} is not a string: {actual}"));
        let pattern = regex::Regex::new(pattern).expect("fixture regex must compile");
        assert!(
            pattern.is_match(value),
            "{case_name}: {path} does not match {}: {actual}",
            pattern.as_str()
        );
        return;
    }
    if let Some(format) = expected.get("format").and_then(Value::as_str) {
        let value = actual
            .as_str()
            .unwrap_or_else(|| panic!("{case_name}: {path} is not a string: {actual}"));
        let valid = match format {
            "date" => chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok(),
            "date-time" => chrono::DateTime::parse_from_rfc3339(value).is_ok(),
            other => panic!("unsupported fixture format constraint: {other}"),
        };
        assert!(valid, "{case_name}: {path} is not {format}: {actual}");
        return;
    }
    assert_subset(actual, expected, case_name, path);
}

fn assert_array_contains(
    actual: Option<&Vec<Value>>,
    expected: Option<&Vec<Value>>,
    case_name: &str,
    path: &str,
) {
    let actual = actual.unwrap_or_else(|| panic!("{case_name}: missing result array {path}"));
    let expected =
        expected.unwrap_or_else(|| panic!("{case_name}: expected {path} must be an array"));
    for expected_item in expected {
        assert!(
            actual
                .iter()
                .any(|actual_item| is_subset(actual_item, expected_item)),
            "{case_name}: {path} does not contain {expected_item:#}: {actual:#?}"
        );
    }
}

fn assert_subset(actual: &Value, expected: &Value, case_name: &str, path: &str) {
    assert!(
        is_subset(actual, expected),
        "{case_name}: mismatch at {path}\nexpected subset: {expected:#}\nactual: {actual:#}"
    );
}

fn is_subset(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected.iter().all(|(key, value)| {
            actual
                .get(key)
                .is_some_and(|actual| is_subset(actual, value))
        }),
        (Value::Array(actual), Value::Array(expected)) => expected
            .iter()
            .all(|value| actual.iter().any(|actual| is_subset(actual, value))),
        _ => actual == expected,
    }
}

#[test]
fn shared_v03_core_collection_fixture_passes() {
    run_suite("core/core-collection.yaml", "core_collection");
}

#[test]
fn shared_v03_lifecycle_fixture_passes() {
    run_suite("lifecycle/lifecycle.yaml", "lifecycle");
}

#[test]
fn shared_v03_cel_fixture_passes() {
    run_suite("cel/cel-profile.yaml", "cel");
}

fn run_suite(relative_path: &str, fixture_set: &str) {
    let path = fixture_path(relative_path);
    if !path.exists() && std::env::var_os("MDBASE_REQUIRE_V03_CONFORMANCE").is_none() {
        return;
    }
    let fixture = fs::read_to_string(path).expect("read shared v0.3 fixture");
    let suite: Suite = serde_yaml::from_str(&fixture).expect("parse shared v0.3 fixture");
    assert_eq!(suite.fixture_set, fixture_set);

    for group in suite.groups {
        for case in group.tests {
            let directory = materialize(&group.setup);
            let collection = Collection::open(directory.path())
                .unwrap_or_else(|error| panic!("{}: open collection: {error:#}", group.name));
            let expected = yaml_to_json(&case.expect);
            let actual = execute(&collection, &group.setup, &case, &expected);
            assert_expectation(&actual, &expected, &case.name);
        }
    }
}
