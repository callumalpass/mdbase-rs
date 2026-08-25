//! Regression tests for BOM-prefixed frontmatter documents.
//!
//! Finding: `.ops/work/bom-prefixed-records-lose-frontmatter-on-update.md`.
//!
//! A leading UTF-8 BOM (`U+FEFF`) must be transparent to frontmatter parsing:
//! a BOM'd document parses as frontmatter + body, and a patch-only update
//! round-trips with exactly ONE frontmatter block. The chosen write policy is
//! BOM preservation: if the original content began with a BOM, the serialized
//! output begins with the same BOM so external tools see minimal byte diffs.

use std::fs;
use std::path::Path;

use tempfile::TempDir;

const BOM: &str = "\u{FEFF}";

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, content).expect("write file");
}

fn setup_collection(root: &Path) -> mdbase::Collection {
    write_file(&root.join("mdbase.yaml"), "spec_version: 0.2.1\n");
    mdbase::Collection::open(root).expect("open collection")
}

/// Split raw document bytes into (frontmatter yaml text, body) for a document
/// that starts with an optional BOM followed by exactly one frontmatter block.
/// Returns None when the document does not start with a single frontmatter block.
fn split_single_frontmatter(raw: &str) -> Option<(&str, &str)> {
    let content = raw.strip_prefix(BOM).unwrap_or(raw);
    let rest = content.strip_prefix("---\n")?;
    let close = rest.find("\n---\n")?;
    let yaml = &rest[..close];
    let after = &rest[close + "\n---\n".len()..];
    // Exactly one block: no further delimiter line anywhere in the remainder.
    let body = after.strip_prefix('\n').unwrap_or(after);
    assert!(
        !body.lines().any(|line| line.trim_end() == "---"),
        "expected exactly one frontmatter block, found another '---' line"
    );
    Some((yaml, body))
}

#[test]
fn bom_prefixed_frontmatter_parses_title_fields_and_body() {
    let source = format!("{BOM}---\ntitle: Original\nstatus: draft\n---\n\nBody paragraph.\n");
    let parsed = mdbase::frontmatter::parser::parse_document(&source);

    assert!(
        parsed.has_frontmatter,
        "BOM'd frontmatter must not be body-only"
    );
    let fm = parsed.frontmatter.expect("frontmatter present");
    let mapping = fm.as_mapping().expect("frontmatter is a mapping");
    let title = mapping
        .get(&serde_yaml::Value::String("title".into()))
        .and_then(|v| v.as_str());
    assert_eq!(title, Some("Original"));
    let status = mapping
        .get(&serde_yaml::Value::String("status".into()))
        .and_then(|v| v.as_str());
    assert_eq!(status, Some("draft"));
    assert_eq!(parsed.body, "\nBody paragraph.\n");
}

#[test]
fn read_classifies_bom_record_frontmatter_not_body() {
    let tmp = TempDir::new().expect("tempdir");
    let collection = setup_collection(tmp.path());
    write_file(
        &tmp.path().join("notes/bom.md"),
        &format!("{BOM}---\ntitle: Original\nstatus: draft\n---\n\nBody paragraph.\n"),
    );

    let result = collection.read(&serde_json::json!({"path": "notes/bom.md"}));
    assert!(result.get("error").is_none(), "read failed: {result:?}");
    assert_eq!(
        result
            .pointer("/frontmatter/title")
            .and_then(|v| v.as_str()),
        Some("Original"),
        "BOM'd frontmatter must surface through the read API"
    );
    assert_eq!(
        result.pointer("/body").and_then(|v| v.as_str()),
        Some("\nBody paragraph.\n"),
        "body must exclude the frontmatter block"
    );
}

