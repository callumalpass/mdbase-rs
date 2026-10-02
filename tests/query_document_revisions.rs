//! Revision/document producer conformance (B3).
use mdbase::{
    runtime::{CanonicalRecordInput, CatalogInput, CompiledCatalog},
    Collection,
};
use serde_json::{json, Value};

fn local() -> (tempfile::TempDir, Collection) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  default_validation: off\n",
    )
    .unwrap();
    let collection = Collection::open(root.path()).unwrap();
    (root, collection)
}
fn catalog() -> CompiledCatalog {
    CompiledCatalog::compile(CatalogInput {
        resource_revision: "catalog".into(),
        configuration_document: "spec_version: 0.3.0\nsettings:\n  default_validation: off\n"
            .into(),
        types: vec![],
        contracts: vec![],
    })
    .unwrap()
}
#[test]
fn query_fast_general_cache_and_external_edit_keep_exact_source_tokens() {
    let (root, collection) = local();
    let document = "---\r\n# comment\r\ntitle: café\r\n---\r\nUnicode 🦀\r\n";
    std::fs::write(root.path().join("one.md"), document).unwrap();
    let ops = collection.v03_operations().unwrap();
    let read = ops.read(&json!({"path":"one.md","include_document":true}));
    assert_eq!(read.result["document"], document);
    for query in [
        json!({}),
        json!({"where":"true","include_body":true}),
        json!({"select":["title"]}),
    ] {
        let result = ops.query(&query);
        assert!(result.valid, "{result:?}");
        assert_eq!(
            result.result["results"][0]["revision"],
            read.result["revision"]
        );
    }
    std::fs::write(root.path().join("one.md"), "---\ntitle: edited\n---\nnew\n").unwrap();
    let revised = ops.query(&json!({"where":"true"}));
    let batch = ops.read(&json!({"paths":["one.md"],"include_document":true}));
    assert_ne!(
        batch.result["items"][0]["record"]["revision"],
        read.result["revision"]
    );
    assert_eq!(
        batch.result["items"][0]["record"]["revision"],
        revised.result["results"][0]["revision"]
    );
    let stale =
        ops.update(&json!({"path":"one.md","if_revision":read.result["revision"],"body":"stale"}));
    assert!(!stale.valid);
}
#[test]
fn batches_preserve_occurrences_omissions_failures_and_caps() {
    let (root, collection) = local();
    std::fs::write(root.path().join("one.md"), "---\ntitle: One\n---\nbody\n").unwrap();
    std::fs::write(root.path().join("invalid.md"), "---\nx: [broken\n---\n").unwrap();
    let ops = collection.v03_operations().unwrap();
    let result = ops
        .read(&json!({"paths":["one.md","missing.md","invalid.md","one.md"],"include_body":false}));
    assert!(result.valid, "{result:?}");
    let items = result.result["items"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    assert_eq!(items[0], items[3]);
    assert_eq!(items[1]["status"], "missing");
    assert_eq!(items[2]["status"], "error");
    assert!(items[0]["record"].get("body").is_none());
    assert!(items[0]["record"].get("document").is_none());
    for input in [
        json!({"paths":[]}),
        json!({"path":"one.md","paths":["one.md"]}),
        json!({"paths":["../escape.md"]}),
        json!({"paths":vec!["one.md";101]}),
        json!({"paths":["one.md"],"include_body":1}),
    ] {
        assert!(!ops.read(&input).valid, "{input}");
    }
    std::fs::write(root.path().join("large.md"), "x".repeat(8 * 1024 * 1024)).unwrap();
    assert!(!ops.read(&json!({"paths":["large.md"]})).valid);
}
#[test]
fn hosted_exact_and_projected_revisions_match_local_source_and_batches() {
    let catalog = catalog();
    let record = CanonicalRecordInput {
        stable_id: None,
        path: "one.md".into(),
        document: "---\r\ntitle: café\r\n---\r\nbody\r\n".into(),
        file_size: 0,
        file_mtime: None,
    };
    let prepared = catalog.project_record(&record).unwrap();
    let resolution = catalog.plan_record_resolution(&prepared.structure).unwrap();
    let resolved = catalog
        .resolve_record_structure(&prepared.structure, &resolution, &[])
        .unwrap();
    let projection = catalog.finalize_projection(prepared, resolved).unwrap();
    let point = catalog
        .read_record_typed(&json!({"path":"one.md"}), &record)
        .unwrap()
        .to_v03();
    for query in [json!({}), json!({"where":"true","include_body":true})] {
        let plan = catalog.compile_hosted_query(&query).unwrap();
        let exact = catalog.evaluate_hosted_residual(&plan, &record).unwrap();
        assert_eq!(
            exact.record.as_ref().unwrap()["revision"],
            point.result["revision"]
        );
        if !plan.requirements.exact_document {
            let projected = catalog
                .evaluate_hosted_projection_residual(&plan, &projection)
                .unwrap();
            assert_eq!(
                projected.record.as_ref().unwrap()["revision"],
                point.result["revision"]
            );
        }
    }
    let request = mdbase::api::ReadManyRequest::parse(
        &json!({"paths":["one.md","missing.md","one.md"],"include_document":true}),
    )
    .unwrap();
    let records = std::collections::BTreeMap::from([("one.md".into(), record)]);
    let batch = catalog
        .read_records_typed(&request, &records)
        .unwrap()
        .to_v03();
    assert!(batch.valid);
    assert_eq!(
        batch.result["items"][0]["record"],
        point
            .result
            .as_object()
            .map(|obj| {
                let mut obj = obj.clone();
                obj.insert(
                    "document".into(),
                    Value::String(records["one.md"].document.clone()),
                );
                Value::Object(obj)
            })
            .unwrap()
    );
}
