//! Long-running collection provider and runtime boundaries.
//!
//! The provider owns serialization of operations against one authority. The
//! filesystem runtime additionally couples successful mutations to the real
//! collection watcher so callers cannot observe a successful write before its
//! corresponding change is available.

use crate::v03::{Diagnostic, OperationResult};
use crate::watch::{CollectionWatcher, WatchError, WatchEvent};
use crate::Collection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Read,
    Query,
    Validate,
    Create,
    Update,
    Delete,
    Rename,
}

impl OperationKind {
    pub fn is_mutation(self) -> bool {
        matches!(
            self,
            Self::Create | Self::Update | Self::Delete | Self::Rename
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Query => "query",
            Self::Validate => "validate",
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Rename => "rename",
        }
    }
}

impl FromStr for OperationKind {
    type Err = ProviderError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "read" => Ok(Self::Read),
            "query" => Ok(Self::Query),
            "validate" => Ok(Self::Validate),
            "create" => Ok(Self::Create),
            "update" => Ok(Self::Update),
            "delete" => Ok(Self::Delete),
            "rename" => Ok(Self::Rename),
            other => Err(ProviderError::UnsupportedOperation(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationRequest {
    pub operation: OperationKind,
    #[serde(default)]
    pub input: Value,
}

impl OperationRequest {
    pub fn new(operation: OperationKind, input: Value) -> Self {
        Self { operation, input }
    }
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("collection failed to open: {0}")]
    CollectionOpen(String),
    #[error("unsupported collection operation: {0}")]
    UnsupportedOperation(String),
    #[error("collection provider operation lock is unavailable")]
    LockPoisoned,
    #[error(transparent)]
    Watch(#[from] WatchError),
}

/// Provider-neutral execution boundary for one authoritative collection.
///
/// Providers return the normative v0.3 operation envelope. Implementations
/// must serialize mutations or provide equivalent compare-and-swap semantics.
pub trait CollectionProvider: Send + Sync {
    fn execute(&self, request: &OperationRequest) -> Result<OperationResult, ProviderError>;
    fn refresh(&self) -> Result<(), ProviderError>;
}

/// Filesystem-backed provider using the canonical mdbase operation engine.
///
/// A persistent gate serializes requests from local control, relay, watcher,
/// and future replica paths. The collection is opened inside the gate so
/// external config, type, and record changes are visible to the next request.
#[derive(Debug)]
pub struct FilesystemProvider {
    root: PathBuf,
    operation_gate: Mutex<()>,
}

impl FilesystemProvider {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ProviderError> {
        let root = root.as_ref().to_path_buf();
        open_collection(&root)?;
        Ok(Self {
            root,
            operation_gate: Mutex::new(()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Execute a compound provider operation against one freshly loaded
    /// collection while retaining the authority's serialization gate.
    ///
    /// Hosts use this for policy checks that must remain atomic with the
    /// resulting read or mutation.
    pub fn with_collection<T, E>(
        &self,
        operation: impl FnOnce(&Collection) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<ProviderError>,
    {
        let _guard = self.lock().map_err(E::from)?;
        let collection = open_collection(&self.root).map_err(E::from)?;
        operation(&collection)
    }

    fn lock(&self) -> Result<MutexGuard<'_, ()>, ProviderError> {
        self.operation_gate
            .lock()
            .map_err(|_| ProviderError::LockPoisoned)
    }
}

impl CollectionProvider for FilesystemProvider {
    fn execute(&self, request: &OperationRequest) -> Result<OperationResult, ProviderError> {
        self.with_collection(|collection| execute_collection(collection, request))
    }

    fn refresh(&self) -> Result<(), ProviderError> {
        let _guard = self.lock()?;
        open_collection(&self.root).map(|_| ())
    }
}

/// Filesystem authority with consistent operation and notification ordering.
pub struct FilesystemRuntime {
    provider: Arc<FilesystemProvider>,
    watcher: CollectionWatcher,
}

impl FilesystemRuntime {
    pub fn open(root: impl AsRef<Path>, debounce: Duration) -> Result<Self, ProviderError> {
        let provider = Arc::new(FilesystemProvider::open(root.as_ref())?);
        let watcher = CollectionWatcher::open(root, debounce)?;
        Ok(Self { provider, watcher })
    }

    pub fn provider(&self) -> Arc<FilesystemProvider> {
        self.provider.clone()
    }

    pub fn execute(&self, request: &OperationRequest) -> Result<OperationResult, ProviderError> {
        self.provider.with_collection(|collection| {
            let result = execute_collection(collection, request)?;
            if request.operation.is_mutation() && result.valid {
                // Keep the provider gate until the watcher has compared the
                // post-write snapshot. Another writer therefore cannot make
                // this mutation's notification describe a later state.
                self.watcher.rescan()?;
            }
            Ok(result)
        })
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<WatchEvent>, ProviderError> {
        self.watcher.recv_timeout(timeout).map_err(Into::into)
    }
}

fn open_collection(root: &Path) -> Result<Collection, ProviderError> {
    Collection::open(root).map_err(|error| ProviderError::CollectionOpen(error_message(&error)))
}

fn execute_collection(
    collection: &Collection,
    request: &OperationRequest,
) -> Result<OperationResult, ProviderError> {
    let operations = collection
        .v03_operations()
        .map_err(|diagnostic| ProviderError::CollectionOpen(diagnostic.message.clone()))?;
    Ok(match request.operation {
        OperationKind::Read => operations.read(&request.input),
        OperationKind::Query => operations.query(&request.input),
        OperationKind::Validate => operations.validate(&request.input),
        OperationKind::Create => operations.create(&request.input),
        OperationKind::Update => operations.update(&request.input),
        OperationKind::Delete => operations.delete(&request.input),
        OperationKind::Rename => operations.rename(&request.input),
    })
}

fn error_message(error: &Value) -> String {
    error
        .pointer("/error/message")
        .or_else(|| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string())
}

pub fn invalid_operation_result(code: &str, message: impl Into<String>) -> OperationResult {
    OperationResult {
        valid: false,
        result: json!({}),
        diagnostics: vec![Diagnostic::error(code, message, None)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn collection() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  default_validation: error\n",
        )
        .unwrap();
        directory
    }

    #[test]
    fn provider_observes_external_changes_between_requests() {
        let directory = collection();
        let provider = FilesystemProvider::open(directory.path()).unwrap();
        fs::write(
            directory.path().join("external.md"),
            "---\ntitle: External\n---\nBody\n",
        )
        .unwrap();

        let read = provider
            .execute(&OperationRequest::new(
                OperationKind::Read,
                json!({"path": "external.md"}),
            ))
            .unwrap();
        assert!(read.valid);
        assert_eq!(read.result["frontmatter"]["title"], "External");
    }

    #[test]
    fn provider_serializes_conditional_writers() {
        let directory = collection();
        fs::write(
            directory.path().join("task.md"),
            "---\ntitle: Original\n---\n",
        )
        .unwrap();
        let provider = Arc::new(FilesystemProvider::open(directory.path()).unwrap());
        let read = provider
            .execute(&OperationRequest::new(
                OperationKind::Read,
                json!({"path": "task.md"}),
            ))
            .unwrap();
        let revision = read.result["revision"].as_str().unwrap().to_string();
        let barrier = Arc::new(Barrier::new(3));

        let handles = ["First", "Second"].map(|title| {
            let provider = provider.clone();
            let barrier = barrier.clone();
            let revision = revision.clone();
            thread::spawn(move || {
                barrier.wait();
                provider
                    .execute(&OperationRequest::new(
                        OperationKind::Update,
                        json!({
                            "path": "task.md",
                            "fields": {"title": title},
                            "if_revision": revision,
                        }),
                    ))
                    .unwrap()
            })
        });
        barrier.wait();
        let results = handles.map(|handle| handle.join().unwrap());

        assert_eq!(results.iter().filter(|result| result.valid).count(), 1);
        assert_eq!(results.iter().filter(|result| !result.valid).count(), 1);
        assert!(results.iter().any(|result| {
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "concurrent_modification")
        }));
    }

    #[test]
    fn runtime_queues_change_before_successful_mutation_returns() {
        let directory = collection();
        let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(40)).unwrap();
        let created = runtime
            .execute(&OperationRequest::new(
                OperationKind::Create,
                json!({
                    "path": "created.md",
                    "frontmatter": {"title": "Created"},
                }),
            ))
            .unwrap();
        assert!(created.valid, "{created:#?}");

        let event = runtime
            .recv_timeout(Duration::ZERO)
            .unwrap()
            .expect("successful mutation must queue a change before returning");
        assert_eq!(event.event_type, "mdbase.record.created");
        assert_eq!(event.payload["path"], "created.md");
        assert_eq!(event.payload["after"]["title"], "Created");
    }

    #[test]
    fn operation_kind_has_a_stable_wire_shape() {
        let request = OperationRequest::new(OperationKind::Rename, json!({"from": "a.md"}));
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({"operation": "rename", "input": {"from": "a.md"}})
        );
        assert_eq!(
            "query".parse::<OperationKind>().unwrap(),
            OperationKind::Query
        );
        assert!("unknown".parse::<OperationKind>().is_err());
    }
}