/// Chosen BOM-write policy, documented by test: preserve the original BOM.
/// A patch-only update on a BOM'd record writes the patched frontmatter under
/// the original leading BOM, keeps the body byte-for-byte, and never demotes
/// the original frontmatter into the body.
#[test]
fn patch_update_preserves_bom_with_exactly_one_frontmatter_block() {
    let tmp = TempDir::new().expect("tempdir");
    let collection = setup_collection(tmp.path());
    let original_body = "\nBody paragraph.\n";
    write_file(
        &tmp.path().join("notes/bom.md"),
        &format!("{BOM}---\ntitle: Original\nstatus: draft\n---{original_body}"),
    );

    let result = collection.update(&serde_json::json!({
        "path": "notes/bom.md",
        "fields": {"title": "Updated", "status": "final"},
    }));
    assert!(result.get("error").is_none(), "update failed: {result:?}");

    let raw = fs::read_to_string(tmp.path().join("notes/bom.md")).expect("read raw bytes");
    assert!(
        raw.starts_with(BOM),
        "serialized output must keep the BOM prefix"
    );

    let (yaml_text, body) =
        split_single_frontmatter(&raw).expect("exactly one frontmatter block expected");

    // The demoted-original corruption signature must never appear.
    assert_eq!(
        raw.matches(BOM).count(),
        1,
        "BOM may only appear at byte 0, never mid-file"
    );
    assert!(
        !yaml_text.contains("Original"),
        "old values must be replaced, not demoted"
    );

    // Patched fields landed in the single remaining block.
    let parsed = mdbase::frontmatter::parser::parse_document(&raw);
    let fm = parsed.frontmatter.expect("frontmatter present");
    let mapping = fm.as_mapping().expect("mapping");
    assert_eq!(
        mapping
            .get(&serde_yaml::Value::String("title".into()))
            .and_then(|v| v.as_str()),
        Some("Updated")
    );
    assert_eq!(
        mapping
            .get(&serde_yaml::Value::String("status".into()))
            .and_then(|v| v.as_str()),
        Some("final")
    );

    // Body preserved byte-for-byte; only the intended field change differs.
    assert_eq!(body, original_body.trim_start_matches('\n'));
}

#[test]
fn no_bom_control_round_trips_without_introducing_a_bom() {
    let tmp = TempDir::new().expect("tempdir");
    let collection = setup_collection(tmp.path());
    write_file(
        &tmp.path().join("notes/plain.md"),
        "---\ntitle: Original\nstatus: draft\n---\n\nBody paragraph.\n",
    );

    let result = collection.update(&serde_json::json!({
        "path": "notes/plain.md",
        "fields": {"title": "Updated", "status": "final"},
    }));
    assert!(result.get("error").is_none(), "update failed: {result:?}");

    let raw = fs::read_to_string(tmp.path().join("notes/plain.md")).expect("read raw bytes");
    assert!(!raw.contains(BOM), "control file must stay BOM-free");
    let (_, body) = split_single_frontmatter(&raw).expect("single frontmatter block");
    assert_eq!(body, "Body paragraph.\n");

    let parsed = mdbase::frontmatter::parser::parse_document(&raw);
    let fm = parsed.frontmatter.expect("frontmatter present");
    let mapping = fm.as_mapping().expect("mapping");
    assert_eq!(
        mapping
            .get(&serde_yaml::Value::String("title".into()))
            .and_then(|v| v.as_str()),
        Some("Updated")
    );
}

#[test]
fn body_starting_with_later_horizontal_rule_is_still_body_only() {
    // A "---" that is not on the very first line is ordinary Markdown,
    // with or without a BOM.
    for source in [
        "Intro\n\n---\nnot frontmatter\n",
        &format!("{BOM}Intro\n\n---\nnot frontmatter\n"),
    ] {
        let parsed = mdbase::frontmatter::parser::parse_document(source);
        assert!(
            !parsed.has_frontmatter,
            "later '---' must not become frontmatter"
        );
        assert!(parsed.frontmatter.is_none());
        let expected_body = source.strip_prefix(BOM).unwrap_or(source);
        assert_eq!(parsed.body, expected_body);
    }
}
