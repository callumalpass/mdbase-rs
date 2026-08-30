#![cfg(feature = "legacy-collection-mutation")]

use mdbase::Collection;
use serde_json::json;
use std::fs;

fn assert_stable_id_links(id: &str, link_target: &str) {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.2.1\nsettings:\n  rename_update_refs: true\n  id_field: id\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(
        root.path().join("_types/item.md"),
        "---\nname: item\nfields: {}\n---\n",
    )
    .unwrap();
    fs::write(
        root.path().join("_types/source.md"),
        "---\nname: source\nfields:\n  typed: { type: link, target: item }\n  typed_alias: { type: link, target: item }\n  typed_anchor: { type: link, target: item }\n---\n",
    )
    .unwrap();
    let from = format!("{id}.md");
    fs::write(
        root.path().join(&from),
        format!("---\ntype: item\nid: {id}\n---\ntarget\n"),
    )
    .unwrap();
    let original = format!(
        "---\ntype: source\nuntyped: '[[{link_target}]]'\nuntyped_alias: '[[{link_target}|front alias]]'\nuntyped_anchor: '[[{link_target}#front|front anchor]]'\ntyped: '[[{link_target}]]'\ntyped_alias: '[[{link_target}|typed alias]]'\ntyped_anchor: '[[{link_target}#typed|typed anchor]]'\nexplicit: '[[./{id}#explicit|path alias]]'\n---\n[[{link_target}]] [[{link_target}|body alias]] [[{link_target}#body|body anchor]] ![[{link_target}#embed|embed alias]] [[./{id}#body-path|path alias]]\n"
    );
    fs::write(root.path().join("ref.md"), &original).unwrap();

    let collection = Collection::open(root.path()).unwrap();
    let result = collection.rename(&json!({
        "from": from,
        "to": "renamed.md",
        "update_refs": true,
    }));
    assert!(result.get("error").is_none(), "{result:#}");
    let rewritten = fs::read_to_string(root.path().join("ref.md")).unwrap();
    let (_, document) = rewritten.split_once("---\n").unwrap();
    let (yaml, body) = document.split_once("---\n").unwrap();
    let frontmatter: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
    for (field, expected) in [
        ("untyped", format!("[[{link_target}]]")),
        ("untyped_alias", format!("[[{link_target}|front alias]]")),
        (
            "untyped_anchor",
            format!("[[{link_target}#front|front anchor]]"),
        ),
        ("typed", format!("[[{link_target}]]")),
        ("typed_alias", format!("[[{link_target}|typed alias]]")),
        (
            "typed_anchor",
            format!("[[{link_target}#typed|typed anchor]]"),
        ),
        ("explicit", "[[renamed#explicit|path alias]]".to_string()),
    ] {
        assert_eq!(
            frontmatter[field].as_str(),
            Some(expected.as_str()),
            "{field}"
        );
    }
    assert_eq!(
        body,
        format!(
            "[[{link_target}]] [[{link_target}|body alias]] [[{link_target}#body|body anchor]] ![[{link_target}#embed|embed alias]] [[renamed#body-path|path alias]]\n"
        )
    );
}

#[test]
fn exact_case_configured_id_is_stable_in_every_rewrite_context() {
    assert_stable_id_links("stable-id", "stable-id");
}

#[test]
fn unicode_mixed_case_configured_id_is_stable_in_every_rewrite_context() {
    assert_stable_id_links("Ärende-ID", "äRENDE-id");
}
