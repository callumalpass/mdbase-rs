use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::observer::NoopObserver;
use super::CollectionSnapshot;
use super::{
    ErrorReporting, ObserverOptions, OperationError, OperationKind, OperationPerformance,
    OperationRequest, ProviderError, RuntimeObserver,
};
use crate::runtime_contracts::{ContractSource, LoadOptions, RuntimeContracts, RuntimeLoadResult};
use crate::v03::OperationResult;
use crate::Collection;
use walkdir::WalkDir;

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
/// A persistent gate allows concurrent reads while serializing mutations from
/// local control, relay, watcher, and future replica paths. The collection is
/// opened inside the gate so external config, type, and record changes are
/// visible to the next request.
pub struct FilesystemProvider {
    root: PathBuf,
    operation_gate: RwLock<()>,
    observer: Arc<dyn RuntimeObserver>,
    observer_options: ObserverOptions,
    runtime_contracts: OnceLock<Result<RuntimeContracts, String>>,
    collection_cache: RwLock<CachedCollection>,
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
        let collection = match open_collection(&root) {
            Ok(collection) => collection,
            Err(error) => {
                report_provider_error(observer.as_ref(), observer_options, "open", "open", &error);
                return Err(error);
            }
        };
        let stamp = CollectionStamp::load(
            &root,
            &collection.settings.types_folder,
            &collection.settings.contracts_folder,
        );
        Ok(Self {
            root,
            operation_gate: RwLock::new(()),
            observer,
            observer_options,
            runtime_contracts: OnceLock::new(),
            collection_cache: RwLock::new(CachedCollection {
                collection: Arc::new(collection),
                stamp,
            }),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Capture collection resources and canonical records at one read boundary.
    ///
    /// The provider gate prevents an mdbase mutation from interleaving with
    /// the capture. External filesystem writers are detected by the ordinary
    /// opaque revisions and by a subsequent capture before cutover.
    pub fn snapshot(&self) -> Result<CollectionSnapshot, ProviderError> {
        self.with_collection_read(Collection::snapshot)
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
        let _guard = self.write_lock().map_err(E::from)?;
        let collection = self.current_collection().map_err(E::from)?;
        operation(collection.as_ref())
    }

    /// Execute a compound read-only provider operation while allowing other
    /// reads against the same collection to make progress concurrently.
    pub fn with_collection_read<T, E>(
        &self,
        operation: impl FnOnce(&Collection) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<ProviderError>,
    {
        let _guard = self.read_lock().map_err(E::from)?;
        let collection = self.current_collection().map_err(E::from)?;
        operation(collection.as_ref())
    }

    /// Load and preflight the effective Runtime Contracts registry while
    /// holding the same local serialization gate as collection operations.
    pub fn load_runtime_contracts(
        &self,
        implicit_sources: Vec<ContractSource>,
        options: &LoadOptions,
    ) -> Result<RuntimeLoadResult, ProviderError> {
        let started = Instant::now();
        let mut timings = OperationTimings::default();
        let queue_started = Instant::now();
        let _guard = match self.read_lock() {
            Ok(guard) => guard,
            Err(error) => {
                timings.queue = queue_started.elapsed();
                return Err(self.finish_provider_error(
                    "runtime_contracts.load",
                    "queue",
                    started,
                    timings,
                    error,
                ));
            }
        };
        timings.queue = queue_started.elapsed();
        let open_started = Instant::now();
        let collection = match self.current_collection() {
            Ok(collection) => collection,
            Err(error) => {
                timings.open = open_started.elapsed();
                return Err(self.finish_provider_error(
                    "runtime_contracts.load",
                    "open",
                    started,
                    timings,
                    error,
                ));
            }
        };
        timings.open = open_started.elapsed();
        let execute_started = Instant::now();
        let engine = match self.runtime_contracts.get_or_init(RuntimeContracts::new) {
            Ok(engine) => engine,
            Err(message) => {
                timings.execute = execute_started.elapsed();
                let error = ProviderError::RuntimeContracts(message.clone());
                return Err(self.finish_provider_error(
                    "runtime_contracts.load",
                    "execute",
                    started,
                    timings,
                    error,
                ));
            }
        };
        let result = engine.load(collection.as_ref(), implicit_sources, options);
        timings.execute = execute_started.elapsed();
        self.observe_performance(
            "runtime_contracts.load",
            started,
            timings,
            result.valid(),
            result.preflight.diagnostics.len(),
            diagnostic_codes(
                result
                    .preflight
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.code.as_str()),
            ),
        );
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
        let mut timings = OperationTimings::default();
        let queue_started = Instant::now();
        let _guard = match self.lock_for(request.operation) {
            Ok(guard) => guard,
            Err(error) => {
                timings.queue = queue_started.elapsed();
                return Err(self.finish_provider_error(
                    request.operation.as_str(),
                    "queue",
                    started,
                    timings,
                    error,
                ));
            }
        };
        timings.queue = queue_started.elapsed();
        let open_started = Instant::now();
        let collection = match self.current_collection() {
            Ok(collection) => collection,
            Err(error) => {
                timings.open = open_started.elapsed();
                return Err(self.finish_provider_error(
                    request.operation.as_str(),
                    "open",
                    started,
                    timings,
                    error,
                ));
            }
        };
        timings.open = open_started.elapsed();
        let execute_started = Instant::now();
        let result = match execute_collection(collection.as_ref(), request) {
            Ok(result) => result,
            Err(error) => {
                timings.execute = execute_started.elapsed();
                return Err(self.finish_provider_error(
                    request.operation.as_str(),
                    "execute",
                    started,
                    timings,
                    error,
                ));
            }
        };
        timings.execute = execute_started.elapsed();
        let synchronize_started = Instant::now();
        if let Err(error) = post(&result) {
            timings.synchronize = synchronize_started.elapsed();
            return Err(self.finish_provider_error(
                request.operation.as_str(),
                "synchronize",
                started,
                timings,
                error,
            ));
        }
        timings.synchronize = synchronize_started.elapsed();
        self.observe_performance(
            request.operation.as_str(),
            started,
            timings,
            result.valid,
            result.diagnostics.len(),
            diagnostic_codes(
                result
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.code.as_str()),
            ),
        );
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

    fn read_lock(&self) -> Result<RwLockReadGuard<'_, ()>, ProviderError> {
        self.operation_gate
            .read()
            .map_err(|_| ProviderError::LockPoisoned)
    }

    fn write_lock(&self) -> Result<RwLockWriteGuard<'_, ()>, ProviderError> {
        self.operation_gate
            .write()
            .map_err(|_| ProviderError::LockPoisoned)
    }

