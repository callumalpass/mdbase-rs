#[path = "support/spec.rs"]
mod spec;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use mdbase::v03::{OperationResult, TypePackAssessmentOptions, TypePackProvision};
use mdbase::Collection;
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
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

#[derive(Debug, Clone, Deserialize)]
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
    /// Test-level setup; a supplied config replaces the group config.
    #[serde(default)]
    setup: Option<CaseSetup>,
    /// A follow-up operation whose result must match after the primary one.
    #[serde(default)]
    verify_after: Option<Verification>,
}

#[derive(Debug, Deserialize)]
struct CaseSetup {
    config: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Verification {
    operation: String,
    #[serde(default)]
    input: serde_yaml::Value,
    #[serde(default)]
    expect: serde_yaml::Value,
}

fn fixture_path(relative_path: &str) -> PathBuf {
    spec_root().join("tests/v0.3").join(relative_path)
}

use spec::spec_root;

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
        "apply_type_pack" => execute_type_pack_case(collection, &input, expected),
        "assess_type_pack" => execute_type_pack_assessment(collection, &input, expected),
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
        "batch" => {
            let mut result = flatten_envelope(operations.batch(&input));
            expose_operation_issues(&mut result);
            result
        }
        "resolve_link" => {
            let resolution = collection.resolve_link(&input);
            serde_json::json!({
                "valid": resolution.get("error").is_none(),
                "resolved": resolution.get("resolved_path").cloned().unwrap_or(Value::Null),
                "error": resolution.get("error").cloned().unwrap_or(Value::Null),
            })
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

fn load_type_pack(relative_path: &str) -> TypePackProvision {
    let manifest_path = spec_root().join(relative_path);
    let manifest_yaml: serde_yaml::Value = serde_yaml::from_str(
        &fs::read_to_string(&manifest_path).expect("read shared type pack manifest"),
    )
    .expect("parse shared type pack manifest");
    let mut manifest = yaml_to_json(&manifest_yaml);
    for resource in manifest["resources"]
        .as_array_mut()
        .expect("type pack resources must be an array")
    {
        if resource.get("mode").is_none() {
            resource["mode"] = Value::String("managed".to_string());
        }
    }
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
        .collect();
    TypePackProvision {
        manifest,
        resources,
    }
}

fn type_pack_options() -> TypePackAssessmentOptions {
    TypePackAssessmentOptions {
        installed_by: "dev.mdbase.conformance".to_string(),
        adopt_resources: BTreeMap::new(),
        preserve_seed_targets: Default::default(),
        target_overrides: BTreeMap::new(),
        contract_setups: Vec::new(),
    }
}

fn apply_reviewed(
    collection: &Collection,
    provision: &TypePackProvision,
    options: TypePackAssessmentOptions,
    assessment: &OperationResult,
) -> OperationResult {
    collection.apply_type_pack(
        provision,
        &mdbase::v03::TypePackApplyOptions {
            installed_by: options.installed_by,
            expected_assessment_digest: assessment.result["assessment_digest"]
                .as_str()
                .expect("assessment digest")
                .to_string(),
            allow_downgrade: false,
            adopt_resources: options.adopt_resources,
            preserve_seed_targets: options.preserve_seed_targets,
            target_overrides: options.target_overrides,
            contract_setups: options.contract_setups,
        },
    )
}

/// Prepares the collection with `input.history` (tests/v0.3/README.md).
fn apply_type_pack_history(collection: &Collection, input: &Value) {
    let root = collection.root();
    for step in input["history"].as_array().into_iter().flatten() {
        if let Some(pack) = step.get("apply").and_then(Value::as_str) {
            let provision = load_type_pack(pack);
            let assessment = collection.assess_type_pack(&provision, &type_pack_options());
            assert!(
                assessment.valid && assessment.result["applicable"] == true,
                "history apply {pack} is not applicable: {:#}",
                serde_json::json!({"result": assessment.result, "diagnostics": assessment.diagnostics})
            );
            let applied = apply_reviewed(collection, &provision, type_pack_options(), &assessment);
            assert!(
                applied.valid,
                "history apply {pack} failed: {:?}",
                applied.diagnostics
            );
        } else if let Some(write) = step.get("write") {
            let path = write["path"].as_str().expect("history write path");
            let content = write["content"].as_str().expect("history write content");
            self::write(root, path, content);
        } else if let Some(replace) = step.get("replace") {
            let path = replace["path"].as_str().expect("history replace path");
            let old = replace["old"].as_str().expect("history replace old");
            let new = replace["new"].as_str().expect("history replace new");
            let current = fs::read_to_string(root.join(path)).expect("read history target");
            assert_eq!(
                current.matches(old).count(),
                1,
                "history replace in {path} requires exactly one {old:?}"
            );
            self::write(root, path, &current.replacen(old, new, 1));
        } else {
            panic!("unsupported type-pack history step: {step}");
        }
    }
}

/// The bytes of every target an expectation requires to stay unchanged.
fn type_pack_snapshot(root: &Path, expected: &Value) -> BTreeMap<String, Option<Vec<u8>>> {
    expected["target_unchanged"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|target| {
            let target = target.as_str().expect("target_unchanged entry").to_string();
            let bytes = fs::read(root.join(&target)).ok();
            (target, bytes)
        })
        .collect()
}

fn split_frontmatter(document: &str) -> (Value, &str) {
    let yaml = document
        .strip_prefix("---\n")
        .expect("type-pack target has frontmatter");
    let end = yaml
        .find("\n---\n")
        .expect("type-pack frontmatter is closed");
    let frontmatter: serde_yaml::Value =
        serde_yaml::from_str(&yaml[..=end]).expect("parse type-pack target frontmatter");
    (yaml_to_json(&frontmatter), &yaml[end + 5..])
}

/// Observes the seed-upgrade expectations on the collection, echoing each
/// expected value that holds so the ordinary subset comparison reports the rest.
fn observe_type_pack_targets(
    root: &Path,
    expected: &Value,
    before: &BTreeMap<String, Option<Vec<u8>>>,
    resources: &Value,
) -> Map<String, Value> {
    let entries = |key: &str| {
        expected
            .get(key)
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
    };
    let read = |target: &str| fs::read_to_string(root.join(target)).unwrap_or_default();
    let source_digest = |source: &str| {
        let bytes = fs::read(spec_root().join(source)).expect("read expected source");
        format!("sha256:{:x}", Sha256::digest(bytes))
    };
    let mut observed = Map::new();
    observed.insert(
        "resources".to_string(),
        resources
            .as_array()
            .into_iter()
            .flatten()
            .map(|resource| {
                let mut entry = serde_json::json!({
                    "target": resource["target"],
                    "action": resource["action"],
                    "reason": resource.get("reason").is_some(),
                });
                if let Some(version) = resource.pointer("/upgrade_baseline/version") {
                    entry["upgrade_baseline_version"] = version.clone();
                }
                entry
            })
            .collect(),
    );
    let matches_source = entries("target_matches_source")
        .map(|(target, source)| {
            let source = source.as_str().expect("target_matches_source value");
            let expected_bytes = fs::read(spec_root().join(source)).expect("read source");
            let holds = fs::read(root.join(target)).ok() == Some(expected_bytes);
            let value = if holds { source.into() } else { read(target) };
            (target.clone(), Value::String(value))
        })
        .collect();
    observed.insert(
        "target_matches_source".into(),
        Value::Object(matches_source),
    );
    let unchanged = before
        .iter()
        .filter(|(target, bytes)| fs::read(root.join(target)).ok() == **bytes)
        .map(|(target, _)| Value::String(target.clone()))
        .collect();
    observed.insert("target_unchanged".into(), Value::Array(unchanged));
    let frontmatter = entries("target_frontmatter")
        .map(|(target, pointers)| {
            let (frontmatter, _) = split_frontmatter(&read(target));
            let values = pointers
                .as_object()
                .expect("target_frontmatter pointers")
                .iter()
                .map(|(pointer, value)| {
                    let actual = frontmatter.pointer(pointer).cloned();
                    let holds = actual.as_ref() == Some(value);
                    let value = if holds {
                        value.clone()
                    } else {
                        serde_json::json!({ "observed": actual })
                    };
                    (pointer.clone(), value)
                })
                .collect();
            (target.clone(), Value::Object(values))
        })
        .collect();
    observed.insert("target_frontmatter".into(), Value::Object(frontmatter));
    let body = entries("target_body_contains")
        .map(|(target, texts)| {
            let document = read(target);
            let (_, body) = split_frontmatter(&document);
            let found = texts
                .as_array()
                .expect("target_body_contains texts")
                .iter()
                .filter(|text| body.contains(text.as_str().expect("body text")))
                .cloned()
                .collect();
            (target.clone(), Value::Array(found))
        })
        .collect();
    observed.insert("target_body_contains".into(), Value::Object(body));
    let lock: Value = serde_yaml::from_str::<serde_yaml::Value>(&read("mdbase.lock.yaml"))
        .map(|lock| yaml_to_json(&lock))
        .unwrap_or(Value::Null);
    let origins = entries("lock_origin")
        .map(|(target, source)| {
            let origin = lock["packs"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|pack| pack["resources"].as_array().into_iter().flatten())
                .find(|resource| resource["target"] == *target)
                .and_then(|resource| resource.get("origin_digest"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let source = source.as_str().expect("lock_origin value");
            let value = match origin {
                None => "absent".to_string(),
                Some(origin) if source != "absent" && origin == source_digest(source) => {
                    source.to_string()
                }
                Some(origin) => origin,
            };
            (target.clone(), Value::String(value))
        })
        .collect();
    observed.insert("lock_origin".into(), Value::Object(origins));
    observed
}

fn execute_type_pack_case(collection: &Collection, input: &Value, expected: &Value) -> Value {
    let pack = input
        .get("pack")
        .and_then(Value::as_str)
        .expect("apply_type_pack requires input.pack");
    apply_type_pack_history(collection, input);
    let before = type_pack_snapshot(collection.root(), expected);
    let TypePackProvision {
        mut manifest,
        resources,
    } = load_type_pack(pack);
    if input.get("corrupt_digest").and_then(Value::as_bool) == Some(true) {
        manifest["resources"][0]["digest"] = Value::String(format!("sha256:{}", "0".repeat(64)));
    }

    let repeat = input.get("repeat").and_then(Value::as_u64).unwrap_or(1);
    let mut runs = Vec::new();
    let mut first_resources = None;
    for _ in 0..repeat {
        let provision = TypePackProvision {
            manifest: manifest.clone(),
            resources: resources.clone(),
        };
        let mut assessment_options = type_pack_options();
        let mut assessment = collection.assess_type_pack(&provision, &assessment_options);
        if input.get("adopt_conflicts").and_then(Value::as_bool) == Some(true) && assessment.valid {
            assessment_options.adopt_resources = assessment.result["resources"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|resource| {
                    resource["action"] == "conflict"
                        && resource["mode"] == "managed"
                        && resource["current_digest"].is_string()
                })
                .map(|resource| {
                    (
                        resource["target"].as_str().unwrap().to_string(),
                        resource["current_digest"].as_str().unwrap().to_string(),
                    )
                })
                .collect();
            assessment = collection.assess_type_pack(&provision, &assessment_options);
        }
        first_resources.get_or_insert_with(|| assessment.result["resources"].clone());
        if let Some(target) = input.get("mutate_after_assess").and_then(Value::as_str) {
            let target = collection.root().join(target);
            fs::create_dir_all(target.parent().expect("mutation target parent"))
                .expect("create mutation target parent");
            fs::write(target, "Changed after assessment.\n").expect("mutate assessed target");
        }
        let result = if assessment.valid {
            apply_reviewed(collection, &provision, assessment_options, &assessment)
        } else {
            assessment.clone()
        };
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
            "status": assessment.result["status"],
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
        "lock_exists": collection.root().join("mdbase.lock.yaml").exists(),
        "targets_exist": targets_exist,
    });
    if !last["error"].is_null() {
        output["error"] = last["error"].clone();
    }
    let observed = observe_type_pack_targets(
        collection.root(),
        expected,
        &before,
        &first_resources.unwrap_or_default(),
    );
    output.as_object_mut().unwrap().extend(observed);
    output
}

fn execute_type_pack_assessment(collection: &Collection, input: &Value, expected: &Value) -> Value {
    apply_type_pack_history(collection, input);
    if let Some(target) = input.get("install_then_modify").and_then(Value::as_str) {
        let install = serde_json::json!({ "pack": input["pack"] });
        let installed = execute_type_pack_case(collection, &install, &Value::Null);
        if !installed["valid"].as_bool().unwrap_or(false) {
            return installed;
        }
        fs::write(collection.root().join(target), "User-authored change.\n")
            .expect("modify installed target");
    }
    let before = type_pack_snapshot(collection.root(), expected);
    let pack = input["pack"].as_str().expect("assessment pack");
    let assessed = collection.assess_type_pack(&load_type_pack(pack), &type_pack_options());
    let mut output = serde_json::json!({
        "valid": assessed.valid,
        "status": assessed.result["status"],
        "applicable": assessed.result["applicable"],
        "actions": assessed.result["resources"].as_array().into_iter().flatten()
            .map(|resource| resource["action"].clone()).collect::<Vec<_>>(),
    });
    if let Some(diagnostic) = assessed.diagnostics.first() {
        output["error"] = serde_json::json!({
            "code": diagnostic.code,
            "message": diagnostic.message,
        });
    }
    let observed = observe_type_pack_targets(
        collection.root(),
        expected,
        &before,
        &assessed.result["resources"],
    );
    output.as_object_mut().unwrap().extend(observed);
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
            "diagnostics_contain" => assert_array_contains(
                actual.get("diagnostics").and_then(Value::as_array),
                expected_value.as_array(),
                case_name,
                "diagnostics",
            ),
            "body_contains" => {
                let expected = expected_value
                    .as_str()
                    .expect("body_contains must be a string");
                let body = actual
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("{case_name}: missing body: {actual:#}"));
                assert!(
                    body.contains(expected),
                    "{case_name}: body does not contain {expected:?}: {body:?}"
                );
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
fn shared_v03_optional_membership_fixture_passes() {
    run_suite("core/optional-membership.yaml", "core_collection", 9);
}

#[test]
fn shared_v03_core_write_fixture_passes() {
    run_suite("core/core-write.yaml", "core_collection", 17);
}

#[test]
fn shared_v03_yaml_document_records_fixture_passes() {
    run_suite("core/yaml-document-records.yaml", "core_collection", 6);
}

#[test]
fn shared_v03_links_and_discovery_fixture_passes() {
    run_suite("core/links-and-discovery.yaml", "core_collection", 8);
}

#[test]
fn shared_v03_lifecycle_fixture_passes() {
    run_suite("lifecycle/lifecycle.yaml", "lifecycle", 8);
}

#[test]
fn shared_v03_cel_fixture_passes() {
    run_suite("cel/cel-profile.yaml", "cel", 27);
}

#[test]
fn shared_v03_saved_views_fixture_passes() {
    run_suite("views/view-records.yaml", "views", 20);
}

#[test]
fn shared_v03_data_contract_fixture_passes() {
    run_suite("data-contracts/data-contracts.yaml", "data_contracts", 18);
}

#[test]
fn shared_v03_type_pack_fixture_passes() {
    run_suite("type-packs/type-packs.yaml", "type_packs", 21);
}

fn run_suite(relative_path: &str, fixture_set: &str, expected_cases: usize) {
    let path = fixture_path(relative_path);
    let fixture = fs::read_to_string(path).expect("read shared v0.3 fixture");
    let suite: Suite = serde_yaml::from_str(&fixture).expect("parse shared v0.3 fixture");
    assert_eq!(suite.fixture_set, fixture_set);

    let mut executed = 0;
    let mut failures = Vec::new();
    for group in &suite.groups {
        for case in &group.tests {
            executed += 1;
            let case_name = case.name.clone();
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_case(group, case)));
            if let Err(panic) = outcome {
                let message = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|text| text.to_string()))
                    .unwrap_or_default();
                failures.push(format!("{case_name}: {message}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {executed} {relative_path} cases failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
    assert_eq!(
        executed, expected_cases,
        "pinned v0.3 fixture case count changed for {fixture_set}"
    );
}

fn run_case(group: &Group, case: &Case) {
    let mut setup = group.setup.clone();
    if let Some(config) = case.setup.as_ref().and_then(|setup| setup.config.clone()) {
        setup.config = config;
    }
    let directory = materialize(&setup);
    let collection = Collection::open(directory.path())
        .unwrap_or_else(|error| panic!("{}: open collection: {error:#}", group.name));
    let expected = yaml_to_json(&case.expect);
    let actual = execute(&collection, &setup, case, &expected);
    assert_expectation(&actual, &expected, &case.name);
    if let Some(verification) = &case.verify_after {
        let collection = Collection::open(directory.path())
            .unwrap_or_else(|error| panic!("{}: reopen collection: {error:#}", case.name));
        let follow_up = Case {
            name: format!("{} (verify_after)", case.name),
            operation: verification.operation.clone(),
            input: verification.input.clone(),
            expect: verification.expect.clone(),
            setup: None,
            verify_after: None,
        };
        let expected = yaml_to_json(&follow_up.expect);
        let actual = execute(&collection, &setup, &follow_up, &expected);
        assert_expectation(&actual, &expected, &follow_up.name);
    }
}
