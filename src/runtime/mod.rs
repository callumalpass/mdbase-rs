//! Long-running collection provider and runtime boundaries.
//!
//! The provider owns serialization of operations against one authority. The
//! filesystem runtime additionally couples successful mutations to the real
//! collection watcher so callers cannot observe a successful write before its
//! corresponding change is available.

mod observer;

#[cfg(feature = "tracing")]
pub use observer::TracingObserver;
pub use observer::{
    ErrorReporting, ObserverOptions, OperationError, OperationPerformance, RuntimeObserver,
};

use crate::v03::{Diagnostic, OperationResult};
use crate::watch::{CollectionWatcher, WatchError, WatchEvent};
use crate::Collection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
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
pub struct FilesystemProvider {
    root: PathBuf,
    operation_gate: Mutex<()>,
    observer: Arc<dyn RuntimeObserver>,
    observer_options: ObserverOptions,
    runtime_contracts: OnceLock<Result<crate::runtime_contracts::RuntimeContracts, String>>,
}

impl std::fmt::Debug for FilesystemProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FilesystemProvider")
            .field("root", &self.root)
            .field("observer_options", &self.observer_options)
            .finish_non_exhaustive()
    }
}

impl FilesystemProvider {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ProviderError> {
        Self::open_observed(
            root,
            Arc::new(observer::NoopObserver),
            ObserverOptions::default(),
        )
    }

    pub fn open_observed(
        root: impl AsRef<Path>,
        observer: Arc<dyn RuntimeObserver>,
        observer_options: ObserverOptions,
    ) -> Result<Self, ProviderError> {
        let root = root.as_ref().to_path_buf();
        if let Err(error) = open_collection(&root) {
            report_provider_error(observer.as_ref(), observer_options, "open", "open", &error);
            return Err(error);
        }
        Ok(Self {
            root,
            operation_gate: Mutex::new(()),
            observer,
            observer_options,
            runtime_contracts: OnceLock::new(),
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

    /// Load and preflight the effective Runtime Contracts registry while
    /// holding the same local serialization gate as collection operations.
    pub fn load_runtime_contracts(
        &self,
        implicit_sources: Vec<crate::runtime_contracts::ContractSource>,
        options: &crate::runtime_contracts::LoadOptions,
    ) -> Result<crate::runtime_contracts::RuntimeLoadResult, ProviderError> {
        let started = Instant::now();
        let queue_started = Instant::now();
        let _guard = match self.lock() {
            Ok(guard) => guard,
            Err(error) => {
                self.report_error("runtime_contracts.load", "queue", &error);
                return Err(error);
            }
        };
        let queue = queue_started.elapsed();
        let open_started = Instant::now();
        let collection = match open_collection(&self.root) {
            Ok(collection) => collection,
            Err(error) => {
                self.report_error("runtime_contracts.load", "open", &error);
                return Err(error);
            }
        };
        let open = open_started.elapsed();
        let execute_started = Instant::now();
        let engine = match self
            .runtime_contracts
            .get_or_init(crate::runtime_contracts::RuntimeContracts::new)
        {
            Ok(engine) => engine,
            Err(message) => {
                let error = ProviderError::RuntimeContracts(message.clone());
                self.report_error("runtime_contracts.load", "execute", &error);
                return Err(error);
            }
        };
        let result = engine.load(&collection, implicit_sources, options);
        let execute = execute_started.elapsed();
        self.observer.on_performance(&OperationPerformance {
            operation: "runtime_contracts.load".to_string(),
            queue_us: micros(queue),
            open_us: micros(open),
            execute_us: micros(execute),
            synchronize_us: 0,
            total_us: micros(started.elapsed()),
            valid: result.valid(),
            diagnostic_count: result.preflight.diagnostics.len(),
            diagnostic_codes: diagnostic_codes(
                result
                    .preflight
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.code.as_str()),
            ),
        });
        if !result.valid() {
            self.report_diagnostic_error(
                "runtime_contracts.load",
                result
                    .preflight
                    .diagnostics
                    .iter()
                    .find(|diagnostic| diagnostic.severity == "error")
                    .map(|diagnostic| (diagnostic.code.as_str(), diagnostic.message.as_str())),
            );
        }
        Ok(result)
    }

    fn lock(&self) -> Result<MutexGuard<'_, ()>, ProviderError> {
        self.operation_gate
            .lock()
            .map_err(|_| ProviderError::LockPoisoned)
    }

    fn execute_with_post(
        &self,
        request: &OperationRequest,
        post: impl FnOnce(&OperationResult) -> Result<(), ProviderError>,
    ) -> Result<OperationResult, ProviderError> {
        let started = Instant::now();
        let queue_started = Instant::now();
        let _guard = match self.lock() {
            Ok(guard) => guard,
            Err(error) => {
                self.report_error(request.operation.as_str(), "queue", &error);
                return Err(error);
            }
        };
        let queue = queue_started.elapsed();
        let open_started = Instant::now();
        let collection = match open_collection(&self.root) {
            Ok(collection) => collection,
            Err(error) => {
                self.report_error(request.operation.as_str(), "open", &error);
                return Err(error);
            }
        };
        let open = open_started.elapsed();
        let execute_started = Instant::now();
        let result = match execute_collection(&collection, request) {
            Ok(result) => result,
            Err(error) => {
                self.report_error(request.operation.as_str(), "execute", &error);
                return Err(error);
            }
        };
        let execute = execute_started.elapsed();
        let synchronize_started = Instant::now();
        if let Err(error) = post(&result) {
            self.report_error(request.operation.as_str(), "synchronize", &error);
            return Err(error);
        }
        let synchronize = synchronize_started.elapsed();
        let codes = diagnostic_codes(
            result
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.code.as_str()),
        );
        self.observer.on_performance(&OperationPerformance {
            operation: request.operation.as_str().to_string(),
            queue_us: micros(queue),
            open_us: micros(open),
            execute_us: micros(execute),
            synchronize_us: micros(synchronize),
            total_us: micros(started.elapsed()),
            valid: result.valid,
            diagnostic_count: result.diagnostics.len(),
            diagnostic_codes: codes,
        });
        if !result.valid {
            self.report_diagnostic_error(
                request.operation.as_str(),
                result
                    .diagnostics
                    .iter()
                    .find(|diagnostic| diagnostic.severity == "error")
                    .map(|diagnostic| (diagnostic.code.as_str(), diagnostic.message.as_str())),
            );
        }
        Ok(result)
    }