    fn lock_for(&self, operation: OperationKind) -> Result<OperationGuard<'_>, ProviderError> {
        if operation.is_mutation() {
            self.write_lock().map(OperationGuard::Write)
        } else {
            self.read_lock().map(OperationGuard::Read)
        }
    }

    fn current_collection(&self) -> Result<Arc<Collection>, ProviderError> {
        {
            let cached = self
                .collection_cache
                .read()
                .map_err(|_| ProviderError::LockPoisoned)?;
            let current = CollectionStamp::load(
                &self.root,
                &cached.collection.settings.types_folder,
                &cached.collection.settings.contracts_folder,
            );
            if current == cached.stamp {
                return Ok(cached.collection.clone());
            }
        }

        let mut cached = self
            .collection_cache
            .write()
            .map_err(|_| ProviderError::LockPoisoned)?;
        let current = CollectionStamp::load(
            &self.root,
            &cached.collection.settings.types_folder,
            &cached.collection.settings.contracts_folder,
        );
        if current == cached.stamp {
            return Ok(cached.collection.clone());
        }
        let collection = open_collection(&self.root)?;
        let stamp = CollectionStamp::load(
            &self.root,
            &collection.settings.types_folder,
            &collection.settings.contracts_folder,
        );
        cached.collection = Arc::new(collection);
        cached.stamp = stamp;
        Ok(cached.collection.clone())
    }

    fn reload_collection(&self) -> Result<(), ProviderError> {
        let collection = open_collection(&self.root)?;
        let stamp = CollectionStamp::load(
            &self.root,
            &collection.settings.types_folder,
            &collection.settings.contracts_folder,
        );
        let mut cached = self
            .collection_cache
            .write()
            .map_err(|_| ProviderError::LockPoisoned)?;
        cached.collection = Arc::new(collection);
        cached.stamp = stamp;
        Ok(())
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

    fn observe_performance(
        &self,
        operation: &str,
        started: Instant,
        timings: OperationTimings,
        valid: bool,
        diagnostic_count: usize,
        diagnostic_codes: Vec<String>,
    ) {
        self.observer.on_performance(&OperationPerformance {
            operation: operation.to_string(),
            queue_us: micros(timings.queue),
            open_us: micros(timings.open),
            execute_us: micros(timings.execute),
            synchronize_us: micros(timings.synchronize),
            total_us: micros(started.elapsed()),
            valid,
            diagnostic_count,
            diagnostic_codes,
        });
    }

    fn finish_provider_error(
        &self,
        operation: &str,
        stage: &str,
        started: Instant,
        timings: OperationTimings,
        error: ProviderError,
    ) -> ProviderError {
        self.observe_performance(
            operation,
            started,
            timings,
            false,
            1,
            vec![error.code().to_string()],
        );
        self.report_error(operation, stage, &error);
        error
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

#[derive(Debug, Clone, Copy, Default)]
struct OperationTimings {
    queue: Duration,
    open: Duration,
    execute: Duration,
    synchronize: Duration,
}

impl CollectionProvider for FilesystemProvider {
    fn execute(&self, request: &OperationRequest) -> Result<OperationResult, ProviderError> {
        self.execute_with_post(request, |_| Ok(()))
    }

    fn refresh(&self) -> Result<(), ProviderError> {
        let _guard = self.write_lock()?;
        self.reload_collection()
    }
}

enum OperationGuard<'a> {
    Read(#[allow(dead_code)] RwLockReadGuard<'a, ()>),
    Write(#[allow(dead_code)] RwLockWriteGuard<'a, ()>),
}

struct CachedCollection {
    collection: Arc<Collection>,
    stamp: CollectionStamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CollectionStamp {
    config_revision: Option<String>,
    types_metadata: u64,
}

impl CollectionStamp {
    fn load(root: &Path, types_folder: &str, contracts_folder: &str) -> Self {
        let config_revision = std::fs::read(root.join("mdbase.yaml"))
            .ok()
            .map(|bytes| crate::v03::revision(&bytes));
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for control_root in [root.join(types_folder), root.join(contracts_folder)] {
            for entry in WalkDir::new(&control_root)
                .sort_by_file_name()
                .follow_links(false)
                .into_iter()
                .flatten()
                .filter(|entry| entry.file_type().is_file())
            {
                entry
                    .path()
                    .strip_prefix(root)
                    .unwrap_or(entry.path())
                    .hash(&mut hasher);
                if let Ok(metadata) = entry.metadata() {
                    metadata.len().hash(&mut hasher);
                    metadata
                        .modified()
                        .ok()
                        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| duration.as_nanos())
                        .hash(&mut hasher);
                }
            }
        }
        Self {
            config_revision,
            types_metadata: hasher.finish(),
        }
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
        OperationKind::ListViews => operations.list_views(&request.input),
        OperationKind::ExecuteView => operations.execute_view(&request.input),
        OperationKind::ReadViewSource => operations.read_view_source(&request.input),
        OperationKind::CreateViewSource => operations.create_view_source(&request.input),
        OperationKind::UpdateViewSource => operations.update_view_source(&request.input),
        OperationKind::DeleteViewSource => operations.delete_view_source(&request.input),
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
