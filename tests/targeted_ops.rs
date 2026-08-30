use std::fs;
use std::path::Path;

use tempfile::TempDir;

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, content).expect("write file");
}

fn open_collection(root: &Path) -> mdbase::Collection {
    mdbase::Collection::open(root).expect("open collection")
}

fn setup_minimal(root: &Path) {
    write_file(&root.join("mdbase.yaml"), "spec_version: 0.2.1\n");
}

#[test]
fn create_rejects_absolute_path() {
    let tmp = TempDir::new().expect("tempdir");
    setup_minimal(tmp.path());
    let collection = open_collection(tmp.path());

    let result = collection.create(&serde_json::json!({
        "path": "/etc/passwd",
        "fields": {"a": 1}
    }));

    assert_eq!(
        result.pointer("/error/code").and_then(|v| v.as_str()),
        Some("invalid_path")
    );
}

#[test]
fn create_rejects_traversal_path() {
    let tmp = TempDir::new().expect("tempdir");
    setup_minimal(tmp.path());
    let collection = open_collection(tmp.path());

    let result = collection.create(&serde_json::json!({
        "path": "../evil.md",
        "fields": {"a": 1}
    }));

    assert_eq!(
        result.pointer("/error/code").and_then(|v| v.as_str()),
        Some("invalid_path")
    );
}

#[test]
fn rename_rejects_absolute_target() {
    let tmp = TempDir::new().expect("tempdir");
    setup_minimal(tmp.path());
    write_file(&tmp.path().join("a.md"), "---\na: 1\n---\n");

    let collection = open_collection(tmp.path());
    let result = collection.rename(&serde_json::json!({
        "from": "a.md",
        "to": "/tmp/evil.md"
    }));

    assert_eq!(
        result.pointer("/error/code").and_then(|v| v.as_str()),
        Some("invalid_path")
    );
}

#[test]
fn rename_dry_run_reports_reference_impact_without_touching_files() {
    let tmp = TempDir::new().expect("tempdir");
    setup_minimal(tmp.path());
    write_file(&tmp.path().join("target.md"), "---\ntitle: Target\n---\n");
    write_file(
        &tmp.path().join("ref.md"),
        "---\ntitle: Referrer\n---\nLinks to [[target]].\n",
    );
    let before = fs::read_to_string(tmp.path().join("ref.md")).expect("read ref");
    let collection = open_collection(tmp.path());

    let preview = collection.rename(&serde_json::json!({
        "from": "target.md",
        "to": "Archive/renamed.md",
        "update_refs": true,
        "dry_run": true
    }));

    assert_eq!(preview.get("dry_run").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        preview.get("would_rename").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        preview
            .pointer("/references_affected/0/path")
            .and_then(|v| v.as_str()),
        Some("ref.md")
    );
    assert!(tmp.path().join("target.md").exists());
    assert!(!tmp.path().join("Archive").exists());
    assert_eq!(
        fs::read_to_string(tmp.path().join("ref.md")).expect("read ref"),
        before
    );
}

#[test]
fn formula_literal_string_does_not_create_dependency() {
    let tmp = TempDir::new().expect("tempdir");
    setup_minimal(tmp.path());
    write_file(&tmp.path().join("note.md"), "---\na: 1\n---\n");

    let collection = open_collection(tmp.path());
    let result = collection.query(&serde_json::json!({
        "query": {
            "formulas": {
                "a": "\"formula.b\"",
                "b": "1"
            }
        }
    }));

    assert!(result.get("error").is_none(), "unexpected error: {result}");
}

#[test]
fn formula_cycle_detected() {
    let tmp = TempDir::new().expect("tempdir");
    setup_minimal(tmp.path());
    write_file(&tmp.path().join("note.md"), "---\na: 1\n---\n");

    let collection = open_collection(tmp.path());
    let result = collection.query(&serde_json::json!({
        "query": {
            "formulas": {
                "a": "formula.b + 1",
                "b": "formula.a + 1"
            }
        }
    }));

    assert_eq!(
        result.pointer("/error/code").and_then(|v| v.as_str()),
        Some("circular_formula")
    );
}

#[test]
fn formula_index_reference_resolves() {
    let tmp = TempDir::new().expect("tempdir");
    setup_minimal(tmp.path());
    write_file(&tmp.path().join("note.md"), "---\na: 1\n---\n");

    let collection = open_collection(tmp.path());
    let result = collection.query(&serde_json::json!({
        "query": {
            "formulas": {
                "b": "1",
                "a": "formula[\"b\"] + 1"
            }
        }
    }));

    assert!(result.get("error").is_none(), "unexpected error: {result}");
    let a = result
        .pointer("/results/0/formulas/a")
        .and_then(|v| v.as_i64());
    assert_eq!(a, Some(2));
}

