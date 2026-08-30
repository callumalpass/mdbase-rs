//! Typed Rust API for canonical mdbase operations.
#![warn(missing_docs)]

mod collection_path;
mod dynamic;
pub(crate) mod operations;
mod query;
pub(crate) mod typed;

pub use collection_path::{CollectionPath, CollectionPathError};
pub(crate) use dynamic::reference_evidence;
pub use dynamic::{ProjectedValue, QueryMetadata, ReferenceEvidence};
pub use query::{FrontmatterMode, QueryDirection, QueryOrder, QueryRequest, QueryResult};
pub use typed::{
    BackfillBatchResult, BackfillDetail, BackfillRequest, BackfillResult,
    BatchDeletePreflightResult, BatchItemResult, BatchOperation, BatchOperationResult,
    BatchRenamePartialUpdates, BatchRenamePreflightResult, BatchRenameResult, BatchRequest,
    BatchResult, CreateRequest, DeletePreflightResult, DeleteRequest, DeleteResult, Diagnostic,
    DiagnosticCode, EmptyBatchOperationResult, MdbaseError, MdbaseResult, OperationOutcome,
    ReadRequest, RecordDocument, RecordFile, RenamePreflightResult, RenameRequest, RenameResult,
    Revision, Severity, TypedCollection, UpdateRequest, V02MigrationChange, V02MigrationRequest,
    V02MigrationResult,
};
