use std::fs;
use std::time::Duration;

use serde_json::{json, Value};

use crate::api::{CollectionPath, RenameRequest, Revision};
use crate::runtime::{
    CanonicalChange, ChangeSet, CommitAttempt, FilesystemRuntime, HostClaimId, OperationContext,
    OperationKind, OperationRequest, PreparationOutcome, RecordChangeKind,
};
use crate::v03::batch::{prepare_single_runtime, RuntimeSinglePreparation};
use crate::v03::OperationResult;
use crate::Collection;

fn basic_collection() -> (tempfile::TempDir, Collection) {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
    fs::write(root.path().join("source.md"), "Source\n").unwrap();
    let collection = Collection::open(root.path()).unwrap();
    (root, collection)
}

fn complex_fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  id_field: id\n",
    )
    .unwrap();
    fs::create_dir_all(root.path().join("a")).unwrap();
    fs::create_dir_all(root.path().join("b")).unwrap();
    fs::write(
        root.path().join("a/same.md"),
        "---\nid: other\n---\nOther\n",
    )
    .unwrap();
    fs::write(
        root.path().join("b/same.md"),
        "\u{feff}---\r\nid: stable-id\r\ntitle: Same\r\n---\r\nSelf [[same#self|alias]].\r\n",
    )
    .unwrap();
    fs::write(
        root.path().join("b/ref.md"),
        "---\ntitle: [broken\n---\n![[same#body|embed]] and [[stable-id]].\n",
    )
    .unwrap();
    fs::write(root.path().join("a/ref.md"), "Same-dir loser [[same]].\n").unwrap();
    fs::write(
        root.path().join("root-ref.md"),
        "Lexical winner [[same]] but stable [[stable-id#id|stable]].\n",
    )
    .unwrap();
    root
}

fn serialization_fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
    fs::write(root.path().join("target.md"), "target\n").unwrap();
    fs::write(
        root.path().join("referrer.md"),
        "---\nholder:\n  ? !key\n    nested: key\n  : !value\n    nested: '[[target]]'\n---\nbody\n",
    )
    .unwrap();
    root
}

fn wire(root: &std::path::Path, input: &Value) -> OperationResult {
    let collection = Collection::open(root).unwrap();
    crate::v03::batch::execute_wire_mutation(&collection, "rename", input)
}

fn runtime(
    root: &std::path::Path,
    input: &Value,
) -> (OperationResult, Option<crate::runtime::ExecutionOutcome>) {
    let runtime = FilesystemRuntime::open(root, Duration::from_millis(5)).unwrap();
    let claim = HostClaimId::generate();
    let request = OperationRequest::new(OperationKind::Rename, input.clone());
    match runtime
        .prepare(&request, &claim, &OperationContext::legacy())
        .unwrap()
    {
        PreparationOutcome::NoMutation(outcome) => (outcome.result, None),
        PreparationOutcome::Prepared(prepared) => match runtime
            .commit(&prepared, &OperationContext::legacy())
            .unwrap()
        {
            CommitAttempt::Committed(outcome) => (outcome.result.clone(), Some(outcome)),
            other => panic!("expected committed runtime rename: {other:?}"),
        },
    }
}

fn normalize_result(mut value: Value) -> Value {
    if let Some(file) = value.get_mut("file").and_then(Value::as_object_mut) {
        file.insert("mtime".to_string(), json!("<mtime>"));
    }
    value
}

fn diagnostics(value: &(impl serde::Serialize + ?Sized)) -> Value {
    let serialized = serde_json::to_value(value).unwrap();
    Value::Array(
        serialized
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                json!({
                    "severity": item.get("severity").cloned().unwrap_or(Value::Null),
                    "code": item.get("code").cloned().unwrap_or(Value::Null),
                    "message": item.get("message").cloned().unwrap_or(Value::Null),
                    "path": item.get("path").cloned().unwrap_or(Value::Null),
                    "field": item.get("field").cloned().unwrap_or(Value::Null),
                    "type_name": item.get("type_name").cloned().unwrap_or(Value::Null),
                    "schema_location": item.get("schema_location").cloned().unwrap_or(Value::Null),
                    "details": item.get("details").cloned().unwrap_or(Value::Null),
                })
            })
            .collect(),
    )
}