#[test]
fn list_item_errors_are_wrapped() {
    let tmp = TempDir::new().expect("tempdir");
    write_file(
        &tmp.path().join("mdbase.yaml"),
        "spec_version: 0.2.1\nsettings:\n  default_validation: error\n",
    );
    write_file(
        &tmp.path().join("_types/task.md"),
        "---\nname: task\nfields:\n  nums:\n    type: list\n    items:\n      type: integer\n---\n",
    );

    let collection = open_collection(tmp.path());
    let result = collection.create(&serde_json::json!({
        "path": "t.md",
        "type": "task",
        "fields": {"nums": [1, "x", 2]}
    }));

    assert_eq!(
        result.pointer("/error/code").and_then(|v| v.as_str()),
        Some("validation_failed")
    );
    let has_wrapped = result
        .pointer("/error/issues")
        .and_then(|v| v.as_array())
        .map(|issues| {
            issues
                .iter()
                .any(|i| i.get("code").and_then(|v| v.as_str()) == Some("list_item_invalid"))
        })
        .unwrap_or(false);
    assert!(has_wrapped, "expected list_item_invalid issue: {result}");
}

#[test]
fn strict_unknown_field_fails_even_at_warn_level() {
    let tmp = TempDir::new().expect("tempdir");
    write_file(
        &tmp.path().join("mdbase.yaml"),
        "spec_version: 0.2.1\nsettings:\n  default_validation: warn\n",
    );
    write_file(
        &tmp.path().join("_types/task.md"),
        "---\nname: task\nstrict: true\nfields:\n  title:\n    type: string\n---\n",
    );

    let collection = open_collection(tmp.path());
    let result = collection.create(&serde_json::json!({
        "path": "t.md",
        "type": "task",
        "fields": {"title": "ok", "extra": 1}
    }));

    assert_eq!(
        result.pointer("/error/code").and_then(|v| v.as_str()),
        Some("validation_failed")
    );
}

#[test]
fn rename_updates_body_wikilink_outside_code_blocks() {
    let tmp = TempDir::new().expect("tempdir");
    write_file(
        &tmp.path().join("mdbase.yaml"),
        "spec_version: 0.2.1\nsettings:\n  rename_update_refs: true\n",
    );
    write_file(&tmp.path().join("target.md"), "---\nid: t\n---\n");
    write_file(&tmp.path().join("ref.md"), "Link [[target]]\n");

    let collection = open_collection(tmp.path());
    let result = collection.rename(&serde_json::json!({
        "from": "target.md",
        "to": "new-target.md"
    }));
    assert!(result.get("error").is_none(), "rename failed: {result}");

    let ref_body = fs::read_to_string(tmp.path().join("ref.md")).expect("read ref");
    assert!(
        ref_body.contains("[[new-target]]"),
        "body not updated: {ref_body}"
    );
}

#[test]
fn rename_updates_filename_markdown_links_images_and_case_folded_wikilinks() {
    let tmp = TempDir::new().expect("tempdir");
    write_file(
        &tmp.path().join("mdbase.yaml"),
        "spec_version: 0.2.1\nsettings:\n  rename_update_refs: true\n",
    );
    write_file(&tmp.path().join("Target.md"), "---\nid: t\n---\n");
    write_file(
        &tmp.path().join("ref.md"),
        "[link](Target.md#section) ![image](Target.md) [[target#anchor|Alias]]\n",
    );

    let collection = open_collection(tmp.path());
    let result = collection.rename(&serde_json::json!({
        "from": "Target.md",
        "to": "Renamed.md",
        "update_refs": true
    }));
    assert!(result.get("error").is_none(), "rename failed: {result}");
    assert_eq!(
        fs::read_to_string(tmp.path().join("ref.md")).unwrap(),
        "[link](./Renamed.md#section) ![image](./Renamed.md) [[Renamed#anchor|Alias]]\n"
    );
}

