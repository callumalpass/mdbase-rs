use super::diff::canonical_changes;
use super::{
    CanonicalOperationOutcome, ChangeSet, ExecutionOutcome, FilesystemRuntime, HostClaimId,
    OperationContext, OperationRequest, PreparationOutcome, PreparedMutation, ProviderError,
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
        return Ok(PreparationOutcome::NoMutation(ExecutionOutcome::new(
            CanonicalOperationOutcome::invalid(
                request.operation,
                vec![crate::api::Diagnostic {
                    severity: crate::api::Severity::Error,
                    code: crate::api::DiagnosticCode::new("invalid_request"),
                    message: "allow_partial batches are not supported by the runtime: one host claim can settle only one atomic transaction".to_string(),
                    path: None,
                    field: None,
                    type_name: None,
                    schema_location: None,
                    details: None,
                }],
            ),
            runtime.current_generation()?,
            ChangeSet::None,
            None,
            None,
        )));
    }
    let (decoded, options) =
        match crate::v03::batch::decode_batch_request(collection, &request.input) {
            Ok(decoded) => decoded,
            Err(diagnostics) => {
                return Ok(PreparationOutcome::NoMutation(ExecutionOutcome::new(
                    CanonicalOperationOutcome::invalid(
                        request.operation,
                        diagnostics.into_iter().map(Into::into).collect(),
                    ),
                    runtime.current_generation()?,
                    ChangeSet::None,
                    None,
                    None,
                )));
            }
        };
    match crate::mutation::prepare_runtime_batch(collection, decoded, options, context)? {
        crate::mutation::RuntimeBatchPreparation::NoMutation(execution) => {
            Ok(PreparationOutcome::NoMutation(ExecutionOutcome::new(
                CanonicalOperationOutcome::batch(execution),
                runtime.current_generation()?,
                ChangeSet::None,
                None,
                None,
            )))
        }
        crate::mutation::RuntimeBatchPreparation::Prepared(plan) => {
            let operation = CanonicalOperationOutcome::batch(plan.execution);
            let changes = canonical_changes(&plan.before, &plan.after, Some(request))?;
            let event_id = super::ChangeEventId::generate();
            match transactions::prepare_runtime_transaction(
                collection,
                transactions::RuntimePrepareInput {
                    baseline: &plan.baseline,
                    desired: &plan.desired,
                    claim,
                    mutation_digest: digest,
                    operation: &operation,
                    changes: &changes,
                    event_id: &event_id,
                },
                context,
            )
            .map_err(super::filesystem::transaction_error)?
            {
                RuntimePrepareOutcome::NoMutation(operation) => {
                    Ok(PreparationOutcome::NoMutation(ExecutionOutcome::new(
                        operation,
                        runtime.current_generation()?,
                        ChangeSet::None,
                        None,
                        None,
                    )))
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
