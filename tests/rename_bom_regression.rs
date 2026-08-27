use std::fs;
use std::path::Path;

use tempfile::TempDir;

const BOM: &str = "\u{feff}";

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn collection(root: &Path) -> mdbase::Collection {
    write(
        &root.join("mdbase.yaml"),
        "spec_version: 0.2.1\nsettings:\n  rename_update_refs: true\n",
    );
    mdbase::Collection::open(root).unwrap()
}

fn rename(root: &Path) -> serde_json::Value {
    collection(root).rename(&serde_json::json!({
        "from": "target.md", "to": "renamed.md", "update_refs": true
    }))
}

fn fixture(reference: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    write(&tmp.path().join("target.md"), "Target\n");
    write(&tmp.path().join("ref.md"), reference);
    tmp
}

fn assert_boms(raw: &str, expected: usize) {
    assert_eq!(raw.matches(BOM).count(), expected, "{raw:?}");
    assert_eq!(raw.starts_with(BOM), expected > 0, "{raw:?}");
}

#[test]
fn public_rename_preserves_bom_for_frontmatter_body_and_combined_rewrites() {
    for (input, expected_body) in [
        (
            format!("{BOM}---\nrelated: '[[target]]'\nkeep: yes\n---\nUntouched\n"),
            "Untouched\n",
        ),
        (
            format!("{BOM}---\nkeep: yes\n---\nSee [target](target.md).\n"),
            "See [target](./renamed.md).\n",
        ),
        (
            format!("{BOM}---\nrelated: '[[target]]'\nkeep: yes\n---\nSee [[target]].\n"),
            "See [[renamed]].\n",
        ),
    ] {
        let tmp = fixture(&input);
        let result = rename(tmp.path());
        assert!(result.get("error").is_none(), "{result}");
        let raw = fs::read_to_string(tmp.path().join("ref.md")).unwrap();
        assert_boms(&raw, 1);
        assert!(raw.contains("keep: yes"));
        assert!(!raw.contains("[[target]]"));
        assert!(raw.contains(expected_body));
    }
}

#[test]
fn body_only_rewrites_preserve_frontmatter_prefix_crlf_and_malformed_yaml() {
    for prefix in [
        format!("{BOM}---\r\nkeep: yes\r\n---\r\n"),
        format!("{BOM}---\ninvalid: [\n---\n"),
    ] {
        let input = format!("{prefix}See [target](target.md).\n");
        let tmp = fixture(&input);
        let result = rename(tmp.path());
        assert!(result.get("error").is_none(), "{result}");
        let raw = fs::read_to_string(tmp.path().join("ref.md")).unwrap();
        assert_boms(&raw, 1);
        assert!(raw.starts_with(&prefix), "prefix changed: {raw:?}");
        assert!(raw.ends_with("See [target](./renamed.md).\n"));
    }
}

#[test]
fn body_only_and_two_bom_documents_restore_all_original_markers() {
    for (input, count) in [
        (format!("{BOM}See [[target]].\n"), 1),
        (format!("{BOM}{BOM}See [[target]].\n"), 2),
    ] {
        let tmp = fixture(&input);
        let result = rename(tmp.path());
        assert!(result.get("error").is_none(), "{result}");
        let raw = fs::read_to_string(tmp.path().join("ref.md")).unwrap();
        assert_boms(&raw, count);
        assert!(raw.ends_with("See [[renamed]].\n"));
    }
}

#[test]
fn dry_run_noop_and_conflict_never_rewrite_reference_bytes() {
    let original = format!("{BOM}---\nkeep: yes\n---\nSee [[target]].\n");

    let dry = fixture(&original);
    let result = collection(dry.path()).rename(&serde_json::json!({
        "from": "target.md", "to": "renamed.md", "update_refs": true, "dry_run": true
    }));
    assert_eq!(result["dry_run"], true);
    assert_eq!(
        fs::read_to_string(dry.path().join("ref.md")).unwrap(),
        original
    );
    assert!(dry.path().join("target.md").exists());

    let noop = fixture(&format!("{BOM}No link here.\n"));
    let before = fs::read(noop.path().join("ref.md")).unwrap();
    assert!(rename(noop.path()).get("error").is_none());
    assert_eq!(fs::read(noop.path().join("ref.md")).unwrap(), before);

    let conflict = fixture(&original);
    write(&conflict.path().join("renamed.md"), "occupied\n");
    let result = rename(conflict.path());
    assert_eq!(
        result.pointer("/error/code").and_then(|v| v.as_str()),
        Some("path_conflict")
    );
    assert_eq!(
        fs::read_to_string(conflict.path().join("ref.md")).unwrap(),
        original
    );
    assert!(conflict.path().join("target.md").exists());
}

#[test]
fn concurrent_reference_refusal_does_not_rewrite_simulated_content() {
    let tmp = fixture(&format!("{BOM}See [[target]].\n"));
    let simulated = format!("{BOM}Externally changed; still [[target]].\n");
    let result = collection(tmp.path()).rename(&serde_json::json!({
        "from": "target.md",
        "to": "renamed.md",
        "update_refs": true,
        "simulate_before_ref_update": [{"path": "ref.md", "content": simulated}],
        "last_known_ref_mtimes": {"ref.md": 0}
    }));
    assert_eq!(
        result.pointer("/error/code").and_then(|v| v.as_str()),
        Some("rename_ref_update_failed")
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join("ref.md")).unwrap(),
        simulated
    );
}
