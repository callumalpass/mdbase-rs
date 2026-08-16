//! Long-running collection provider and runtime boundaries.
//!
//! The provider owns serialization of operations against one authority. The
//! filesystem runtime additionally couples successful mutations to the real
//! collection watcher so callers cannot observe a successful write before its
//! corresponding change is available.

mod api;
#[cfg(feature = "hosted-storage-benchmark")]
mod benchmark;
mod catalog;
mod context;
mod cursor;
mod diff;
mod external;
mod feed;
mod filesystem;
mod gate;
mod hosted_query;
mod observer;
mod operation;
mod outcome;
mod projection;
mod provider;
mod record_resolution;
mod record_structure;
mod snapshot;

pub use crate::data_contracts::{ResolvedRecordContract, ResolvedRecordContractImplementation};
pub use api::CollectionRuntime;
#[cfg(feature = "hosted-storage-benchmark")]
pub use benchmark::{
    BenchmarkDiagnostic, BenchmarkFileFacts, BenchmarkProjection, CandidateExpression,
    CandidateTruth, CompiledCandidate, ProjectionRelationship, QueryRequirements,
};
pub use catalog::{
    CanonicalRecordInput, CatalogError, CatalogInput, CompiledCatalog, ResolvedTypeResource,
};
pub use context::{OperationContext, OperationDeadline};
pub use filesystem::FilesystemRuntime;
pub use hosted_query::{
    CandidateComparison, CandidateComparisonOperator, CandidateComparisonPruning, CandidateField,
    CandidatePredicate, CandidateVerdict, CanonicalResidual, HostedAggregate, HostedGroup,
    HostedOrder, HostedOrderDirection, HostedQueryBudgets, HostedQueryPlan,
    HostedQueryRequirements, HostedResidualEvaluation, HostedSortSemantics, ProjectionAvailability,
    HOSTED_QUERY_PLAN_VERSION,
};
#[cfg(feature = "tracing")]
pub use observer::TracingObserver;
pub use observer::{
    ErrorReporting, ObserverOptions, OperationError, OperationPerformance, RuntimeObserver,
};
pub use operation::{invalid_operation_result, OperationKind, OperationRequest};
pub use outcome::{
    CancelOutcome, CanonicalChange, CanonicalFieldChangeSet, CanonicalTypeSet, ChangeBatch,
    ChangeBatchDescriptor, ChangeEventId, ChangeEventIdentity, ChangeFeed, ChangeFeedBaseline,
    ChangeFeedOwnerId, ChangeFeedTransfer, ChangeFeedTransferId, ChangeFeedTransferIntent,
    ChangeFeedTransferReceipt, ChangeOrigin, ChangePage, ChangePageCursor, ChangeSet,
    ChangeWatermark, CollectionGeneration, CommitAttempt, CommitId, CommitRejection,
    DurableCommitState, ExecutionOutcome, HostClaimId, PreparationOutcome, PreparedMutation,
    ReadCursor, ReadPage, RebuildReason, RecordChange, RecordChangeKind, ResourceChange,
    ResourceChangeKind, RuntimeChangeEvent, RuntimeChangeEventPage, RuntimeMeasurements,
};
pub use projection::{
    PreparedSemanticProjection, RecordResolutionKey, RecordResolutionKeyKind, SemanticFileFacts,
    SemanticProjection, SemanticProjectionFacts, SEMANTIC_PROJECTION_FORMAT_VERSION,
    SEMANTIC_PROJECTION_SCHEMA_VERSION,
};
pub use provider::{CollectionProvider, FilesystemProvider};
pub use record_resolution::{
    OccurrenceResolutionLookup, RecordResolutionPlan, ResolutionCandidate, ResolutionLookupKey,
    ResolvedRecordStructure, ResolvedStructuralOccurrence, MAX_RESOLUTION_CANDIDATES,
    MAX_RESOLUTION_LOOKUPS, MAX_STRUCTURAL_OCCURRENCES,
};
pub use record_structure::{
    parse_record_structure, RecordStructure, RecordStructureLink, RecordStructureModel,
    RecordStructureParser, StructuralLinkKind, StructuralOccurrence, StructuralResolution,
    StructuralSourceKind, RECORD_STRUCTURE_SCHEMA_VERSION,
};
pub(crate) use snapshot::is_schema_resource_path;
pub use snapshot::{
    CollectionSnapshot, CollectionSnapshotRecord, CollectionSnapshotResource,
    CollectionSnapshotResourceKind,
};

