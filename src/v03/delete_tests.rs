use std::fs;

use serde_json::{json, Value};

use super::{OperationResult, Operations};
use crate::api::{CollectionPath, DeleteRequest, MdbaseError, Revision};
use crate::Collection;

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
    fs::write(root.path().join("target.md"), "target\n").unwrap();
    fs::write(root.path().join("ref.md"), "See [[target]].\n").unwrap();
    root
}

fn diagnostic_values(error: &MdbaseError) -> Value {
    typed_diagnostic_values(error.diagnostics())
}

fn typed_diagnostic_values(diagnostics: &[crate::api::Diagnostic]) -> Value {
    Value::Array(
        diagnostics
            .iter()
            .map(|item| {
                json!({
                    "severity": item.severity,
                    "code": item.code,
                    "message": item.message,
                    "path": item.path,
                    "field": item.field,
                    "type_name": item.type_name,
                    "schema_location": item.schema_location,
                    "details": item.details,
                })
            })
            .collect(),
    )
}

fn wire_diagnostic_values(outcome: &OperationResult) -> Value {
    Value::Array(
        outcome
            .diagnostics
            .iter()
            .map(|item| {
                json!({
                    "severity": item.severity,
                    "code": item.code,
                    "message": item.message,
                    "path": item.path,
                    "field": item.field,
                    "type_name": item.type_name,
                    "schema_location": item.schema_location,
                    "details": item.details,
                })
            })
            .collect(),
    )
}

fn expected_delete_envelope(value: &crate::api::DeleteResult) -> Value {
    let mut result = json!({"path": value.path, "deleted": value.deleted});
    if !value.broken_links.is_empty() {
        result["broken_links"] = json!(value.broken_links);
    }
    result
}

fn expected_preflight_envelope(value: &crate::api::DeletePreflightResult) -> Value {
    let mut result = json!({
        "path": value.path,
        "deleted": false,
        "dry_run": true,
        "would_delete": value.would_delete,
    });
    if !value.broken_links.is_empty() {
        result["broken_links"] = json!(value.broken_links);
    }
    result
}

#[test]
fn typed_and_wire_delete_results_and_full_diagnostics_are_differential() {
    for check_backlinks in [false, true] {
        let typed_root = fixture();
        let wire_root = fixture();
        let typed_collection = Collection::open(typed_root.path()).unwrap();
        let wire_collection = Collection::open(wire_root.path()).unwrap();
        let mut request = DeleteRequest::new(CollectionPath::new("target.md").unwrap());
        request.check_backlinks = check_backlinks;

        let typed_preview = typed_collection
            .typed()
            .unwrap()
            .preflight_delete(request.clone())
            .unwrap();
        let wire_preview = Operations::new(&wire_collection).unwrap().delete(&json!({
            "path": "target.md",
            "check_backlinks": check_backlinks,
            "dry_run": true,
        }));
        assert!(wire_preview.valid);
        assert_eq!(
            wire_preview.result,
            expected_preflight_envelope(&typed_preview.value)
        );
        assert_eq!(
            wire_diagnostic_values(&wire_preview),
            typed_diagnostic_values(&typed_preview.diagnostics)
        );

        let typed = typed_collection.typed().unwrap().delete(request).unwrap();
        let wire = Operations::new(&wire_collection).unwrap().delete(&json!({
            "path": "target.md",
            "check_backlinks": check_backlinks,
        }));
        assert!(wire.valid);
        assert_eq!(wire.result, expected_delete_envelope(&typed.value));
        assert_eq!(
            wire_diagnostic_values(&wire),
            typed_diagnostic_values(&typed.diagnostics)
        );
    }

    for (path, revision) in [("missing.md", None), ("target.md", Some("sha256:stale"))] {
        let typed_root = fixture();
        let wire_root = fixture();
        let typed_collection = Collection::open(typed_root.path()).unwrap();
        let wire_collection = Collection::open(wire_root.path()).unwrap();
        let mut request = DeleteRequest::new(CollectionPath::new(path).unwrap());
        request.if_revision = revision.map(|value| Revision::parse(value).unwrap());
        let typed = typed_collection
            .typed()
            .unwrap()
            .delete(request)
            .unwrap_err();
        let mut input = json!({"path": path});
        if let Some(revision) = revision {
            input["if_revision"] = json!(revision);
        }
        let wire = Operations::new(&wire_collection).unwrap().delete(&input);
        assert!(!wire.valid);
        assert_eq!(wire.result, json!({}));
        assert_eq!(wire_diagnostic_values(&wire), diagnostic_values(&typed));
    }
}

