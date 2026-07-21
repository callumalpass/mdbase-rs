use std::fs;

use mdbase::Collection;
use regex::Regex;
use serde_json::json;
use tempfile::TempDir;

fn collection(type_files: &[(&str, &str)], files: &[(&str, &str)]) -> (TempDir, Collection) {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("mdbase.yaml"),
        "spec_version: \"0.3.0\"\nsettings:\n  validation: error\n",
    )
    .unwrap();
    fs::create_dir(directory.path().join("_types")).unwrap();
    for (path, content) in type_files {
        fs::write(directory.path().join("_types").join(path), content).unwrap();
    }
    for (path, content) in files {
        let target = directory.path().join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, content).unwrap();
    }
    let loaded = Collection::open(directory.path()).unwrap_or_else(|error| panic!("{error:#}"));
    (directory, loaded)
}

const PROVIDER_TYPE: &str = r#"---
kind: mdbase.type
name: generated
version: 1
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    required: [type, source, copied, slug, uuid, stamp, createdAt, date]
    properties:
      type: { const: generated }
      source: { type: string }
      copied: { type: string }
      slug: { type: string }
      uuid: { type: string }
      stamp: {}
      createdAt: { type: string, format: date-time }
      modifiedAt: { type: string, format: date-time }
      date: { type: string, format: date }
lifecycle:
  on_create:
    set:
      copied: { copy: source }
      slug: { slugify: source }
      uuid: { uuid: true }
      stamp: { literal: { source: lifecycle, version: 1 } }
      createdAt: { now: true }
      modifiedAt: { now: true }
      date: { today: true }
  on_update:
    set:
      modifiedAt: { now: true }
---
"#;

#[test]
fn all_standard_value_providers_are_persisted_before_validation() {
    let (directory, collection) = collection(&[("generated.md", PROVIDER_TYPE)], &[]);
    let operations = collection.v03_operations().unwrap();
    let result = operations.create(&json!({
        "type": "generated",
        "path": "records/one.md",
        "frontmatter": {"type": "generated", "source": "Hello, World!"},
    }));
    assert!(result.valid, "{result:#?}");
    let frontmatter = result.result["frontmatter"].as_object().unwrap();
    assert_eq!(frontmatter["copied"], "Hello, World!");
    assert_eq!(frontmatter["slug"], "hello-world");
    assert_eq!(
        frontmatter["stamp"],
        json!({"source": "lifecycle", "version": 1})
    );
    assert!(
        Regex::new("^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$")
            .unwrap()
            .is_match(frontmatter["uuid"].as_str().unwrap())
    );
    assert!(
        chrono::DateTime::parse_from_rfc3339(frontmatter["createdAt"].as_str().unwrap()).is_ok()
    );
    assert!(
        chrono::NaiveDate::parse_from_str(frontmatter["date"].as_str().unwrap(), "%Y-%m-%d")
            .is_ok()
    );
    assert_eq!(frontmatter["createdAt"], frontmatter["modifiedAt"]);

    let persisted = fs::read_to_string(directory.path().join("records/one.md")).unwrap();
    assert!(persisted.contains("copied: Hello, World!"));
    assert!(persisted.contains("source: lifecycle"));
}

#[test]
fn update_lifecycle_preserves_create_values_and_refreshes_managed_values() {
    let (_directory, collection) = collection(
        &[("generated.md", PROVIDER_TYPE)],
        &[(
            "record.md",
            "---\ntype: generated\nsource: Before\ncopied: Before\nslug: before\nuuid: 67dcfd25-4ec7-4227-8ce5-1570984995f0\nstamp: {source: lifecycle, version: 1}\ncreatedAt: 2020-01-01T00:00:00Z\nmodifiedAt: 2020-01-01T00:00:00Z\ndate: 2020-01-01\n---\n",
        )],
    );
    let result = collection
        .v03_operations()
        .unwrap()
        .update(&json!({"path": "record.md", "patch": {"source": "After"}}));
    assert!(result.valid, "{result:#?}");
    assert_eq!(
        result.result["frontmatter"]["createdAt"],
        "2020-01-01T00:00:00Z"
    );
    assert_ne!(
        result.result["frontmatter"]["modifiedAt"],
        "2020-01-01T00:00:00Z"
    );
    assert_eq!(result.result["frontmatter"]["copied"], "Before");
}

