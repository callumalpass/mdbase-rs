//! Re-runnable authority serialization benchmark; orchestration lives in sdk-review/bench.
use mdbase::Collection;
use serde_json::json;
#[test]
#[ignore = "50k-file fixture; run sdk-review/bench/wb-authority.mjs"]
fn annotation_metadata_discovery() {
    let output = std::path::PathBuf::from(
        std::env::var("WB_AUTHORITY_BENCH_OUTPUT")
            .expect("explicit benchmark output directory required"),
    );
    std::fs::create_dir_all(&output).unwrap();
    let root = tempfile::tempdir().unwrap();
    for dir in ["_types", "annotations", "other"] {
        std::fs::create_dir(root.path().join(dir)).unwrap();
    }
    std::fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  default_validation: off\n",
    )
    .unwrap();
    std::fs::write(root.path().join("_types/annotation.md"), "---\nkind: mdbase.type\nname: annotation\nversion: 1\nmatch:\n  path_glob: annotations/*.md\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n").unwrap();
    let body = "Synthetic SDK benchmark body. ".repeat(74);
    for i in 0..50_000 {
        let dir = if i < 30_000 { "annotations" } else { "other" };
        let document = format!("---\ntitle: Annotation {i}\nsource: '[[book-{}]]'\nstatus: open\ntags: [bench, synthetic]\nnested:\n  score: {}\n  text: {}\nsummary: {}\n---\n{}", i%100, i%100, "x".repeat(256), "s".repeat(128), &body[..2048]);
        std::fs::write(root.path().join(format!("{dir}/{i:06}.md")), document).unwrap();
    }
    let collection = Collection::open(root.path()).unwrap();
    let ops = collection.v03_operations().unwrap();
    let mut query = json!({"types":["annotation"],"select":["source","file.path"]});
    let mut measurements = Vec::new();
    for mode in ["ordinary", "metadata"] {
        if mode == "metadata" {
            query["output"] = json!("metadata");
        }
        let started = std::time::Instant::now();
        let response = ops.query(&query);
        assert!(response.valid, "{response:?}");
        let rows = response.result["results"].as_array().unwrap();
        assert_eq!(rows.len(), 30_000);
        let bytes = serde_json::to_vec(&response).unwrap();
        measurements.push(json!({"mode":mode,"rows":rows.len(),"responseBytes":bytes.len(),"authorityMs":started.elapsed().as_millis()}));
        std::fs::write(output.join(format!("{mode}.json")), bytes).unwrap();
    }
    std::fs::write(output.join("authority.json"), serde_json::to_vec_pretty(&json!({"fixtureRecords":50_000,"annotationRows":30_000,"selected":["source","file.path"],"measurements":measurements})).unwrap()).unwrap();
}
