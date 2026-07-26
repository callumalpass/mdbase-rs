use std::fs;
use std::path::{Path, PathBuf};

use mdbase::runtime_contracts::{
    ContractDocument, ContractKind, ContractSource, LoadOptions, RuntimeContracts, RuntimeRegistry,
};
use mdbase::Collection;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
struct Suite {
    groups: Vec<Group>,
}

#[derive(Deserialize)]
struct Group {
    #[serde(default)]
    setup: Setup,
    tests: Vec<Case>,
}

#[derive(Default, Deserialize)]
struct Setup {
    #[serde(default)]
    implicit_contracts: Vec<serde_yaml::Value>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    operation: String,
    input: serde_yaml::Value,
    expect: serde_yaml::Value,
}

#[test]
fn shared_runtime_contract_fixture_passes_every_pure_operation() {
    let fixture = spec_root().join("tests/v0.3/runtime-contracts/runtime-contracts.yaml");
    let suite: Suite = serde_yaml::from_str(&fs::read_to_string(fixture).unwrap()).unwrap();
    let runtime = RuntimeContracts::new().unwrap();
    let mut executed = 0;
    let mut execution_cases = 0;

    for group in suite.groups {
        for case in group.tests {
            if case.operation == "runtime_execute_event" {
                // Workflow dispatch is intentionally outside the pure Runtime
                // Contracts profile and belongs to workflow/0.1.
                execution_cases += 1;
                continue;
            }
            let input = serde_json::to_value(&case.input).unwrap();
            let expected = serde_json::to_value(&case.expect).unwrap();
            let (temporary, collection) = collection_for(&input);
            let implicit = if input
                .get("include_setup_implicit_contracts")
                .and_then(Value::as_bool)
                == Some(true)
            {
                vec![ContractSource::built_in(
                    group
                        .setup
                        .implicit_contracts
                        .iter()
                        .map(|value| {
                            ContractDocument::virtual_contract(serde_json::to_value(value).unwrap())
                        })
                        .collect(),
                )]
            } else {
                vec![]
            };
            let actual = execute(&runtime, &collection, implicit, &case.operation, &input);
            assert_expected(&case.name, &actual, &expected);
            drop(temporary);
            executed += 1;
        }
    }

    assert_eq!(
        execution_cases, 1,
        "fixture dispatch case should remain explicit"
    );
    assert_eq!(executed, 19, "pinned pure runtime fixture count changed");
}

fn execute(
    runtime: &RuntimeContracts,
    collection: &Collection,
    implicit: Vec<ContractSource>,
    operation: &str,
    input: &Value,
) -> Value {
    let loaded = runtime.load(collection, implicit, &LoadOptions::default());
    match operation {
        "runtime_load_contracts" => package_result(&loaded.contracts),
        "runtime_compose_registry" => registry_result(&loaded.registry),
        "runtime_preflight_workflows" => json!({
            "valid": loaded.preflight.valid,
            "diagnostics": loaded.preflight.diagnostics,
        }),
        "runtime_reference_load" => json!({
            "valid": loaded.preflight.valid,
            "diagnostics": loaded.preflight.diagnostics,
            "counts": registry_counts(&loaded.registry),
        }),
        "runtime_validate_event" => {
            let value = input.get("value").cloned().unwrap_or_else(|| {
                let relative = input.get("event").and_then(Value::as_str).unwrap();
                serde_json::from_str(&fs::read_to_string(spec_root().join(relative)).unwrap())
                    .unwrap()
            });
            validation_json(runtime.validate_event(&loaded.registry, &value))
        }
        "runtime_validate_action_input" => validation_json(runtime.validate_action_input(
            &loaded.registry,
            input.get("action").and_then(Value::as_str).unwrap(),
            input.get("value").unwrap(),
        )),
        "runtime_validate_action_output" => validation_json(runtime.validate_action_output(
            &loaded.registry,
            input.get("action").and_then(Value::as_str).unwrap(),
            input.get("value").unwrap(),
        )),
        "runtime_materialize_contract" => {
            let id = input.get("contract").and_then(Value::as_str).unwrap();
            let contract = [
                ContractKind::Provider,
                ContractKind::Action,
                ContractKind::Event,
                ContractKind::Capability,
                ContractKind::Workflow,
                ContractKind::RuntimePolicy,
            ]
            .into_iter()
            .find_map(|kind| loaded.registry.contract(kind, id))
            .expect("fixture contract must resolve");
            match runtime.materialize_contract(&contract.contract, None) {
                Ok(markdown) => json!({"valid": true, "markdown": markdown}),
                Err(diagnostics) => json!({"valid": false, "diagnostics": diagnostics}),
            }
        }
        _ => panic!("unsupported pure runtime fixture operation: {operation}"),
    }
}

