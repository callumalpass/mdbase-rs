use std::fs;
use std::path::{Path, PathBuf};

use mdbase::runtime_contracts::{
    ComposeOptions, ContractDocument, ContractKind, ContractSource, LoadOptions, PolicySelector,
    RuntimeContracts,
};
use mdbase::Collection;
use serde_json::{json, Value};

fn spec_root() -> PathBuf {
    std::env::var_os("MDBASE_SPEC_REPO_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../mdbase-spec"))
}

fn canvas_collection() -> Collection {
    Collection::open(&spec_root().join("examples/v0.3/canvas-runtime")).unwrap()
}

#[test]
fn loads_composes_and_preflights_the_canonical_canvas_runtime_end_to_end() {
    let runtime = RuntimeContracts::new().unwrap();
    let collection = canvas_collection();
    let loaded = runtime.load(&collection, vec![], &LoadOptions::default());

    assert!(
        loaded.contracts.valid(),
        "{:#?}",
        loaded.contracts.diagnostics
    );
    assert_eq!(loaded.contracts.type_files.len(), 12);
    assert_eq!(loaded.contracts.providers.len(), 2);
    assert_eq!(loaded.contracts.actions.len(), 1);
    assert_eq!(loaded.contracts.events.len(), 2);
    assert_eq!(loaded.contracts.capabilities.len(), 1);
    assert_eq!(loaded.contracts.workflows.len(), 1);
    assert_eq!(loaded.contracts.policies.len(), 1);

    assert!(loaded.registry.providers.contains_key("mdbase"));
    assert!(loaded.registry.providers.contains_key("canvas-bases"));
    assert!(loaded.registry.events.contains_key("canvas.drop"));
    assert!(loaded
        .registry
        .events
        .contains_key("mdbase.record.modified"));
    assert!(loaded.registry.actions.contains_key("mdbase.record.patch"));
    assert!(loaded
        .registry
        .capability_ids
        .contains("mdbase.record.write"));
    assert_eq!(
        loaded.registry.selected_policy_ids,
        ["local.canvas-runtime.policy"]
    );
    assert!(
        loaded.preflight.valid,
        "{:#?}",
        loaded.preflight.diagnostics
    );

    let workflow = &loaded.preflight.workflows["canvas.zone.set-status"];
    assert_eq!(workflow.executor.as_deref(), Some("obsidian"));
    assert!(workflow.events.contains("canvas.drop"));
    assert!(workflow.events.contains("mdbase.record.modified"));
    assert!(workflow.actions.contains("mdbase.record.patch"));
    assert!(workflow.capabilities.contains("mdbase.record.write"));
}

#[test]
fn validates_event_action_and_materialization_contracts() {
    let runtime = RuntimeContracts::new().unwrap();
    let collection = canvas_collection();
    let loaded = runtime.load(&collection, vec![], &LoadOptions::default());
    let event: Value = serde_json::from_str(
        &fs::read_to_string(
            spec_root()
                .join("examples/v0.3/canvas-runtime/runtime-events/sample-canvas-drop-event.json"),
        )
        .unwrap(),
    )
    .unwrap();

    assert!(runtime.validate_event(&loaded.registry, &event).valid);
    let mut wrong_provider = event.clone();
    wrong_provider["source"]["provider"] = Value::String("wrong-provider".to_string());
    assert_code(
        &runtime
            .validate_event(&loaded.registry, &wrong_provider)
            .diagnostics,
        "event_provider_mismatch",
    );
    let mut wrong_version = event;
    wrong_version["contract_version"] = json!(999);
    assert_code(
        &runtime
            .validate_event(&loaded.registry, &wrong_version)
            .diagnostics,
        "contract_version_mismatch",
    );

    assert!(
        runtime
            .validate_action_input(
                &loaded.registry,
                "mdbase.record.patch",
                &json!({"path": "tasks/card-001.md", "patch": {"status": "doing"}}),
            )
            .valid
    );
    assert_code(
        &runtime
            .validate_action_input(
                &loaded.registry,
                "mdbase.record.patch",
                &json!({"patch": {"status": "doing"}}),
            )
            .diagnostics,
        "schema_required",
    );
    assert!(
        runtime
            .validate_action_output(
                &loaded.registry,
                "mdbase.record.patch",
                &json!({
                    "path": "tasks/card-001.md",
                    "frontmatter": {"type": "task", "status": "doing"}
                }),
            )
            .valid
    );

    let action = &loaded.registry.actions["mdbase.record.patch"].contract;
    let markdown = runtime.materialize_contract(action, None).unwrap();
    assert!(markdown.starts_with("---\n"));
    assert!(markdown.contains("type: action"));
    assert!(markdown.contains("id: mdbase.record.patch"));
    assert!(markdown.contains("# Patch record frontmatter"));
    let parsed = mdbase::frontmatter::parser::parse_document(&markdown);
    assert!(parsed.has_frontmatter);
}