use crate::watch::WatchError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("collection failed to open: {0}")]
    CollectionOpen(String),
    #[error("unsupported collection operation: {0}")]
    UnsupportedOperation(String),
    #[error("collection provider operation lock is unavailable")]
    LockPoisoned,
    #[error("collection operation was cancelled before its durable boundary")]
    OperationCancelled,
    #[error("collection operation deadline elapsed before its durable boundary")]
    OperationDeadline,
    #[error("collection generation sequence is exhausted")]
    GenerationExhausted,
    #[error("collection change watermark is exhausted")]
    WatermarkExhausted,
    #[error("canonical change set is invalid: {0}")]
    InvalidChangeSet(String),
    #[error("canonical change page cursor is invalid or expired")]
    InvalidChangePage,
    #[error("host mutation claim was reused with different canonical input")]
    ClaimMismatch,
    #[error("runtime transaction capacity is exhausted")]
    RuntimeCapacityExhausted,
    #[error("durable transaction failed ({code}): {message}")]
    Transaction { code: &'static str, message: String },
    #[error("change feed is owned by a different capability")]
    ChangeFeedOwned,
    #[error("change feed handle was fenced by a newer open or transfer")]
    ChangeFeedFenced,
    #[error("change feed acknowledgement is invalid")]
    InvalidChangeFeedAck,
    #[error("change feed history before the retained baseline is unavailable")]
    ChangeFeedRetentionGap,
    #[error("change feed transfer intent does not match durable state")]
    ChangeFeedTransferMismatch,
    #[error("change feed capacity is exhausted until the owner acknowledges events")]
    ChangeFeedCapacityExhausted,
    #[error("generation-pinned read capacity is exhausted")]
    CursorCapacityExhausted,
    #[error("generation-pinned read expired or belongs to another runtime epoch")]
    GenerationExpired,
    #[error("generation-pinned read cursor is invalid")]
    InvalidReadCursor,
    #[error(transparent)]
    Watch(#[from] WatchError),
}

impl ProviderError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CollectionOpen(_) => "collection_open_failed",
            Self::UnsupportedOperation(_) => "unsupported_operation",
            Self::LockPoisoned => "operation_lock_unavailable",
            Self::OperationCancelled => "operation_cancelled",
            Self::OperationDeadline => "operation_deadline",
            Self::GenerationExhausted => "generation_exhausted",
            Self::WatermarkExhausted => "change_watermark_exhausted",
            Self::InvalidChangeSet(_) => "invalid_change_set",
            Self::InvalidChangePage => "invalid_change_page",
            Self::ClaimMismatch => "claim_mismatch",
            Self::RuntimeCapacityExhausted => "runtime_capacity_exhausted",
            Self::Transaction { code, .. } => code,
            Self::ChangeFeedOwned => "change_feed_owned",
            Self::ChangeFeedFenced => "change_feed_fenced",
            Self::InvalidChangeFeedAck => "invalid_change_feed_ack",
            Self::ChangeFeedRetentionGap => "change_feed_retention_gap",
            Self::ChangeFeedTransferMismatch => "change_feed_transfer_mismatch",
            Self::ChangeFeedCapacityExhausted => "change_feed_capacity_exhausted",
            Self::CursorCapacityExhausted => "cursor_capacity_exhausted",
            Self::GenerationExpired => "generation_expired",
            Self::InvalidReadCursor => "invalid_read_cursor",
            Self::Watch(_) => "watch_failed",
        }
    }
}

#[cfg(test)]
mod tests;
