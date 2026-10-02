//! Native B6 policy fixtures and the Connect linksTo() producer contract.
use std::fs;

use mdbase::Collection;
use serde_json::{json, Value};
use tempfile::TempDir;

fn write(root: &TempDir, path: &str, text: &str) {
    let path = root.path().join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn fixture() -> (TempDir, Collection, Value) {
    let fixture: Value =
        serde_json::from_str(include_str!("../conformance/link-resolution-v1.json")).unwrap();
    let root = tempfile::tempdir().unwrap();
    write(&root, "mdbase.yaml", "spec_version: 0.3.0\nsettings:\n  timezone: UTC\n  validation: warn\n  id_field: id\n  record_extensions: [md, mdx, png]\n");
    for name in ["source", "note", "annotation", "probe"] {
        let links = if name == "annotation" {
            "collection:\n  links:\n    source: { target_type: source }\n    blocked: { target_type: source }\n    related: { target_type: source }\n"
        } else {
            ""
        };
        write(&root, &format!("_types/{name}.md"), &format!("---\nkind: mdbase.type\nname: {name}\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    properties:\n      type: {{ const: {name} }}\n      source: {{ type: string }}\n      blocked: {{ type: string }}\n      related: {{ type: array, items: {{ type: string }} }}\n{links}---\n"));
    }
    for record in fixture["records"].as_array().unwrap() {
        write(
            &root,
            record["path"].as_str().unwrap(),
            &format!(
                "---\n{}---\n",
                serde_yaml::to_string(&record["frontmatter"]).unwrap()
            ),
        );
    }
    let collection = Collection::open(root.path()).unwrap();
    (root, collection, fixture)
}

fn path_expr(link: &str, options: &str) -> String {
    let call = format!("{}.asFile({options})", serde_json::to_string(link).unwrap());
    format!("{call} == null ? null : {call}.file.path")
}

#[test]
fn fixture_corpus_runs_on_the_real_engine_with_and_without_cache() {
    let (_root, collection, fixture) = fixture();
    let operations = collection.v03_operations().unwrap();
    let mut select = Vec::new();
    let mut expected = serde_json::Map::new();
    for (index, case) in fixture["cases"].as_array().unwrap().iter().enumerate() {
        for (mode, options) in [
            ("native", ""),
            ("unique", "{\"ambiguity\":\"unique\"}"),
            ("source_type", "{\"types\":[\"SOURCE\"]}"),
        ] {
            let name = format!("case_{index}_{mode}");
            select.push(
                json!({"name":name,"expr":path_expr(case["link"].as_str().unwrap(), options)}),
            );
            expected.insert(name, case[mode].clone());
        }
    }
    let request = json!({"where":"file.path == 'annotations/origin.md'", "select":select});
    for cached in [false, true] {
        if cached {
            assert_eq!(collection.cache_rebuild()["success"], true);
        }
        let (result, profile) = operations.query_profiled(&request);
        assert!(result.valid, "{result:#?}");
        assert!(result.diagnostics.is_empty(), "{:#?}", result.diagnostics);
        assert_eq!(profile.cache_used, cached);
        let values = &result.result["results"][0]["values"];
        for (index, case) in fixture["cases"].as_array().unwrap().iter().enumerate() {
            for mode in ["native", "unique", "source_type"] {
                let key = format!("case_{index}_{mode}");
                assert_eq!(
                    values[&key], expected[&key],
                    "{} ({mode}, cached={cached})",
                    case["name"]
                );
            }
        }
    }
}

#[test]
fn option_queries_re_resolve_stored_links_intersect_types_and_keep_provenance() {
    let (root, collection, _) = fixture();
    write(&root, "annotations/scoped.md", "---\ntype: annotation\nsource: '[[book]]'\nblocked: '[[notes/book]]'\nrelated: ['[[book]]']\n---\n");
    let operations = collection.v03_operations().unwrap();
    let request = json!({
        "where":"file.path == 'annotations/scoped.md'",
        "context":{"this":{"path":"annotations/origin.md"}},
        "select":[
            {"name":"default", "expr":"source.asFile().file.path"},
            {"name":"unique", "expr":"source.asFile({'ambiguity':'unique'}).file.path"},
            {"name":"intersection", "expr":"source.asFile({'types':['note']})"},
            {"name":"explicit_intersection", "expr":"source.asFile('notes/book.md', {'types':['note']})"},
            {"name":"explicit_list_intersection", "expr":"related.exists(l, l.asFile('notes/book.md', {'types':['note']}) != null)"},
            {"name":"blocked", "expr":"blocked.asFile({'types':['source']})"},
            {"name":"list", "expr":"related.exists(l, l.asFile({'ambiguity':'unique','types':['source']}).file.path == 'sources/book.md')"},
            {"name":"traversed", "expr":"source.asFile({'types':['source']}).lead.asFile({'ambiguity':'unique'}).file.path"},
            {"name":"context", "expr":"this.related.exists(l, l.asFile({'types':['source']}).file.path == 'sources/book.md')"},
            {"name":"explicit", "expr":"'[[./book]]'.asFile('sources/companion.md', {'ambiguity':'unique'}).file.path"}
        ]
    });
    for cached in [false, true] {
        if cached {
            assert_eq!(collection.cache_rebuild()["success"], true);
        }
        let result = operations.query(&request);
        assert!(result.valid && result.diagnostics.is_empty(), "{result:#?}");
        assert_eq!(
            result.result["results"][0]["values"],
            json!({
                "default":"sources/book.md", "unique":"sources/book.md", "intersection":null,
                "explicit_intersection":null, "explicit_list_intersection":false,
                "blocked":null, "list":true, "traversed":"sources/companion.md", "context":true,
                "explicit":"sources/book.md"
            })
        );
    }
}

#[test]
fn untyped_copies_and_source_overrides_cannot_erase_declared_policy_constraints() {
    let (root, collection, _) = fixture();
    write(
        &root,
        "annotations/copied.md",
        "---\ntype: annotation\ncopy: '[[book]]'\nsource: '[[book]]'\n---\n",
    );
    let result = collection.v03_operations().unwrap().query(&json!({
        "where":"file.path == 'annotations/copied.md'",
        "select":[
            // Keep the baseline's frontmatter-first stored winner unchanged.
            {"name":"default", "expr":"source.asFile().file.path"},
            {"name":"unique", "expr":"source.asFile({'ambiguity':'unique'}).file.path"},
            {"name":"intersection", "expr":"source.asFile('notes/book.md', {'types':['note']})"},
        ]
    }));
    assert!(result.valid && result.diagnostics.is_empty(), "{result:#?}");
    assert_eq!(
        result.result["results"][0]["values"],
        json!({
            "default":"notes/book.md", "unique":"sources/book.md", "intersection":null
        })
    );
}

#[test]
fn conflicting_typed_fields_fail_closed_instead_of_widening_either_declaration() {
    let (root, _collection, _) = fixture();
    write(&root, "_types/conflicted.md", "---\nkind: mdbase.type\nname: conflicted\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    properties:\n      type: { const: conflicted }\n      source: { type: string }\n      note: { type: string }\ncollection:\n  links:\n    source: { target_type: source }\n    note: { target_type: note }\n---\n");
    write(
        &root,
        "annotations/conflict.md",
        "---\ntype: conflicted\nsource: '[[book]]'\nnote: '[[book]]'\n---\n",
    );
    let collection = Collection::open(root.path()).unwrap();
    let operations = collection.v03_operations().unwrap();
    let default = operations.evaluate_cel(
        &json!({"path":"annotations/conflict.md", "expression":"source.asFile().file.path"}),
    );
    assert!(default.diagnostics.is_empty());
    assert_eq!(default.result["value"], "notes/book.md");
    for expression in [
        "source.asFile({'types':['note']})",
        "note.asFile({'types':['source']})",
    ] {
        let result = operations
            .evaluate_cel(&json!({"path":"annotations/conflict.md", "expression":expression}));
        assert!(
            result.diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("link_resolution_field_context_required")),
            "{result:#?}"
        );
    }
}