#[test]
fn composes_virtual_sources_deterministically_and_keeps_origins() {
    let runtime = RuntimeContracts::new().unwrap();
    let event = event("timer.fired", "timer");
    let built_in =
        ContractSource::built_in(vec![ContractDocument::virtual_contract(event.clone())]);
    let identical_pack = ContractSource::pack(
        "z-pack",
        vec![ContractDocument::new("events/timer.md", event.clone())],
    );
    let conflict = ContractSource::pack(
        "a-pack",
        vec![ContractDocument::virtual_contract(json!({
            "type": "event",
            "id": "timer.fired",
            "version": 2,
            "provider": "timer",
            "name": "Conflicting timer",
            "schemas": {"dialect": "json-schema-2020-12", "payload": {"type": "object"}}
        }))],
    );

    let first = runtime.compose(
        vec![identical_pack.clone(), conflict.clone(), built_in.clone()],
        &ComposeOptions::default(),
    );
    let second = runtime.compose(
        vec![built_in, conflict, identical_pack],
        &ComposeOptions::default(),
    );
    assert_eq!(
        first.events["timer.fired"].contract,
        second.events["timer.fired"].contract
    );
    assert_eq!(first.events["timer.fired"].contract["version"], 1);
    assert_eq!(first.events["timer.fired"].origins.len(), 2);
    assert_code(&first.diagnostics, "contract_conflict");
    assert_eq!(first.diagnostics, second.diagnostics);
}

#[test]
fn rejects_a_provider_source_atomically_when_its_listing_is_false() {
    let runtime = RuntimeContracts::new().unwrap();
    let descriptor = json!({
        "type": "provider",
        "id": "timer",
        "version": 1,
        "provider_version": "1.2.3",
        "name": "Timer",
        "contracts": {"events": ["timer.fired", "timer.missing"]}
    });
    let registry = runtime.compose(
        vec![ContractSource::provider(
            "timer",
            vec![
                ContractDocument::virtual_contract(descriptor),
                ContractDocument::virtual_contract(event("timer.fired", "timer")),
            ],
        )],
        &ComposeOptions::default(),
    );

    assert_code(&registry.diagnostics, "provider_contract_mismatch");
    assert!(registry.providers.is_empty());
    assert!(registry.events.is_empty());
}

#[test]
fn explicit_provider_listings_validate_ownership_but_capability_catalogs_stay_optional() {
    let runtime = RuntimeContracts::new().unwrap();
    let documents = vec![
        json!({
            "type": "provider",
            "id": "owner",
            "version": 1,
            "provider_version": "1.0.0",
            "name": "Owner",
            "contracts": {
                "events": ["wrong.owner"],
                "actions": [],
                "capabilities": ["virtual.capability"]
            }
        }),
        event("wrong.owner", "someone-else"),
        action("unadvertised.action", "owner", &[]),
    ]
    .into_iter()
    .map(ContractDocument::virtual_contract)
    .collect();
    let registry = runtime.compose(
        vec![ContractSource::collection(documents)],
        &ComposeOptions::default(),
    );
    let report = runtime.preflight(&registry);

    assert!(registry.capabilities.is_empty());
    assert!(registry.capability_ids.contains("virtual.capability"));
    assert_eq!(
        report
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "provider_contract_mismatch")
            .count(),
        2
    );
    assert!(!report.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "unresolved_provider_capability"
            && diagnostic.id.as_deref() == Some("virtual.capability")
    }));
}

