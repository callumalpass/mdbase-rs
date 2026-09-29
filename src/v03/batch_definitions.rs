//! Runtime preparation for mutations that change only definition resources.
use serde_json::Value;

use super::{
    adapt_mtime_precondition, collect_collection_files_context, copy_sparse_controls,
    copy_sparse_resource, execute_non_record_runtime_operation, invalid_request, Operations,
    RuntimeMutationPlan, RuntimeSinglePreparation, ShadowCollection,
};
use crate::runtime::{CanonicalOperationOutcome, OperationContext, OperationKind, ProviderError};
use crate::Collection;
use std::path::Path;

/// A definitions-only working copy for mutations that change nothing but
/// definition resources: configuration, both locks, the type and contract
/// folders, their referenced schemas, and `targets` (managed resources such as
/// schemas that may live outside those folders). Records are never copied, so
/// preparation follows the definitions rather than the collection. The baseline
/// is exactly the captured bytes, which the transaction compares at commit.
pub(crate) fn definition_workspace(
    collection: &Collection,
    targets: &std::collections::BTreeSet<String>,
    context: &OperationContext,
) -> Result<ShadowCollection, ProviderError> {
    context.check()?;
    let directory =
        tempfile::tempdir().map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
    let mut captured_entries = 0_u64;
    let mut resource_entries = 0_u64;
    copy_sparse_controls(
        collection,
        directory.path(),
        context,
        &mut captured_entries,
        &mut resource_entries,
    )?;
    for target in targets {
        let relative = Path::new(target);
        if !directory.path().join(relative).is_file()
            && collection.held_root().exists_file(relative)
        {
            copy_sparse_resource(
                collection,
                directory.path(),
                relative,
                context,
                &mut captured_entries,
                &mut resource_entries,
            )?;
        }
    }
    context.check()?;
    let staged = Collection::open(directory.path())
        .map_err(|error| ProviderError::CollectionOpen(format!("{error:?}")))?;
    let baseline = collect_collection_files_context(&staged, context)?;
    Ok(ShadowCollection {
        directory,
        collection: staged,
        baseline,
    })
}

/// Prepare a mutation that changes only definition resources in a
/// definitions-only workspace. Records are neither copied nor re-read after the
/// `before` capture: the `after` snapshot reinterprets them under the staged
/// definitions.
pub(super) fn prepare_definition_runtime(
    collection: &Collection,
    operation: &str,
    input: &Value,
    context: &OperationContext,
) -> Result<RuntimeSinglePreparation, ProviderError> {
    let kind = operation.parse::<OperationKind>()?;
    let staged = if kind == OperationKind::ApplyTypePack {
        let decode = |key: &str| input.get(key).cloned().unwrap_or(Value::Null);
        match (
            serde_json::from_value(decode("provision")),
            serde_json::from_value(decode("options")),
        ) {
            (Ok(provision), Ok(options)) => crate::v03::type_pack::stage_reviewed_type_pack(
                collection, &provision, &options, context,
            )?
            .map(|staged| (staged.result, staged.workspace, staged.desired)),
            _ => Err(invalid_request(
                "Type-pack apply input requires valid provision and options.",
            )),
        }
    } else {
        let workspace = definition_workspace(collection, &Default::default(), context)?;
        let input = match adapt_mtime_precondition(collection, &workspace.collection, input) {
            Ok(input) => input,
            Err(diagnostic) => {
                return Ok(RuntimeSinglePreparation::NoMutation(
                    CanonicalOperationOutcome::invalid(kind, vec![(*diagnostic).into()]),
                ))
            }
        };
        let operations = Operations::new(&workspace.collection)
            .map_err(|diagnostic| ProviderError::CollectionOpen(diagnostic.message.clone()))?;
        let result = execute_non_record_runtime_operation(&operations, operation, &input);
        if result.valid {
            let desired = collect_collection_files_context(&workspace.collection, context)?;
            Ok((result, workspace, desired))
        } else {
            Err(result)
        }
    };
    let (result, staged) = match staged {
        Ok((result, workspace, desired)) => (result, Some((workspace, desired))),
        Err(result) => (result, None),
    };
    let outcome = if kind == OperationKind::ApplyTypePack {
        CanonicalOperationOutcome::definition(kind, result)?
    } else {
        CanonicalOperationOutcome::recover_v03(kind, result)?
    };
    let Some((workspace, desired)) =
        staged.filter(|(workspace, desired)| *desired != workspace.baseline)
    else {
        return Ok(RuntimeSinglePreparation::NoMutation(outcome));
    };
    context.check()?;
    let staged = Collection::open(workspace.directory.path())
        .map_err(|error| ProviderError::CollectionOpen(format!("{error:?}")))?;
    let before = collection.snapshot_with_context(context)?;
    let after = crate::runtime::definition_change_snapshot(
        collection,
        &before,
        &staged,
        &workspace.baseline,
        &desired,
        context,
    )?;
    Ok(RuntimeSinglePreparation::Prepared(Box::new(
        RuntimeMutationPlan {
            operation: outcome,
            baseline: workspace.baseline,
            desired,
            before,
            after,
        },
    )))
}
