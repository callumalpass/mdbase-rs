//! Seed-type upgrades from listed baselines, chosen by the seed's recorded
//! origin (v0.3 05A "Type Packs" and "Pack Identity And Portable Provenance").

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use mdbase::v03::{
    OperationResult, TypePackApplyOptions, TypePackAssessmentOptions, TypePackProvision,
    TypePackResource,
};
use mdbase::Collection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const TARGET: &str = "_types/note.md";

fn digest(document: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(document.as_bytes()))
}

/// The starter `note` type at publisher version `version`; each version adds
/// one property and changes the description.
fn note(version: u64) -> String {
    let properties = [
        "title: { type: string }",
        "created: { type: string }",
        "tags: { type: array }",
    ][..version as usize]
        .iter()
        .map(|property| format!("      {property}\n"))
        .collect::<String>();
    format!(
        "---\nkind: mdbase.type\nname: note\nversion: {version}\ndescription: Note v{version}.\n\
         schema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    properties:\n\
         {properties}    additionalProperties: true\n---\n# Note\n\nA note in this collection.\n"
    )
}

fn baseline(version: u64) -> Value {
    json!({ "digest": digest(&note(version)), "version": version, "document": note(version) })
}

/// Pack version `version` shipping `note(version)` as a seed.
fn pack(version: u64, upgrade_from: Option<Value>) -> TypePackProvision {
    let document = note(version);
    let mut resource = json!({
        "kind": "type", "mode": "seed", "source": "note.md", "target": TARGET,
        "digest": digest(&document),
    });
    if let Some(upgrade_from) = upgrade_from {
        resource["upgrade_from"] = upgrade_from;
    }
    TypePackProvision {
        manifest: json!({
            "kind": "mdbase.type-pack", "id": "example.seed-notes",
            "version": format!("{version}.0.0"), "resources": [resource],
        }),
        resources: vec![TypePackResource {
            source: "note.md".to_string(),
            document,
        }],
    }
}

fn collection() -> TempDir {
    let directory = tempfile::tempdir().expect("temp collection");
    fs::write(
        directory.path().join("mdbase.yaml"),
        "spec_version: \"0.3.0\"\nsettings:\n  validation: error\n",
    )
    .expect("write config");
    directory
}

fn options(preserve: &[&str]) -> TypePackAssessmentOptions {
    TypePackAssessmentOptions {
        installed_by: "dev.mdbase.tests".to_string(),
        preserve_seed_targets: preserve.iter().map(|target| target.to_string()).collect(),
        ..Default::default()
    }
}

fn assess(root: &Path, pack: &TypePackProvision) -> OperationResult {
    Collection::open(root)
        .expect("open collection")
        .assess_type_pack(pack, &options(&[]))
}

/// Assesses and applies `pack`, returning the reviewed assessment.
fn apply_with(root: &Path, pack: &TypePackProvision, preserve: &[&str]) -> Value {
    let collection = Collection::open(root).expect("open collection");
    let assessment = collection.assess_type_pack(pack, &options(preserve));
    assert!(assessment.valid, "{:?}", assessment.diagnostics);
    assert_eq!(
        assessment.result["applicable"], true,
        "{:#}",
        assessment.result
    );
    let applied = collection.apply_type_pack(
        pack,
        &TypePackApplyOptions {
            installed_by: "dev.mdbase.tests".to_string(),
            expected_assessment_digest: assessment.result["assessment_digest"]
                .as_str()
                .expect("assessment digest")
                .to_string(),
            allow_downgrade: false,
            adopt_resources: BTreeMap::new(),
            preserve_seed_targets: preserve
                .iter()
                .map(|target| target.to_string())
                .collect::<BTreeSet<_>>(),
            target_overrides: BTreeMap::new(),
            contract_setups: Vec::new(),
        },
    );
    assert!(applied.valid, "{:?}", applied.diagnostics);
    assessment.result["resources"][0].clone()
}

fn apply(root: &Path, pack: &TypePackProvision) -> Value {
    apply_with(root, pack, &[])
}

fn lock(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(root.join("mdbase.lock.yaml")).expect("read lock"))
        .expect("lock is JSON")
}