fn typed_result(root: &std::path::Path) -> (Value, Value) {
    let collection = Collection::open(root).unwrap();
    let outcome = collection
        .typed()
        .unwrap()
        .rename(
            RenameRequest::new(
                CollectionPath::new("b/same.md").unwrap(),
                CollectionPath::new("b/renamed.md").unwrap(),
            )
            .with_document(),
        )
        .unwrap();
    (
        normalize_result(serde_json::to_value(outcome.value).unwrap()),
        diagnostics(&outcome.diagnostics),
    )
}

#[test]
fn wire_and_runtime_rename_use_one_full_shadow_without_nested_planning() {
    let (_wire_root, wire_collection) = basic_collection();
    crate::mutation::reset_mutation_path_probes();
    let result = crate::v03::batch::execute_wire_mutation(
        &wire_collection,
        "rename",
        &json!({
            "from": "source.md",
            "to": "destination.md",
            "update_refs": true
        }),
    );
    assert!(result.valid, "{:?}", result.diagnostics);
    assert_eq!(
        crate::mutation::mutation_path_probes(),
        crate::mutation::MutationPathProbes {
            wire_request_decodes: 1,
            full_shadows: 1,
            ..crate::mutation::MutationPathProbes::default()
        }
    );

    let (_runtime_root, runtime_collection) = basic_collection();
    crate::cache::runtime::rebuild(
        &runtime_collection,
        &crate::runtime::CollectionGeneration::initial(),
    )
    .unwrap();
    crate::mutation::reset_mutation_path_probes();
    let prepared = prepare_single_runtime(
        &runtime_collection,
        "rename",
        &json!({"from": "source.md", "to": "destination.md"}),
        &OperationContext::legacy(),
    )
    .unwrap();
    assert!(matches!(prepared, RuntimeSinglePreparation::Prepared(_)));
    assert_eq!(
        crate::mutation::mutation_path_probes(),
        crate::mutation::MutationPathProbes {
            wire_request_decodes: 1,
            runtime_request_decodes: 1,
            full_shadows: 1,
            ..crate::mutation::MutationPathProbes::default()
        }
    );
}

