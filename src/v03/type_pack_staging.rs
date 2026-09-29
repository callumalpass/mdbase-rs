//! Staging reviewed type packs into definitions-only workspaces.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use super::{
    failed, pack_diagnostic, pack_plan_error, plan_type_pack, read_type_pack_lock,
    stage_type_pack_plan, ManifestResource, TypePackApplyOptions, TypePackAssessmentOptions,
    TypePackProvision,
};
use crate::api::CollectionPath;
use crate::mutation::shadow as mutation_shadow;
use crate::v03::{Diagnostic, OperationResult};
use crate::Collection;

/// One reviewed type pack staged into a definitions-only workspace.
pub(crate) struct StagedTypePack {
    pub(crate) workspace: mutation_shadow::ShadowCollection,
    pub(crate) desired: crate::transactions::FileBaseline,
    pub(crate) result: OperationResult,
}

/// Plan, review and stage one type pack without copying records. Direct apply
/// and the runtime share it, so both enforce the same staleness, conflict and
/// downgrade checks. The outer error is capture infrastructure (deadline,
/// cancellation, limits); the inner one is the pack's own rejection.
pub(crate) fn stage_reviewed_type_pack(
    collection: &Collection,
    provision: &TypePackProvision,
    options: &TypePackApplyOptions,
    context: &crate::runtime::OperationContext,
) -> Result<Result<StagedTypePack, OperationResult>, crate::runtime::ProviderError> {
    let targets = match definition_targets(
        collection,
        std::iter::once((provision, &options.target_overrides)),
    ) {
        Ok(targets) => targets,
        Err(diagnostic) => return Ok(Err(failed(vec![*diagnostic]))),
    };
    let mut workspace = crate::v03::batch::definition_workspace(collection, &targets, context)?;
    let assessment_options = TypePackAssessmentOptions {
        installed_by: options.installed_by.clone(),
        adopt_resources: options.adopt_resources.clone(),
        preserve_seed_targets: options.preserve_seed_targets.clone(),
        target_overrides: options.target_overrides.clone(),
        contract_setups: options.contract_setups.clone(),
    };
    let plan = match plan_type_pack(&workspace.collection, provision, &assessment_options) {
        Ok(plan) => plan,
        Err(diagnostic) => return Ok(Err(failed(vec![*diagnostic]))),
    };
    if plan.assessment_digest != options.expected_assessment_digest {
        return Ok(Err(pack_diagnostic(
            "concurrent_modification",
            "The managed type-pack assessment is stale. Assess the collection again before applying it.",
        )));
    }
    if plan.assessment["applicable"].as_bool() != Some(true) {
        let reason = plan
            .resources
            .iter()
            .find(|resource| resource.action == "conflict")
            .and_then(|resource| resource.reason.as_deref())
            .unwrap_or("The managed type pack has unresolved conflicts.");
        return Ok(Err(pack_diagnostic("type_pack_conflict", reason)));
    }
    if plan.assessment["status"].as_str() == Some("downgrade") && !options.allow_downgrade {
        return Ok(Err(pack_diagnostic(
            "type_pack_downgrade",
            "A managed type-pack downgrade requires explicit approval.",
        )));
    }
    if let Err(diagnostic) = stage_type_pack_plan(&mut workspace, &plan) {
        return Ok(Err(failed(vec![*diagnostic])));
    }
    let desired =
        mutation_shadow::collect_collection_files_context(&workspace.collection, context)?;
    let mut result = plan.assessment;
    result["receipt"] = serde_json::to_value(
        plan.next_lock
            .packs
            .iter()
            .find(|receipt| receipt.id == result["desired"]["id"])
            .expect("desired receipt retained"),
    )
    .expect("receipt serializes");
    result["cleanup_deferred"] = Value::Bool(false);
    Ok(Ok(StagedTypePack {
        workspace,
        desired,
        result: OperationResult {
            valid: true,
            result,
            diagnostics: Vec::new(),
        },
    }))
}

