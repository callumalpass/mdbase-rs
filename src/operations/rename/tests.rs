#[cfg(unix)]
use super::hooks::{inject_parent_swap, inject_root_replacement};
use super::hooks::{inject_reference_open_failure, inject_reference_removal};
use super::*;
use serde_json::json;
use std::fs;

fn collection() -> (tempfile::TempDir, Collection) {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.2.1\nsettings:\n  rename_update_refs: true\n",
    )
    .unwrap();
    let collection = Collection::open(root.path()).unwrap();
    (root, collection)
}

#[test]
fn many_references_use_one_scan_and_one_initial_load_per_record() {
    let (root, collection) = collection();
    fs::write(root.path().join("target.md"), "target\n").unwrap();
    for index in 0..8 {
        fs::write(
            root.path().join(format!("ref-{index}.md")),
            "See [[target]].\n",
        )
        .unwrap();
    }
    crate::reset_snapshot_scan_calls_for_test();
    crate::record_load::reset_snapshot_record_loads_for_test();

    let result = collection.rename(&json!({
        "from": "target.md",
        "to": "renamed.md",
        "update_refs": true,
    }));
    assert!(result.get("error").is_none(), "{result:#}");
    assert_eq!(crate::snapshot_scan_calls_for_test(), 1);
    assert_eq!(crate::record_load::snapshot_record_loads_for_test(), 18);
}

#[test]
fn dry_run_loads_only_the_authoritative_generation() {
    let (root, collection) = collection();
    fs::write(root.path().join("target.md"), "target\n").unwrap();
    for index in 0..4 {
        fs::write(
            root.path().join(format!("ref-{index}.md")),
            "See [[target]].\n",
        )
        .unwrap();
    }
    crate::reset_snapshot_scan_calls_for_test();
    crate::record_load::reset_snapshot_record_loads_for_test();

    let result = collection.rename(&json!({
        "from": "target.md",
        "to": "renamed.md",
        "update_refs": true,
        "dry_run": true,
    }));
    assert_eq!(result["dry_run"], true, "{result:#}");
    assert_eq!(crate::snapshot_scan_calls_for_test(), 1);
    assert_eq!(crate::record_load::snapshot_record_loads_for_test(), 5);
    assert!(root.path().join("target.md").exists());
    assert!(!root.path().join("renamed.md").exists());
}

#[test]
fn disappearing_reference_is_partial_and_a_sibling_still_updates() {
    let (root, collection) = collection();
    fs::write(root.path().join("target.md"), "target\n").unwrap();
    let disappearing = root.path().join("a-ref.md");
    fs::write(&disappearing, "See [[target]].\n").unwrap();
    fs::write(root.path().join("b-ref.md"), "See [[target]].\n").unwrap();
    inject_reference_removal(&disappearing);

    let result = collection.rename(&json!({
        "from": "target.md",
        "to": "renamed.md",
        "update_refs": true,
    }));
    assert_eq!(
        result["error"]["code"], RENAME_REF_UPDATE_FAILED,
        "{result:#}"
    );
    assert_eq!(
        result["partial_updates"]["failed"][0]["path"], "a-ref.md",
        "{result:#}"
    );
    assert!(fs::read_to_string(root.path().join("b-ref.md"))
        .unwrap()
        .contains("[[renamed]]"));
}

#[test]
fn unreadable_reference_is_partial_and_never_overwritten() {
    let (root, collection) = collection();
    fs::write(root.path().join("target.md"), "target\n").unwrap();
    let unreadable = root.path().join("ref.md");
    fs::write(&unreadable, "See [[target]].\n").unwrap();
    inject_reference_open_failure(&unreadable);

    let result = collection.rename(&json!({
        "from": "target.md",
        "to": "renamed.md",
        "update_refs": true,
    }));
    assert_eq!(
        result["error"]["code"], RENAME_REF_UPDATE_FAILED,
        "{result:#}"
    );
    assert_eq!(
        result["partial_updates"]["failed"][0]["reason"], "io_error",
        "{result:#}"
    );
    assert_eq!(fs::read_to_string(unreadable).unwrap(), "See [[target]].\n");
}

