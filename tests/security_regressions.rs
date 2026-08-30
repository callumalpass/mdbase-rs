#![cfg(feature = "legacy-collection-mutation")]

use std::fs;
use std::path::Path;

#[cfg(unix)]
use mdbase::Collection;
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

#[test]
fn traversal_paths_are_rejected_for_core_operations() {
    let tmp = TempDir::new().expect("tempdir");
    let col_root = tmp.path().join("col");
    let outside_root = tmp.path().join("outside");
    fs::create_dir_all(&col_root).expect("mkdir col");
    fs::create_dir_all(&outside_root).expect("mkdir outside");

    write_file(&col_root.join("mdbase.yaml"), "spec_version: 0.2.0\n");
    write_file(&col_root.join("inside.md"), "---\na: 1\n---\ninside\n");
    write_file(
        &outside_root.join("secret.md"),
        "---\nleak: true\n---\nout\n",
    );

    let collection = open_collection(&col_root);

    let read_res = collection.read(&serde_json::json!({ "path": "../outside/secret.md" }));
    assert_eq!(
        read_res.pointer("/error/code").and_then(|v| v.as_str()),
        Some("invalid_path")
    );

    let update_res = collection.update(&serde_json::json!({
        "path": "../outside/secret.md",
        "fields": { "x": 1 }
    }));
    assert_eq!(
        update_res.pointer("/error/code").and_then(|v| v.as_str()),
        Some("invalid_path")
    );

    let delete_res = collection.delete(&serde_json::json!({ "path": "../outside/secret.md" }));
    assert_eq!(
        delete_res.pointer("/error/code").and_then(|v| v.as_str()),
        Some("invalid_path")
    );

    let rename_res = collection.rename(&serde_json::json!({
        "from": "inside.md",
        "to": "../outside/pwn.md"
    }));
    assert_eq!(
        rename_res.pointer("/error/code").and_then(|v| v.as_str()),
        Some("invalid_path")
    );
    assert!(col_root.join("inside.md").exists());
    assert!(!outside_root.join("pwn.md").exists());
}

#[cfg(unix)]
#[test]
fn symlinks_cannot_escape_the_collection_boundary() {
    use std::os::unix::fs::symlink;

    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().join("collection");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&root).expect("create collection");
    fs::create_dir_all(&outside).expect("create outside folder");
    write_file(&root.join("mdbase.yaml"), "spec_version: 0.3.0\n");
    write_file(
        &outside.join("secret.md"),
        "---\nsecret: never-return-this\n---\noutside\n",
    );
    write_file(
        &root.join("link.md"),
        "---\ntarget: secret.md\n---\ninside\n",
    );
    symlink(&outside, root.join("escape")).expect("create directory symlink");
    symlink(outside.join("secret.md"), root.join("secret.md")).expect("create file symlink");

    let collection = open_collection(&root);
    for (index, result) in [
        collection.read(&serde_json::json!({ "path": "secret.md" })),
        collection.update(&serde_json::json!({
            "path": "secret.md",
            "fields": { "compromised": true }
        })),
        collection.delete(&serde_json::json!({ "path": "secret.md" })),
        collection.validate_op(&serde_json::json!({ "path": "secret.md" })),
        collection.migrate(&serde_json::json!({ "path": "escape/migration.md" })),
        collection.create(&serde_json::json!({
            "path": "escape/created.md",
            "frontmatter": {},
            "body": "must stay contained"
        })),
        collection.rename(&serde_json::json!({
            "from": "secret.md",
            "to": "renamed.md",
            "update_refs": false
        })),
        collection.rename(&serde_json::json!({
            "from": "link.md",
            "to": "renamed-link.md",
            "simulate_before_ref_update": [{
                "path": "escape/simulated.md",
                "content": "must not be written"
            }]
        })),
        collection.batch_update(
            &serde_json::json!({
                "updates": [{
                    "path": "escape/secret.md",
                    "fields": {"compromised": true}
                }]
            }),
            None,
            false,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            result
                .pointer("/error/code")
                .and_then(|value| value.as_str()),
            Some("path_traversal"),
            "unexpected result at operation {index}: {result}"
        );
        assert!(
            !result.to_string().contains("never-return-this"),
            "operation leaked the external payload: {result}"
        );
    }

    assert!(!outside.join("created.md").exists());
    assert!(!outside.join("simulated.md").exists());
    assert!(root.join("link.md").exists());
    assert_eq!(
        fs::read_to_string(outside.join("secret.md")).expect("external file remains readable"),
        "---\nsecret: never-return-this\n---\noutside\n"
    );

    let query = collection
        .v03_operations()
        .unwrap()
        .query(&serde_json::json!({}));
    assert!(query.valid, "{query:#?}");
    assert_eq!(query.result["meta"]["total_count"], 1);
    assert!(!serde_json::to_string(&query)
        .unwrap()
        .contains("never-return-this"));

    let resolved = collection.resolve_link(&serde_json::json!({
        "path": "link.md",
        "field": "target"
    }));
    assert!(resolved["resolved_path"].is_null(), "{resolved}");
}

