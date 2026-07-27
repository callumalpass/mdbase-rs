//! Long-running collection provider and runtime boundaries.
//!
//! The provider owns serialization of operations against one authority. The
//! filesystem runtime additionally couples successful mutations to the real
//! collection watcher so callers cannot observe a successful write before its
//! corresponding change is available.

mod filesystem;
mod observer;
mod operation;
mod provider;
mod snapshot;

pub use filesystem::FilesystemRuntime;
#[cfg(feature = "tracing")]
pub use observer::TracingObserver;
pub use observer::{
    ErrorReporting, ObserverOptions, OperationError, OperationPerformance, RuntimeObserver,
};
pub use operation::{invalid_operation_result, OperationKind, OperationRequest};
pub use provider::{CollectionProvider, FilesystemProvider};
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
    #[error("runtime contracts could not be initialized: {0}")]
    RuntimeContracts(String),
    #[error(transparent)]
    Watch(#[from] WatchError),
}

impl ProviderError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CollectionOpen(_) => "collection_open_failed",
            Self::UnsupportedOperation(_) => "unsupported_operation",
            Self::LockPoisoned => "operation_lock_unavailable",
            Self::RuntimeContracts(_) => "runtime_contracts_unavailable",
            Self::Watch(_) => "watch_failed",
        }
    }
}

#[cfg(test)]
mod tests;