fn collection_for(input: &Value) -> (Option<tempfile::TempDir>, Collection) {
    if let Some(relative) = input.get("collection").and_then(Value::as_str) {
        return (
            None,
            Collection::open(&spec_root().join(relative)).expect("open fixture collection"),
        );
    }
    let temporary = tempfile::tempdir().unwrap();
    let inline = input
        .get("collection_inline")
        .and_then(Value::as_object)
        .expect("fixture input requires collection or collection_inline");
    if !inline.contains_key("mdbase.yaml") {
        fs::write(
            temporary.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
    }
    for (path, contents) in inline {
        let target = temporary.path().join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, contents.as_str().unwrap()).unwrap();
    }
    let collection = Collection::open(temporary.path()).expect("open inline fixture collection");
    (Some(temporary), collection)
}

fn package_result(package: &mdbase::runtime_contracts::RuntimePackage) -> Value {
    json!({
        "valid": package.valid(),
        "diagnostics": package.diagnostics,
        "counts": {
            "type_files": package.type_files.len(),
            "providers": package.providers.len(),
            "actions": package.actions.len(),
            "events": package.events.len(),
            "capabilities": package.capabilities.len(),
            "workflows": package.workflows.len(),
        }
    })
}

fn registry_result(registry: &RuntimeRegistry) -> Value {
    json!({
        "valid": registry.valid(),
        "diagnostics": registry.diagnostics,
        "registry": {
            "providers": registry.providers.keys().collect::<Vec<_>>(),
            "actions": registry.actions.keys().collect::<Vec<_>>(),
            "events": registry.events.keys().collect::<Vec<_>>(),
            "capabilities": registry.capability_ids.iter().collect::<Vec<_>>(),
            "workflows": registry.workflows.keys().collect::<Vec<_>>(),
        }
    })
}

fn registry_counts(registry: &RuntimeRegistry) -> Value {
    json!({
        "providers": registry.providers.len(),
        "actions": registry.actions.len(),
        "events": registry.events.len(),
        "capabilities": registry.capability_ids.len(),
        "workflows": registry.workflows.len(),
    })
}

fn validation_json(result: mdbase::runtime_contracts::ValidationResult) -> Value {
    json!({"valid": result.valid, "diagnostics": result.diagnostics})
}

fn assert_expected(name: &str, actual: &Value, expected: &Value) {
    if let Some(valid) = expected.get("valid") {
        assert_eq!(&actual["valid"], valid, "{name}: {actual:#}");
    }
    if let Some(counts) = expected.get("counts").and_then(Value::as_object) {
        for (key, value) in counts {
            assert_eq!(&actual["counts"][key], value, "{name}: count {key}");
        }
    }
    if let Some(expected_registry) = expected.get("registry").and_then(Value::as_object) {
        for (kind, ids) in expected_registry {
            let actual_ids = actual["registry"][kind].as_array().unwrap();
            for id in ids.as_array().unwrap() {
                assert!(actual_ids.contains(id), "{name}: missing {kind} {id}");
            }
        }
    }
    if let Some(expected_diagnostics) = expected.get("diagnostics").and_then(Value::as_array) {
        let actual_diagnostics = actual
            .get("diagnostics")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if expected_diagnostics.is_empty() {
            assert!(
                actual_diagnostics.is_empty(),
                "{name}: unexpected diagnostics {actual_diagnostics:#?}"
            );
        }
        for diagnostic in expected_diagnostics {
            let fields = diagnostic.as_object().unwrap();
            assert!(
                actual_diagnostics.iter().any(|actual| fields
                    .iter()
                    .all(|(key, expected)| actual.get(key) == Some(expected))),
                "{name}: missing diagnostic {diagnostic}; actual {actual_diagnostics:#?}"
            );
        }
    }
    if let Some(parts) = expected.get("markdown_contains").and_then(Value::as_array) {
        let markdown = actual["markdown"].as_str().unwrap();
        for part in parts {
            assert!(markdown.contains(part.as_str().unwrap()), "{name}: {part}");
        }
    }
}

fn spec_root() -> PathBuf {
    std::env::var_os("MDBASE_SPEC_REPO_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../mdbase-spec"))
}