#[cfg(unix)]
#[test]
fn collection_metadata_never_loads_through_symlinks() {
    use std::os::unix::fs::symlink;

    let tmp = TempDir::new().expect("tempdir");
    let external_config = tmp.path().join("external-config.yaml");
    write_file(&external_config, "spec_version: 0.3.0\n");
    let linked_config_root = tmp.path().join("linked-config");
    fs::create_dir(&linked_config_root).unwrap();
    symlink(&external_config, linked_config_root.join("mdbase.yaml")).unwrap();
    let config_error = Collection::open(&linked_config_root)
        .err()
        .expect("config link must fail");
    assert_eq!(
        config_error
            .pointer("/error/code")
            .and_then(|value| value.as_str()),
        Some("invalid_config")
    );
    assert_eq!(
        mdbase::config::load_config(&linked_config_root)
            .pointer("/error/code")
            .and_then(|value| value.as_str()),
        Some("invalid_config")
    );

    for folder in ["_types", ".mdbase"] {
        let root = tmp
            .path()
            .join(format!("linked-{}", folder.trim_start_matches('.')));
        let outside = tmp
            .path()
            .join(format!("outside-{}", folder.trim_start_matches('.')));
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        write_file(&root.join("mdbase.yaml"), "spec_version: 0.3.0\n");
        symlink(&outside, root.join(folder)).unwrap();
        let error = Collection::open(&root)
            .err()
            .expect("system folder link must fail");
        assert_eq!(
            error
                .pointer("/error/code")
                .and_then(|value| value.as_str()),
            Some("path_traversal"),
            "unexpected error for {folder}: {error}"
        );
    }

    let root = tmp.path().join("linked-type-entry");
    let outside = tmp.path().join("outside-type-entry");
    fs::create_dir_all(root.join("_types")).unwrap();
    fs::create_dir(&outside).unwrap();
    write_file(&root.join("mdbase.yaml"), "spec_version: 0.3.0\n");
    write_file(
        &outside.join("external.md"),
        "---\nkind: mdbase.type\nname: external\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n",
    );
    symlink(outside.join("external.md"), root.join("_types/external.md")).unwrap();
    let collection = open_collection(&root);
    assert!(!collection.types().contains_key("external"));
}

#[test]
fn concurrent_renames_never_replace_the_winner() {
    use std::sync::{Arc, Barrier};

    const WRITERS: usize = 12;
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    write_file(&root.join("mdbase.yaml"), "spec_version: 0.3.0\n");
    for index in 0..WRITERS {
        write_file(
            &root.join(format!("source-{index}.md")),
            &format!("---\nwriter: {index}\n---\nwriter-{index}\n"),
        );
    }

    let barrier = Arc::new(Barrier::new(WRITERS));
    let handles = (0..WRITERS)
        .map(|index| {
            let root = root.to_path_buf();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let collection = open_collection(&root);
                barrier.wait();
                (
                    index,
                    collection.rename(&serde_json::json!({
                        "from": format!("source-{index}.md"),
                        "to": "winner.md",
                        "update_refs": false
                    })),
                )
            })
        })
        .collect::<Vec<_>>();

    let results = handles
        .into_iter()
        .map(|handle| handle.join().expect("rename worker"))
        .collect::<Vec<_>>();
    let successes = results
        .iter()
        .filter(|(_, result)| result.get("error").is_none())
        .collect::<Vec<_>>();
    assert_eq!(successes.len(), 1, "results: {results:?}");

    for (_, result) in &results {
        if result.get("error").is_some() {
            assert_eq!(
                result
                    .pointer("/error/code")
                    .and_then(|value| value.as_str()),
                Some("path_conflict"),
                "unexpected loser result: {result}"
            );
        }
    }

    let winner_index = successes[0].0;
    let winner = fs::read_to_string(root.join("winner.md")).expect("winner exists");
    assert!(winner.contains(&format!("writer-{winner_index}")));
    for index in 0..WRITERS {
        assert_eq!(
            root.join(format!("source-{index}.md")).exists(),
            index != winner_index,
            "only the winning source should move"
        );
    }
}

#[cfg(unix)]
#[test]
fn rename_reports_ref_update_io_failures() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    write_file(
        &root.join("mdbase.yaml"),
        "spec_version: 0.2.0\nsettings:\n  rename_update_refs: true\n",
    );
    write_file(&root.join("target.md"), "---\nid: stable-target-id\n---\n");
    write_file(&root.join("ref.md"), "---\nref: \"[[target]]\"\n---\n");

    let ref_path = root.join("ref.md");
    let mut perms = fs::metadata(&ref_path).expect("metadata").permissions();
    perms.set_mode(0o444);
    fs::set_permissions(&ref_path, perms).expect("set readonly");

    let collection = open_collection(root);
    let result = collection.rename(&serde_json::json!({
        "from": "target.md",
        "to": "new-target.md",
        "update_refs": true
    }));

    let mut restore = fs::metadata(&ref_path).expect("metadata").permissions();
    restore.set_mode(0o644);
    fs::set_permissions(&ref_path, restore).expect("restore perms");

    assert_eq!(
        result.pointer("/error/code").and_then(|v| v.as_str()),
        Some("rename_ref_update_failed")
    );
    assert!(result.get("references_updated").is_none());
    assert!(result.pointer("/partial_updates/failed").is_some());
    let ref_content = fs::read_to_string(&ref_path).expect("read ref");
    assert!(
        ref_content.contains("[[target]]"),
        "ref should be unchanged"
    );
}
