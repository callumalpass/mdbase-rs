//! Normal/narrow producer conformance (B2), not client-side stripping.
use mdbase::{
    api::{QueryOutput, QueryRequest},
    runtime::{CanonicalRecordInput, CatalogInput, CompiledCatalog},
    Collection,
};
use serde_json::{json, Value};

fn fixture() -> (
    tempfile::TempDir,
    Collection,
    CompiledCatalog,
    Vec<CanonicalRecordInput>,
) {
    let root = tempfile::tempdir().unwrap();
    let config = "spec_version: 0.3.0\nsettings:\n  default_validation: off\n";
    std::fs::write(root.path().join("mdbase.yaml"), config).unwrap();
    let records = [
        (
            "a.md",
            "---\nsource: book\nrank: 2\nnullable: null\n---\nText #tag\n",
        ),
        ("b.md", "---\nsource: book\nrank: 1\n---\nSecond\n"),
    ]
    .into_iter()
    .map(|(path, document)| {
        std::fs::write(root.path().join(path), document).unwrap();
        CanonicalRecordInput {
            stable_id: None,
            path: path.into(),
            document: document.into(),
            file_size: document.len() as u64,
            file_mtime: None,
        }
    })
    .collect();
    let collection = Collection::open(root.path()).unwrap();
    let catalog = CompiledCatalog::compile(CatalogInput {
        resource_revision: "catalog".into(),
        configuration_document: config.into(),
        types: vec![],
        contracts: vec![],
    })
    .unwrap();
    (root, collection, catalog, records)
}
fn assert_narrow(ordinary: &Value, narrow: &Value) {
    let keys = narrow
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(keys, ["path", "revision", "types", "values"]);
    for key in ["path", "revision", "types"] {
        assert_eq!(ordinary[key], narrow[key], "{key}");
    }
    assert_eq!(
        ordinary.get("values").cloned().unwrap_or(json!({})),
        narrow["values"]
    );
    let _: mdbase::api::MetadataQueryRecord = serde_json::from_value(narrow.clone()).unwrap();
}
#[test]
fn normal_narrow_membership_order_counts_groups_values_and_diagnostics_match() {
    let (root, collection, _, _) = fixture();
    std::fs::write(root.path().join("invalid.md"), "---\nbad: [oops\n---\n").unwrap();
    let ops = collection.v03_operations().unwrap();
    let queries = [
        json!({}),
        json!({"select":["source","nullable","missing"],"order_by":[{"field":"rank"}]}),
        json!({"select":["source","projection.alias"],"projections":{"alias":{"expr":"source + '-resolved'"}},"where":"rank > 0","limit":1}),
        json!({"where":"true","select":[{"name":"size","expr":"file.size"}],"group_by":[{"field":"source"}],"summaries":[{"field":"rank","function":"sum"}]}),
        json!({"where":"false","summaries":[{"field":"rank","function":"count"}]}),
        json!({"select":["file.tags","file.links"],"frontmatter_mode":"both"}),
    ];
    for query in queries {
        let mut metadata = query.clone();
        metadata["output"] = json!("metadata");
        let normal = ops.query(&query);
        let narrow = ops.query(&metadata);
        assert!(normal.valid && narrow.valid, "{normal:?}\n{narrow:?}");
        assert_eq!(narrow.result["output"], "metadata");
        assert!(mdbase::v03::validate_query_result(&normal.result).is_empty());
        assert!(mdbase::v03::validate_query_result(&narrow.result).is_empty());
        if !narrow.result["results"].as_array().unwrap().is_empty() {
            for key in ["revision", "values"] {
                let mut malformed = narrow.result.clone();
                malformed["results"][0].as_object_mut().unwrap().remove(key);
                assert!(!mdbase::v03::validate_query_result(&malformed).is_empty());
            }
            let mut leaked = narrow.result.clone();
            leaked["results"][0]["body"] = json!("not metadata");
            assert!(!mdbase::v03::validate_query_result(&leaked).is_empty());
        }
        assert!(normal.result.get("output").is_none());
        assert_eq!(normal.result["meta"], narrow.result["meta"]);
        assert_eq!(normal.diagnostics, narrow.diagnostics);
        for (ordinary, narrow) in normal.result["results"]
            .as_array()
            .unwrap()
            .iter()
            .zip(narrow.result["results"].as_array().unwrap())
        {
            assert_narrow(ordinary, narrow);
        }
    }
    for invalid in [
        json!({"output":"metadata","include_body":true}),
        json!({"output":"metadata","select":[]}),
        json!({"output":"other"}),
    ] {
        assert!(!ops.query(&invalid).valid, "{invalid}");
    }
    let typed = collection
        .typed()
        .unwrap()
        .query(QueryRequest {
            output: Some(QueryOutput::Metadata),
            ..QueryRequest::default()
        })
        .unwrap();
    assert_eq!(typed.value.output, Some(QueryOutput::Metadata));
    assert_eq!(
        serde_json::to_value(typed.value).unwrap()["output"],
        "metadata"
    );
}
#[test]
fn hosted_projection_selections_do_not_require_exact_hydration_and_match_exact() {
    let (_root, collection, catalog, records) = fixture();
    for query in [
        json!({}),
        json!({"select":["source","nullable","missing"]}),
        json!({"select":["projection.alias"],"projections":{"alias":{"expr":"source + '-resolved'"}},"where":"rank > 0"}),
    ] {
        let mut metadata = query.clone();
        metadata["output"] = json!("metadata");
        let normal_plan = catalog.compile_hosted_query(&query).unwrap();
        let narrow_plan = catalog.compile_hosted_query(&metadata).unwrap();
        assert!(!narrow_plan.requirements.exact_document, "{metadata}");
        assert_ne!(normal_plan.plan_digest, narrow_plan.plan_digest);
        for record in &records {
            let prepared = catalog.project_record(record).unwrap();
            let resolution = catalog.plan_record_resolution(&prepared.structure).unwrap();
            let resolved = catalog
                .resolve_record_structure(&prepared.structure, &resolution, &[])
                .unwrap();
            let projection = catalog.finalize_projection(prepared, resolved).unwrap();
            let normal = catalog
                .evaluate_hosted_residual(&normal_plan, record)
                .unwrap();
            let exact = catalog
                .evaluate_hosted_residual(&narrow_plan, record)
                .unwrap();
            let projected = catalog
                .evaluate_hosted_projection_residual(&narrow_plan, &projection)
                .unwrap();
            assert_eq!(exact, projected);
            assert_narrow(
                normal.record.as_ref().unwrap(),
                exact.record.as_ref().unwrap(),
            );
        }
        let local = collection.v03_operations().unwrap().query(&metadata);
        assert!(local.valid);
    }
    let plan = catalog
        .compile_hosted_query(&json!({"output":"metadata","select":["file.tags"]}))
        .unwrap();
    assert!(plan.requirements.exact_document); // structural facts need canonical body context today
}
