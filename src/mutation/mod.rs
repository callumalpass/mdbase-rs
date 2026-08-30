//! Version-neutral typed create/update/delete mutation service.
//!
//! Phase 3 deliberately retains one full-collection shadow for recoverable
//! filesystem commit. The typed request and outcome never cross a JSON wire
//! envelope, and the persisted record is projected from the planned bytes.

mod lifecycle;
mod membership;
mod model;
mod preparation;
pub(crate) mod service;
pub(crate) mod shadow;

pub(crate) use lifecycle::LifecycleEvent;
pub(crate) use membership::ResolvedWriteMembership;
pub(crate) use model::{
    diagnostic_from_issue, MutationFailure, MutationFailureKind, PlannedDelete, PlannedRecord,
    PreparationOptions, PreparedCreate, PreparedDelete, PreparedUpdate,
};
pub(crate) use preparation::{prepare_create, prepare_delete, prepare_update};
pub(crate) use service::{
    create, delete, plan_delete, preflight_delete, project_record, staged_create, staged_delete,
    staged_update, update,
};
#[cfg(test)]
pub(crate) use service::{
    mutation_path_probes, probe_full_shadow, probe_hydration_read, probe_legacy_parse,
    probe_request_value, probe_result_decode, probe_runtime_decode, probe_sparse_shadow,
    probe_wire_decode, reset_mutation_path_probes, MutationPathProbes,
};
pub(crate) use shadow::{
    collect_collection_files, collect_collection_files_context, shadow_collection,
    shadow_collection_context, ShadowCollection,
};
