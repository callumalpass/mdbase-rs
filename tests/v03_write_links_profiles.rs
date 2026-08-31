use std::fs;

use mdbase::Collection;
use serde_json::json;
use tempfile::TempDir;

fn collection() -> (TempDir, Collection) {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  timezone: UTC\n  validation: error\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(
        root.path().join("_types/task.md"),
        r#"---
kind: mdbase.type
name: task
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    required: [type, title, status]
    properties:
      type: { const: task }
      title: { type: string }
      status: { type: string }
      priority: { type: integer }
      related: { type: string }
      assignee: { type: string }
collection:
  read_defaults:
    status: open
    priority: 1
  links:
    related:
      target_type: [project, person]
      validate_exists: true
    assignee:
      target_type: person
      validate_exists: true
---
"#,
    )
    .unwrap();
    for name in ["project", "person"] {
        fs::write(
            root.path().join(format!("_types/{name}.md")),
            format!(
                "---\nkind: mdbase.type\nname: {name}\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    properties:\n      type: {{ const: {name} }}\n      title: {{ type: string }}\n---\n"
            ),
        )
        .unwrap();
    }
    write_record(
        &root,
        "projects/alpha.md",
        "---\ntype: project\ntitle: Alpha\n---\n",
    );
    write_record(
        &root,
        "people/alice.md",
        "---\ntype: person\ntitle: Alice\n---\n",
    );
    let opened = Collection::open(root.path()).unwrap();
    (root, opened)
}

fn write_record(root: &TempDir, path: &str, contents: &str) {
    let target = root.path().join(path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, contents).unwrap();
}