#[test]
fn invalid_revision_and_malformed_delete_inputs_are_canonical_at_the_wire_edge() {
    let root = fixture();
    let collection = Collection::open(root.path()).unwrap();
    let operations = Operations::new(&collection).unwrap();
    for revision in [json!(""), json!(null), json!(7), json!({})] {
        let delete = operations.delete(&json!({"path": "target.md", "if_revision": revision}));
        let update = operations.update(&json!({
            "path": "target.md",
            "patch": {},
            "if_revision": revision,
        }));
        assert!(!delete.valid);
        assert_eq!(
            wire_diagnostic_values(&delete),
            wire_diagnostic_values(&update)
        );
        assert_eq!(delete.diagnostics[0].code, "invalid_request");
    }

    for input in [
        json!({}),
        json!({"path": 7}),
        json!({"path": "target.md", "check_backlinks": "yes"}),
        json!({"path": "target.md", "dry_run": "yes"}),
    ] {
        let outcome = operations.delete(&input);
        if input.get("path").and_then(Value::as_str).is_none() {
            assert!(!outcome.valid);
            assert_eq!(outcome.diagnostics[0].code, "invalid_path");
        } else {
            // Optional non-boolean controls retain the established false default.
            assert!(outcome.valid, "{:?}", outcome.diagnostics);
            fs::write(root.path().join("target.md"), "target\n").unwrap();
        }
    }

    assert!(CollectionPath::new("../escape.md").is_err());
    let unsafe_path = operations.delete(&json!({"path": "../escape.md"}));
    assert!(!unsafe_path.valid);
    assert_eq!(unsafe_path.result, json!({}));
    assert_eq!(unsafe_path.diagnostics[0].code, "path_traversal");

    for path in ["mdbase.yaml", "_types/type.md"] {
        let typed_root = fixture();
        let wire_root = fixture();
        let typed_collection = Collection::open(typed_root.path()).unwrap();
        let wire_collection = Collection::open(wire_root.path()).unwrap();
        let typed = typed_collection
            .typed()
            .unwrap()
            .delete(DeleteRequest::new(CollectionPath::new(path).unwrap()))
            .unwrap_err();
        let wire = Operations::new(&wire_collection)
            .unwrap()
            .delete(&json!({"path": path}));
        assert!(!wire.valid);
        assert_eq!(wire.result, json!({}));
        assert_eq!(wire_diagnostic_values(&wire), diagnostic_values(&typed));
    }
}

#[test]
fn typed_and_wire_delete_invalid_record_matrix_preserves_path_types() {
    for (name, bytes) in [
        ("malformed.md", b"---\ntitle: [bad\n---\nBody\n".as_slice()),
        ("nonmapping.md", b"---\n- item\n---\nBody\n".as_slice()),
        ("binary.md", b"bad\xffutf8".as_slice()),
    ] {
        let typed_root = fixture_with_path_type(name, bytes);
        let wire_root = fixture_with_path_type(name, bytes);
        let typed_collection = Collection::open(typed_root.path()).unwrap();
        let wire_collection = Collection::open(wire_root.path()).unwrap();
        let request = DeleteRequest::new(CollectionPath::new(name).unwrap());
        let typed = typed_collection.typed().unwrap().delete(request).unwrap();
        let wire = Operations::new(&wire_collection)
            .unwrap()
            .delete(&json!({"path": name}));
        assert!(typed.value.deleted);
        assert!(wire.valid, "{:?}", wire.diagnostics);
        assert_eq!(wire.result, expected_delete_envelope(&typed.value));
        assert!(!typed_root.path().join(name).exists());
        assert!(!wire_root.path().join(name).exists());
    }
}

fn fixture_with_path_type(name: &str, bytes: &[u8]) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(
        root.path().join("_types/path-record.md"),
        format!(
            "---\nkind: mdbase.type\nname: path-record\nmatch:\n  path_glob: '{name}'\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n"
        ),
    )
    .unwrap();
    fs::write(root.path().join(name), bytes).unwrap();
    root
}

#[test]
fn delete_probe_matrix_counts_real_boundaries() {
    let root = fixture();
    let collection = Collection::open(root.path()).unwrap();
    crate::mutation::reset_mutation_path_probes();
    let legacy = collection.delete(&json!({"path": "target.md", "dry_run": true}));
    assert_eq!(legacy["would_delete"], true);
    assert_eq!(
        crate::mutation::mutation_path_probes(),
        crate::mutation::service::MutationPathProbes {
            legacy_request_parses: 1,
            ..Default::default()
        }
    );

    crate::mutation::reset_mutation_path_probes();
    crate::transactions::inject_post_commit_replacement(
        root.path(),
        "target.md",
        Some(b"external replacement".to_vec()),
    );
    let deleted = Operations::new(&collection)
        .unwrap()
        .delete(&json!({"path": "target.md"}));
    assert!(deleted.valid, "{:?}", deleted.diagnostics);
    assert_eq!(
        fs::read(root.path().join("target.md")).unwrap(),
        b"external replacement"
    );
    assert_eq!(
        crate::mutation::mutation_path_probes(),
        crate::mutation::service::MutationPathProbes {
            wire_request_decodes: 1,
            full_shadows: 1,
            ..Default::default()
        }
    );
}