fn origin(root: &Path) -> Option<String> {
    lock(root)["packs"][0]["resources"][0]
        .get("origin_digest")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn target(root: &Path) -> String {
    fs::read_to_string(root.join(TARGET)).expect("read target")
}

fn edit(root: &Path, old: &str, new: &str) {
    let edited = target(root).replacen(old, new, 1);
    fs::write(root.join(TARGET), edited).expect("edit target");
}

fn v3() -> TypePackProvision {
    pack(3, Some(json!([baseline(2), baseline(1)])))
}

#[test]
fn single_object_and_list_forms_upgrade_an_unedited_seed_to_the_exact_bytes() {
    for upgrade_from in [baseline(1), json!([baseline(1)])] {
        let directory = collection();
        let root = directory.path();
        let created = apply(root, &pack(1, None));
        assert_eq!(created["action"], "create");
        assert_eq!(origin(root), Some(digest(&note(1))));
        let updated = apply(root, &pack(2, Some(upgrade_from)));
        assert_eq!(updated["action"], "update");
        assert_eq!(
            updated["upgrade_baseline"],
            json!({ "digest": digest(&note(1)), "version": 1 })
        );
        assert_eq!(target(root), note(2));
        assert_eq!(origin(root), Some(digest(&note(2))));
        // The seed's installed digest stays the pack resource digest.
        assert_eq!(
            lock(root)["packs"][0]["resources"][0]["digest"],
            digest(&note(2))
        );
    }
}

#[test]
fn an_edited_seed_merges_against_its_origin_and_then_stays_preserved() {
    let directory = collection();
    let root = directory.path();
    apply(root, &pack(1, None));
    edit(root, "# Note\n", "# My notes\n");
    edit(
        root,
        "      title: { type: string }\n",
        "      title: { type: string }\n      mood: { type: string }\n",
    );
    let updated = apply(root, &v3());
    assert_eq!(updated["action"], "update");
    assert_eq!(updated["upgrade_baseline"]["version"], 1);
    let merged = target(root);
    let (frontmatter, body) = merged[4..].split_once("\n---\n").unwrap();
    let frontmatter: Value = serde_yaml::from_str(frontmatter).unwrap();
    assert_eq!(frontmatter["version"], 3);
    assert_eq!(frontmatter["description"], "Note v3.");
    let properties = &frontmatter["schema"]["value"]["properties"];
    assert!(["title", "created", "tags", "mood"]
        .iter()
        .all(|key| properties.get(key).is_some()));
    assert_eq!(body, "# My notes\n\nA note in this collection.\n");
    assert_eq!(origin(root), Some(digest(&note(3))));
    // Edited after its upgrade: the origin is the desired document.
    edit(root, "# My notes\n", "# Our notes\n");
    let repeat = assess(root, &v3()).result;
    assert_eq!(repeat["status"], "current");
    assert_eq!(repeat["resources"][0]["action"], "preserve");
    assert!(repeat["resources"][0].get("reason").is_none());
}

#[test]
fn origin_survives_a_pack_version_that_preserves_the_seed() {
    let directory = collection();
    let root = directory.path();
    apply(root, &pack(1, None));
    // A version without upgrade_from preserves the unedited v1 seed.
    let preserved = apply(root, &pack(2, None));
    assert_eq!(preserved["action"], "preserve");
    assert_eq!(origin(root), Some(digest(&note(1))));
    edit(root, "# Note\n", "# My notes\n");
    let updated = apply(root, &v3());
    assert_eq!(updated["action"], "update");
    assert_eq!(updated["upgrade_baseline"]["version"], 1);
    assert!(target(root).contains("created") && target(root).contains("# My notes"));
}

#[test]
fn an_unknown_or_unlisted_origin_is_preserved_with_a_reason_without_conflict() {
    // Unknown: the target existed before the pack was installed.
    let directory = collection();
    let root = directory.path();
    fs::create_dir_all(root.join("_types")).unwrap();
    let own = note(1).replace("Note v1.", "The collection's own note.");
    fs::write(root.join(TARGET), &own).unwrap();
    assert_eq!(apply(root, &pack(1, None))["action"], "preserve");
    assert_eq!(origin(root), None);
    let preserved = apply(root, &v3());
    assert_eq!(preserved["action"], "preserve");
    assert!(preserved["reason"].is_string());
    assert!(preserved.get("upgrade_baseline").is_none());
    assert_eq!(target(root), own);
    assert_eq!(origin(root), None);

    // Unlisted: the origin is v1 but only v2 is a supported baseline.
    let directory = collection();
    let root = directory.path();
    apply(root, &pack(1, None));
    edit(root, "# Note\n", "# My notes\n");
    let edited = target(root);
    let preserved = apply(root, &pack(3, Some(baseline(2))));
    assert_eq!(preserved["action"], "preserve");
    assert!(preserved["reason"].is_string());
    assert_eq!(target(root), edited);
    assert_eq!(origin(root), Some(digest(&note(1))));
}

#[test]
fn a_lock_written_before_origins_preserves_an_edited_seed() {
    let directory = collection();
    let root = directory.path();
    apply(root, &pack(1, None));
    let mut legacy = lock(root);
    legacy["packs"][0]["resources"][0]
        .as_object_mut()
        .unwrap()
        .remove("origin_digest");
    fs::write(
        root.join("mdbase.lock.yaml"),
        serde_json::to_vec_pretty(&legacy).unwrap(),
    )
    .unwrap();
    edit(root, "# Note\n", "# My notes\n");
    let assessment = assess(root, &v3()).result;
    assert_eq!(assessment["resources"][0]["action"], "preserve");
    assert!(assessment["resources"][0]["reason"].is_string());
}

#[test]
fn bytes_equal_to_a_baseline_prove_the_origin_whatever_the_lock_records() {
    let directory = collection();
    let root = directory.path();
    apply(root, &pack(1, None));
    apply(root, &v3());
    assert_eq!(origin(root), Some(digest(&note(3))));
    // Reverted to the exact v2 starter after upgrading to v3.
    fs::write(root.join(TARGET), note(2)).unwrap();
    let updated = assess(root, &v3()).result;
    assert_eq!(updated["resources"][0]["action"], "update");
    assert_eq!(updated["resources"][0]["upgrade_baseline"]["version"], 2);
    assert_eq!(updated["resources"][0]["digest"], digest(&note(3)));
}

#[test]
fn competing_edits_still_fail_closed_and_preserved_targets_are_not_upgraded() {
    let directory = collection();
    let root = directory.path();
    apply(root, &pack(1, None));
    edit(root, "description: Note v1.", "description: Mine.");
    let assessment = assess(root, &v3()).result;
    assert_eq!(assessment["status"], "conflict");
    assert_eq!(assessment["resources"][0]["action"], "conflict");

    let directory = collection();
    let root = directory.path();
    apply(root, &pack(1, None));
    let preserved = apply_with(root, &v3(), &[TARGET]);
    assert_eq!(preserved["action"], "preserve");
    assert_eq!(target(root), note(1));
    assert_eq!(origin(root), Some(digest(&note(1))));
}

#[test]
fn invalid_baselines_reject_the_manifest() {
    let mut tampered = baseline(1);
    tampered["document"] = json!(note(1).replace("Note v1.", "Tampered."));
    let mut renamed = baseline(1);
    renamed["document"] = json!(note(1).replace("name: note", "name: memo"));
    renamed["digest"] = json!(digest(renamed["document"].as_str().unwrap()));
    let mut rekinded = baseline(1);
    rekinded["document"] = json!(note(1).replace("kind: mdbase.type", "kind: mdbase.view"));
    rekinded["digest"] = json!(digest(rekinded["document"].as_str().unwrap()));
    let mut misversioned = baseline(1);
    misversioned["version"] = json!(2);
    let unversioned = json!({ "digest": digest(&note(1)), "document": note(1) });
    let cases = [
        ("digest", json!(tampered)),
        ("duplicate", json!([baseline(1), unversioned])),
        ("self", baseline(2)),
        ("name", renamed),
        ("kind", rekinded),
        ("version", misversioned),
    ];
    for (rule, upgrade_from) in cases {
        let directory = collection();
        let root = directory.path();
        let pack = pack(2, Some(upgrade_from));
        let result = assess(root, &pack);
        assert!(!result.valid, "{rule} must be rejected");
        assert_eq!(result.diagnostics[0].code, "invalid_type_pack", "{rule}");
        // Even a fresh install publishes nothing.
        let applied = Collection::open(root).unwrap().apply_type_pack(
            &pack,
            &TypePackApplyOptions {
                installed_by: "dev.mdbase.tests".to_string(),
                expected_assessment_digest: digest("unreviewed"),
                allow_downgrade: false,
                adopt_resources: BTreeMap::new(),
                preserve_seed_targets: BTreeSet::new(),
                target_overrides: BTreeMap::new(),
                contract_setups: Vec::new(),
            },
        );
        assert_eq!(applied.diagnostics[0].code, "invalid_type_pack", "{rule}");
        assert!(!root.join(TARGET).exists() && !root.join("mdbase.lock.yaml").exists());
    }
    let mut managed = pack(2, Some(baseline(1)));
    managed.manifest["resources"][0]["mode"] = json!("managed");
    let result = assess(collection().path(), &managed);
    assert!(!result.valid);
    assert_eq!(result.diagnostics[0].code, "invalid_type_pack");
    assert!(!assess(collection().path(), &pack(2, Some(json!([])))).valid);
    // A version-free baseline is valid.
    let unversioned = json!({ "digest": digest(&note(1)), "document": note(1) });
    assert!(assess(collection().path(), &pack(2, Some(unversioned))).valid);
}

#[test]
fn recording_only_an_origin_keeps_the_pack_current() {
    let directory = collection();
    let root = directory.path();
    apply(root, &v3());
    let mut legacy = lock(root);
    legacy["packs"][0]["resources"][0]
        .as_object_mut()
        .unwrap()
        .remove("origin_digest");
    fs::write(
        root.join("mdbase.lock.yaml"),
        serde_json::to_vec_pretty(&legacy).unwrap(),
    )
    .unwrap();
    let assessment = assess(root, &v3()).result;
    assert_eq!(assessment["status"], "current");
    assert_eq!(assessment["lock"]["action"], "update");
    assert_eq!(apply(root, &v3())["action"], "preserve");
    assert_eq!(origin(root), Some(digest(&note(3))));
    assert_eq!(assess(root, &v3()).result["lock"]["action"], "unchanged");
}

#[test]
fn byte_equality_sets_the_origin_of_predating_and_intentionally_preserved_targets() {
    for preserve in [&[][..], &[TARGET][..]] {
        let directory = collection();
        let root = directory.path();
        fs::create_dir_all(root.join("_types")).unwrap();
        fs::write(root.join(TARGET), note(1)).unwrap();
        assert_eq!(
            apply_with(root, &pack(1, None), preserve)["action"],
            "preserve"
        );
        assert_eq!(origin(root), Some(digest(&note(1))));
    }
}