#[test]
fn invalid_options_and_invalid_links_are_diagnostics_not_missing_targets() {
    let (_root, collection, _) = fixture();
    let operations = collection.v03_operations().unwrap();
    // No-option diagnostics/missing behavior stays at the compatibility baseline.
    for expression in ["'[[../../escape]]'.asFile()", "'[[broken'.asFile()"] {
        let result = operations
            .evaluate_cel(&json!({"path":"annotations/origin.md", "expression":expression}));
        assert!(result.valid && result.diagnostics.is_empty(), "{result:#?}");
        assert_eq!(result.result["value"], Value::Null);
    }
    for expression in [
        "source.asFile({'scope':'sources'})",
        "source.asFile({'ambiguity':'first'})",
        "source.asFile({'types':[]})",
        "source.asFile({'types':[1]})",
        "source.asFile({'types':['not registered']})",
        "source.asFile({'types':['unknown']})",
        "'[[../../escape]]'.asFile({})",
        "'[[broken'.asFile({})",
        "'C:/escape.md'.asFile({})",
        "'[[sources/book]] [[sources/companion]]'.asFile({})",
        "source.asFile('annotations/origin.md', 'annotations/origin.md')",
    ] {
        let result = operations
            .evaluate_cel(&json!({"path":"annotations/origin.md", "expression":expression}));
        assert!(!result.diagnostics.is_empty(), "{expression}: {result:#?}");
        assert_eq!(result.result["value"], Value::Null, "{expression}");
    }
}

// Copied expression construction contract from Connect's query-predicates.ts.
// This is not a resolver: all semantics below execute in the actual Rust engine.
fn links_to(field: &str, path: &str, multiple: bool) -> String {
    let key = serde_json::to_string(field).unwrap();
    let value = format!("record[{key}]");
    let target = serde_json::to_string(path).unwrap();
    let matches = |link: &str| {
        format!(
            "{link} != null && {link}.asFile() != null && {link}.asFile().file.path == {target}"
        )
    };
    let condition = if multiple {
        format!(
            "{value} != null && {value}.exists(link, {})",
            matches("link")
        )
    } else {
        matches(&value)
    };
    format!("{key} in record && {condition}")
}

