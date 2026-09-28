//! Spike: `.base` files as whole-document YAML records. Reports each step.

use std::fs;
use std::path::Path;

use mdbase::Collection;
use serde_json::json;

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

const BASE: &str = "# Obsidian comment\nfilters:\n  and:\n    - 'status == \"open\"'\nviews:\n  - type: table\n    name: Open\n    order: [file.name]\n";

fn report(step: &str, result: &mdbase::v03::OperationResult) {
    let summary = json!({
        "valid": result.valid,
        "types": result.result.get("types"),
        "diagnostics": result.diagnostics.iter().map(|d| format!("{}: {}", d.code, d.message)).collect::<Vec<_>>(),
    });
    eprintln!("SPIKE {step}: {summary}");
}

#[test]
fn base_files_as_yaml_document_records() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "mdbase.yaml",
        "spec_version: \"0.3.0\"\nsettings:\n  record_extensions: [md, base]\n",
    );
    write(
        root,
        "_contracts/obsidian.base.md",
        "---\nkind: mdbase.contract\ncontract_type: record\nid: obsidian.base\nversion: 1.0.0\nrecord_schema: {dialect: json-schema-2020-12, value: {type: object, required: [views], properties: {filters: {}, formulas: {}, properties: {}, views: {type: array}}}}\n---\n",
    );
    write(
        root,
        "_types/obsidian-base.md",
        "---\nkind: mdbase.type\nname: obsidian_base\nversion: 1\nmatch: {path_glob: 'views/**/*.base'}\nschema: {dialect: json-schema-2020-12, value: {type: object, properties: {filters: {}, formulas: {}, properties: {}, views: {type: array}}}}\nimplements: [{contract: obsidian.base, version: 1.0.0, fields: {filters: filters, views: views}}]\n---\n",
    );
    write(root, "views/open.base", BASE);
    write(root, "notes/a.md", "---\nstatus: open\n---\nA\n");

    let collection = Collection::open(root).unwrap();
    let ops = collection.v03_operations().unwrap();

    let read = ops.read(&json!({"path": "views/open.base"}));
    report("read", &read);
    eprintln!("SPIKE read.frontmatter: {}", read.result["frontmatter"]);
    eprintln!("SPIKE read.body: {:?}", read.result["body"]);

    report(
        "validate",
        &ops.validate(&json!({"path": "views/open.base"})),
    );

    let query = ops.query(&json!({"types": ["obsidian_base"]}));
    report("query by type", &query);
    eprintln!(
        "SPIKE query.paths: {:?}",
        query.result["results"]
            .as_array()
            .map(|r| r.iter().map(|x| x["path"].clone()).collect::<Vec<_>>())
    );
    let all = ops.query(&json!({}));
    eprintln!(
        "SPIKE unfiltered query paths: {:?}",
        all.result["results"]
            .as_array()
            .map(|r| r.iter().map(|x| x["path"].clone()).collect::<Vec<_>>())
    );

    let view = collection.get_contract_view("views/open.base", "obsidian.base", "1.0.0", None);
    eprintln!(
        "SPIKE contract view valid={} view={}",
        view.valid, view.view
    );

    let listed = ops.list_views(&json!({}));
    report("list_views", &listed);
    eprintln!("SPIKE list_views sources: {}", listed.result["views"]);

    report(
        "calibration: update markdown patch",
        &ops.update(&json!({"path": "notes/a.md", "patch": {"status": "done"}})),
    );
    eprintln!(
        "SPIKE calibration bytes: {:?}",
        fs::read_to_string(root.join("notes/a.md")).unwrap()
    );
    {
        use std::os::unix::fs::MetadataExt;
        let m = fs::symlink_metadata(root.join("views/open.base"));
        eprintln!(
            "SPIKE before patch: exists={} nlink={:?}",
            m.is_ok(),
            m.map(|m| m.nlink()).ok()
        );
        eprintln!(
            "SPIKE dir: {:?}",
            fs::read_dir(root.join("views"))
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect::<Vec<_>>()
        );
    }
    let patched =
        ops.update(&json!({"path": "views/open.base", "patch": {"formulas": {"x": "1"}}}));
    report("update patch", &patched);
    eprintln!(
        "SPIKE after patch bytes:\n{}",
        fs::read_to_string(root.join("views/open.base")).unwrap()
    );

    let replaced = ops.update(&json!({"path": "views/open.base", "document": BASE}));
    report("update document", &replaced);
    eprintln!(
        "SPIKE after document replace identical={}",
        fs::read_to_string(root.join("views/open.base")).unwrap() == BASE
    );

    let body = ops.update(&json!({"path": "views/open.base", "body": "text"}));
    report("update body (should be rejected)", &body);

    let created = ops.create(&json!({"path": "views/new.base", "frontmatter": {"views": [{"type": "table", "name": "New"}]}}));
    report("create", &created);
    eprintln!(
        "SPIKE created bytes:\n{}",
        fs::read_to_string(root.join("views/new.base")).unwrap_or_default()
    );

    let renamed = ops.rename(&json!({"from": "views/new.base", "to": "views/renamed.base"}));
    report("rename", &renamed);

    let snapshot = mdbase::runtime::FilesystemProvider::open(root).and_then(|p| p.snapshot());
    match snapshot {
        Ok(snapshot) => eprintln!(
            "SPIKE snapshot records: {:?}",
            snapshot
                .records
                .iter()
                .map(|r| (r.path.clone(), r.types.clone(), r.body.len()))
                .collect::<Vec<_>>()
        ),
        Err(error) => eprintln!("SPIKE snapshot error: {error}"),
    }
}
