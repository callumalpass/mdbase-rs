//! Typed Rust API for canonical mdbase operations.
#![warn(missing_docs)]

mod collection_path;
pub(crate) mod operations;
mod query;
mod typed;

pub use collection_path::{CollectionPath, CollectionPathError};
pub use query::{FrontmatterMode, QueryDirection, QueryOrder, QueryRequest, QueryResult};
pub use typed::{
    BatchDeletePreflightResult, BatchItemResult, BatchOperation, BatchOperationResult,
    BatchRenamePartialUpdates, BatchRenamePreflightResult, BatchRenameResult, BatchRequest,
    BatchResult, CreateRequest, DeletePreflightResult, DeleteRequest, DeleteResult, Diagnostic,
    DiagnosticCode, EmptyBatchOperationResult, MdbaseError, MdbaseResult, OperationOutcome,
    ReadRequest, RecordDocument, RecordFile, RenamePreflightResult, RenameRequest, RenameResult,
    Revision, Severity, TypedCollection, UpdateRequest, V02MigrationChange, V02MigrationRequest,
    V02MigrationResult,
};