#[test]
fn contract_validation_is_strict_and_never_resolves_external_schemas() {
    let runtime = RuntimeContracts::new().unwrap();
    let allowed_extension = ContractDocument::virtual_contract(json!({
        "type": "event",
        "id": "extended.event",
        "version": 1,
        "provider": "source",
        "name": "Extended",
        "schemas": {"dialect": "json-schema-2020-12", "payload": {"type": "object"}},
        "x-host": {"safe": true}
    }));
    assert!(runtime.validate_contract(&allowed_extension).valid);

    let unknown_key = ContractDocument::virtual_contract(json!({
        "type": "event",
        "id": "bad.event",
        "version": 1,
        "provider": "source",
        "name": "Bad",
        "schemas": {"dialect": "json-schema-2020-12", "payload": {"type": "object"}},
        "typoed": true
    }));
    assert_code(
        &runtime.validate_contract(&unknown_key).diagnostics,
        "schema_additional_properties",
    );

    let remote_schema = ContractDocument::virtual_contract(json!({
        "type": "action",
        "id": "remote.action",
        "version": 1,
        "provider": "source",
        "name": "Remote",
        "schemas": {
            "dialect": "json-schema-2020-12",
            "input": {"$ref": "https://attacker.invalid/schema.json"},
            "output": null
        }
    }));
    assert_code(
        &runtime.validate_contract(&remote_schema).diagnostics,
        "invalid_embedded_schema",
    );
}

#[test]
fn workflow_preflight_reports_references_versions_duplicates_policy_and_executor() {
    let runtime = RuntimeContracts::new().unwrap();
    let documents = vec![
        provider("timer", "1.4.0", &["timer.fired"], &["task.patch"]),
        event("timer.fired", "timer"),
        action("task.patch", "timer", &["record.write"]),
        json!({
            "type": "runtime_policy",
            "id": "locked",
            "version": 1,
            "name": "Locked",
            "capabilities": {"record.write": {"mode": "deny"}}
        }),
        json!({
            "type": "workflow",
            "id": "broken.workflow",
            "version": 1,
            "name": "Broken",
            "enabled": true,
            "requires": {"providers": [{"id": "timer", "version": ">=2.0.0"}]},
            "triggers": [
                {"id": "tick", "event": "timer.fired"},
                {"id": "tick", "event": "missing.event"}
            ],
            "steps": [
                {"id": "patch", "action": "task.patch"},
                {"id": "patch", "action": "missing.action"}
            ],
            "run": {"execution": {"mode": "single_executor"}}
        }),
    ]
    .into_iter()
    .map(ContractDocument::virtual_contract)
    .collect();
    let registry = runtime.compose(
        vec![ContractSource::collection(documents)],
        &ComposeOptions {
            selected_policies: vec![PolicySelector::Id("locked".to_string())],
        },
    );
    let report = runtime.preflight(&registry);

    for code in [
        "provider_version_mismatch",
        "duplicate_trigger",
        "duplicate_step",
        "unresolved_event",
        "unresolved_action",
        "capability_denied",
        "executor_not_selected",
    ] {
        assert_code(&report.diagnostics, code);
    }
    assert!(!report.workflows["broken.workflow"].valid);
}