#[test]
fn self_reference_reloads_the_destination_against_the_source_revision() {
    let (root, collection) = collection();
    fs::write(
        root.path().join("self.md"),
        "---\nname: self\n---\nSee [[self]].\n",
    )
    .unwrap();
    let result = collection.rename(&json!({
        "from": "self.md",
        "to": "self-new.md",
        "update_refs": true,
    }));
    assert!(result.get("error").is_none(), "{result:#}");
    let renamed = fs::read_to_string(root.path().join("self-new.md")).unwrap();
    assert!(renamed.contains("[[self-new]]"), "{renamed}");
}

#[test]
fn dry_run_and_execution_plan_identical_self_and_mixed_reference_details() {
    let (root, collection) = collection();
    fs::write(
        root.path().join("self.md"),
        "---\nrelated: '[[self#front|alias]]'\n---\nSelf ![[self#body]].\n",
    )
    .unwrap();
    fs::write(
        root.path().join("other.md"),
        "---\nrelated: '[[self]]'\n---\nOther [[self|alias]].\n",
    )
    .unwrap();

    crate::record_load::reset_snapshot_record_loads_for_test();
    let dry = collection.rename(&json!({
        "from": "self.md",
        "to": "self-new.md",
        "update_refs": true,
        "dry_run": true,
    }));
    assert_eq!(crate::record_load::snapshot_record_loads_for_test(), 2);
    assert!(root.path().join("self.md").exists());
    let affected = dry["references_affected"].clone();
    assert!(affected
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item["path"] == "self-new.md")));

    let executed = collection.rename(&json!({
        "from": "self.md",
        "to": "self-new.md",
        "update_refs": true,
    }));
    assert!(executed.get("error").is_none(), "{executed:#}");
    assert_eq!(
        affected, executed["references_updated"],
        "dry={dry:#}\nreal={executed:#}"
    );
}

#[test]
fn duplicate_stem_winner_loser_matrix_uses_canonical_source_relative_ranking() {
    let scenarios = [
        ("a/same.md", "b/same.md", "b/ref.md", "b/same.md"),
        ("deep/a/same.md", "b/same.md", "refs/ref.md", "b/same.md"),
        ("a/same.md", "b/same.md", "refs/ref.md", "a/same.md"),
    ];
    for (first, second, source, winner) in scenarios {
        for creation_order in [[first, second], [second, first]] {
            for renamed in [first, second] {
                let (root, collection) = collection();
                for candidate in creation_order {
                    fs::create_dir_all(root.path().join(candidate).parent().unwrap()).unwrap();
                    fs::write(root.path().join(candidate), "target\n").unwrap();
                }
                fs::create_dir_all(root.path().join(source).parent().unwrap()).unwrap();
                let explicit = renamed.trim_end_matches(".md");
                let original = format!(
                        "---\nplain: '[[same]]'\nalias: '[[same|front alias]]'\nanchor: '[[same#front|front alias]]'\nexplicit: '[[{explicit}#front|path alias]]'\n---\n[[same]] [[same|body alias]] [[same#body]] ![[same#embed|embed alias]] [[{explicit}#body|path alias]]\n"
                    );
                fs::write(root.path().join(source), original).unwrap();
                let parent = std::path::Path::new(renamed)
                    .parent()
                    .and_then(|path| path.to_str())
                    .unwrap_or("");
                let to = if parent.is_empty() {
                    "renamed.md".to_string()
                } else {
                    format!("{parent}/renamed.md")
                };

                let result = collection.rename(&json!({
                    "from": renamed,
                    "to": to,
                    "update_refs": true,
                }));
                assert!(result.get("error").is_none(), "{result:#}");
                assert!(result.get("warnings").is_none(), "{result:#}");
                let rewritten = fs::read_to_string(root.path().join(source)).unwrap();
                let is_winner = renamed == winner;
                for (old, new) in [
                    ("[[same]]", "[[renamed]]"),
                    ("[[same|front alias]]", "[[renamed|front alias]]"),
                    (
                        "[[same#front|front alias]]",
                        "[[renamed#front|front alias]]",
                    ),
                    ("[[same|body alias]]", "[[renamed|body alias]]"),
                    ("[[same#body]]", "[[renamed#body]]"),
                    (
                        "![[same#embed|embed alias]]",
                        "![[renamed#embed|embed alias]]",
                    ),
                ] {
                    assert_eq!(
                        rewritten.contains(new),
                        is_winner,
                        "{renamed} {source}: {rewritten}"
                    );
                    assert_eq!(
                        rewritten.contains(old),
                        !is_winner,
                        "{renamed} {source}: {rewritten}"
                    );
                }
                let to_no_ext = to.trim_end_matches(".md");
                assert!(
                    rewritten.contains(&format!("[[{to_no_ext}#front|path alias]]")),
                    "{rewritten}"
                );
                assert!(
                    rewritten.contains(&format!("[[{to_no_ext}#body|path alias]]")),
                    "{rewritten}"
                );
            }
        }
    }
}

