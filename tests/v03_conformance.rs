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
    #[serde(default)]
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
    contracts: HashMap<String, String>,
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

impl Default for Setup {
    fn default() -> Self {
        Self {
            config: default_config(),
            types: HashMap::new(),
            contracts: HashMap::new(),
            files: HashMap::new(),
            event: None,
            steps: None,
        }
    }
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
    spec_root().join("tests/v0.3").join(relative_path)
}

fn spec_root() -> PathBuf {
    std::env::var_os("MDBASE_SPEC_REPO_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../mdbase-spec"))
}

fn materialize(setup: &Setup) -> TempDir {
    let directory = tempfile::tempdir().expect("create fixture collection");
    fs::write(directory.path().join("mdbase.yaml"), &setup.config).expect("write config");

    let config: serde_yaml::Value =
        serde_yaml::from_str(&setup.config).expect("parse fixture config");
    let config = yaml_to_json(&config);
    let types_folder = config
        .pointer("/settings/types_folder")
        .and_then(Value::as_str)
        .unwrap_or("_types");
    let contracts_folder = config
        .pointer("/settings/contracts_folder")
        .and_then(Value::as_str)
        .unwrap_or("_contracts");
    let types_directory = directory.path().join(types_folder);
    fs::create_dir_all(&types_directory).expect("create types directory");
    for (relative_path, content) in &setup.types {
        write(&types_directory, relative_path, content);
    }
    let contracts_directory = directory.path().join(contracts_folder);
    fs::create_dir_all(&contracts_directory).expect("create contracts directory");
    for (relative_path, content) in &setup.contracts {
        write(&contracts_directory, relative_path, content);
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
        "data_contract_implementation_validate"
        | "data_contract_digest"
        | "data_contract_implementation_digest"
        | "data_contract_registry_validate" => {
            execute_standalone_data_contract_case(&case.operation, &input)
        }
        "install_type_pack" => execute_type_pack_case(collection, &input),
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
        "read" => flatten_envelope(operations.read(&input)),
        "query" => {
            let mut result = flatten_envelope(operations.query(&input));
            let body_returned = result
                .get("results")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|record| record.get("body").is_some());
            result["body_returned"] = Value::Bool(body_returned);
            expose_query_aliases(&mut result);
            result
        }
        "list_views" => flatten_envelope(operations.list_views(&input)),
        "execute_view" => {
            let mut result = flatten_envelope(operations.execute_view(&input));
            expose_query_aliases(&mut result);
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
            match collection.types().get(name) {
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
        "get_data_contracts" => {
            let contract = input
                .get("contract")
                .and_then(Value::as_str)
                .expect("get_data_contracts requires input.contract");
            let version = input
                .get("version")
                .and_then(Value::as_str)
                .expect("get_data_contracts requires input.version");
            serde_json::json!({
                "implementations": collection.get_data_contract_implementations(contract, version)
            })
        }
        "get_contract_view" => {
            let path = input
                .get("path")
                .and_then(Value::as_str)
                .expect("get_contract_view requires input.path");
            let contract = input
                .get("contract")
                .and_then(Value::as_str)
                .expect("get_contract_view requires input.contract");
            let version = input
                .get("version")
                .and_then(Value::as_str)
                .expect("get_contract_view requires input.version");
            serde_json::to_value(collection.get_contract_view(
                path,
                contract,
                version,
                input.get("type").and_then(Value::as_str),
            ))
            .expect("serialize contract view")
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
                .get("frontmatter")
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

fn execute_type_pack_case(collection: &Collection, input: &Value) -> Value {
    let manifest_path = spec_root().join(
        input
            .get("pack")
            .and_then(Value::as_str)
            .expect("install_type_pack requires input.pack"),
    );
    let manifest_yaml = fs::read_to_string(&manifest_path).expect("read shared type pack manifest");
    let manifest_yaml: serde_yaml::Value =
        serde_yaml::from_str(&manifest_yaml).expect("parse shared type pack manifest");
    let mut manifest = yaml_to_json(&manifest_yaml);
    let resources = manifest["resources"]
        .as_array()
        .expect("type pack resources must be an array")
        .iter()
        .map(|resource| {
            let source = resource["source"]
                .as_str()
                .expect("type pack resource source")
                .to_string();
            let document = fs::read_to_string(
                manifest_path
                    .parent()
                    .expect("type pack manifest parent")
                    .join(&source),
            )
            .expect("read shared type pack resource");
            mdbase::v03::TypePackResource { source, document }
        })
        .collect::<Vec<_>>();
    if input.get("corrupt_digest").and_then(Value::as_bool) == Some(true) {
        manifest["resources"][0]["digest"] = Value::String(format!("sha256:{}", "0".repeat(64)));
    }

    let repeat = input.get("repeat").and_then(Value::as_u64).unwrap_or(1);
    let mut runs = Vec::new();
    for _ in 0..repeat {
        let result = collection.install_type_pack(&manifest, &resources, false);
        let actions = result
            .result
            .get("resources")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|resource| resource.get("action").cloned())
            .collect::<Vec<_>>();
        let error = result.diagnostics.first().map(|diagnostic| {
            serde_json::json!({
                "code": diagnostic.code,
                "message": diagnostic.message,
            })
        });
        runs.push(serde_json::json!({
            "valid": result.valid,
            "actions": actions,
            "error": error,
        }));
        if !result.valid {
            break;
        }
    }

    let last = runs.last().expect("type pack executes at least once");
    let reopened = Collection::open(collection.root()).expect("reopen type pack collection");
    let implementations = reopened
        .get_data_contract_implementations("tasknotes.task", "0.2.0")
        .len();
    let targets_exist = manifest["resources"]
        .as_array()
        .expect("type pack resources")
        .iter()
        .map(|resource| {
            Value::Bool(
                resource["target"]
                    .as_str()
                    .map(|target| collection.root().join(target).exists())
                    .unwrap_or(false),
            )
        })
        .collect::<Vec<_>>();
    let mut output = serde_json::json!({
        "valid": last["valid"],
        "runs": runs,
        "implementations": implementations,
        "targets_exist": targets_exist,
    });
    if !last["error"].is_null() {
        output["error"] = last["error"].clone();
    }
    output
}

fn execute_standalone_data_contract_case(operation: &str, input: &Value) -> Value {
    let directory = tempfile::tempdir().expect("create standalone contract fixture");
    write(directory.path(), "mdbase.yaml", "spec_version: \"0.3.0\"\n");

    let copy_fixture = |key: &str, destination: &str| {
        let relative = input
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{operation} requires input.{key}"));
        let content =
            fs::read_to_string(spec_root().join(relative)).expect("read data contract fixture");
        write(directory.path(), destination, &content);
    };

    match operation {
        "data_contract_registry_validate" => {
            for (index, relative) in input["paths"]
                .as_array()
                .expect("registry paths must be an array")
                .iter()
                .enumerate()
            {
                let relative = relative.as_str().expect("registry path must be a string");
                let content = fs::read_to_string(spec_root().join(relative))
                    .expect("read data contract registry fixture");
                write(
                    directory.path(),
                    &format!("_contracts/{index}.md"),
                    &content,
                );
            }
        }
        _ => {
            copy_fixture("contract", "_contracts/contract.md");
            if input.get("type").is_some() {
                copy_fixture("type", "_types/type.md");
            }
        }
    }

    let collection = match Collection::open(directory.path()) {
        Ok(collection) => collection,
        Err(error) => {
            return serde_json::json!({
                "valid": false,
                "error": error.to_string(),
            });
        }
    };

    match operation {
        "data_contract_implementation_validate" => {
            let Some(contract) = collection.list_data_contracts().into_iter().next() else {
                return serde_json::json!({
                    "valid": false,
                    "error": "expected one data contract",
                });
            };
            let implementations =
                collection.get_data_contract_implementations(&contract.id, &contract.version);
            if implementations.len() != 1 {
                return serde_json::json!({
                    "valid": false,
                    "error": format!(
                        "expected exactly one implementation, found {}",
                        implementations.len()
                    ),
                });
            }
            let Some(record_path) = input.get("record").and_then(Value::as_str) else {
                return serde_json::json!({"valid": true});
            };
            let record: serde_yaml::Value = serde_yaml::from_str(
                &fs::read_to_string(spec_root().join(record_path))
                    .expect("read contract record fixture"),
            )
            .expect("parse contract record fixture");
            let projected = collection.project_contract_type(
                &implementations[0].type_name,
                &contract.id,
                &contract.version,
                &yaml_to_json(&record),
            );
            serde_json::json!({
                "valid": projected.valid,
                "error": projected.diagnostics.first().map(|diagnostic| diagnostic.message.clone()),
            })
        }
        "data_contract_digest" => serde_json::json!({
            "digest": collection.list_data_contracts()[0].digest
        }),
        "data_contract_implementation_digest" => serde_json::json!({
            "digest": collection
                .get_data_contract_implementations("tasknotes.task", "0.2.0")[0]
                .implementation_digest
        }),
        "data_contract_registry_validate" => serde_json::json!({"valid": true}),
        _ => unreachable!("standalone operation was already matched"),
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

fn expose_query_aliases(result: &mut Value) {
    let paths = result
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|record| record.get("path").cloned())
        .collect::<Vec<_>>();
    result["paths"] = Value::Array(paths);
    if let Some(context) = result.pointer("/meta/context").cloned() {
        result["context"] = context;
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
            "error_contains" => {
                let expected = expected_value
                    .as_str()
                    .expect("error_contains must be a string");
                let error = actual
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("{case_name}: missing string error: {actual:#}"));
                assert!(
                    error.to_lowercase().contains(&expected.to_lowercase()),
                    "{case_name}: error does not contain {expected:?}: {error:?}"
                );
            }
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
    run_suite("core/core-collection.yaml", "core_collection", 24);
}

#[test]
fn shared_v03_lifecycle_fixture_passes() {
    run_suite("lifecycle/lifecycle.yaml", "lifecycle", 7);
}

#[test]
fn shared_v03_cel_fixture_passes() {
    run_suite("cel/cel-profile.yaml", "cel", 15);
}

#[test]
fn shared_v03_saved_views_fixture_passes() {
    run_suite("views/view-records.yaml", "views", 17);
}

#[test]
fn shared_v03_data_contract_fixture_passes() {
    run_suite("data-contracts/data-contracts.yaml", "data_contracts", 13);
}

#[test]
fn shared_v03_type_pack_fixture_passes() {
    run_suite("type-packs/type-packs.yaml", "type_packs", 3);
}

fn run_suite(relative_path: &str, fixture_set: &str, expected_cases: usize) {
    let path = fixture_path(relative_path);
    let fixture = fs::read_to_string(path).expect("read shared v0.3 fixture");
    let suite: Suite = serde_yaml::from_str(&fixture).expect("parse shared v0.3 fixture");
    assert_eq!(suite.fixture_set, fixture_set);

    let mut executed = 0;
    for group in suite.groups {
        for case in group.tests {
            let directory = materialize(&group.setup);
            let collection = Collection::open(directory.path())
                .unwrap_or_else(|error| panic!("{}: open collection: {error:#}", group.name));
            let expected = yaml_to_json(&case.expect);
            let actual = execute(&collection, &group.setup, &case, &expected);
            assert_expectation(&actual, &expected, &case.name);
            executed += 1;
        }
    }
    assert_eq!(
        executed, expected_cases,
        "pinned v0.3 fixture case count changed for {fixture_set}"
    );
}