#[test]
fn dispatch_preflight_never_substitutes_for_host_authorization() {
    let runtime = RuntimeContracts::new().unwrap();
    let documents = vec![
        action("task.patch", "mdbase", &["record.write"]),
        json!({
            "type": "runtime_policy",
            "id": "local",
            "version": 1,
            "name": "Local",
            "capabilities": {"record.write": {"mode": "allow"}}
        }),
    ]
    .into_iter()
    .map(ContractDocument::virtual_contract)
    .collect();
    let registry = runtime.compose(
        vec![ContractSource::collection(documents)],
        &ComposeOptions {
            selected_policies: vec![PolicySelector::Id("local".to_string())],
        },
    );
    let context = json!({
        "actor": {"id": "local-user", "kind": "user"},
        "origin": {"workflow": "task.workflow"},
        "run_id": "run_01",
        "correlation_id": "corr_01",
        "executor": "desktop"
    });
    assert!(
        runtime
            .preflight_action(&registry, "task.patch", &context)
            .valid
    );

    let missing_provenance =
        runtime.preflight_action(&registry, "task.patch", &json!({"actor": {"id": "user"}}));
    assert_code(&missing_provenance.diagnostics, "invalid_dispatch_context");
}

#[test]
fn collection_adapter_honors_exclusions_and_nested_boundaries() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
    )
    .unwrap();
    write_contract(
        root.path(),
        "events/visible.md",
        &event("visible", "source"),
    );
    write_contract(root.path(), ".git/ignored.md", &event("ignored", "source"));
    fs::create_dir_all(root.path().join("nested")).unwrap();
    fs::write(
        root.path().join("nested/mdbase.yaml"),
        "spec_version: 0.3.0\n",
    )
    .unwrap();
    write_contract(root.path(), "nested/hidden.md", &event("hidden", "source"));

    let collection = Collection::open(root.path()).unwrap();
    let runtime = RuntimeContracts::new().unwrap();
    let loaded = runtime.load(&collection, vec![], &LoadOptions::default());
    assert_eq!(
        loaded.registry.events.keys().collect::<Vec<_>>(),
        ["visible"]
    );
}

fn provider(id: &str, version: &str, events: &[&str], actions: &[&str]) -> Value {
    json!({
        "type": "provider",
        "id": id,
        "version": 1,
        "provider_version": version,
        "name": id,
        "contracts": {"events": events, "actions": actions}
    })
}

fn event(id: &str, provider: &str) -> Value {
    json!({
        "type": "event",
        "id": id,
        "version": 1,
        "provider": provider,
        "name": id,
        "schemas": {
            "dialect": "json-schema-2020-12",
            "payload": {"type": "object"}
        }
    })
}

fn action(id: &str, provider: &str, effects: &[&str]) -> Value {
    json!({
        "type": "action",
        "id": id,
        "version": 1,
        "provider": provider,
        "name": id,
        "schemas": {
            "dialect": "json-schema-2020-12",
            "input": {"type": "object"},
            "output": null
        },
        "effects": effects
    })
}

fn write_contract(root: &Path, path: &str, value: &Value) {
    let target = root.join(path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(
        target,
        format!("---\n{}---\n", serde_yaml::to_string(value).unwrap()),
    )
    .unwrap();
}

fn assert_code(diagnostics: &[mdbase::runtime_contracts::RuntimeDiagnostic], code: &str) {
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic.code == code),
        "missing {code}: {diagnostics:#?}"
    );
}

#[test]
fn registry_indexes_are_kind_scoped() {
    let runtime = RuntimeContracts::new().unwrap();
    let registry = runtime.compose(
        vec![ContractSource::collection(vec![
            ContractDocument::virtual_contract(event("shared.id", "source")),
            ContractDocument::virtual_contract(action("shared.id", "source", &[])),
        ])],
        &ComposeOptions::default(),
    );
    assert!(registry
        .contract(ContractKind::Event, "shared.id")
        .is_some());
    assert!(registry
        .contract(ContractKind::Action, "shared.id")
        .is_some());
    assert!(!registry
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "contract_conflict"));
}
