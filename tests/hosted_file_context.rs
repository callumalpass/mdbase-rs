use mdbase::runtime::{
    CanonicalRecordInput, CatalogInput, CompiledCatalog, HostedMutationRequest,
    ResolvedTypeResource,
};
use serde_json::json;

fn catalog(typed_file_link: bool) -> CompiledCatalog {
    let schema = json!({"type": "object"});
    CompiledCatalog::compile(CatalogInput {
        resource_revision: "files-1".into(),
        configuration_document: "spec_version: 0.3.0\nsettings:\n  validation: error\n".into(),
        types: vec![ResolvedTypeResource {
            path: "_types/annotation.md".into(),
            revision: "type-1".into(),
            definition: json!({
                "kind": "mdbase.type", "name": "annotation", "version": 1,
                "match": {"path_glob": "annotations/*.md"},
                "schema": {"dialect": "json-schema-2020-12", "value": schema},
                "collection": {"links": {"document.file": {
                    "target_type": if typed_file_link { "annotation" } else { "any" },
                    "validate_exists": true
                }}}
            }),
            schema,
        }],
        contracts: vec![],
    })
    .unwrap()
}

fn create(path: &str) -> HostedMutationRequest {
    HostedMutationRequest {
        operation: "create".into(),
        primary_stable_id: "annotation-1".into(),
        input: json!({"path": "annotations/highlight.md", "type": "annotation",
            "frontmatter": {"document": {"file": path}}}),
        records: vec![],
    }
}

fn record(path: &str) -> CanonicalRecordInput {
    let document = format!("---\ntype: annotation\ndocument:\n  file: '{path}'\n---\nHighlight\n");
    CanonicalRecordInput {
        stable_id: Some("annotation-1".into()),
        path: "annotations/highlight.md".into(),
        file_size: document.len() as u64,
        document,
        file_mtime: None,
    }
}

#[test]
fn attachment_links_validate_without_attachment_bytes() {
    let catalog = catalog(false);
    for path in [
        "files/reader/article.html",
        "files/book.pdf",
        "files/book.epub",
    ] {
        let files = vec![path.into()];
        let request = create(path);
        let missing = catalog.plan_hosted_mutation_typed(&request).unwrap();
        assert!(!missing.operation.is_valid());
        assert!(missing.changes.is_empty());
        let present = catalog
            .plan_hosted_mutation_with_files_typed(&request, &files)
            .unwrap();
        assert!(present.operation.is_valid(), "{:?}", present.operation);
        assert_eq!(
            present.changes.len(),
            1,
            "file witnesses must not be writes"
        );
        assert_eq!(
            present.changes[0].after.as_ref().unwrap().path.as_str(),
            "annotations/highlight.md"
        );

        let target = record(path);
        let plan = catalog
            .plan_hosted_validation(&json!({"path": target.path}), &target)
            .unwrap();
        let missing = catalog
            .execute_hosted_validation_typed(&plan, std::slice::from_ref(&target))
            .unwrap()
            .to_v03();
        assert!(missing
            .diagnostics
            .iter()
            .any(|issue| issue.code == "link_not_found"));
        let present = catalog
            .execute_hosted_validation_with_files_typed(&plan, &[target], &files)
            .unwrap()
            .to_v03();
        assert!(
            present.valid && present.diagnostics.is_empty(),
            "{present:?}"
        );
    }
}

#[test]
fn attachment_context_survives_updates_batches_and_dry_runs() {
    let catalog = catalog(false);
    let path = "files/article.html";
    let files = vec![path.into()];
    let mut request = create(path);
    request.input["dry_run"] = json!(true);
    let dry_run = catalog
        .plan_hosted_mutation_with_files_typed(&request, &files)
        .unwrap();
    assert!(dry_run.operation.is_valid(), "{:?}", dry_run.operation);
    request.operation = "update".into();
    request.input = json!({"patch": {"title": "Edited"}});
    request.records = vec![record(path)];
    let updated = catalog
        .plan_hosted_mutation_with_files_typed(&request, &files)
        .unwrap();
    assert!(updated.operation.is_valid(), "{:?}", updated.operation);
    request.operation = "batch".into();
    request.records.clear();
    request.input = json!({"operations": [{"kind": "create", "stable_id": "annotation-1", "input": {
        "path": "annotations/highlight.md", "type": "annotation", "frontmatter": {"document": {"file": path}}
    }}]});
    let batch = catalog
        .plan_hosted_mutation_with_files_typed(&request, &files)
        .unwrap();
    assert!(batch.operation.is_valid(), "{:?}", batch.operation);
}

#[test]
fn ordinary_file_witnesses_do_not_invent_record_types_or_missing_files() {
    let catalog = catalog(true);
    let request = create("files/article.html");
    let result = catalog
        .plan_hosted_mutation_with_files_typed(&request, &["files/article.html".into()])
        .unwrap();
    assert!(!result.operation.is_valid());
    assert!(result.changes.is_empty());
    assert!(result
        .operation
        .diagnostics()
        .iter()
        .any(|issue| issue.code.as_str() == "link_wrong_type"));

    let catalog = self::catalog(false);
    let result = catalog
        .plan_hosted_mutation_with_files_typed(&request, &["files/other.html".into()])
        .unwrap();
    assert!(!result.operation.is_valid());
    assert!(result
        .operation
        .diagnostics()
        .iter()
        .any(|issue| issue.code.as_str() == "link_not_found"));
}

#[test]
fn file_context_rejects_record_resource_traversal_duplicate_and_unbounded_paths() {
    let catalog = catalog(false);
    let request = create("files/article.html");
    for paths in [
        vec!["records/fake.md".into()],
        vec!["../outside.html".into()],
        vec!["_types/fake.html".into()],
        vec!["mdbase.yaml".into()],
        vec!["files/article.html".into(), "files/article.html".into()],
        vec![format!("files/{}.html", "a".repeat(1024))],
    ] {
        let error = catalog
            .plan_hosted_mutation_with_files_typed(&request, &paths)
            .unwrap_err();
        assert_eq!(error.code, "invalid_file_path", "{paths:?}");
    }
    let error = catalog
        .plan_hosted_mutation_with_files_typed(&request, &vec!["files/file.html".into(); 2001])
        .unwrap_err();
    assert_eq!(error.code, "hosted_file_context_budget_exceeded");
}
