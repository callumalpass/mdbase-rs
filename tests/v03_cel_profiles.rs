use std::fs;

use mdbase::Collection;
use serde_json::json;
use tempfile::TempDir;

fn v03_collection(type_file: &str, records: &[(&str, &str)]) -> (TempDir, Collection) {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  timezone: UTC\n  validation: error\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(root.path().join("_types/test.md"), type_file).unwrap();
    for (path, contents) in records {
        let target = root.path().join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, contents).unwrap();
    }
    let collection = Collection::open(root.path()).unwrap();
    (root, collection)
}

#[test]
fn portable_cel_accepts_the_required_depth_and_rejects_excess_depth() {
    let (_root, collection) = v03_collection(
        "---\nkind: mdbase.type\nname: test\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n",
        &[],
    );
    let operations = collection.v03_operations().unwrap();

    let nested = |depth| {
        (0..depth).fold("true".to_string(), |inner, _| {
            format!("(true ? {inner} : false)")
        })
    };
    let supported = operations.evaluate_cel(&json!({"expression": nested(100)}));
    assert!(supported.valid, "{supported:#?}");
    assert_eq!(supported.result["value"], true);

    let rejected = operations.evaluate_cel(&json!({"expression": nested(129)}));
    assert!(!rejected.valid);
    assert_eq!(rejected.diagnostics[0].code, "expression_depth_exceeded");
}

#[test]
fn portable_cel_bounds_source_and_uses_standard_durations() {
    let (_root, collection) = v03_collection(
        "---\nkind: mdbase.type\nname: test\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n",
        &[],
    );
    let operations = collection.v03_operations().unwrap();

    let duration = operations.evaluate_cel(&json!({
        "expression": "duration('26h30m') == duration('95400s')"
    }));
    assert!(duration.valid, "{duration:#?}");
    assert_eq!(duration.result["value"], true);

    let stable_clock = operations.evaluate_cel(&json!({
        "expression": "now() == now() && today() == today()",
        "timezone": "Australia/Melbourne"
    }));
    assert!(stable_clock.valid, "{stable_clock:#?}");
    assert_eq!(stable_clock.result["value"], true);

    let oversized = operations.evaluate_cel(&json!({
        "expression": format!("'{}'", "x".repeat(64 * 1024))
    }));
    assert!(!oversized.valid);
    assert_eq!(
        oversized.diagnostics[0].code,
        "expression_source_limit_exceeded"
    );
}

#[test]
fn match_evaluation_errors_are_reported_and_do_not_match() {
    let (_root, collection) = v03_collection(
        "---\nkind: mdbase.type\nname: test\nmatch:\n  path_glob: records/**/*.md\n  expr:\n    $expr: '\"not-a-number\" - 1 > 1'\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n",
        &[("records/failing.md", "---\ntitle: Failing\n---\n")],
    );
    let operations = collection.v03_operations().unwrap();

    let matched = operations.get_types(&json!({"path": "records/failing.md"}));
    assert!(matched.valid, "{matched:#?}");
    assert_eq!(matched.result["types"], json!([]));
    assert_eq!(matched.diagnostics.len(), 1);
    assert_eq!(matched.diagnostics[0].code, "expression_evaluation_error");
    assert_eq!(matched.diagnostics[0].type_name.as_deref(), Some("test"));
    assert_eq!(
        matched.diagnostics[0]
            .details
            .as_ref()
            .and_then(|details| details.get("context")),
        Some(&json!("match"))
    );

    let read = operations.read(&json!({"path": "records/failing.md"}));
    assert!(read.valid, "{read:#?}");
    assert_eq!(read.result["types"], json!([]));
    assert_eq!(read.diagnostics[0].code, "expression_evaluation_error");
}

#[test]
fn links_resolve_relative_to_the_record_they_were_read_from() {
    let (_root, collection) = v03_collection(
        "---\nkind: mdbase.type\nname: test\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n",
        &[
            (
                "projects/alpha.md",
                "---\nlead: \"[Bob](people/bob.md)\"\nrelated: [\"[[./beta]]\"]\n---\nSee [the plan](plan.md) and [[projects/beta|Beta]].\n",
            ),
            ("projects/beta.md", "---\ntitle: Beta\n---\n"),
            ("projects/plan.md", "---\ntitle: Plan\n---\n"),
            ("projects/people/bob.md", "---\nname: Bob\n---\n"),
            ("tasks/t1.md", "---\nproject: \"[[alpha]]\"\n---\n"),
        ],
    );
    let operations = collection.v03_operations().unwrap();
    let evaluate = |expression: &str| {
        let evaluated =
            operations.evaluate_cel(&json!({"path": "tasks/t1.md", "expression": expression}));
        assert!(evaluated.valid, "{expression}: {evaluated:#?}");
        evaluated.result["value"].clone()
    };
    // Links read from an asFile() result resolve relative to that target.
    assert_eq!(
        evaluate("project.asFile().lead.asFile().name"),
        json!("Bob")
    );
    assert_eq!(
        evaluate("project.asFile().related.map(r, r.asFile().file.path)"),
        json!(["projects/beta.md"])
    );
    assert_eq!(
        evaluate("project.asFile().file.links.exists(l, l.asFile().title == \"Plan\")"),
        json!(true)
    );
    // file.links holds link values that resolve as the originals did.
    assert_eq!(
        evaluate("project.asFile().file.links"),
        json!(["./beta", "[[projects/beta]]", "./plan.md"])
    );
    assert_eq!(
        evaluate(
            "project.asFile().file.links.map(l, l.asFile() == null ? \"\" : l.asFile().file.path)"
        ),
        json!(["projects/beta.md", "projects/beta.md", "projects/plan.md"])
    );
    assert_eq!(
        evaluate("project.asFile().file.hasLink(link(project.asFile().related[0]))"),
        json!(true)
    );

    // Links read from the invocation context resolve relative to it.
    let query = operations.query(&json!({
        "context": {"this": {"path": "projects/alpha.md"}},
        "where": "file.inFolder(\"tasks\") && this.related.exists(r, r.asFile().title == \"Beta\")",
        "select": [{"name": "lead", "expr": "this.lead.asFile().name"}],
    }));
    assert!(query.valid, "{query:#?}");
    assert_eq!(query.result["results"][0]["values"], json!({"lead": "Bob"}));
}
