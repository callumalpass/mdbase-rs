//! Fallible, operation-scoped views of authoritative collection data.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

use crate::expressions::evaluator::ResolvedFileData;
use crate::query::cache_source::{FileRecord, InvalidRecordStub};

/// A consistent set of records loaded for one read operation.
///
/// The SQLite cache may supply the records, but Markdown remains authoritative.
/// Cache failures are handled before this value is constructed.
pub(crate) struct CollectionSnapshot {
    pub records: Vec<FileRecord>,
    pub invalid_records: Vec<InvalidRecordStub>,
    pub all_files: Option<Arc<Vec<ResolvedFileData>>>,
    pub backlinks: Option<Arc<HashMap<String, Vec<String>>>>,
}

/// Failure while discovering collection resources.
#[derive(Debug, Error)]
pub(crate) enum CollectionScanError {
    #[error("failed to read collection directory '{}': {source}", path.display())]
    ReadDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "failed to read an entry in collection directory '{}': {source}",
        directory.display()
    )]
    ReadEntry {
        directory: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to inspect collection entry '{}': {source}", path.display())]
    InspectEntry {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("collection entry is outside the configured root: {}", path.display())]
    OutsideRoot { path: PathBuf },
}

/// Failure while constructing an authoritative collection snapshot.
#[derive(Debug, Error)]
pub(crate) enum SnapshotError {
    #[error("collection operation cancelled")]
    Cancelled,
    #[error("coordinated runtime cache is unavailable: {0}")]
    Cache(String),
    #[error(transparent)]
    Scan(#[from] CollectionScanError),
    #[error("collection entry is outside the configured root: {}", path.display())]
    OutsideRoot { path: PathBuf },
    #[error("failed to read collection file '{}': {source}", path.display())]
    ReadFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl SnapshotError {
    pub(crate) fn path(&self) -> Option<&Path> {
        match self {
            Self::Cancelled | Self::Cache(_) => None,
            Self::Scan(CollectionScanError::ReadDirectory { path, .. })
            | Self::Scan(CollectionScanError::InspectEntry { path, .. })
            | Self::Scan(CollectionScanError::OutsideRoot { path })
            | Self::OutsideRoot { path }
            | Self::ReadFile { path, .. } => Some(path),
            Self::Scan(CollectionScanError::ReadEntry { directory, .. }) => Some(directory),
        }
    }
}