#[test]
fn writes_validate_the_persisted_draft_and_never_materialize_read_defaults() {
    let (root, collection) = collection();
    let operations = collection.v03_operations().unwrap();

    let invalid = operations.create(&json!({
        "path": "tasks/invalid.md",
        "type": "task",
        "frontmatter": {"title": "Missing persisted status"}
    }));
    assert!(!invalid.valid);
    assert!(!root.path().join("tasks/invalid.md").exists());
    assert!(invalid
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "schema_required"));

    let created = operations.create(&json!({
        "path": "tasks/valid.md",
        "type": "task",
        "frontmatter": {"title": "Valid", "status": "open"},
        "body": "Body stays byte-for-byte.\n"
    }));
    assert!(created.valid, "{created:#?}");
    assert!(created.result["revision"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert_eq!(created.result["frontmatter"]["status"], "open");
    assert!(created.result["frontmatter"].get("priority").is_none());
    let persisted = fs::read_to_string(root.path().join("tasks/valid.md")).unwrap();
    assert!(!persisted.contains("priority:"));
    assert!(persisted.ends_with("Body stays byte-for-byte.\n"));

    let failed_update = operations.update(&json!({
        "path": "tasks/valid.md",
        "fields": {"status": null},
        "if_revision": created.result["revision"]
    }));
    assert!(!failed_update.valid);
    assert_eq!(
        fs::read_to_string(root.path().join("tasks/valid.md")).unwrap(),
        persisted
    );
}

#[test]
fn batch_preflight_and_dry_run_never_partially_mutate_the_collection() {
    let (root, collection) = collection();
    write_record(
        &root,
        "tasks/existing.md",
        "---\ntype: task\ntitle: Existing\nstatus: open\n---\nOriginal body\n",
    );
    let operations = collection.v03_operations().unwrap();
    let before = fs::read_to_string(root.path().join("tasks/existing.md")).unwrap();

    let rejected = operations.batch(&json!({
        "operations": [
            {
                "kind": "update",
                "input": {"path": "tasks/existing.md", "fields": {"title": "Changed"}}
            },
            {
                "kind": "create",
                "input": {
                    "path": "tasks/invalid-batch.md",
                    "type": "task",
                    "frontmatter": {"title": "Missing status"}
                }
            }
        ]
    }));
    assert!(!rejected.valid);
    assert_eq!(rejected.result["preflight"], true);
    assert_eq!(
        fs::read_to_string(root.path().join("tasks/existing.md")).unwrap(),
        before
    );
    assert!(!root.path().join("tasks/invalid-batch.md").exists());

    let preview = operations.batch(&json!({
        "dry_run": true,
        "operations": [{
            "kind": "create",
            "input": {
                "path": "tasks/preview.md",
                "type": "task",
                "frontmatter": {"title": "Preview", "status": "open"}
            }
        }]
    }));
    assert!(preview.valid, "{preview:#?}");
    assert_eq!(preview.result["dry_run"], true);
    assert!(!root.path().join("tasks/preview.md").exists());

    let committed = operations.batch(&json!({
        "operations": [{
            "kind": "create",
            "input": {
                "path": "tasks/committed.md",
                "type": "task",
                "frontmatter": {"title": "Committed", "status": "open"}
            }
        }]
    }));
    assert!(committed.valid, "{committed:#?}");
    assert!(root.path().join("tasks/committed.md").exists());
}

#[test]
fn non_partial_batch_commits_one_staged_multi_file_plan() {
    let (root, collection) = collection();
    for (path, title) in [
        ("tasks/update.md", "Update"),
        ("tasks/rename.md", "Rename"),
        ("tasks/delete.md", "Delete"),
    ] {
        write_record(
            &root,
            path,
            &format!("---\ntype: task\ntitle: {title}\nstatus: open\n---\n"),
        );
    }
    let operations = collection.v03_operations().unwrap();
    let committed = operations.batch(&json!({
        "operations": [
            {
                "kind": "update",
                "input": {"path": "tasks/update.md", "patch": {"status": "done"}}
            },
            {
                "kind": "rename",
                "input": {"from": "tasks/rename.md", "to": "archive/renamed.md"}
            },
            {
                "kind": "create",
                "input": {
                    "path": "tasks/created.md",
                    "type": "task",
                    "frontmatter": {"title": "Created", "status": "open"}
                }
            },
            {
                "kind": "delete",
                "input": {"path": "tasks/delete.md"}
            }
        ]
    }));

    assert!(committed.valid, "{committed:#?}");
    assert_eq!(committed.result["succeeded"], 4);
    assert!(fs::read_to_string(root.path().join("tasks/update.md"))
        .unwrap()
        .contains("status: done"));
    assert!(root.path().join("archive/renamed.md").is_file());
    assert!(!root.path().join("tasks/rename.md").exists());
    assert!(root.path().join("tasks/created.md").is_file());
    assert!(!root.path().join("tasks/delete.md").exists());
    assert_eq!(
        fs::read_dir(root.path().join(".mdbase/transactions"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn untrusted_backlink_inputs_with_unsafe_paths_fail_closed() {
    let (_root, collection) = collection();
    let backlinks = collection.build_backlinks_index(&[
        mdbase::expressions::evaluator::ResolvedFileData {
            path: "../escape/source.md".to_string(),
            frontmatter: json!({}),
            body: "[[alice]]".to_string(),
        },
        mdbase::expressions::evaluator::ResolvedFileData {
            path: "../escape/alice.md".to_string(),
            frontmatter: json!({"type": "person"}),
            body: String::new(),
        },
    ]);
    assert!(backlinks.is_empty());
}

#[test]
fn target_scoped_links_drive_backlinks_to_the_same_winner() {
    let (root, collection) = collection();
    write_record(
        &root,
        "tasks/id.md",
        "---\ntype: task\nid: alice\ntitle: Wrong type\nstatus: open\n---\n",
    );
    write_record(
        &root,
        "tasks/source.md",
        "---\ntype: task\ntitle: Source\nstatus: open\nassignee: '[[alice]]'\n---\n",
    );

    let all_files = collection.build_all_files_data();
    let backlinks = collection.build_backlinks_index(&all_files);
    assert_eq!(
        backlinks.get("people/alice.md"),
        Some(&vec!["tasks/source.md".to_string()])
    );
    assert!(!backlinks.contains_key("tasks/id.md"));

    assert_eq!(collection.cache_rebuild()["success"], true);
    let cached = collection
        .v03_operations()
        .unwrap()
        .query(&json!({"where": "file.backlinks.length > 0"}));
    assert!(cached.valid, "{cached:#?}");
    assert_eq!(
        cached.result["results"][0]["file"]["path"],
        "people/alice.md"
    );
    assert_eq!(cached.result["meta"]["total_count"], 1);
}

#[test]
fn multi_target_links_validate_existence_and_any_allowed_target_type() {
    let (root, collection) = collection();
    write_record(
        &root,
        "tasks/links.md",
        "---\ntype: task\ntitle: Links\nstatus: open\nrelated: '[[projects/alpha.md]]'\n---\n",
    );
    let operations = collection.v03_operations().unwrap();

    let project = operations.validate(&json!({"path": "tasks/links.md"}));
    assert!(project.valid, "{project:#?}");

    fs::write(
        root.path().join("tasks/links.md"),
        "---\ntype: task\ntitle: Links\nstatus: open\nrelated: ../people/alice.md\n---\n",
    )
    .unwrap();
    let person = operations.validate(&json!({"path": "tasks/links.md"}));
    assert!(person.valid, "{person:#?}");

    fs::write(
        root.path().join("tasks/links.md"),
        "---\ntype: task\ntitle: Links\nstatus: open\nrelated: missing.md\n---\n",
    )
    .unwrap();
    let missing = operations.validate(&json!({"path": "tasks/links.md"}));
    assert!(!missing.valid);
    assert!(missing
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "link_not_found"));
}

#[test]
fn link_validation_uses_canonical_targets_and_unicode_case_matching() {
    let (root, collection) = collection();
    write_record(
        &root,
        "projects/Über.md",
        "---\ntype: project\ntitle: Über\n---\n\n# Details\n",
    );
    write_record(
        &root,
        "tasks/links.md",
        "---\ntype: task\ntitle: Links\nstatus: open\nrelated: '[[alpha|Project]]'\n---\n",
    );
    let operations = collection.v03_operations().unwrap();

    for link in [
        "[[alpha|Project]]",
        "[[alpha#Details]]",
        "[Project](../projects/alpha.md#Details)",
        "[[über]]",
    ] {
        fs::write(
            root.path().join("tasks/links.md"),
            format!("---\ntype: task\ntitle: Links\nstatus: open\nrelated: {link:?}\n---\n"),
        )
        .unwrap();
        let result = operations.validate(&json!({"path": "tasks/links.md"}));
        assert!(result.valid, "{link} should resolve: {result:#?}");
    }

    fs::write(
        root.path().join("tasks/links.md"),
        "---\ntype: task\ntitle: Links\nstatus: open\nrelated: '[Missing](../projects/missing.md#Details)'\n---\n",
    )
    .unwrap();
    let missing = operations.validate(&json!({"path": "tasks/links.md"}));
    assert!(!missing.valid);
    assert!(missing
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "link_not_found"));

    fs::write(
        root.path().join("tasks/links.md"),
        "---\ntype: task\ntitle: Links\nstatus: open\nrelated: '[Escape](../../outside.md#Details)'\n---\n",
    )
    .unwrap();
    let traversal = operations.validate(&json!({"path": "tasks/links.md"}));
    assert!(!traversal.valid);
    assert!(traversal
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "path_traversal"));
}

#[test]
fn query_cel_exposes_link_tag_and_embed_helpers_without_returning_body() {
    let (root, collection) = collection();
    write_record(
        &root,
        "tasks/helpers.md",
        "---\ntype: task\ntitle: Helpers\nstatus: open\n---\n#urgent [[projects/alpha.md]] ![[people/alice.md]]\n",
    );
    let result = collection.v03_operations().unwrap().query(&json!({
        "types": ["task"],
        "where": "file.hasTag('urgent') && file.hasLink('projects/alpha.md')",
        "select": ["file.links", "file.embeds", "file.tags"],
        "include_body": false
    }));
    assert!(result.valid, "{result:#?}");
    assert_eq!(result.result["meta"]["total_count"], 1);
    assert!(result.result["results"][0].get("body").is_none());
    assert_eq!(
        result.result["results"][0]["values"]["links"],
        json!(["projects/alpha.md"])
    );
    assert_eq!(
        result.result["results"][0]["values"]["embeds"],
        json!(["people/alice.md"])
    );
    assert_eq!(
        result.result["results"][0]["values"]["tags"],
        json!(["urgent"])
    );
}
