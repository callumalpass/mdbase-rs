//! Version-neutral typed create/update/delete/rename mutation service.
//!
//! Phase 3 deliberately retains one full-collection shadow for recoverable
//! filesystem commit. The typed request and outcome never cross a JSON wire
//! envelope, and the persisted record is projected from the planned bytes.

mod batch;
mod lifecycle;
mod membership;
mod model;
mod preparation;
pub(crate) mod service;
pub(crate) mod shadow;

pub(crate) use batch::{
    aggregate_partial, batch, batch_wire, execute_partial_item, prepare_runtime_batch,
    BatchExecution, BatchWireOptions, RuntimeBatchPreparation,
};
pub(crate) use lifecycle::LifecycleEvent;
pub(crate) use membership::ResolvedWriteMembership;
pub(crate) use model::{
    diagnostic_from_issue, MutationFailure, MutationFailureKind, PlannedDelete, PlannedRecord,
    PlannedRename, PreparationOptions, PreparedCreate, PreparedDelete, PreparedRename,
    PreparedUpdate,
};
pub(crate) use preparation::{prepare_create, prepare_delete, prepare_rename, prepare_update};
pub(crate) use service::{
    create, delete, plan_delete, plan_rename, preflight_delete, preflight_rename, project_record,
    rename, staged_create, staged_delete, staged_rename, staged_update, update,
};
#[cfg(test)]
pub(crate) use service::{
    mutation_path_probes, probe_full_shadow, probe_legacy_parse, probe_request_value,
    probe_runtime_decode, probe_sparse_shadow, probe_wire_decode, reset_mutation_path_probes,
    MutationPathProbes,
};
pub(crate) use shadow::{
    collect_collection_files, collect_collection_files_context, shadow_collection,
    shadow_collection_context, ShadowCollection,
};
