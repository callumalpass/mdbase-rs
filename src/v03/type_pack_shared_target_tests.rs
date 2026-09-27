//! Packs that ship the same managed file (Reader's source contract, shipped by
//! Reader and by apps that cite from it): the later pack defers to the owner.
use super::tests::{
    apply_pack, assessment_options, collection, manifest, provision, resource, task_resources,
};
use super::*;

fn pack_from(
    id: &str,
    version: &str,
    definitions: &[(&str, &str, &str, &str)],
) -> TypePackProvision {
    let mut manifest = manifest(definitions);
    manifest["id"] = Value::String(id.to_string());
    manifest["version"] = Value::String(version.to_string());
    let resources = definitions
        .iter()
        .map(|(_, source, _, document)| resource(source, document))
        .collect();
    provision(manifest, resources)
}

fn actions(result: &OperationResult) -> Vec<&str> {
    result.result["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|resource| resource["action"].as_str().unwrap())
        .collect()
}

fn receipt_modes(root: &Path, pack: &str) -> Vec<String> {
    let lock: Value =
        serde_json::from_slice(&fs::read(root.join(TYPE_PACK_LOCK_PATH)).unwrap()).unwrap();
    lock["packs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|receipt| receipt["id"] == pack)
        .unwrap()["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|resource| resource["mode"].as_str().unwrap().to_string())
        .collect()
}

const REVISED_CONTRACT: &str = "---\nkind: mdbase.contract\ncontract_type: record\nid: example.task\nversion: 1.0.0\ndescription: Revised.\nrecord_schema:\n  dialect: json-schema-2020-12\n  ref: ../schemas/task-contract.schema.json\n---\n";

#[test]
fn a_pack_defers_to_another_that_manages_identical_bytes() {
    let (root, collection) = collection();
    let definitions = task_resources();
    let first = apply_pack(
        &collection,
        &pack_from("example.one", "1.0.0", &definitions),
    );
    assert!(first.valid, "{:?}", first.diagnostics);

    // The second pack ships the same files: it installs, deferring to the
    // first, and the lock keeps a single owner per target.
    let second_pack = pack_from("example.two", "1.0.0", &definitions);
    let second = apply_pack(&Collection::open(root.path()).unwrap(), &second_pack);
    assert!(second.valid, "{:?}", second.diagnostics);
    assert_eq!(actions(&second), ["preserve", "preserve", "preserve"]);
    assert_eq!(
        receipt_modes(root.path(), "example.two"),
        ["seed", "seed", "seed"]
    );
    assert_eq!(
        receipt_modes(root.path(), "example.one"),
        ["managed", "managed", "managed"]
    );
    let again = Collection::open(root.path())
        .unwrap()
        .assess_type_pack(&second_pack, &assessment_options());
    assert_eq!(again.result["status"], "current");

    // The owner moves a file forward; the deferring pack keeps deferring.
    let mut revised = definitions.clone();
    revised[1].3 = REVISED_CONTRACT;
    let upgraded = apply_pack(
        &Collection::open(root.path()).unwrap(),
        &pack_from("example.one", "1.1.0", &revised),
    );
    assert!(upgraded.valid, "{:?}", upgraded.diagnostics);
    let behind = apply_pack(
        &Collection::open(root.path()).unwrap(),
        &pack_from("example.two", "1.0.1", &definitions),
    );
    assert!(behind.valid, "{:?}", behind.diagnostics);
    assert_eq!(
        fs::read_to_string(root.path().join("_contracts/example.task.md")).unwrap(),
        REVISED_CONTRACT
    );
}

#[test]
fn a_pack_claims_what_it_deferred_once_the_owner_lets_it_go() {
    let (root, collection) = collection();
    let definitions = task_resources();
    assert!(
        apply_pack(
            &collection,
            &pack_from("example.one", "1.0.0", &definitions)
        )
        .valid
    );
    assert!(
        apply_pack(
            &Collection::open(root.path()).unwrap(),
            &pack_from("example.two", "1.0.0", &definitions)
        )
        .valid
    );

    // The owner drops the type. The deferring pack still relies on it, so
    // the file stays, and that pack claims it on its next apply.
    let dropped = apply_pack(
        &Collection::open(root.path()).unwrap(),
        &pack_from("example.one", "1.1.0", &definitions[..2]),
    );
    assert!(dropped.valid, "{:?}", dropped.diagnostics);
    let claimed = apply_pack(
        &Collection::open(root.path()).unwrap(),
        &pack_from("example.two", "1.1.0", &definitions),
    );
    assert!(claimed.valid, "{:?}", claimed.diagnostics);
    assert_eq!(
        receipt_modes(root.path(), "example.two"),
        ["seed", "seed", "managed"]
    );
    assert!(root.path().join("_types/task.md").exists());
}

#[test]
fn different_bytes_for_a_managed_target_still_conflict() {
    let (root, collection) = collection();
    let definitions = task_resources();
    assert!(
        apply_pack(
            &collection,
            &pack_from("example.one", "1.0.0", &definitions)
        )
        .valid
    );
    let mut revised = definitions.clone();
    revised[1].3 = REVISED_CONTRACT;
    let assessment = Collection::open(root.path()).unwrap().assess_type_pack(
        &pack_from("example.two", "1.0.0", &revised),
        &assessment_options(),
    );
    assert_eq!(assessment.result["applicable"], false);
    assert_eq!(
        assessment.result["resources"][1]["reason"],
        "_contracts/example.task.md is managed by example.one with different content."
    );
}
