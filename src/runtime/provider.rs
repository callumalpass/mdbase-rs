use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::gate::{RuntimeGate, RuntimeReadGuard, RuntimeWriteGuard};
use super::observer::NoopObserver;
use super::{CollectionSnapshot, CollectionSnapshotRecord, OperationContext};
use super::{
    ErrorReporting, ObserverOptions, OperationError, OperationKind, OperationPerformance,
    OperationRequest, ProviderError, RuntimeObserver,
};
use crate::v03::OperationResult;
use crate::Collection;
use walkdir::WalkDir;

/// Provider-neutral execution boundary for one authoritative collection.
///
/// Providers return the normative v0.3 operation envelope. Implementations
/// must serialize mutations or provide equivalent compare-and-swap semantics.
pub trait CollectionProvider: Send + Sync {
    /// Execute with explicit cancellation and deadline ownership.
    fn execute_with_context(
        &self,
        request: &OperationRequest,
        context: &OperationContext,
    ) -> Result<OperationResult, ProviderError>;

    /// Refresh provider state with explicit cancellation and deadline ownership.
    fn refresh_with_context(&self, context: &OperationContext) -> Result<(), ProviderError>;

    /// Execute through the compatibility entry point while hosts migrate to
    /// explicit operation contexts.
    fn execute(&self, request: &OperationRequest) -> Result<OperationResult, ProviderError> {
        self.execute_with_context(request, &OperationContext::legacy())
    }

    /// Refresh through the compatibility entry point while hosts migrate to
    /// explicit operation contexts.
    fn refresh(&self) -> Result<(), ProviderError> {
        self.refresh_with_context(&OperationContext::legacy())
    }
}