#[test]
fn lifecycle_membership_changes_fail_without_writing() {
    let first = r#"---
kind: mdbase.type
name: first
version: 1
match: { fields_present: [title] }
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    properties: { title: {type: string}, marker: {type: string} }
lifecycle:
  on_create:
    set: { marker: { literal: added } }
---
"#;
    let second = r#"---
kind: mdbase.type
name: second
version: 1
match: { fields_present: [marker] }
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    properties: { marker: {type: string} }
---
"#;
    let (directory, collection) = collection(&[("first.md", first), ("second.md", second)], &[]);
    let result = collection.v03_operations().unwrap().create(&json!({
        "path": "record.md",
        "frontmatter": {"title": "Membership"},
    }));
    assert!(!result.valid, "{result:#?}");
    assert_eq!(result.diagnostics[0].code, "type_membership_changed");
    assert!(!directory.path().join("record.md").exists());
}

#[test]
fn invalid_and_failing_guards_never_mutate_records() {
    let invalid = PROVIDER_TYPE.replace(
        "  on_update:\n    set:\n      modifiedAt: { now: true }",
        "  on_update:\n    - if: '('\n      set:\n        modifiedAt: { now: true }",
    );
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("mdbase.yaml"),
        "spec_version: \"0.3.0\"\n",
    )
    .unwrap();
    fs::create_dir(directory.path().join("_types")).unwrap();
    fs::write(directory.path().join("_types/generated.md"), invalid).unwrap();
    assert!(Collection::open(directory.path()).is_err());

    let failing = PROVIDER_TYPE.replace(
        "  on_update:\n    set:\n      modifiedAt: { now: true }",
        "  on_update:\n    - if: 'source + 1'\n      set:\n        modifiedAt: { now: true }",
    );
    let (directory, collection) = collection(
        &[("generated.md", &failing)],
        &[(
            "record.md",
            "---\ntype: generated\nsource: Before\ncopied: Before\nslug: before\nuuid: 67dcfd25-4ec7-4227-8ce5-1570984995f0\nstamp: {source: lifecycle, version: 1}\ncreatedAt: 2020-01-01T00:00:00Z\nmodifiedAt: 2020-01-01T00:00:00Z\ndate: 2020-01-01\n---\n",
        )],
    );
    let before = fs::read_to_string(directory.path().join("record.md")).unwrap();
    let result = collection
        .v03_operations()
        .unwrap()
        .update(&json!({"path": "record.md", "patch": {"source": "After"}}));
    assert!(!result.valid, "{result:#?}");
    assert_eq!(result.diagnostics[0].code, "lifecycle_expression_error");
    assert_eq!(
        fs::read_to_string(directory.path().join("record.md")).unwrap(),
        before
    );
}

#[test]
fn v03_updates_preserve_explicit_nulls() {
    let simple = r#"---
kind: mdbase.type
name: simple
version: 1
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    properties: { type: {const: simple}, optional: {} }
---
"#;
    let (directory, collection) = collection(
        &[("simple.md", simple)],
        &[("record.md", "---\ntype: simple\noptional: value\n---\n")],
    );
    let result = collection
        .v03_operations()
        .unwrap()
        .update(&json!({"path": "record.md", "patch": {"optional": null}}));
    assert!(result.valid, "{result:#?}");
    assert!(result.result["frontmatter"]["optional"].is_null());
    assert!(fs::read_to_string(directory.path().join("record.md"))
        .unwrap()
        .contains("optional: null"));
}
