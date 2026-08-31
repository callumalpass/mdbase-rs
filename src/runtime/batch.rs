use super::diff::canonical_changes;
use super::{
    ChangeSet, ExecutionOutcome, FilesystemRuntime, HostClaimId, OperationContext,
    OperationRequest, PreparationOutcome, PreparedMutation, ProviderError,
};
use crate::transactions::{self, RuntimePrepareOutcome};

pub(super) fn prepare(
    runtime: &FilesystemRuntime,
    collection: &crate::Collection,
    request: &OperationRequest,
    claim: &HostClaimId,
    digest: &str,
    context: &OperationContext,
) -> Result<PreparationOutcome, ProviderError> {
    if request
        .input
        .get("allow_partial")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return Ok(PreparationOutcome::NoMutation(ExecutionOutcome {
            result: crate::runtime::invalid_operation_result(
                "invalid_request",
                "allow_partial batches are not supported by the runtime: one host claim can settle only one atomic transaction",
            ),
            generation: runtime.current_generation()?,
            changes: ChangeSet::None,
            commit_id: None,
            change_event: None,
        }));
    }
    let (decoded, options) =
        match crate::v03::batch::decode_batch_request(collection, &request.input) {
            Ok(decoded) => decoded,
            Err(diagnostics) => {
                return Ok(PreparationOutcome::NoMutation(ExecutionOutcome {
                    result: crate::v03::OperationResult {
                        valid: false,
                        result: serde_json::json!({}),
                        diagnostics,
                    },
                    generation: runtime.current_generation()?,
                    changes: ChangeSet::None,
                    commit_id: None,
                    change_event: None,
                }));
            }
        };
    match crate::mutation::prepare_runtime_batch(collection, decoded, options, context)? {
        crate::mutation::RuntimeBatchPreparation::NoMutation(execution) => {
            Ok(PreparationOutcome::NoMutation(ExecutionOutcome {
                result: crate::v03::batch::batch_operation_result(execution),
                generation: runtime.current_generation()?,
                changes: ChangeSet::None,
                commit_id: None,
                change_event: None,
            }))
        }
        crate::mutation::RuntimeBatchPreparation::Prepared(plan) => {
            let result = crate::v03::batch::batch_operation_result(plan.execution);
            let changes = canonical_changes(&plan.before, &plan.after, Some(request))?;
            let event_id = super::ChangeEventId::generate();
            match transactions::prepare_runtime_transaction(
                collection,
                transactions::RuntimePrepareInput {
                    baseline: &plan.baseline,
                    desired: &plan.desired,
                    claim,
                    mutation_digest: digest,
                    result: &result,
                    changes: &changes,
                    event_id: &event_id,
                },
                context,
            )
            .map_err(super::filesystem::transaction_error)?
            {
                RuntimePrepareOutcome::NoMutation(result) => {
                    Ok(PreparationOutcome::NoMutation(ExecutionOutcome {
                        result,
                        generation: runtime.current_generation()?,
                        changes: ChangeSet::None,
                        commit_id: None,
                        change_event: None,
                    }))
                }
                RuntimePrepareOutcome::Prepared(commit_id) => {
                    Ok(PreparationOutcome::Prepared(PreparedMutation {
                        commit_id,
                        claim: claim.clone(),
                    }))
                }
                RuntimePrepareOutcome::Existing(_) => Err(ProviderError::Transaction {
                    code: "claim_already_finalized",
                    message: "host claim already resolves to a final durable state".to_string(),
                }),
            }
        }
    }
}
