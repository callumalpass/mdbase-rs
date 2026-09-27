//! Seed-type upgrade tests, kept beside the type-pack test helpers they use.
use super::tests::{apply_pack, collection, manifest, provision, resource, task_resources, write};
use super::*;

#[test]
fn seed_upgrade_atomically_changes_exact_contract_and_preserves_customizations() {
    for customized in [false, true] {
        let (root, collection) = collection();
        let definitions = task_resources();
        let mut old_manifest = manifest(&definitions);
        old_manifest["resources"][2]["mode"] = json!("seed");
        let old = provision(
            old_manifest,
            definitions
                .iter()
                .map(|(_, s, _, d)| resource(s, d))
                .collect(),
        );
        assert!(apply_pack(&collection, &old).valid);
        let type_path = root.path().join("_types/task.md");
        if customized {
            let document = definitions[2]
                .3
                .replace("title: title", "title: label")
                .replace("title: { type: string }", "label: { type: string }")
                .replace("required: [title]", "required: [label]");
            write(&type_path, &format!("{document}My custom documentation.\n"));
        }
        let task = "---\ntype: task\ntitle: Keep\nlabel: Keep\n---\nKeep this body.\n";
        write(&root.path().join("task.md"), task);
        let upgraded = seed_upgrade_provision(&definitions);
        let collection = Collection::open(root.path()).unwrap();
        let applied = apply_pack(&collection, &upgraded);
        assert!(applied.valid, "{:?}", applied.diagnostics);
        let reopened = Collection::open(root.path()).unwrap();
        assert_eq!(
            reopened
                .get_data_contract_implementations("example.task", "2.0.0")
                .len(),
            1
        );
        assert!(reopened
            .get_data_contract_implementations("example.task", "1.0.0")
            .is_empty());
        let result = fs::read_to_string(&type_path).unwrap();
        assert!(result.contains("assignees"));
        if customized {
            assert!(result.contains("title: label"));
            assert!(result.ends_with("My custom documentation.\n"));
        }
        assert_eq!(
            fs::read_to_string(root.path().join("task.md")).unwrap(),
            task
        );
        let repeated = apply_pack(&reopened, &upgraded);
        assert!(repeated.valid, "{:?}", repeated.diagnostics);
        assert_eq!(fs::read_to_string(&type_path).unwrap(), result);
    }
}

fn seed_upgrade_provision(definitions: &[(&str, &str, &str, &str)]) -> TypePackProvision {
    let mut schema: Value = serde_json::from_str(definitions[0].3).unwrap();
    schema["properties"]["assignees"] = json!({"type":"array", "items":{"type":"string"}});
    let schema = schema.to_string();
    let contract = definitions[1].3.replace("1.0.0", "2.0.0");
    let document = definitions[2].3.replace("version: 1\n", "version: 2\n")
        .replace("version: 1.0.0", "version: 2.0.0")
        .replace("      title: { type: string }", "      title: { type: string }\n      assignees: { type: array, items: { type: string } }")
        .replace("      title: title", "      title: title\n      assignees: assignees");
    let mut entries = definitions.to_vec();
    entries[0].3 = &schema;
    entries[1].3 = &contract;
    entries[2].3 = &document;
    let mut desired = manifest(&entries);
    desired["version"] = json!("2.0.0");
    desired["resources"][2]["mode"] = json!("seed");
    desired["resources"][2]["upgrade_from"] = json!({
        "digest": revision(definitions[2].3.as_bytes()), "document": definitions[2].3
    });
    provision(
        desired,
        entries.iter().map(|(_, s, _, d)| resource(s, d)).collect(),
    )
}

#[test]
fn seed_upgrade_rejects_remaining_old_references_without_publishing_any_resource() {
    let (root, collection) = collection();
    let definitions = task_resources();
    let mut initial_manifest = manifest(&definitions);
    initial_manifest["resources"][2]["mode"] = json!("seed");
    let old = provision(
        initial_manifest,
        definitions
            .iter()
            .map(|(_, s, _, d)| resource(s, d))
            .collect(),
    );
    assert!(apply_pack(&collection, &old).valid);
    write(
        &root.path().join("_types/other.md"),
        &definitions[2].3.replace("name: task", "name: other"),
    );
    let lock = fs::read(root.path().join("mdbase.lock.yaml")).unwrap();
    let upgraded = seed_upgrade_provision(&definitions);
    let reopened = Collection::open(root.path()).unwrap();
    let result = apply_pack(&reopened, &upgraded);
    assert!(!result.valid);
    assert!(result
        .diagnostics
        .iter()
        .any(|d| d.message.contains("1.0.0")));
    for (_, _, target, document) in definitions {
        assert_eq!(
            fs::read_to_string(root.path().join(target)).unwrap(),
            document
        );
    }
    assert_eq!(
        fs::read(root.path().join("mdbase.lock.yaml")).unwrap(),
        lock
    );
}

#[test]
fn seed_upgrade_rejects_a_tampered_baseline_even_on_fresh_install() {
    let (root, collection) = collection();
    let mut pack = seed_upgrade_provision(&task_resources());
    pack.manifest["resources"][2]["upgrade_from"]["document"] = json!("tampered");
    let result = apply_pack(&collection, &pack);
    assert!(!result.valid);
    assert!(!root.path().join("_types/task.md").exists());
    assert!(!root.path().join("mdbase.lock.yaml").exists());
}

#[test]
fn renamed_seed_source_does_not_resurrect_a_deleted_type() {
    let (root, collection) = collection();
    let definitions = task_resources();
    let mut initial = manifest(&definitions);
    initial["resources"][2]["mode"] = json!("seed");
    let pack = provision(
        initial,
        definitions
            .iter()
            .map(|(_, s, _, d)| resource(s, d))
            .collect(),
    );
    assert!(apply_pack(&collection, &pack).valid);
    fs::remove_file(root.path().join("_types/task.md")).unwrap();
    let mut upgraded = seed_upgrade_provision(&definitions);
    upgraded.manifest["resources"][2]["source"] = json!("task-v2.md");
    upgraded.resources[2].source = "task-v2.md".to_string();
    let reopened = Collection::open(root.path()).unwrap();
    let result = apply_pack(&reopened, &upgraded);
    assert!(result.valid, "{:?}", result.diagnostics);
    assert!(!root.path().join("_types/task.md").exists());
}