    fn report_error(&self, operation: &str, stage: &str, error: &ProviderError) {
        report_provider_error(
            self.observer.as_ref(),
            self.observer_options,
            operation,
            stage,
            error,
        );
    }

    fn report_diagnostic_error(&self, operation: &str, diagnostic: Option<(&str, &str)>) {
        let Some((code, message)) = diagnostic else {
            return;
        };
        report_error(
            self.observer.as_ref(),
            self.observer_options,
            operation,
            "execute",
            code,
            message,
        );
    }
}

impl CollectionProvider for FilesystemProvider {
    fn execute(&self, request: &OperationRequest) -> Result<OperationResult, ProviderError> {
        self.execute_with_post(request, |_| Ok(()))
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
        Self::open_observed(
            root,
            debounce,
            Arc::new(observer::NoopObserver),
            ObserverOptions::default(),
        )
    }

    pub fn open_observed(
        root: impl AsRef<Path>,
        debounce: Duration,
        observer: Arc<dyn RuntimeObserver>,
        observer_options: ObserverOptions,
    ) -> Result<Self, ProviderError> {
        let provider = Arc::new(FilesystemProvider::open_observed(
            root.as_ref(),
            observer,
            observer_options,
        )?);
        let watcher = CollectionWatcher::open(root, debounce)?;
        Ok(Self { provider, watcher })
    }

    pub fn provider(&self) -> Arc<FilesystemProvider> {
        self.provider.clone()
    }