#[test]
fn complex_typed_wire_runtime_results_and_exact_changes_match() {
    let typed_root = complex_fixture();
    let wire_root = complex_fixture();
    let runtime_root = complex_fixture();
    let (typed_value, typed_diagnostics) = typed_result(typed_root.path());
    let input = json!({
        "from": "b/same.md",
        "to": "b/renamed.md",
        "update_refs": true,
        "include_document": true
    });
    let wire_outcome = wire(wire_root.path(), &input);
    assert!(wire_outcome.valid, "{:?}", wire_outcome.diagnostics);
    assert_eq!(normalize_result(wire_outcome.result), typed_value);
    assert_eq!(diagnostics(&wire_outcome.diagnostics), typed_diagnostics);

    let (runtime_result, runtime_outcome) = runtime(runtime_root.path(), &input);
    assert!(runtime_result.valid, "{:?}", runtime_result.diagnostics);
    assert_eq!(normalize_result(runtime_result.result), typed_value);
    assert_eq!(diagnostics(&runtime_result.diagnostics), typed_diagnostics);
    let ChangeSet::Exact(changes) = &runtime_outcome.unwrap().changes else {
        panic!("runtime rename must publish exact changes")
    };
    let records = changes
        .items()
        .iter()
        .map(|change| match change {
            CanonicalChange::Record(record) => record,
            _ => panic!("rename emitted a resource change"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        records
            .iter()
            .filter(|record| record.kind == RecordChangeKind::Renamed)
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record.kind == RecordChangeKind::Updated)
            .count(),
        1
    );
    assert_eq!(typed_value["frontmatter"]["id"], "stable-id");
    assert!(records.iter().all(|record| {
        record.before_revision.is_some()
            && record.after_revision.is_some()
            && record.before_types.iter().eq(record.after_types.iter())
            && (record.changed_fields.iter().next().is_some() || record.body_changed)
    }));
    assert!(fs::read_to_string(runtime_root.path().join("b/ref.md"))
        .unwrap()
        .contains("[[renamed"));
    assert!(fs::read_to_string(runtime_root.path().join("a/ref.md"))
        .unwrap()
        .contains("[[same]]"));
    assert!(fs::read_to_string(runtime_root.path().join("root-ref.md"))
        .unwrap()
        .contains("[[same]]"));
}

#[test]
fn failure_dry_run_and_simulation_adapter_matrix_is_ordered_and_differential() {
    for (setup, input) in [
        (
            "stale",
            json!({"from": "source.md", "to": "renamed.md", "if_revision": "sha256:stale"}),
        ),
        (
            "conflict",
            json!({"from": "source.md", "to": "destination.md"}),
        ),
    ] {
        let typed_root = basic_collection().0;
        let wire_root = basic_collection().0;
        let runtime_root = basic_collection().0;
        if setup == "conflict" {
            for root in [&typed_root, &wire_root, &runtime_root] {
                fs::write(root.path().join("destination.md"), "occupied\n").unwrap();
            }
        }
        let typed_collection = Collection::open(typed_root.path()).unwrap();
        let mut request = RenameRequest::new(
            CollectionPath::new("source.md").unwrap(),
            CollectionPath::new(if setup == "conflict" {
                "destination.md"
            } else {
                "renamed.md"
            })
            .unwrap(),
        );
        if setup == "stale" {
            request.if_revision = Some(Revision::parse("sha256:stale").unwrap());
        }
        let typed_error = typed_collection
            .typed()
            .unwrap()
            .rename(request)
            .unwrap_err();
        let wire_result = wire(wire_root.path(), &input);
        let (runtime_result, runtime_change) = runtime(runtime_root.path(), &input);
        assert!(!wire_result.valid);
        assert!(!runtime_result.valid);
        assert!(runtime_change.is_none());
        assert_eq!(wire_result.result, runtime_result.result);
        assert_eq!(
            diagnostics(&wire_result.diagnostics),
            diagnostics(&runtime_result.diagnostics)
        );
        assert_eq!(
            diagnostics(typed_error.diagnostics()),
            diagnostics(&wire_result.diagnostics)
        );
    }

    let typed_root = basic_collection().0;
    let wire_root = basic_collection().0;
    let runtime_root = basic_collection().0;
    let typed = Collection::open(typed_root.path())
        .unwrap()
        .typed()
        .unwrap()
        .preflight_rename(RenameRequest::new(
            CollectionPath::new("source.md").unwrap(),
            CollectionPath::new("renamed.md").unwrap(),
        ))
        .unwrap();
    let dry_input = json!({"from": "source.md", "to": "renamed.md", "dry_run": true});
    let wire_dry = wire(wire_root.path(), &dry_input);
    let (runtime_dry, runtime_change) = runtime(runtime_root.path(), &dry_input);
    assert!(wire_dry.valid && runtime_dry.valid);
    assert!(runtime_change.is_none());
    assert_eq!(wire_dry.result, runtime_dry.result);
    assert_eq!(wire_dry.result["from"], json!(typed.value.from));
    assert_eq!(wire_dry.result["to"], json!(typed.value.to));
    assert_eq!(
        wire_dry
            .result
            .get("references_affected")
            .cloned()
            .unwrap_or_else(|| json!([])),
        json!(typed.value.references_affected)
    );

    let simulation = json!({
        "from": "source.md",
        "to": "renamed.md",
        "simulate_before_ref_update": [{"path": "other.md", "content": "changed"}]
    });
    let wire_root = basic_collection().0;
    let runtime_root = basic_collection().0;
    let wire_simulation = wire(wire_root.path(), &simulation);
    let (runtime_simulation, runtime_change) = runtime(runtime_root.path(), &simulation);
    assert!(runtime_change.is_none());
    assert_eq!(wire_simulation, runtime_simulation);
    assert_eq!(wire_simulation.diagnostics[0].code, "invalid_request");
}

#[cfg(unix)]
#[test]
fn typed_wire_and_runtime_reject_replaced_collection_roots() {
    use std::os::unix::fs::symlink;

    fn replace(root: &std::path::Path, external: &std::path::Path) -> std::path::PathBuf {
        let held = root.with_extension("held-root");
        fs::rename(root, &held).unwrap();
        symlink(external, root).unwrap();
        held
    }
    fn restore(root: &std::path::Path, held: &std::path::Path) {
        fs::remove_file(root).unwrap();
        fs::rename(held, root).unwrap();
    }

    let (typed_root, typed_collection) = basic_collection();
    let typed_external = tempfile::tempdir().unwrap();
    fs::write(
        typed_external.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\n",
    )
    .unwrap();
    fs::write(typed_external.path().join("source.md"), "external\n").unwrap();
    let held = replace(typed_root.path(), typed_external.path());
    let typed = typed_collection.typed().unwrap().rename(RenameRequest::new(
        CollectionPath::new("source.md").unwrap(),
        CollectionPath::new("renamed.md").unwrap(),
    ));
    assert!(typed.is_err());
    assert!(!typed_external.path().join("renamed.md").exists());
    restore(typed_root.path(), &held);

    let (wire_root, wire_collection) = basic_collection();
    let wire_external = tempfile::tempdir().unwrap();
    fs::write(
        wire_external.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\n",
    )
    .unwrap();
    fs::write(wire_external.path().join("source.md"), "external\n").unwrap();
    let held = replace(wire_root.path(), wire_external.path());
    let wire = crate::v03::batch::execute_wire_mutation(
        &wire_collection,
        "rename",
        &json!({"from": "source.md", "to": "renamed.md"}),
    );
    assert!(!wire.valid);
    assert!(!wire_external.path().join("renamed.md").exists());
    restore(wire_root.path(), &held);

    let (runtime_root, _collection) = basic_collection();
    let runtime = FilesystemRuntime::open(runtime_root.path(), Duration::from_millis(5)).unwrap();
    let runtime_external = tempfile::tempdir().unwrap();
    fs::write(
        runtime_external.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\n",
    )
    .unwrap();
    fs::write(runtime_external.path().join("source.md"), "external\n").unwrap();
    let held = replace(runtime_root.path(), runtime_external.path());
    let request = OperationRequest::new(
        OperationKind::Rename,
        json!({"from": "source.md", "to": "renamed.md"}),
    );
    let runtime_result = runtime.execute(&request);
    assert!(runtime_result.is_err() || runtime_result.is_ok_and(|result| !result.valid));
    assert!(!runtime_external.path().join("renamed.md").exists());
    restore(runtime_root.path(), &held);
}

#[test]
fn serialization_partial_failure_details_match_wire_and_runtime_and_typed_diagnostics() {
    let typed_root = serialization_fixture();
    let wire_root = serialization_fixture();
    let runtime_root = serialization_fixture();
    let typed_error = Collection::open(typed_root.path())
        .unwrap()
        .typed()
        .unwrap()
        .rename(RenameRequest::new(
            CollectionPath::new("target.md").unwrap(),
            CollectionPath::new("renamed.md").unwrap(),
        ))
        .unwrap_err();
    let input = json!({"from": "target.md", "to": "renamed.md", "update_refs": true});
    let wire_result = wire(wire_root.path(), &input);
    let (runtime_result, runtime_change) = runtime(runtime_root.path(), &input);
    assert!(!wire_result.valid && !runtime_result.valid);
    assert!(runtime_change.is_none());
    assert_eq!(wire_result.result, runtime_result.result);
    assert_eq!(
        wire_result.result["partial_updates"]["failed"][0]["reason"],
        "frontmatter_serialization_failed"
    );
    assert_eq!(
        diagnostics(&wire_result.diagnostics),
        diagnostics(&runtime_result.diagnostics)
    );
    assert_eq!(
        diagnostics(typed_error.diagnostics()),
        diagnostics(&wire_result.diagnostics)
    );
}