#[test]
fn duplicate_ids_are_never_rewritten_by_basename_fallback() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.2.1\nsettings:\n  rename_update_refs: true\n  id_field: id\n",
    )
    .unwrap();
    for directory in ["a", "b"] {
        fs::create_dir(root.path().join(directory)).unwrap();
        fs::write(
            root.path().join(directory).join("same.md"),
            "---\nid: same\n---\ntarget\n",
        )
        .unwrap();
    }
    let original = "---\nref: '[[same#front|alias]]'\n---\n![[same#body|embed]]\n";
    fs::write(root.path().join("ref.md"), original).unwrap();
    let collection = Collection::open(root.path()).unwrap();
    let result = collection.rename(&json!({
        "from": "a/same.md",
        "to": "a/renamed.md",
        "update_refs": true,
    }));
    assert!(result.get("error").is_none(), "{result:#}");
    assert_eq!(
        fs::read_to_string(root.path().join("ref.md")).unwrap(),
        original
    );
}

#[test]
fn typed_frontmatter_and_unscoped_body_use_their_own_canonical_winners() {
    for renamed_type in ["person", "project"] {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("mdbase.yaml"),
            "spec_version: 0.2.1\nsettings:\n  rename_update_refs: true\n",
        )
        .unwrap();
        fs::create_dir(root.path().join("_types")).unwrap();
        fs::write(
            root.path().join("_types/person.md"),
            "---\nname: person\nfields: {}\n---\n",
        )
        .unwrap();
        fs::write(
            root.path().join("_types/project.md"),
            "---\nname: project\nfields: {}\n---\n",
        )
        .unwrap();
        fs::write(
            root.path().join("_types/source.md"),
            "---\nname: source\nfields:\n  ref: { type: link, target: person }\n---\n",
        )
        .unwrap();
        for (directory, type_name) in [("a-project", "project"), ("z-person", "person")] {
            fs::create_dir(root.path().join(directory)).unwrap();
            fs::write(
                root.path().join(directory).join("same.md"),
                format!("---\ntype: {type_name}\n---\ntarget\n"),
            )
            .unwrap();
        }
        fs::write(
            root.path().join("ref.md"),
            "---\ntype: source\nref: '[[same|front]]'\n---\n[[same|body]]\n",
        )
        .unwrap();
        let (from, to) = if renamed_type == "person" {
            ("z-person/same.md", "z-person/renamed.md")
        } else {
            ("a-project/same.md", "a-project/renamed.md")
        };
        let collection = Collection::open(root.path()).unwrap();
        let result = collection.rename(&json!({
            "from": from,
            "to": to,
            "update_refs": true,
        }));
        assert!(result.get("error").is_none(), "{result:#}");
        let rewritten = fs::read_to_string(root.path().join("ref.md")).unwrap();
        if renamed_type == "person" {
            assert!(rewritten.contains("[[renamed|front]]"), "{rewritten}");
            assert!(rewritten.contains("[[same|body]]"), "{rewritten}");
        } else {
            assert!(rewritten.contains("[[same|front]]"), "{rewritten}");
            assert!(rewritten.contains("[[renamed|body]]"), "{rewritten}");
        }
    }
}

