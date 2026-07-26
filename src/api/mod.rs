//! Typed Rust API for canonical mdbase operations.

mod collection_path;
pub(crate) mod operations;
mod typed;

pub use collection_path::{CollectionPath, CollectionPathError};
pub use typed::{
    CreateRequest, DeleteRequest, DeleteResult, Diagnostic, DiagnosticCode, FrontmatterMode,
    MdbaseError, MdbaseResult, MutationResult, OperationOutcome, QueryDirection, QueryOrder,
    QueryRequest, QueryResult, ReadRequest, ReadResult, RecordFile, RenameRequest, RenameResult,
    Revision, Severity, TypedCollection, UpdateRequest,
};