/// Filesystem-backed provider using the canonical mdbase operation engine.
///
/// A persistent gate allows concurrent reads while serializing mutations from
/// local control, relay, watcher, and future replica paths. The collection is
/// opened inside the gate so external config, type, and record changes are
/// visible to the next request.
pub struct FilesystemProvider {
    root: PathBuf,
    coordinated: bool,
    operation_gate: RuntimeGate,
    observer: Arc<dyn RuntimeObserver>,
    observer_options: ObserverOptions,
    collection_cache: RwLock<CachedCollection>,
    runtime_cache_generation: RwLock<Option<super::CollectionGeneration>>,
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
        Self::open_with_mode(root, observer, observer_options, false)
    }

    pub(crate) fn open_runtime_observed(
        root: impl AsRef<Path>,
        observer: Arc<dyn RuntimeObserver>,
        observer_options: ObserverOptions,
    ) -> Result<Self, ProviderError> {
        Self::open_with_mode(root, observer, observer_options, true)
    }

    fn open_with_mode(
        root: impl AsRef<Path>,
        observer: Arc<dyn RuntimeObserver>,
        observer_options: ObserverOptions,
        coordinated: bool,
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
            coordinated,
            operation_gate: RuntimeGate::new(),
            observer,
            observer_options,
            collection_cache: RwLock::new(CachedCollection {
                collection: Arc::new(collection),
                stamp,
            }),
            runtime_cache_generation: RwLock::new(None),
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
        self.snapshot_with_context(&OperationContext::legacy())
    }

    /// Capture a snapshot while honoring the caller's operation boundary.
    pub fn snapshot_with_context(
        &self,
        context: &OperationContext,
    ) -> Result<CollectionSnapshot, ProviderError> {
        self.with_collection_read_context(context, Collection::snapshot)
    }

    /// Materialize one record at the provider's read boundary.
    pub fn snapshot_record(&self, path: &str) -> Result<CollectionSnapshotRecord, ProviderError> {
        self.snapshot_record_with_context(path, &OperationContext::legacy())
    }

    /// Materialize one record while honoring the caller's operation boundary.
    pub fn snapshot_record_with_context(
        &self,
        path: &str,
        context: &OperationContext,
    ) -> Result<CollectionSnapshotRecord, ProviderError> {
        self.with_collection_read_context(context, |collection| collection.snapshot_record(path))
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
        self.with_collection_context(&OperationContext::legacy(), operation)
    }

    /// Execute one compound exclusive operation with an explicit boundary.
    pub fn with_collection_context<T, E>(
        &self,
        context: &OperationContext,
        operation: impl FnOnce(&Collection) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<ProviderError>,
    {
        let _guard = self.write_lock(context).map_err(E::from)?;
        context.check().map_err(E::from)?;
        let collection = self.current_collection().map_err(E::from)?;
        context.check().map_err(E::from)?;
        let result = operation(collection.as_ref())?;
        context.check().map_err(E::from)?;
        Ok(result)
    }

    /// Execute an exclusive operation whose closure owns a durable boundary.
    ///
    /// The caller context bounds gate acquisition and collection opening. The
    /// closure is responsible for its own pre-boundary checks; this wrapper
    /// deliberately performs no post-call cancellation check because doing so
    /// could misreport a durable prepare, commit, cancel, or acknowledgement.
    pub(crate) fn with_collection_boundary_context<T, E>(
        &self,
        context: &OperationContext,
        operation: impl FnOnce(&Collection) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<ProviderError>,
    {
        let _guard = self.write_lock(context).map_err(E::from)?;
        context.check().map_err(E::from)?;
        let collection = self.current_collection().map_err(E::from)?;
        context.check().map_err(E::from)?;
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
        self.with_collection_read_context(&OperationContext::legacy(), operation)
    }

    /// Execute one compound read operation with an explicit boundary.
    pub fn with_collection_read_context<T, E>(
        &self,
        context: &OperationContext,
        operation: impl FnOnce(&Collection) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<ProviderError>,
    {
        let _guard = self.read_lock(context).map_err(E::from)?;
        context.check().map_err(E::from)?;
        let collection = self.current_collection().map_err(E::from)?;
        context.check().map_err(E::from)?;
        let result = operation(collection.as_ref())?;
        context.check().map_err(E::from)?;
        Ok(result)
    }

    pub(super) fn execute_with_post_context(
        &self,
        request: &OperationRequest,
        context: &OperationContext,
        post: impl FnOnce(&OperationResult) -> Result<(), ProviderError>,
    ) -> Result<OperationResult, ProviderError> {
        if self.coordinated && request.operation.is_mutation() {
            return Err(ProviderError::UnsupportedOperation(format!(
                "{} must use the coordinated prepare/commit runtime boundary",
                request.operation.as_str()
            )));
        }
        let started = Instant::now();
        let mut timings = OperationTimings::default();
        let queue_started = Instant::now();
        let _guard = match self.lock_for(request.operation, context) {
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
        context.check()?;
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
        context.check()?;
        let execute_started = Instant::now();
        let result =
            match execute_collection(collection.as_ref(), request, context, self.coordinated) {
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
        if !request.operation.is_mutation() {
            context.check()?;
        }
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
        if !request.operation.is_mutation() {
            context.check()?;
        }
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

    fn read_lock(&self, context: &OperationContext) -> Result<RuntimeReadGuard<'_>, ProviderError> {
        self.operation_gate.read(context)
    }

    fn write_lock(
        &self,
        context: &OperationContext,
    ) -> Result<RuntimeWriteGuard<'_>, ProviderError> {
        self.operation_gate.write(context)
    }

    fn lock_for(
        &self,
        operation: OperationKind,
        context: &OperationContext,
    ) -> Result<OperationGuard<'_>, ProviderError> {
        if operation.is_mutation() {
            self.write_lock(context).map(OperationGuard::Write)
        } else {
            self.read_lock(context).map(OperationGuard::Read)
        }
    }

    fn current_collection(&self) -> Result<Arc<Collection>, ProviderError> {
        if self.coordinated {
            return self
                .collection_cache
                .read()
                .map(|cached| cached.collection.clone())
                .map_err(|_| ProviderError::LockPoisoned);
        }
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

    pub(crate) fn initialize_runtime_cache(
        &self,
        generation: &super::CollectionGeneration,
    ) -> Result<(), ProviderError> {
        let context = OperationContext::legacy();
        let _guard = self.write_lock(&context)?;
        let collection = self.current_collection()?;
        crate::cache::runtime::rebuild(collection.as_ref(), generation).map_err(cache_error)?;
        *self
            .runtime_cache_generation
            .write()
            .map_err(|_| ProviderError::LockPoisoned)? = Some(generation.clone());
        Ok(())
    }

    /// Reset copied, host-owned runtime support before assigning a collection
    /// fork a new identity. Canonical Markdown is preserved and will seed a
    /// fresh generation, cache, feed owner, and host-claim namespace on open.
    pub fn reset_runtime_support_for_fork(
        &self,
        context: &OperationContext,
    ) -> Result<(), ProviderError> {
        context.check()?;
        self.with_collection_boundary_context(context, |collection| {
            context.check()?;
            let settlement = OperationContext::legacy();
            crate::transactions::reset_runtime_support_for_fork(collection, &settlement)
                .map_err(super::filesystem::transaction_error)?;
            super::feed::reset_for_fork(collection)
        })
    }

    pub(crate) fn ensure_runtime_cache(
        &self,
        generation: &super::CollectionGeneration,
        context: &OperationContext,
    ) -> Result<(), ProviderError> {
        if self
            .runtime_cache_generation
            .read()
            .map_err(|_| ProviderError::LockPoisoned)?
            .as_ref()
            == Some(generation)
        {
            return Ok(());
        }
        let _guard = self.write_lock(context)?;
        context.check()?;
        if self
            .runtime_cache_generation
            .read()
            .map_err(|_| ProviderError::LockPoisoned)?
            .as_ref()
            == Some(generation)
        {
            return Ok(());
        }
        let collection = self.current_collection()?;
        if !crate::cache::runtime::matches_generation(collection.as_ref(), generation)
            .map_err(cache_error)?
        {
            crate::cache::runtime::rebuild(collection.as_ref(), generation).map_err(cache_error)?;
        }
        *self
            .runtime_cache_generation
            .write()
            .map_err(|_| ProviderError::LockPoisoned)? = Some(generation.clone());
        Ok(())
    }

    pub(crate) fn apply_runtime_cache_changes(
        &self,
        changes: &super::ChangeSet,
        generation: &super::CollectionGeneration,
    ) -> Result<(), ProviderError> {
        if matches!(changes, super::ChangeSet::Exact(batch) if batch.items().iter().any(|change| matches!(change, super::CanonicalChange::Resource(_))))
            || matches!(changes, super::ChangeSet::CollectionWide { .. })
        {
            self.reload_collection()?;
        }
        let collection = self.current_collection()?;
        crate::cache::runtime::apply_changes(collection.as_ref(), changes, generation)
            .map_err(cache_error)?;
        *self
            .runtime_cache_generation
            .write()
            .map_err(|_| ProviderError::LockPoisoned)? = Some(generation.clone());
        Ok(())
    }

    pub(super) fn report_error(&self, operation: &str, stage: &str, error: &ProviderError) {
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
    fn execute_with_context(
        &self,
        request: &OperationRequest,
        context: &OperationContext,
    ) -> Result<OperationResult, ProviderError> {
        self.execute_with_post_context(request, context, |_| Ok(()))
    }

    fn refresh_with_context(&self, context: &OperationContext) -> Result<(), ProviderError> {
        let _guard = self.write_lock(context)?;
        context.check()?;
        let result = self.reload_collection();
        context.check()?;
        result
    }
}

enum OperationGuard<'a> {
    Read(#[allow(dead_code)] RuntimeReadGuard<'a>),
    Write(#[allow(dead_code)] RuntimeWriteGuard<'a>),
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
    context: &OperationContext,
    coordinated: bool,
) -> Result<OperationResult, ProviderError> {
    let operations = collection
        .v03_operations()
        .map_err(|diagnostic| ProviderError::CollectionOpen(diagnostic.message.clone()))?;
    Ok(match request.operation {
        OperationKind::Read => operations.read(&request.input),
        OperationKind::Query => (if coordinated {
            operations.query_runtime_cancellable(&request.input, context.cancellation())
        } else {
            operations.query_cancellable(&request.input, context.cancellation())
        })
        .map_err(|_| {
            context
                .check()
                .expect_err("the bounded cancellation token stopped the query")
        })?,
        OperationKind::ListViews => operations.list_views(&request.input),
        OperationKind::ExecuteView => operations.execute_view(&request.input),
        OperationKind::ReadViewSource => operations.read_view_source(&request.input),
        OperationKind::CreateViewSource => operations.create_view_source(&request.input),
        OperationKind::UpdateViewSource => operations.update_view_source(&request.input),
        OperationKind::DeleteViewSource => operations.delete_view_source(&request.input),
        OperationKind::Validate => operations.validate(&request.input),
        OperationKind::Batch => operations.batch(&request.input),
        OperationKind::Create => operations.create(&request.input),
        OperationKind::Update => operations.update(&request.input),
        OperationKind::Delete => operations.delete(&request.input),
        OperationKind::Rename => operations.rename(&request.input),
        OperationKind::ListTypes => operations.list_types(&request.input),
        OperationKind::ReadType => operations.read_type(&request.input),
        OperationKind::CreateType => operations.create_type(&request.input),
        OperationKind::UpdateType => operations.update_type(&request.input),
        OperationKind::AssessTypePack => execute_type_pack(collection, &request.input, false),
        OperationKind::ApplyTypePack => execute_type_pack(collection, &request.input, true),
        OperationKind::AssessCollectionSetup => {
            execute_collection_setup(collection, &request.input, false)
        }
        OperationKind::ApplyCollectionSetup => {
            execute_collection_setup(collection, &request.input, true)
        }
    })
}

fn execute_type_pack(collection: &Collection, input: &Value, apply: bool) -> OperationResult {
    let Some(provision) = input.get("provision").cloned() else {
        return crate::runtime::invalid_operation_result(
            "invalid_request",
            "type-pack input requires provision",
        );
    };
    let provision = match serde_json::from_value::<crate::v03::TypePackProvision>(provision) {
        Ok(provision) => provision,
        Err(error) => {
            return crate::runtime::invalid_operation_result(
                "invalid_request",
                format!("type-pack provision is invalid: {error}"),
            )
        }
    };
    let options = input
        .get("options")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if apply {
        match serde_json::from_value::<crate::v03::TypePackApplyOptions>(options) {
            Ok(options) => collection.apply_type_pack(&provision, &options),
            Err(error) => crate::runtime::invalid_operation_result(
                "invalid_request",
                format!("type-pack apply options are invalid: {error}"),
            ),
        }
    } else {
        match serde_json::from_value::<crate::v03::TypePackAssessmentOptions>(options) {
            Ok(options) => collection.assess_type_pack(&provision, &options),
            Err(error) => crate::runtime::invalid_operation_result(
                "invalid_request",
                format!("type-pack assessment options are invalid: {error}"),
            ),
        }
    }
}

fn execute_collection_setup(
    collection: &Collection,
    input: &Value,
    apply: bool,
) -> OperationResult {
    let Some(setup) = input.get("setup").cloned() else {
        return crate::runtime::invalid_operation_result(
            "invalid_request",
            "collection setup input requires setup",
        );
    };
    let setup = match serde_json::from_value::<crate::v03::CollectionSetup>(setup) {
        Ok(setup) => setup,
        Err(error) => {
            return crate::runtime::invalid_operation_result(
                "invalid_request",
                format!("collection setup is invalid: {error}"),
            )
        }
    };
    if apply {
        let options = input
            .get("options")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        match serde_json::from_value::<crate::v03::CollectionSetupApplyOptions>(options) {
            Ok(options) => collection.apply_collection_setup(&setup, &options),
            Err(error) => crate::runtime::invalid_operation_result(
                "invalid_request",
                format!("collection setup apply options are invalid: {error}"),
            ),
        }
    } else {
        collection.assess_collection_setup(&setup)
    }
}

fn cache_error(error: crate::cache::CacheError) -> ProviderError {
    ProviderError::Transaction {
        code: "cache_maintenance_failed",
        message: error.to_string(),
    }
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