#[test]
fn rename_preserves_opaque_frontmatter_when_only_the_body_changes() {
    for frontmatter in ["title: [broken", "- one\n- two", "null", "scalar"] {
        let tmp = TempDir::new().expect("tempdir");
        write_file(
            &tmp.path().join("mdbase.yaml"),
            "spec_version: 0.2.1\nsettings:\n  rename_update_refs: true\n",
        );
        write_file(&tmp.path().join("target.md"), "Target.\n");
        let original = format!("---\n{frontmatter}\n---\nSee [target](target.md).\n");
        write_file(&tmp.path().join("ref.md"), &original);

        let collection = open_collection(tmp.path());
        let result = collection.rename(&serde_json::json!({
            "from": "target.md",
            "to": "renamed.md",
            "update_refs": true
        }));
        assert!(result.get("error").is_none(), "rename failed: {result}");
        assert_eq!(
            fs::read_to_string(tmp.path().join("ref.md")).unwrap(),
            format!("---\n{frontmatter}\n---\nSee [target](./renamed.md).\n")
        );
    }
}

#[test]
fn resolve_link_rejects_relative_syntax_that_crosses_the_root() {
    let tmp = TempDir::new().expect("tempdir");
    setup_minimal(tmp.path());
    write_file(&tmp.path().join("inside.md"), "Inside.\n");
    write_file(
        &tmp.path().join("tasks/source.md"),
        "---\nrelated: '[[../../inside]]'\n---\n",
    );

    let collection = open_collection(tmp.path());
    let resolved = collection.resolve_link(&serde_json::json!({
        "path": "tasks/source.md",
        "field": "related"
    }));
    assert_eq!(resolved["error"]["code"], "path_traversal", "{resolved}");

    write_file(
        &tmp.path().join("source.md"),
        "---\nrelated: '[Inside](a/../../inside)'\n---\n",
    );
    let resolved = collection.resolve_link(&serde_json::json!({
        "path": "source.md",
        "field": "related"
    }));
    assert_eq!(resolved["error"]["code"], "path_traversal", "{resolved}");
}

#[test]
fn duplicate_filenames_resolve_deterministically_and_drive_backlinks() {
    let tmp = TempDir::new().expect("tempdir");
    setup_minimal(tmp.path());
    write_file(&tmp.path().join("one/duplicate.md"), "One.\n");
    write_file(&tmp.path().join("two/duplicate.md"), "Two.\n");
    write_file(
        &tmp.path().join("source.md"),
        "---\nrelated: '[[duplicate]]'\n---\nSee [[duplicate]].\n",
    );

    let collection = open_collection(tmp.path());
    let resolved = collection.resolve_link(&serde_json::json!({
        "path": "source.md",
        "field": "related"
    }));
    assert_eq!(resolved["resolved_path"], "one/duplicate.md", "{resolved}");
    let deleted = collection.delete(&serde_json::json!({
        "path": "one/duplicate.md",
        "check_backlinks": true,
        "dry_run": true
    }));
    assert_eq!(deleted["would_delete"], true, "{deleted}");
    assert!(
        deleted["broken_links"]
            .as_array()
            .is_some_and(|links| !links.is_empty()),
        "{deleted}"
    );
}

#[test]
fn duplicate_configured_ids_remain_ambiguous() {
    let tmp = TempDir::new().expect("tempdir");
    setup_minimal(tmp.path());
    write_file(&tmp.path().join("one/a.md"), "---\nid: duplicate-id\n---\n");
    write_file(&tmp.path().join("two/b.md"), "---\nid: duplicate-id\n---\n");
    write_file(
        &tmp.path().join("source.md"),
        "---\nrelated: '[[duplicate-id]]'\n---\n",
    );

    let collection = open_collection(tmp.path());
    let resolved = collection.resolve_link(&serde_json::json!({
        "path": "source.md",
        "field": "related"
    }));
    assert!(resolved["resolved_path"].is_null(), "{resolved}");
}

#[test]
fn rename_does_not_update_links_inside_code_blocks() {
    let tmp = TempDir::new().expect("tempdir");
    write_file(
        &tmp.path().join("mdbase.yaml"),
        "spec_version: 0.2.1\nsettings:\n  rename_update_refs: true\n",
    );
    write_file(&tmp.path().join("target.md"), "---\nid: t\n---\n");
    write_file(
        &tmp.path().join("ref.md"),
        "```md\n[[target]]\n```\noutside [[target]]\n",
    );

    let collection = open_collection(tmp.path());
    let result = collection.rename(&serde_json::json!({
        "from": "target.md",
        "to": "new-target.md"
    }));
    assert!(result.get("error").is_none(), "rename failed: {result}");

    let ref_body = fs::read_to_string(tmp.path().join("ref.md")).expect("read ref");
    assert!(
        ref_body.contains("```md\n[[target]]\n```"),
        "code block was modified: {ref_body}"
    );
    assert!(
        ref_body.contains("outside [[new-target]]"),
        "non-code link not updated: {ref_body}"
    );
}

