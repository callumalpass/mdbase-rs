use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::observer::NoopObserver;
use super::{
    ErrorReporting, ObserverOptions, OperationError, OperationKind, OperationPerformance,
    OperationRequest, ProviderError, RuntimeObserver,
};
use crate::runtime_contracts::{ContractSource, LoadOptions, RuntimeContracts, RuntimeLoadResult};
use crate::v03::OperationResult;
use crate::Collection;

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
    runtime_contracts: OnceLock<Result<RuntimeContracts, String>>,
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
        Self::open_observed(root, Arc::new(NoopObserver), ObserverOptions::default())
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
        implicit_sources: Vec<ContractSource>,
        options: &LoadOptions,
    ) -> Result<RuntimeLoadResult, ProviderError> {
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
        let engine = match self.runtime_contracts.get_or_init(RuntimeContracts::new) {
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

    pub(super) fn execute_with_post(
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
        self.observer.on_performance(&OperationPerformance {
            operation: request.operation.as_str().to_string(),
            queue_us: micros(queue),
            open_us: micros(open),
            execute_us: micros(execute),
            synchronize_us: micros(synchronize),
            total_us: micros(started.elapsed()),
            valid: result.valid,
            diagnostic_count: result.diagnostics.len(),
            diagnostic_codes: diagnostic_codes(
                result
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.code.as_str()),
            ),
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

    fn lock(&self) -> Result<MutexGuard<'_, ()>, ProviderError> {
        self.operation_gate
            .lock()
            .map_err(|_| ProviderError::LockPoisoned)
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