/// Every managed target a type-pack plan may read or retire: the installed
/// lock's resources and each desired pack's (override-resolved) targets. The
/// live authority is checked for symlink components here because the
/// workspace that plans against these copies cannot observe them. Malformed
/// manifests, locks and targets are left to the canonical planner's diagnostics.
pub(crate) fn definition_targets<'a>(
    collection: &Collection,
    packs: impl IntoIterator<Item = (&'a TypePackProvision, &'a BTreeMap<String, String>)>,
) -> Result<BTreeSet<String>, Box<Diagnostic>> {
    let mut targets = BTreeSet::new();
    if let Ok((lock, _)) = read_type_pack_lock(collection) {
        targets.extend(
            lock.packs
                .into_iter()
                .flat_map(|pack| pack.resources)
                .map(|resource| resource.target),
        );
    }
    for (provision, overrides) in packs {
        let Ok(resources) = serde_json::from_value::<Vec<ManifestResource>>(
            provision
                .manifest
                .get("resources")
                .cloned()
                .unwrap_or(Value::Null),
        ) else {
            continue;
        };
        targets.extend(resources.into_iter().map(|resource| {
            overrides
                .get(&resource.target)
                .cloned()
                .unwrap_or(resource.target)
        }));
    }
    let mut safe = BTreeSet::new();
    for target in targets {
        let Ok(path) = CollectionPath::new(&target) else {
            continue;
        };
        collection
            .held_root()
            .ensure_no_symlink_components(Path::new(path.as_str()))
            .map_err(|error| pack_plan_error(format!("Unsafe type-pack target: {error}")))?;
        safe.insert(path.as_str().to_string());
    }
    Ok(safe)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        assessment_options, collection, manifest, provision, resource, task_resources, write,
    };
    use super::*;
    use serde_json::json;

    #[test]
    fn runtime_definition_mutations_copy_no_records_and_match_a_recapture() {
        use crate::runtime::OperationContext;
        use crate::v03::batch::{prepare_single_runtime, RuntimeSinglePreparation};
        let (root, _) = collection();
        write(
            &root.path().join("tasks/one.md"),
            "---\ntype: task\ntitle: One\n---\n",
        );
        write(
            &root.path().join("notes/plain.md"),
            "---\ntitle: Plain\n---\n",
        );
        let collection = Collection::open(root.path()).unwrap();
        let definitions = task_resources();
        let pack = provision(
            manifest(&definitions),
            definitions
                .iter()
                .map(|(_, source, _, document)| resource(source, document))
                .collect(),
        );
        let assessment = collection.assess_type_pack(&pack, &assessment_options());
        let options = TypePackApplyOptions {
            installed_by: "dev.mdbase.tests".to_string(),
            expected_assessment_digest: assessment.result["assessment_digest"]
                .as_str()
                .unwrap()
                .to_string(),
            allow_downgrade: false,
            adopt_resources: BTreeMap::new(),
            preserve_seed_targets: BTreeSet::new(),
            target_overrides: BTreeMap::new(),
            contract_setups: Vec::new(),
        };
        let context = OperationContext::internal();
        let is_record = |path: &String| path.starts_with("tasks/") || path.starts_with("notes/");
        for (operation, input) in [
            (
                "apply_type_pack",
                json!({"provision": pack, "options": options}),
            ),
            (
                "create_type",
                json!({"document": "---\nkind: mdbase.type\nname: note\nschema:\n  dialect: json-schema-2020-12\n  value: { type: object }\n---\n"}),
            ),
        ] {
            let collection = Collection::open(root.path()).unwrap();
            crate::mutation::reset_mutation_path_probes();
            let prepared =
                prepare_single_runtime(&collection, operation, &input, &context).unwrap();
            assert_eq!(crate::mutation::mutation_path_probes().full_shadows, 0);
            let RuntimeSinglePreparation::Prepared(plan) = prepared else {
                panic!("expected a {operation} plan")
            };
            assert!(!plan
                .baseline
                .keys()
                .chain(plan.desired.keys())
                .any(is_record));
            crate::transactions::commit_migration(&collection, &plan.baseline, &plan.desired)
                .unwrap();
            let recaptured = Collection::open(root.path()).unwrap().snapshot().unwrap();
            assert_eq!(plan.after, recaptured, "{operation}");
        }
    }
}