#[test]
fn uniqueness_check_reports_duplicate_id_on_update() {
    let tmp = TempDir::new().expect("tempdir");
    write_file(
        &tmp.path().join("mdbase.yaml"),
        "spec_version: 0.2.1\nsettings:\n  default_validation: error\n",
    );
    write_file(
        &tmp.path().join("_types/task.md"),
        "---\nname: task\nfields:\n  id:\n    type: string\n---\n",
    );
    write_file(&tmp.path().join("a.md"), "---\ntype: task\nid: same\n---\n");
    write_file(
        &tmp.path().join("b.md"),
        "---\ntype: task\nid: other\n---\n",
    );

    let collection = open_collection(tmp.path());
    let result = collection.update(&serde_json::json!({
        "path": "b.md",
        "fields": {"id": "same"}
    }));

    assert_eq!(
        result.pointer("/error/code").and_then(|v| v.as_str()),
        Some("validation_failed")
    );
    let has_duplicate_id = result
        .pointer("/error/issues")
        .and_then(|v| v.as_array())
        .map(|issues| {
            issues
                .iter()
                .any(|i| i.get("code").and_then(|v| v.as_str()) == Some("duplicate_id"))
        })
        .unwrap_or(false);
    assert!(has_duplicate_id, "missing duplicate_id issue: {result}");
}

#[test]
fn delete_with_backlink_check_reports_referrers() {
    let tmp = TempDir::new().expect("tempdir");
    setup_minimal(tmp.path());
    write_file(&tmp.path().join("a.md"), "---\nid: a\n---\n");
    write_file(&tmp.path().join("b.md"), "---\nref: \"[[a]]\"\n---\n");

    let collection = open_collection(tmp.path());
    let result = collection.delete(&serde_json::json!({
        "path": "a.md",
        "check_backlinks": true
    }));

    assert_eq!(result.get("deleted").and_then(|v| v.as_bool()), Some(true));
    let broken = result
        .pointer("/broken_links/0/path")
        .and_then(|v| v.as_str());
    assert_eq!(broken, Some("b.md"));
}

#[test]
fn delete_dry_run_reports_backlinks_without_removing_the_file() {
    let tmp = TempDir::new().expect("tempdir");
    setup_minimal(tmp.path());
    write_file(&tmp.path().join("a.md"), "---\nid: a\n---\n");
    write_file(&tmp.path().join("b.md"), "---\nref: \"[[a]]\"\n---\n");

    let collection = open_collection(tmp.path());
    let result = collection.delete(&serde_json::json!({
        "path": "a.md",
        "check_backlinks": true,
        "dry_run": true
    }));

    assert_eq!(result.get("deleted").and_then(|v| v.as_bool()), Some(false));
    assert_eq!(result.get("dry_run").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        result.get("would_delete").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        result
            .pointer("/broken_links/0/path")
            .and_then(|v| v.as_str()),
        Some("b.md")
    );
    assert!(tmp.path().join("a.md").exists());
}

#[test]
fn query_types_works_after_identity_bound_cache_rebuild() {
    let tmp = TempDir::new().expect("tempdir");
    write_file(
        &tmp.path().join("mdbase.yaml"),
        "spec_version: 0.2.1\nsettings:\n  explicit_type_keys: []\n",
    );
    write_file(
        &tmp.path().join("_types/person.md"),
        "---\nname: person\nmatch:\n  where:\n    tags:\n      contains: person\nfields:\n  tags:\n    type: list\n    items:\n      type: string\n---\n",
    );
    write_file(&tmp.path().join("alice.md"), "---\ntags: [person]\n---\n");

    let collection = open_collection(tmp.path());
    let rebuild = collection.cache_rebuild();
    assert_eq!(rebuild.get("success").and_then(|v| v.as_bool()), Some(true));

    let result = collection.query(&serde_json::json!({
        "query": {
            "types": ["person"]
        }
    }));

    assert!(result.get("error").is_none(), "query failed: {result}");
    assert_eq!(
        result.pointer("/meta/total_count").and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        result.pointer("/results/0/path").and_then(|v| v.as_str()),
        Some("alice.md")
    );
}
