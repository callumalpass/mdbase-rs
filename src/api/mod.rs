//! Typed Rust API for canonical mdbase operations.
#![warn(missing_docs)]

mod collection_path;
pub(crate) mod operations;
mod typed;

pub use collection_path::{CollectionPath, CollectionPathError};
pub use typed::{
    BatchItemResult, BatchOperation, BatchRequest, BatchResult, CreateRequest,
    DeletePreflightResult, DeleteRequest, DeleteResult, Diagnostic, DiagnosticCode,
    FrontmatterMode, MdbaseError, MdbaseResult, OperationOutcome, QueryDirection, QueryOrder,
    QueryRequest, QueryResult, ReadRequest, RecordDocument, RecordFile, RenamePreflightResult,
    RenameRequest, RenameResult, Revision, Severity, TypedCollection, UpdateRequest,
    V02MigrationChange, V02MigrationRequest, V02MigrationResult,
};