    pub fn execute(&self, request: &OperationRequest) -> Result<OperationResult, ProviderError> {
        self.provider.execute_with_post(request, |result| {
            if request.operation.is_mutation() && result.valid {
                // Keep the provider gate until the watcher has compared the
                // post-write snapshot. Another writer therefore cannot make
                // this mutation's notification describe a later state.
                self.watcher.rescan()?;
            }
            Ok(())
        })
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<WatchEvent>, ProviderError> {
        self.watcher.recv_timeout(timeout).map_err(Into::into)
    }
}

fn micros(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn diagnostic_codes<'a>(codes: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut codes = codes.map(str::to_string).collect::<Vec<_>>();
    codes.sort();
    codes.dedup();
    codes
}

fn report_provider_error(
    observer: &dyn RuntimeObserver,
    options: ObserverOptions,
    operation: &str,
    stage: &str,
    error: &ProviderError,
) {
    report_error(
        observer,
        options,
        operation,
        stage,
        error.code(),
        &error.to_string(),
    );
}

fn report_error(
    observer: &dyn RuntimeObserver,
    options: ObserverOptions,
    operation: &str,
    stage: &str,
    code: &str,
    message: &str,
) {
    if options.errors == ErrorReporting::Disabled {
        return;
    }
    observer.on_error(&OperationError {
        operation: operation.to_string(),
        stage: stage.to_string(),
        code: code.to_string(),
        message: (options.errors == ErrorReporting::Messages).then(|| message.to_string()),
    });
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

    #[derive(Debug, Default)]
    struct RecordingObserver {
        performance: Mutex<Vec<OperationPerformance>>,
        errors: Mutex<Vec<OperationError>>,
    }

    impl RuntimeObserver for RecordingObserver {
        fn on_performance(&self, observation: &OperationPerformance) {
            self.performance.lock().unwrap().push(observation.clone());
        }

        fn on_error(&self, observation: &OperationError) {
            self.errors.lock().unwrap().push(observation.clone());
        }
    }

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

    #[test]
    fn observer_reports_payload_free_performance_and_opt_in_errors() {
        let directory = collection();
        let observer = Arc::new(RecordingObserver::default());
        let provider = FilesystemProvider::open_observed(
            directory.path(),
            observer.clone(),
            ObserverOptions {
                errors: ErrorReporting::Codes,
            },
        )
        .unwrap();

        let valid = provider
            .execute(&OperationRequest::new(
                OperationKind::Create,
                json!({
                    "path": "safe.md",
                    "frontmatter": {"private": "must-not-be-observed"},
                }),
            ))
            .unwrap();
        assert!(valid.valid);
        let invalid = provider
            .execute(&OperationRequest::new(
                OperationKind::Create,
                json!({
                    "path": "../escape.md",
                    "frontmatter": {"private": "must-not-be-observed"},
                }),
            ))
            .unwrap();
        assert!(!invalid.valid);
        let invalid_code = invalid.diagnostics[0].code.clone();

        let observations = observer.performance.lock().unwrap();
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0].operation, "create");
        assert!(observations[0].valid);
        assert!(!observations[1].valid);
        assert!(observations[1]
            .diagnostic_codes
            .iter()
            .any(|code| code == &invalid_code));
        let serialized = serde_json::to_string(&*observations).unwrap();
        assert!(!serialized.contains("safe.md"));
        assert!(!serialized.contains("must-not-be-observed"));
        drop(observations);

        let errors = observer.errors.lock().unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].stage, "execute");
        assert!(errors[0].message.is_none());
    }

    #[test]
    fn provider_loads_runtime_contracts_inside_its_authority_gate() {
        let directory = collection();
        fs::write(
            directory.path().join("event.md"),
            concat!(
                "---\ntype: event\nid: test.event\nversion: 1\n",
                "provider: test\nname: Test\nschemas:\n",
                "  dialect: json-schema-2020-12\n  payload:\n    type: object\n---\n"
            ),
        )
        .unwrap();
        let observer = Arc::new(RecordingObserver::default());
        let provider = FilesystemProvider::open_observed(
            directory.path(),
            observer.clone(),
            ObserverOptions::default(),
        )
        .unwrap();

        let loaded = provider
            .load_runtime_contracts(vec![], &crate::runtime_contracts::LoadOptions::default())
            .unwrap();
        assert!(loaded.registry.events.contains_key("test.event"));
        let performance = observer.performance.lock().unwrap();
        assert_eq!(performance.len(), 1);
        assert_eq!(performance[0].operation, "runtime_contracts.load");
    }
}