#[test]
fn unreadable_snapshot_record_fails_before_source_mutation() {
    let (root, collection) = collection();
    fs::write(root.path().join("target.md"), "target\n").unwrap();
    fs::write(root.path().join("ref.md"), "See [[target]].\n").unwrap();
    crate::operations::set_record_open_failure(
        root.path(),
        "ref.md",
        Some(std::io::ErrorKind::PermissionDenied),
    );
    let result = collection.rename(&json!({
        "from": "target.md",
        "to": "renamed.md",
        "update_refs": true,
    }));
    crate::operations::set_record_open_failure(root.path(), "ref.md", None);
    assert_eq!(
        result["error"]["code"], "collection_snapshot_failed",
        "{result:#}"
    );
    assert!(root.path().join("target.md").exists());
    assert!(!root.path().join("renamed.md").exists());
}

#[cfg(unix)]
#[test]
fn descendant_replacement_never_redirects_reference_writes() {
    let (root, collection) = collection();
    fs::write(root.path().join("target.md"), "target\n").unwrap();
    fs::create_dir(root.path().join("refs")).unwrap();
    fs::write(root.path().join("refs/ref.md"), "See [[target]].\n").unwrap();
    let external = tempfile::tempdir().unwrap();
    fs::write(external.path().join("ref.md"), "external [[target]]\n").unwrap();
    crate::replace_descendant_on_scan_for_test(root.path(), "refs", external.path());

    let result = collection.rename(&json!({
        "from": "target.md",
        "to": "renamed.md",
        "update_refs": true,
    }));
    assert_eq!(result["to"], "renamed.md", "{result:#}");
    assert!(!root.path().join("target.md").exists());
    assert!(root.path().join("renamed.md").is_file());
    assert_eq!(
        fs::read_to_string(external.path().join("ref.md")).unwrap(),
        "external [[target]]\n"
    );
}

#[cfg(unix)]
#[test]
fn root_replacement_keeps_rename_on_the_held_collection() {
    let (root, collection) = collection();
    fs::write(root.path().join("target.md"), "inside\n").unwrap();
    let external = tempfile::tempdir().unwrap();
    fs::write(external.path().join("target.md"), "external\n").unwrap();
    fs::write(external.path().join("sentinel"), "untouched\n").unwrap();
    inject_root_replacement(root.path(), external.path());
    let original_root = root.path().to_path_buf();
    let held_root = original_root.with_extension("rename-held-root");

    let result = collection.rename(&json!({
        "from": "target.md",
        "to": "new/deep/renamed.md",
        "update_refs": false,
    }));
    assert_eq!(result["to"], "new/deep/renamed.md", "{result:#}");
    assert_eq!(
        fs::read_to_string(external.path().join("target.md")).unwrap(),
        "external\n"
    );
    assert_eq!(
        fs::read_to_string(external.path().join("sentinel")).unwrap(),
        "untouched\n"
    );
    assert!(!external.path().join("new").exists());
    assert!(!held_root.join("target.md").exists());
    assert_eq!(
        fs::read_to_string(held_root.join("new/deep/renamed.md")).unwrap(),
        "inside\n"
    );

    fs::remove_file(&original_root).unwrap();
    fs::rename(held_root, &original_root).unwrap();
}

#[cfg(unix)]
#[test]
fn destination_parent_symlink_swap_cannot_redirect_rename_outside() {
    let (root, collection) = collection();
    fs::write(root.path().join("target.md"), "inside\n").unwrap();
    let external = tempfile::tempdir().unwrap();
    fs::write(external.path().join("sentinel"), "untouched\n").unwrap();
    inject_parent_swap(root.path(), std::path::Path::new("new"), external.path());

    let result = collection.rename(&json!({
        "from": "target.md",
        "to": "new/deep/renamed.md",
        "update_refs": false,
    }));
    assert_eq!(result["error"]["code"], "io_error", "{result:#}");
    assert_eq!(
        fs::read_to_string(root.path().join("target.md")).unwrap(),
        "inside\n"
    );
    assert_eq!(
        fs::read_to_string(external.path().join("sentinel")).unwrap(),
        "untouched\n"
    );
    assert!(!external.path().join("deep").exists());
    assert!(!external.path().join("renamed.md").exists());
}
