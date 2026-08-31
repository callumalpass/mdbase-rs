use jsonschema::{Draft, JSONSchema};
use mdbase::api::{CollectionPath, ReadRequest};
use mdbase::Collection;
use serde_json::Value;

#[test]
fn typed_read_documents_with_and_without_exact_source_match_the_published_schema() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
    std::fs::write(root.path().join("note.md"), "Body\n").unwrap();
    let collection = Collection::open(root.path()).unwrap();
    let schema: Value =
        serde_json::from_str(include_str!("../schemas/v0.3/record-document.schema.json")).unwrap();
    assert_eq!(schema["properties"]["document"]["type"], "string");
    let compiled = JSONSchema::options()
        .with_draft(Draft::Draft202012)
        .compile(&schema)
        .unwrap();

    for include_document in [false, true] {
        let request = ReadRequest {
            path: CollectionPath::new("note.md").unwrap(),
            include_document,
        };
        let emitted =
            serde_json::to_value(collection.typed().unwrap().read(request).unwrap().value).unwrap();
        assert_eq!(emitted.get("document").is_some(), include_document);
        assert!(compiled.is_valid(&emitted), "{emitted:#}");
    }
}
