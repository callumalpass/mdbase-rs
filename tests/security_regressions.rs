use std::fs;
use std::path::Path;
use std::process::Command;

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
    write_file(&outside_root.join("secret.md"), "---\nleak: true\n---\nout\n");

    let collection = open_collection(&col_root);

    let read_res = collection.read(&serde_json::json!({ "path": "../outside/secret.md" }));
    assert_eq!(read_res.pointer("/error/code").and_then(|v| v.as_str()), Some("invalid_path"));

    let update_res = collection.update(&serde_json::json!({
        "path": "../outside/secret.md",
        "fields": { "x": 1 }
    }));
    assert_eq!(update_res.pointer("/error/code").and_then(|v| v.as_str()), Some("invalid_path"));

    let delete_res = collection.delete(&serde_json::json!({ "path": "../outside/secret.md" }));
    assert_eq!(delete_res.pointer("/error/code").and_then(|v| v.as_str()), Some("invalid_path"));

    let rename_res = collection.rename(&serde_json::json!({
        "from": "inside.md",
        "to": "../outside/pwn.md"
    }));
    assert_eq!(rename_res.pointer("/error/code").and_then(|v| v.as_str()), Some("invalid_path"));
    assert!(col_root.join("inside.md").exists());
    assert!(!outside_root.join("pwn.md").exists());
}

#[test]
fn cli_update_refs_flag_is_honored() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    write_file(
        &root.join("mdbase.yaml"),
        "spec_version: 0.2.0\nsettings:\n  rename_update_refs: false\n",
    );
    write_file(&root.join("a.md"), "---\nid: a\n---\n");
    write_file(&root.join("r.md"), "---\nref: \"[[a]]\"\n---\n");

    let status = Command::new(env!("CARGO_BIN_EXE_mdb"))
        .arg("-C")
        .arg(root)
        .arg("rename")
        .arg("a.md")
        .arg("b.md")
        .arg("--update-refs")
        .status()
        .expect("run mdb rename");
    assert!(status.success());

    let ref_content = fs::read_to_string(root.join("r.md")).expect("read r.md");
    assert!(ref_content.contains("[[b]]"), "reference not updated: {}", ref_content);
}

#[test]
fn cli_cache_clear_uses_configured_cache_folder() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    write_file(
        &root.join("mdbase.yaml"),
        "spec_version: 0.2.0\nsettings:\n  cache_folder: custom-cache\n",
    );
    write_file(&root.join("custom-cache/cache.db"), "x");

    let status = Command::new(env!("CARGO_BIN_EXE_mdb"))
        .arg("-C")
        .arg(root)
        .arg("cache")
        .arg("clear")
        .status()
        .expect("run mdb cache clear");
    assert!(status.success());
    assert!(
        !root.join("custom-cache/cache.db").exists(),
        "custom cache db should be removed"
    );
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
    write_file(&root.join("target.md"), "---\nid: target\n---\n");
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
    assert!(ref_content.contains("[[target]]"), "ref should be unchanged");
}