#[test]
fn connect_links_to_scalar_and_list_shapes_evaluate_relative_bare_and_aliased_links() {
    let (root, collection, _) = fixture();
    // Raw simple strings use the native key lookup, so make book unambiguous.
    fs::remove_file(root.path().join("notes/book.md")).unwrap();
    let links = [
        "[[sources/book|Title]]",
        "[[../sources/book]]",
        "[[book]]",
        "../sources/book.md",
        "book",
    ];
    for (index, link) in links.iter().enumerate() {
        // Includes declared scalar/list link fields and raw simple key values.
        write(&root, &format!("annotations/predicate-{index}.md"), &format!("---\ntype: annotation\nsource: '{link}'\nrelated: [null, '[[missing]]', '{link}']\n---\n"));
    }
    for (name, fields) in [
        ("missing", ""),
        ("null", "source: null\nrelated: null\n"),
        (
            "unresolved",
            "source: '[[missing]]'\nrelated: ['[[missing]]']\n",
        ),
    ] {
        write(
            &root,
            &format!("annotations/predicate-{name}.md"),
            &format!("---\ntype: annotation\n{fields}---\n"),
        );
    }
    let scalar = links_to("source", "sources/book.md", false);
    assert_eq!(scalar, "\"source\" in record && record[\"source\"] != null && record[\"source\"].asFile() != null && record[\"source\"].asFile().file.path == \"sources/book.md\"");
    let operations = collection.v03_operations().unwrap();
    for cached in [false, true] {
        if cached {
            assert_eq!(collection.cache_rebuild()["success"], true);
        }
        for predicate in [&scalar, &links_to("related", "sources/book.md", true)] {
            let result = operations.query(&json!({
                "types":["annotation"],
                "where":format!("file.path.startsWith('annotations/predicate-') && ({predicate})"),
                "order_by":[{"field":"file.path","direction":"asc"}]
            }));
            assert!(result.valid && result.diagnostics.is_empty(), "{result:#?}");
            let paths = result.result["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["file"]["path"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                paths,
                (0..links.len())
                    .map(|index| format!("annotations/predicate-{index}.md"))
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn policy_candidates_refresh_after_inventory_and_catalog_changes() {
    let (root, collection, _) = fixture();
    let expression = path_expr("[[duplicate]]", "{'ambiguity':'unique','types':['source']}");
    let run = |collection: &Collection| {
        let result = collection.v03_operations().unwrap().query(&json!({
            "where":"file.path == 'annotations/origin.md'",
            "select":[{"name":"target", "expr":expression}]
        }));
        assert!(result.valid && result.diagnostics.is_empty(), "{result:#?}");
        result.result["results"][0]["values"]["target"].clone()
    };
    assert_eq!(collection.cache_rebuild()["success"], true);
    assert_eq!(run(&collection), Value::Null);
    fs::remove_file(root.path().join("other/duplicate.md")).unwrap();
    assert_eq!(run(&collection), json!("sources/duplicate.md"));
    fs::rename(
        root.path().join("sources/duplicate.md"),
        root.path().join("sources/renamed.md"),
    )
    .unwrap();
    assert_eq!(run(&collection), Value::Null);
    // Reopening a changed type catalog must not reuse the previous eligible universe.
    write(
        &root,
        "other/duplicate.md",
        "---\ntitle: Unclassified\n---\n",
    );
    assert_eq!(run(&collection), Value::Null);
    write(&root, "_types/source.md", "---\nkind: mdbase.type\nname: source\nmatch:\n  path_glob: other/*.md\nschema:\n  dialect: json-schema-2020-12\n  value: { type: object }\n---\n");
    let reopened = Collection::open(root.path()).unwrap();
    assert_eq!(run(&reopened), json!("other/duplicate.md"));
}

#[test]
fn writer_js_probe_targets_are_recorded_without_claiming_js_cel_support() {
    let (root, collection, fixture) = fixture();
    // Match Writer's inventory/source provenance exactly: no same-folder duplicate.
    fs::remove_file(root.path().join("annotations/duplicate.md")).unwrap();
    let probe = &fixture["writer_probe"];
    let operations = collection.v03_operations().unwrap();
    for (link, target) in probe["links"]
        .as_array()
        .unwrap()
        .iter()
        .zip(probe["native_targets"].as_array().unwrap())
    {
        let evaluated = operations.evaluate_cel(&json!({"path":"annotations/origin.md", "expression":path_expr(link.as_str().unwrap(), "")}));
        assert!(evaluated.valid, "{evaluated:#?}");
        assert_eq!(&evaluated.result["value"], target);
    }
    assert!(probe["producer"].as_str().unwrap().contains("JavaScript"));
}
