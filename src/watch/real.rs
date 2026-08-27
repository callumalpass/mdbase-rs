use super::{PortableWatchEvent, WatchEvent};
use crate::operations::read::RecordFileFacts;
use crate::runtime::{snapshot::materialize_snapshot_record, CollectionSnapshotResourceKind};
use crate::Collection;
use notify::{
    event::{CreateKind, MetadataKind, ModifyKind, RemoveKind},
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WatchError {
    #[error("collection could not be opened: {0}")]
    Collection(String),
    #[error("filesystem watcher could not be started: {0}")]
    Notify(#[from] notify::Error),
    #[error("collection watcher stopped")]
    Stopped,
}

/// A debounced stream of collection-level changes.
///
/// Filesystem notifications are treated as invalidation hints. Each debounce
/// cycle rebuilds the visible collection snapshot, so consumers observe the
/// final record state after atomic-save sequences and cache/index updates can
/// be completed before an event is forwarded.
pub struct CollectionWatcher {
    events: mpsc::Receiver<WatchEvent>,
    commands: mpsc::Sender<WorkerInput>,
    worker: Option<thread::JoinHandle<()>>,
}

enum Command {
    Rescan(mpsc::SyncSender<()>),
    RescanPaths(Vec<PathBuf>, mpsc::SyncSender<()>),
    Stop,
}

enum WorkerInput {
    Command(Command),
    Filesystem(Result<Event, notify::Error>),
}

impl CollectionWatcher {
    pub fn open(root: impl AsRef<Path>, debounce: Duration) -> Result<Self, WatchError> {
        Self::open_internal(root.as_ref(), debounce)
    }

    fn open_internal(root: &Path, debounce: Duration) -> Result<Self, WatchError> {
        let root = root.to_path_buf();
        let initial = Snapshot::load(&root)?;
        let (events_tx, events) = mpsc::channel();
        let (commands, command_rx) = mpsc::channel();
        let filesystem_tx = commands.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("mdbase-watch".to_string())
            .spawn(move || {
                watch_loop(
                    root,
                    debounce,
                    initial,
                    WorkerChannels {
                        inputs: command_rx,
                        filesystem_tx,
                        events: events_tx,
                        ready: ready_tx,
                    },
                )
            })
            .map_err(|error| WatchError::Collection(error.to_string()))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                events,
                commands,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(WatchError::Notify(error))
            }
            Err(_) => {
                let _ = worker.join();
                Err(WatchError::Stopped)
            }
        }
    }

    pub fn recv(&self) -> Result<WatchEvent, WatchError> {
        self.events.recv().map_err(|_| WatchError::Stopped)
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<WatchEvent>, WatchError> {
        match self.events.recv_timeout(timeout) {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(WatchError::Stopped),
        }
    }

    /// Receive the next event in the portable v0.3 Watch profile shape.
    pub fn recv_portable(&self) -> Result<PortableWatchEvent, WatchError> {
        self.recv().map(WatchEvent::into_portable)
    }

    /// Receive a portable event, returning `None` when the timeout expires.
    pub fn recv_portable_timeout(
        &self,
        timeout: Duration,
    ) -> Result<Option<PortableWatchEvent>, WatchError> {
        self.recv_timeout(timeout)
            .map(|event| event.map(WatchEvent::into_portable))
    }

    /// Request a snapshot comparison without waiting for an OS notification.
    /// This is useful after an in-process mutation and for deterministic hosts.
    pub fn rescan(&self) -> Result<(), WatchError> {
        let (ready, receiver) = mpsc::sync_channel(0);
        self.commands
            .send(WorkerInput::Command(Command::Rescan(ready)))
            .map_err(|_| WatchError::Stopped)?;
        receiver.recv().map_err(|_| WatchError::Stopped)
    }

    /// Compare only the supplied record paths with the current snapshot.
    ///
    /// This is the preferred synchronization path after an in-process
    /// mutation. It preserves operation/event ordering without turning every
    /// write into an O(collection size) reload.
    pub fn rescan_paths<I, P>(&self, paths: I) -> Result<(), WatchError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let paths = paths.into_iter().map(Into::into).collect();
        let (ready, receiver) = mpsc::sync_channel(0);
        self.commands
            .send(WorkerInput::Command(Command::RescanPaths(paths, ready)))
            .map_err(|_| WatchError::Stopped)?;
        receiver.recv().map_err(|_| WatchError::Stopped)
    }
}

impl Drop for CollectionWatcher {
    fn drop(&mut self) {
        let _ = self.commands.send(WorkerInput::Command(Command::Stop));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct WorkerChannels {
    inputs: mpsc::Receiver<WorkerInput>,
    filesystem_tx: mpsc::Sender<WorkerInput>,
    events: mpsc::Sender<WatchEvent>,
    ready: mpsc::SyncSender<Result<(), notify::Error>>,
}

fn watch_loop(root: PathBuf, debounce: Duration, mut snapshot: Snapshot, channels: WorkerChannels) {
    let WorkerChannels {
        inputs,
        filesystem_tx,
        events,
        ready,
    } = channels;
    let mut watcher: RecommendedWatcher = match notify::recommended_watcher(move |event| {
        let _ = filesystem_tx.send(WorkerInput::Filesystem(event));
    }) {
        Ok(watcher) => watcher,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    if let Err(error) = watcher.watch(&root, RecursiveMode::Recursive) {
        let _ = ready.send(Err(error));
        return;
    }
    let mut sequence = 0_u64;
    // Close the gap between the caller's initial snapshot and OS watch
    // registration before reporting readiness. Hosts can now treat `open` as
    // a stable boundary instead of triggering an additional full rescan.
    let mut startup_refresh_failure = None;
    match Snapshot::load(&root) {
        Ok(next) => {
            for event in snapshot.diff(&next) {
                sequence += 1;
                if events
                    .send(WatchEvent {
                        event_type: event.event_type,
                        sequence,
                        occurred_at: now(),
                        payload: event.payload,
                    })
                    .is_err()
                {
                    return;
                }
            }
            snapshot = next;
        }
        Err(error) => {
            startup_refresh_failure = Some(Instant::now());
            sequence += 1;
            if events
                .send(watch_error_event(sequence, error.to_string()))
                .is_err()
            {
                return;
            }
        }
    }
    let _ = ready.send(Ok(()));

    let tick = debounce
        .min(Duration::from_millis(50))
        .max(Duration::from_millis(5));
    // Some backends (notably FSEvents) can acknowledge registration before
    // their event stream is observable. Reconcile once after startup so a
    // write immediately following `open` cannot fall through that gap.
    let mut deadline = Some(startup_refresh_failure.map_or_else(
        || Instant::now() + debounce,
        |failed_at| failed_at + INITIAL_REFRESH_RETRY,
    ));
    let mut pending_rescans = Vec::new();
    let mut pending_paths = BTreeSet::new();
    let mut full_rescan = true;
    let mut retry_backoff = INITIAL_REFRESH_RETRY;
    let mut last_refresh_diagnostic = startup_refresh_failure;

    loop {
        let current_time = Instant::now();
        let wait = deadline
            .map(|deadline| deadline.saturating_duration_since(current_time).min(tick))
            .unwrap_or(tick);
        match inputs.recv_timeout(wait) {
            Ok(WorkerInput::Command(Command::Stop)) => return,
            Ok(WorkerInput::Command(Command::Rescan(ready))) => {
                schedule_relevant_refresh(
                    &mut deadline,
                    &mut retry_backoff,
                    last_refresh_diagnostic.is_some(),
                    Duration::ZERO,
                );
                full_rescan = true;
                pending_rescans.push(ready);
            }
            Ok(WorkerInput::Command(Command::RescanPaths(paths, ready))) => {
                schedule_relevant_refresh(
                    &mut deadline,
                    &mut retry_backoff,
                    last_refresh_diagnostic.is_some(),
                    Duration::ZERO,
                );
                pending_paths.extend(paths);
                pending_rescans.push(ready);
            }
            Ok(WorkerInput::Filesystem(Ok(event))) if invalidates_snapshot(&event) => {
                let pathless = event.paths.is_empty();
                let invalidation = snapshot.invalidation_paths(&root, &event);
                let relevant = invalidation.as_ref().is_none_or(|paths| !paths.is_empty());
                if relevant {
                    if watch_profile_enabled() {
                        eprintln!(
                            "mdbase_watch invalidation kind={:?} mode={} record_paths={}",
                            event.kind,
                            if invalidation.is_some() {
                                "incremental"
                            } else {
                                "full"
                            },
                            invalidation.as_ref().map_or(0, BTreeSet::len),
                        );
                    }
                    match invalidation {
                        Some(paths) => pending_paths.extend(paths),
                        None => full_rescan = true,
                    }
                    let refresh_is_failing = last_refresh_diagnostic.is_some();
                    if !(pathless && refresh_is_failing) {
                        schedule_relevant_refresh(
                            &mut deadline,
                            &mut retry_backoff,
                            refresh_is_failing,
                            debounce,
                        );
                    }
                }
            }
            Ok(WorkerInput::Filesystem(Ok(_))) => {}
            Ok(WorkerInput::Filesystem(Err(error))) => {
                sequence += 1;
                if events
                    .send(watch_error_event(sequence, error.to_string()))
                    .is_err()
                {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            deadline = None;
            let refresh_started = Instant::now();
            let refresh_mode = if full_rescan { "full" } else { "incremental" };
            let refresh_path_count = pending_paths.len();
            let refreshed = if full_rescan {
                Snapshot::load(&root).map(|next| {
                    let diff = snapshot.diff(&next);
                    snapshot = next;
                    diff
                })
            } else {
                snapshot.refresh_paths(&root, &pending_paths)
            };
            match refreshed {
                Ok(changes) => {
                    for event in changes {
                        sequence += 1;
                        if events
                            .send(WatchEvent {
                                event_type: event.event_type,
                                sequence,
                                occurred_at: now(),
                                payload: event.payload,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    pending_paths.clear();
                    full_rescan = false;
                    retry_backoff = INITIAL_REFRESH_RETRY;
                    last_refresh_diagnostic = None;
                    for ready in pending_rescans.drain(..) {
                        let _ = ready.send(());
                    }
                }
                Err(error) => {
                    let failed_at = Instant::now();
                    deadline = Some(failed_at + retry_backoff);
                    retry_backoff = retry_backoff.saturating_mul(2).min(MAX_REFRESH_RETRY);
                    let diagnostic_due = last_refresh_diagnostic.is_none_or(|last| {
                        failed_at.saturating_duration_since(last) >= REFRESH_DIAGNOSTIC_INTERVAL
                    });
                    if diagnostic_due {
                        sequence += 1;
                        if events
                            .send(watch_error_event(sequence, error.to_string()))
                            .is_err()
                        {
                            return;
                        }
                        last_refresh_diagnostic = Some(failed_at);
                    }
                }
            }
            if watch_profile_enabled() {
                eprintln!(
                    "mdbase_watch refresh mode={} record_paths={} elapsed_us={}",
                    refresh_mode,
                    refresh_path_count,
                    refresh_started.elapsed().as_micros(),
                );
            }
        }
    }
}

const INITIAL_REFRESH_RETRY: Duration = Duration::from_millis(250);
const MAX_REFRESH_RETRY: Duration = Duration::from_secs(5);
const REFRESH_DIAGNOSTIC_INTERVAL: Duration = Duration::from_secs(5);

fn schedule_relevant_refresh(
    deadline: &mut Option<Instant>,
    retry_backoff: &mut Duration,
    refresh_is_failing: bool,
    debounce: Duration,
) {
    let now = Instant::now();
    if refresh_is_failing {
        *retry_backoff = INITIAL_REFRESH_RETRY;
        let fresh_retry = now + INITIAL_REFRESH_RETRY;
        *deadline = Some(deadline.map_or(fresh_retry, |existing| existing.min(fresh_retry)));
    } else {
        *deadline = Some(now + debounce);
        *retry_backoff = INITIAL_REFRESH_RETRY;
    }
}

fn watch_profile_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("MDBASE_WATCH_PROFILE")
            .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
    })
}

/// Return whether a filesystem event could change the collection snapshot.
///
/// Loading a snapshot opens every visible collection file. Some watcher
/// backends report those reads as access events, so treating every event as an
/// invalidation makes the snapshot loader continuously trigger itself.
fn invalidates_snapshot(event: &Event) -> bool {
    !matches!(
        event.kind,
        EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime))
    )
}

fn watch_error_event(sequence: u64, message: String) -> WatchEvent {
    WatchEvent {
        event_type: "mdbase.collection.invalidated".to_string(),
        sequence,
        occurred_at: now(),
        payload: json!({
            "diagnostic": {
                "severity": "error",
                "code": "collection_reload_failed",
                "message": message,
            }
        }),
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[derive(Clone)]
struct Snapshot {
    resources: BTreeMap<String, ResourceState>,
    records: BTreeMap<String, RecordState>,
    types_folder: String,
    contracts_folder: String,
    cache_folder: String,
    migrations_folder: String,
    exclude: Vec<String>,
    include_subfolders: bool,
    record_extensions: BTreeSet<String>,
}

#[derive(Clone, Eq, PartialEq)]
struct ResourceState {
    kind: CollectionSnapshotResourceKind,
    revision: String,
}

#[derive(Clone)]
struct RecordState {
    revision: String,
    raw_frontmatter: Map<String, Value>,
    effective_frontmatter: Value,
    types: Value,
    body: String,
}

impl Snapshot {
    fn load(root: &Path) -> Result<Self, WatchError> {
        let collection = Collection::open_for_observation(root)
            .map_err(|error| WatchError::Collection(collection_error(&error)))?;
        let canonical = collection
            .snapshot()
            .map_err(|error| WatchError::Collection(error.to_string()))?;
        let resources = canonical
            .resources
            .into_iter()
            .map(|resource| {
                (
                    resource.path,
                    ResourceState {
                        kind: resource.kind,
                        revision: resource.revision,
                    },
                )
            })
            .collect();
        let mut records = BTreeMap::new();
        for record in canonical
            .records
            .into_iter()
            .filter(|record| record.frontmatter_error.is_none())
        {
            let effective = collection
                .apply_defaults(&Value::Object(record.frontmatter.clone()), &record.types);
            let effective = collection.coerce_types(&effective, &record.types);
            records.insert(
                record.path,
                RecordState {
                    revision: record.revision,
                    raw_frontmatter: record.frontmatter,
                    effective_frontmatter: effective,
                    types: json!(record.types),
                    body: record.body,
                },
            );
        }
        Ok(Self {
            resources,
            records,
            types_folder: collection.settings.types_folder.clone(),
            contracts_folder: collection.settings.contracts_folder.clone(),
            cache_folder: collection.settings.cache_folder.clone(),
            migrations_folder: collection.settings.migrations_folder.clone(),
            exclude: collection.settings.exclude.clone(),
            include_subfolders: collection.settings.include_subfolders,
            record_extensions: std::iter::once("md".to_string())
                .chain(
                    collection
                        .settings
                        .extensions
                        .iter()
                        .map(|extension| extension.trim_start_matches('.').to_string()),
                )
                .collect(),
        })
    }

    fn invalidation_paths(&self, root: &Path, event: &Event) -> Option<BTreeSet<PathBuf>> {
        if event.paths.is_empty() {
            return None;
        }
        // FSEvents and other coalescing backends can report the watched root
        // rather than a changed leaf, and may canonicalize an aliased root (for
        // example `/var` to `/private/var`). Empty or non-relative backend paths
        // are collection-wide, not irrelevant, so reconcile authoritatively.
        if event
            .paths
            .iter()
            .any(|path| match path.strip_prefix(root) {
                Ok(relative) => relative.as_os_str().is_empty(),
                Err(_) => true,
            })
        {
            return None;
        }

        // Filter paths that can never affect the collection before escalating
        // directory-shaped events. Backends commonly report hidden/cache
        // directory churn as folder create/remove events.
        let visible = event
            .paths
            .iter()
            .filter_map(|path| {
                let relative = path.strip_prefix(root).unwrap_or(path);
                let normalized = relative.to_string_lossy().replace('\\', "/");
                (!normalized.is_empty() && !self.is_ignored_path(&normalized))
                    .then_some((path, relative, normalized))
            })
            .collect::<Vec<_>>();
        if visible.is_empty() {
            return Some(BTreeSet::new());
        }
        if matches!(event.kind, EventKind::Any | EventKind::Other) {
            return None;
        }

        if matches!(
            event.kind,
            EventKind::Modify(ModifyKind::Name(_))
                | EventKind::Create(CreateKind::Folder)
                | EventKind::Remove(RemoveKind::Folder | RemoveKind::Any | RemoveKind::Other)
        ) || matches!(
            event.kind,
            EventKind::Create(CreateKind::Any | CreateKind::Other)
        ) && visible.iter().any(|(path, _, _)| {
            std::fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        }) {
            return None;
        }

        let mut records = BTreeSet::new();
        for (_, relative, normalized) in visible {
            if normalized == "mdbase.yaml"
                || normalized == "mdbase.lock.yaml"
                || normalized == "mdbase.provisions.yaml"
                || normalized == self.types_folder
                || normalized.starts_with(&format!("{}/", self.types_folder))
                || normalized == self.contracts_folder
                || normalized.starts_with(&format!("{}/", self.contracts_folder))
                || self.resources.contains_key(&normalized)
                || crate::runtime::is_schema_resource_path(&normalized)
                || relative.extension().and_then(|value| value.to_str()) == Some("base")
            {
                return None;
            }
            let extension = relative.extension().and_then(|value| value.to_str());
            if extension.is_some_and(|extension| self.record_extensions.contains(extension)) {
                records.insert(PathBuf::from(normalized));
            }
        }
        Some(records)
    }

    fn is_ignored_path(&self, path: &str) -> bool {
        crate::record_path::has_hidden_component(path)
            || path == self.cache_folder
            || path.starts_with(&format!("{}/", self.cache_folder))
            || path == self.migrations_folder
            || path.starts_with(&format!("{}/", self.migrations_folder))
            || self
                .exclude
                .iter()
                .any(|pattern| crate::matching::glob::match_glob_pattern(pattern, path))
            || (!self.include_subfolders && path.contains('/'))
    }

    fn refresh_paths(
        &mut self,
        root: &Path,
        paths: &BTreeSet<PathBuf>,
    ) -> Result<Vec<PendingEvent>, WatchError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let collection = Collection::open_for_observation(root)
            .map_err(|error| WatchError::Collection(collection_error(&error)))?;
        let mut before = BTreeMap::new();
        let mut replacements = Vec::new();
        for path in paths {
            let relative = path.to_string_lossy().replace('\\', "/");
            if let Some(record) = self.records.get(&relative) {
                before.insert(relative.clone(), record.clone());
            }
            replacements.push((relative.clone(), load_record(&collection, &relative)?));
        }
        for (path, record) in replacements {
            if let Some(record) = record {
                self.records.insert(path, record);
            } else {
                self.records.remove(&path);
            }
        }
        let after = paths
            .iter()
            .filter_map(|path| {
                let path = path.to_string_lossy().replace('\\', "/");
                self.records
                    .get(&path)
                    .cloned()
                    .map(|record| (path, record))
            })
            .collect::<BTreeMap<_, _>>();
        Ok(record_events(&before, &after))
    }

    fn diff(&self, next: &Self) -> Vec<PendingEvent> {
        let mut events = Vec::new();
        for path in self
            .resources
            .keys()
            .chain(next.resources.keys())
            .collect::<BTreeSet<_>>()
        {
            let before = self.resources.get(path);
            let after = next.resources.get(path);
            if before != after {
                let kind = after.or(before).expect("resource exists on one side").kind;
                events.push(PendingEvent::new(
                    resource_event_type(kind),
                    json!({
                        "path": path,
                        "previous_revision": before.map(|resource| &resource.revision),
                        "revision": after.map(|resource| &resource.revision),
                    }),
                ));
            }
        }

        events.extend(record_events(&self.records, &next.records));

        events
    }
}

fn resource_event_type(kind: CollectionSnapshotResourceKind) -> &'static str {
    match kind {
        CollectionSnapshotResourceKind::Configuration => "mdbase.config.changed",
        CollectionSnapshotResourceKind::Type => "mdbase.type.changed",
        CollectionSnapshotResourceKind::Contract => "mdbase.contract.changed",
        CollectionSnapshotResourceKind::Schema => "mdbase.schema.changed",
        CollectionSnapshotResourceKind::View => "mdbase.view.changed",
        CollectionSnapshotResourceKind::Lock => "mdbase.lock.changed",
    }
}

fn record_events(
    before: &BTreeMap<String, RecordState>,
    after: &BTreeMap<String, RecordState>,
) -> Vec<PendingEvent> {
    let mut events = Vec::new();
    let mut deleted: BTreeSet<String> = before
        .keys()
        .filter(|path| !after.contains_key(*path))
        .cloned()
        .collect();
    let mut created: BTreeSet<String> = after
        .keys()
        .filter(|path| !before.contains_key(*path))
        .cloned()
        .collect();
    let revision_counts = |paths: &BTreeSet<String>, states: &BTreeMap<String, RecordState>| {
        let mut counts = BTreeMap::<String, usize>::new();
        for path in paths {
            *counts.entry(states[path].revision.clone()).or_default() += 1;
        }
        counts
    };
    let deleted_revisions = revision_counts(&deleted, before);
    let created_revisions = revision_counts(&created, after);
    let deleted_paths = deleted.iter().cloned().collect::<Vec<_>>();
    for from in deleted_paths {
        let previous = &before[&from];
        if deleted_revisions.get(&previous.revision) == Some(&1)
            && created_revisions.get(&previous.revision) == Some(&1)
        {
            let to = created
                .iter()
                .find(|to| after[*to].revision == previous.revision)
                .expect("unique created revision has one path")
                .clone();
            let current = &after[&to];
            deleted.remove(&from);
            created.remove(&to);
            events.push(PendingEvent::new(
                "mdbase.record.renamed",
                json!({
                    "from": from,
                    "to": to,
                    "before": previous.effective_frontmatter,
                    "after": current.effective_frontmatter,
                    "raw_before": previous.raw_frontmatter,
                    "raw_after": current.raw_frontmatter,
                    "body_changed": previous.body != current.body,
                    "previous_revision": previous.revision,
                    "revision": current.revision,
                    "previous_types": previous.types,
                    "types": current.types,
                }),
            ));
        }
    }

    for path in deleted {
        let previous = &before[&path];
        events.push(PendingEvent::new(
            "mdbase.record.deleted",
            json!({
                "path": path,
                "before": previous.effective_frontmatter,
                "raw_before": previous.raw_frontmatter,
                "body_changed": true,
                "previous_revision": previous.revision,
                "types": previous.types,
            }),
        ));
    }
    for path in created {
        let current = &after[&path];
        events.push(PendingEvent::new(
            "mdbase.record.created",
            json!({
                "path": path,
                "after": current.effective_frontmatter,
                "raw_after": current.raw_frontmatter,
                "body_changed": true,
                "changed_fields": current.raw_frontmatter.keys().collect::<Vec<_>>(),
                "revision": current.revision,
                "types": current.types,
            }),
        ));
    }
    for path in before.keys() {
        let (Some(previous), Some(current)) = (before.get(path), after.get(path)) else {
            continue;
        };
        if previous.revision != current.revision {
            events.push(PendingEvent::new(
                "mdbase.record.modified",
                json!({
                    "path": path,
                    "before": previous.effective_frontmatter,
                    "after": current.effective_frontmatter,
                    "raw_before": previous.raw_frontmatter,
                    "raw_after": current.raw_frontmatter,
                    "body_changed": previous.body != current.body,
                    "changed_fields": changed_fields(previous, current),
                    "previous_revision": previous.revision,
                    "revision": current.revision,
                    "previous_types": previous.types,
                    "types": current.types,
                }),
            ));
        }
    }
    events
}

fn load_record(collection: &Collection, path: &str) -> Result<Option<RecordState>, WatchError> {
    if collection.is_excluded(path) || !collection.is_valid_extension(path) {
        return Ok(None);
    }
    let Some((document, file_facts)) = safely_read_local_record(collection, path)? else {
        return Ok(None);
    };
    let read = collection.read_document(path, &document, &file_facts, false);
    let revision = match read.get("revision").and_then(Value::as_str) {
        Some(revision) => revision.to_string(),
        None if is_invalid_yaml_frontmatter(&read) => return Ok(None),
        None => return Err(WatchError::Collection(collection_error(&read))),
    };
    let canonical = materialize_snapshot_record(collection, path, document);
    let raw_frontmatter = canonical.frontmatter;
    let effective_frontmatter = read
        .get("effective_frontmatter")
        .cloned()
        .unwrap_or_else(|| Value::Object(raw_frontmatter.clone()));
    let types = read.get("types").cloned().unwrap_or_else(|| json!([]));
    Ok(Some(RecordState {
        revision,
        raw_frontmatter,
        effective_frontmatter,
        types,
        body: canonical.body,
    }))
}

fn safely_read_local_record(
    collection: &Collection,
    path: &str,
) -> Result<Option<(String, RecordFileFacts)>, WatchError> {
    let Some(mut file) = crate::operations::open_regular_record_no_follow(&collection.root, path)
        .map_err(|error| WatchError::Collection(error.to_string()))?
    else {
        return Ok(None);
    };
    let metadata = file
        .metadata()
        .map_err(|error| WatchError::Collection(error.to_string()))?;
    if !metadata.is_file() {
        return Ok(None);
    }
    let file_facts = RecordFileFacts {
        size: metadata.len(),
        mtime: metadata.modified().ok().map(|time| {
            let datetime: chrono::DateTime<chrono::Utc> = time.into();
            datetime.format("%Y-%m-%dT%H:%M:%SZ").to_string()
        }),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| WatchError::Collection(error.to_string()))?;
    let document = String::from_utf8(bytes)
        .map_err(|_| WatchError::Collection("File contains invalid UTF-8".to_string()))?;
    Ok(Some((document, file_facts)))
}

fn is_invalid_yaml_frontmatter(read: &Value) -> bool {
    read.pointer("/error/code").and_then(Value::as_str) == Some(crate::errors::INVALID_FRONTMATTER)
        && read.pointer("/error/message").and_then(Value::as_str)
            == Some("Failed to parse YAML frontmatter")
}

struct PendingEvent {
    event_type: String,
    payload: Value,
}

impl PendingEvent {
    fn new(event_type: &str, payload: Value) -> Self {
        Self {
            event_type: event_type.to_string(),
            payload,
        }
    }
}

fn changed_fields(previous: &RecordState, current: &RecordState) -> Vec<String> {
    previous
        .raw_frontmatter
        .keys()
        .chain(current.raw_frontmatter.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|key| previous.raw_frontmatter.get(*key) != current.raw_frontmatter.get(*key))
        .cloned()
        .collect()
}

fn collection_error(error: &Value) -> String {
    error
        .pointer("/error/message")
        .or_else(|| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("collection reload failed")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, AccessMode, DataChange, RenameMode};
    use std::fs;

    #[test]
    fn non_mutating_filesystem_events_do_not_invalidate_the_snapshot() {
        for kind in [
            EventKind::Access(AccessKind::Any),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Open(AccessMode::Read)),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)),
        ] {
            assert!(!invalidates_snapshot(&Event::new(kind)), "{kind:?}");
        }

        for kind in [
            EventKind::Any,
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            EventKind::Remove(RemoveKind::File),
            EventKind::Other,
        ] {
            assert!(invalidates_snapshot(&Event::new(kind)), "{kind:?}");
        }
    }

    #[test]
    fn synthetic_directory_and_rename_events_force_full_reconciliation() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("extant")).unwrap();
        let snapshot = Snapshot {
            resources: BTreeMap::new(),
            records: BTreeMap::new(),
            types_folder: "_types".to_string(),
            contracts_folder: "_contracts".to_string(),
            cache_folder: ".mdbase/cache".to_string(),
            migrations_folder: ".mdbase/migrations".to_string(),
            exclude: Vec::new(),
            include_subfolders: true,
            record_extensions: BTreeSet::from(["md".to_string()]),
        };

        for event in [
            Event::new(EventKind::Modify(ModifyKind::Any)).add_path(directory.path().to_path_buf()),
            Event::new(EventKind::Modify(ModifyKind::Any)).add_path(
                directory
                    .path()
                    .parent()
                    .unwrap()
                    .join("reported-root-alias"),
            ),
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Any)))
                .add_path(directory.path().join("old"))
                .add_path(directory.path().join("new")),
            Event::new(EventKind::Create(CreateKind::Folder))
                .add_path(directory.path().join("folder")),
            Event::new(EventKind::Remove(RemoveKind::Folder))
                .add_path(directory.path().join("folder")),
            Event::new(EventKind::Create(CreateKind::Any))
                .add_path(directory.path().join("extant")),
        ] {
            assert_eq!(snapshot.invalidation_paths(directory.path(), &event), None);
        }

        for (kind, path) in [
            (EventKind::Create(CreateKind::Folder), ".hidden/folder"),
            (EventKind::Remove(RemoveKind::Folder), ".hidden/folder"),
            (EventKind::Remove(RemoveKind::Any), ".mdbase/cache/gone"),
            (EventKind::Remove(RemoveKind::Other), ".hidden/gone"),
            (EventKind::Create(CreateKind::File), ".hidden/state.json"),
            (EventKind::Other, ".hidden/unknown"),
            (EventKind::Create(CreateKind::File), "assets/image.png"),
        ] {
            let event = Event::new(kind).add_path(directory.path().join(path));
            assert_eq!(
                snapshot.invalidation_paths(directory.path(), &event),
                Some(BTreeSet::new()),
                "{path}"
            );
        }

        for kind in [RemoveKind::Any, RemoveKind::Other] {
            let event =
                Event::new(EventKind::Remove(kind)).add_path(directory.path().join("visible-gone"));
            assert_eq!(snapshot.invalidation_paths(directory.path(), &event), None);
        }
    }

    #[test]
    fn hidden_and_binary_changes_do_not_reload_the_record_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        fs::create_dir(directory.path().join(".obsidian")).unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(60)).unwrap();

        fs::write(
            directory.path().join(".obsidian/workspace.json"),
            "{\"active\":true}\n",
        )
        .unwrap();
        fs::write(directory.path().join("attachment.pdf"), b"binary fixture").unwrap();

        assert!(watcher
            .recv_timeout(Duration::from_millis(300))
            .unwrap()
            .is_none());

        fs::write(
            directory.path().join("note.md"),
            "---\ntitle: Visible\n---\n",
        )
        .unwrap();
        // FSEvents may coalesce a later file event with an earlier directory
        // notification even though both classifications are correct. The
        // explicit path rescan is the host's deterministic post-write path and
        // also proves that ignored churn did not stop the worker.
        watcher.rescan_paths(["note.md"]).unwrap();
        let event = watcher
            .recv_timeout(Duration::ZERO)
            .unwrap()
            .expect("record event");
        assert_eq!(event.event_type, "mdbase.record.created");
        assert_eq!(event.payload["path"], "note.md");
    }

    #[test]
    fn filesystem_invalidation_routes_only_semantic_collection_paths() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        let snapshot = Snapshot::load(directory.path()).unwrap();
        let changed = |path: &str| {
            Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
                .add_path(directory.path().join(path))
        };

        assert_eq!(
            snapshot.invalidation_paths(directory.path(), &changed(".obsidian/workspace.json")),
            Some(BTreeSet::new())
        );
        assert_eq!(
            snapshot.invalidation_paths(directory.path(), &changed("attachments/photo.png")),
            Some(BTreeSet::new())
        );
        assert_eq!(
            snapshot.invalidation_paths(directory.path(), &changed("notes/today.md")),
            Some(BTreeSet::from([PathBuf::from("notes/today.md")]))
        );
        assert_eq!(
            snapshot.invalidation_paths(directory.path(), &changed("schemas/note.json")),
            None
        );
        assert_eq!(
            snapshot.invalidation_paths(directory.path(), &changed("views/tasks.base")),
            None
        );
        assert_eq!(
            snapshot.invalidation_paths(directory.path(), &changed("mdbase.yaml")),
            None
        );
    }

    #[test]
    fn watcher_observation_never_settles_a_runtime_transaction() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        let runtime =
            crate::runtime::FilesystemRuntime::open(directory.path(), Duration::from_secs(60))
                .unwrap();
        let request = crate::runtime::OperationRequest::new(
            crate::runtime::OperationKind::Create,
            json!({
                "path": "pending.md",
                "frontmatter": {"id": "pending"},
                "body": "pending body"
            }),
        );
        let prepared = match runtime
            .prepare(
                &request,
                &crate::runtime::HostClaimId::generate(),
                &crate::runtime::OperationContext::legacy(),
            )
            .unwrap()
        {
            crate::runtime::PreparationOutcome::Prepared(prepared) => prepared,
            other => panic!("expected prepared mutation, got {other:?}"),
        };
        crate::transactions::set_runtime_crash_point(prepared.commit_id(), 1);
        assert!(runtime
            .commit(&prepared, &crate::runtime::OperationContext::legacy())
            .is_err());

        let journal_path = directory
            .path()
            .join(".mdbase/transactions")
            .join(prepared.commit_id().as_str())
            .join("journal.json");
        let phase = || {
            serde_json::from_slice::<Value>(&fs::read(&journal_path).unwrap()).unwrap()["phase"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(phase(), "committing");
        assert!(!directory.path().join("pending.md").exists());

        let observed = Snapshot::load(directory.path()).unwrap();
        assert!(observed.records.is_empty());
        assert_eq!(phase(), "committing");
        assert!(!directory.path().join("pending.md").exists());
    }

    #[test]
    fn real_watcher_debounces_to_the_final_record_state() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        // Keep the real-filesystem writes comfortably inside one debounce
        // window even when a shared CI runner pauses this test between them.
        let debounce = Duration::from_millis(250);
        let watcher = CollectionWatcher::open(directory.path(), debounce).unwrap();

        fs::write(directory.path().join("note.md"), "---\ntitle: One\n---\n").unwrap();
        fs::write(directory.path().join("note.md"), "---\ntitle: Two\n---\n").unwrap();

        let event = watcher
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .expect("record event");
        assert_eq!(event.event_type, "mdbase.record.created");
        assert_eq!(event.payload["path"], "note.md");
        assert_eq!(event.payload["after"]["title"], "Two");
        assert!(watcher
            .recv_timeout(debounce + Duration::from_millis(250))
            .unwrap()
            .is_none());
    }

    #[test]
    fn watcher_observes_many_first_writes_in_new_directories() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(30)).unwrap();
        let expected = (0..32)
            .map(|index| format!("new-{index}/note.md"))
            .collect::<BTreeSet<_>>();

        for path in &expected {
            let path = directory.path().join(path);
            fs::create_dir(path.parent().unwrap()).unwrap();
            fs::write(path, "---\ntitle: Immediate\n---\n").unwrap();
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut observed = BTreeSet::new();
        while observed.len() < expected.len() && Instant::now() < deadline {
            if let Some(event) = watcher.recv_timeout(Duration::from_millis(100)).unwrap() {
                if event.event_type == "mdbase.record.created" {
                    observed.insert(event.payload["path"].as_str().unwrap().to_string());
                }
            }
        }
        assert_eq!(observed, expected);
    }

    #[test]
    fn watcher_reconciles_recursive_directory_rename() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        fs::create_dir_all(directory.path().join("before/nested")).unwrap();
        fs::write(
            directory.path().join("before/one.md"),
            "---\ntitle: One\n---\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("before/nested/two.md"),
            "---\ntitle: Two\n---\n",
        )
        .unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(30)).unwrap();

        fs::rename(
            directory.path().join("before"),
            directory.path().join("after"),
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut renames = BTreeSet::new();
        while renames.len() < 2 && Instant::now() < deadline {
            if let Some(event) = watcher.recv_timeout(Duration::from_millis(100)).unwrap() {
                if event.event_type == "mdbase.record.renamed" {
                    renames.insert((
                        event.payload["from"].as_str().unwrap().to_string(),
                        event.payload["to"].as_str().unwrap().to_string(),
                    ));
                }
            }
        }
        assert_eq!(
            renames,
            BTreeSet::from([
                (
                    "before/nested/two.md".to_string(),
                    "after/nested/two.md".to_string()
                ),
                ("before/one.md".to_string(), "after/one.md".to_string()),
            ])
        );
        assert!(watcher
            .recv_timeout(Duration::from_millis(200))
            .unwrap()
            .is_none());
    }

    #[test]
    fn asymmetric_duplicate_revisions_never_invent_rename_identity() {
        let state = || RecordState {
            revision: "sha256:duplicate".to_string(),
            raw_frontmatter: Map::new(),
            effective_frontmatter: json!({}),
            types: json!([]),
            body: "same".to_string(),
        };
        for (before, after) in [
            (
                BTreeMap::from([
                    ("before-a.md".to_string(), state()),
                    ("before-b.md".to_string(), state()),
                ]),
                BTreeMap::from([("after.md".to_string(), state())]),
            ),
            (
                BTreeMap::from([("before.md".to_string(), state())]),
                BTreeMap::from([
                    ("after-a.md".to_string(), state()),
                    ("after-b.md".to_string(), state()),
                ]),
            ),
        ] {
            let events = record_events(&before, &after);
            assert!(events
                .iter()
                .all(|event| event.event_type != "mdbase.record.renamed"));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.event_type == "mdbase.record.deleted")
                    .count(),
                before.len()
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.event_type == "mdbase.record.created")
                    .count(),
                after.len()
            );
        }
    }

    #[test]
    fn duplicate_content_recursive_rename_stays_exact_and_has_no_duplicates() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        fs::create_dir_all(directory.path().join("before/nested")).unwrap();
        let duplicate = "---\ntitle: Duplicate\n---\nSame\n";
        fs::write(directory.path().join("before/one.md"), duplicate).unwrap();
        fs::write(directory.path().join("before/nested/two.md"), duplicate).unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(30)).unwrap();

        fs::rename(
            directory.path().join("before"),
            directory.path().join("after"),
        )
        .unwrap();

        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while events.len() < 4 && Instant::now() < deadline {
            if let Some(event) = watcher.recv_timeout(Duration::from_millis(100)).unwrap() {
                events.push((
                    event.event_type,
                    event.payload["path"].as_str().unwrap().to_string(),
                ));
            }
        }
        assert_eq!(
            events,
            vec![
                (
                    "mdbase.record.deleted".to_string(),
                    "before/nested/two.md".to_string()
                ),
                (
                    "mdbase.record.deleted".to_string(),
                    "before/one.md".to_string()
                ),
                (
                    "mdbase.record.created".to_string(),
                    "after/nested/two.md".to_string()
                ),
                (
                    "mdbase.record.created".to_string(),
                    "after/one.md".to_string()
                ),
            ]
        );
        assert!(watcher
            .recv_timeout(Duration::from_millis(200))
            .unwrap()
            .is_none());
    }

    #[test]
    fn watcher_reconciles_real_recursive_directory_removal_once() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        fs::create_dir_all(directory.path().join("gone/nested")).unwrap();
        fs::write(
            directory.path().join("gone/one.md"),
            "---\ntitle: One\n---\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("gone/nested/two.md"),
            "---\ntitle: Two\n---\n",
        )
        .unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(30)).unwrap();

        fs::remove_dir_all(directory.path().join("gone")).unwrap();
        let mut deleted = BTreeSet::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while deleted.len() < 2 && Instant::now() < deadline {
            if let Some(event) = watcher.recv_timeout(Duration::from_millis(100)).unwrap() {
                assert_eq!(event.event_type, "mdbase.record.deleted");
                deleted.insert(event.payload["path"].as_str().unwrap().to_string());
            }
        }
        assert_eq!(
            deleted,
            BTreeSet::from(["gone/nested/two.md".to_string(), "gone/one.md".to_string()])
        );
        assert!(watcher
            .recv_timeout(Duration::from_millis(200))
            .unwrap()
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn watcher_rejects_tracked_symlink_replacements_and_keeps_converging() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        let marker = "EXTERNAL_MARKER_MUST_NOT_BE_READ";
        fs::write(
            outside.path().join("poison.md"),
            format!("---\ntitle: Poison\nmarker: {marker}\n---\n"),
        )
        .unwrap();
        fs::write(
            directory.path().join("tracked.md"),
            "---\ntitle: Tracked\n---\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("dangling.md"),
            "---\ntitle: Dangling\n---\n",
        )
        .unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(30)).unwrap();

        fs::remove_file(directory.path().join("tracked.md")).unwrap();
        symlink(
            outside.path().join("poison.md"),
            directory.path().join("tracked.md"),
        )
        .unwrap();
        fs::remove_file(directory.path().join("dangling.md")).unwrap();
        symlink(
            outside.path().join("missing.md"),
            directory.path().join("dangling.md"),
        )
        .unwrap();
        watcher
            .commands
            .send(WorkerInput::Filesystem(Ok(Event::new(EventKind::Other))))
            .unwrap();
        fs::write(
            directory.path().join("unrelated.md"),
            "---\ntitle: Safe\n---\n",
        )
        .unwrap();

        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while events.len() < 3 && Instant::now() < deadline {
            if let Some(event) = watcher.recv_timeout(Duration::from_millis(100)).unwrap() {
                assert!(!event.payload.to_string().contains(marker));
                events.push((event.event_type, event.payload));
            }
        }
        assert_eq!(
            events
                .iter()
                .map(|(kind, payload)| (kind.as_str(), payload["path"].as_str().unwrap()))
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                ("mdbase.record.deleted", "dangling.md"),
                ("mdbase.record.deleted", "tracked.md"),
                ("mdbase.record.created", "unrelated.md"),
            ])
        );
        fs::write(
            directory.path().join("continued.md"),
            "---\ntitle: Continued\n---\n",
        )
        .unwrap();
        watcher.rescan_paths(["continued.md"]).unwrap();
        let continued = watcher.recv_timeout(Duration::ZERO).unwrap().unwrap();
        assert_eq!(continued.payload["path"], "continued.md");
        assert!(!continued.payload.to_string().contains(marker));
        assert!(watcher
            .recv_timeout(Duration::from_millis(200))
            .unwrap()
            .is_none());
    }

    #[test]
    fn transient_record_open_failure_retains_state_waiter_and_pending_reconciliation() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("tracked.md"),
            "---\ntitle: Before\n---\noriginal body\n",
        )
        .unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(20)).unwrap();
        watcher.rescan().unwrap();
        assert!(watcher.recv_timeout(Duration::ZERO).unwrap().is_none());

        crate::operations::set_record_open_failure(
            directory.path(),
            "tracked.md",
            Some(std::io::ErrorKind::Interrupted),
        );
        fs::write(
            directory.path().join("tracked.md"),
            "---\ntitle: After\n---\nupdated body\n",
        )
        .unwrap();
        let (waiter, waiter_rx) = mpsc::sync_channel(0);
        watcher
            .commands
            .send(WorkerInput::Command(Command::RescanPaths(
                vec![PathBuf::from("tracked.md")],
                waiter,
            )))
            .unwrap();

        let diagnostic = watcher
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .expect("failed refresh diagnostic");
        assert_eq!(
            diagnostic
                .payload
                .pointer("/diagnostic/code")
                .and_then(Value::as_str),
            Some("collection_reload_failed")
        );
        assert!(!diagnostic.payload.to_string().contains("tracked.md"));
        assert!(matches!(
            waiter_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        let quiet_until = Instant::now() + Duration::from_millis(350);
        while Instant::now() < quiet_until {
            if let Some(event) = watcher.recv_timeout(Duration::from_millis(25)).unwrap() {
                assert_ne!(event.event_type, "mdbase.record.deleted");
                assert!(event.payload.get("diagnostic").is_some());
            }
        }
        assert!(matches!(
            waiter_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        crate::operations::set_record_open_failure(directory.path(), "tracked.md", None);
        let deadline = Instant::now() + Duration::from_secs(2);
        let modified = loop {
            assert!(Instant::now() < deadline, "pending refresh did not recover");
            let Some(event) = watcher.recv_timeout(Duration::from_millis(100)).unwrap() else {
                continue;
            };
            assert_ne!(event.event_type, "mdbase.record.deleted");
            if event.event_type == "mdbase.record.modified" {
                break event;
            }
        };
        assert_eq!(modified.payload["path"], "tracked.md");
        assert_eq!(modified.payload["before"]["title"], "Before");
        assert_eq!(modified.payload["after"]["title"], "After");
        waiter_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("retained waiter completes after retry");
        assert!(watcher
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn failed_refresh_retries_recover_with_bounded_diagnostics() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let config = "spec_version: 0.3.0\nsettings:\n  validation: warn\n";
        fs::write(directory.path().join("mdbase.yaml"), config).unwrap();
        fs::write(outside.path().join("mdbase.yaml"), config).unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(20)).unwrap();

        fs::remove_file(directory.path().join("mdbase.yaml")).unwrap();
        symlink(
            outside.path().join("mdbase.yaml"),
            directory.path().join("mdbase.yaml"),
        )
        .unwrap();
        fs::write(
            directory.path().join("pending.md"),
            "---\ntitle: Pending\n---\n",
        )
        .unwrap();

        let first = watcher
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .expect("failed refresh diagnostic");
        assert_eq!(
            first
                .payload
                .pointer("/diagnostic/code")
                .and_then(Value::as_str),
            Some("collection_reload_failed")
        );
        let (waiter, waiter_rx) = mpsc::sync_channel(0);
        watcher
            .commands
            .send(WorkerInput::Command(Command::Rescan(waiter)))
            .unwrap();
        let churn_until = Instant::now() + Duration::from_millis(900);
        let mut index = 0;
        while Instant::now() < churn_until {
            let hidden = directory.path().join(format!(".hidden-{index}"));
            fs::create_dir(&hidden).unwrap();
            fs::write(hidden.join("state.json"), b"ignored").unwrap();
            fs::write(
                directory.path().join(format!("attachment-{index}.bin")),
                b"binary",
            )
            .unwrap();
            fs::remove_dir_all(hidden).unwrap();
            watcher
                .commands
                .send(WorkerInput::Filesystem(Ok(Event::new(EventKind::Other))))
                .unwrap();
            index += 1;
            thread::sleep(Duration::from_millis(15));
        }
        assert!(
            waiter_rx.try_recv().is_err(),
            "failed refresh waiter settled early"
        );

        fs::remove_file(directory.path().join("mdbase.yaml")).unwrap();
        fs::write(directory.path().join("mdbase.yaml"), config).unwrap();
        fs::write(
            directory.path().join("fresh.md"),
            "---\ntitle: Fresh\n---\n",
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut created = BTreeSet::new();
        let mut diagnostics = 1;
        while created.len() < 2 && Instant::now() < deadline {
            if let Some(event) = watcher.recv_timeout(Duration::from_millis(100)).unwrap() {
                if event.payload.get("diagnostic").is_some() {
                    diagnostics += 1;
                } else if event.event_type == "mdbase.record.created" {
                    created.insert(event.payload["path"].as_str().unwrap().to_string());
                }
            }
        }
        assert_eq!(
            created,
            BTreeSet::from(["fresh.md".to_string(), "pending.md".to_string()])
        );
        assert_eq!(diagnostics, 1);
        waiter_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("retained waiter completes after recovery");
    }

    #[test]
    fn explicit_rescan_returns_after_changes_are_available() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(60)).unwrap();

        fs::write(directory.path().join("note.md"), "---\ntitle: Ready\n---\n").unwrap();
        watcher.rescan().unwrap();

        let event = watcher
            .recv_timeout(Duration::ZERO)
            .unwrap()
            .expect("rescan must queue the record event before returning");
        assert_eq!(event.event_type, "mdbase.record.created");
        assert_eq!(event.payload["path"], "note.md");
    }

    #[test]
    fn watcher_emits_contract_schema_and_view_resource_changes() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\nx-obsidian:\n  bases:\n    include:\n      - views/**/*.base\n",
        )
        .unwrap();
        fs::create_dir(directory.path().join("_contracts")).unwrap();
        fs::create_dir(directory.path().join("_schemas")).unwrap();
        fs::create_dir(directory.path().join("views")).unwrap();
        fs::write(
            directory.path().join("_contracts/task.md"),
            "---\nkind: mdbase.contract\ncontract_type: record\nid: example.task\nversion: 1.0.0\nrecord_schema:\n  dialect: json-schema-2020-12\n  ref: ../_schemas/task.json\n---\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("_schemas/task.json"),
            "{\"$schema\":\"https://json-schema.org/draft/2020-12/schema\",\"type\":\"object\"}\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("views/tasks.base"),
            "views:\n  - type: table\n    name: Tasks\n",
        )
        .unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(20)).unwrap();

        fs::write(
            directory.path().join("_contracts/task.md"),
            "---\nkind: mdbase.contract\ncontract_type: record\nid: example.task\nversion: 1.0.1\nrecord_schema:\n  dialect: json-schema-2020-12\n  ref: ../_schemas/task.json\n---\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("_schemas/task.json"),
            "{\"$schema\":\"https://json-schema.org/draft/2020-12/schema\",\"type\":\"object\",\"additionalProperties\":false}\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("views/tasks.base"),
            "views:\n  - type: table\n    name: Changed\n",
        )
        .unwrap();
        watcher.rescan().unwrap();

        let mut kinds = BTreeSet::new();
        while let Some(event) = watcher.recv_timeout(Duration::ZERO).unwrap() {
            kinds.insert(event.event_type);
        }
        assert_eq!(
            kinds,
            [
                "mdbase.contract.changed".to_string(),
                "mdbase.schema.changed".to_string(),
                "mdbase.view.changed".to_string(),
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn watcher_skips_invalid_yaml_during_open_and_recovers_after_fix() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("broken.md"),
            "---\ntitle: One\ntitle: Two\n---\n",
        )
        .unwrap();

        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(60))
            .expect("invalid record frontmatter must not prevent watcher startup");

        fs::write(
            directory.path().join("broken.md"),
            "---\ntitle: Fixed\n---\n",
        )
        .unwrap();
        watcher.rescan_paths(["broken.md"]).unwrap();

        let event = watcher
            .recv_timeout(Duration::ZERO)
            .unwrap()
            .expect("fixed record should enter the snapshot");
        assert_eq!(event.event_type, "mdbase.record.created");
        assert_eq!(event.payload["path"], "broken.md");
        assert_eq!(event.payload["after"]["title"], "Fixed");
    }

    #[test]
    fn watcher_skips_new_invalid_yaml_without_stopping() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(60)).unwrap();

        fs::write(
            directory.path().join("broken.md"),
            "---\ntitle: One\ntitle: Two\n---\n",
        )
        .unwrap();
        watcher.rescan_paths(["broken.md"]).unwrap();
        assert!(watcher.recv_timeout(Duration::ZERO).unwrap().is_none());

        fs::write(
            directory.path().join("valid.md"),
            "---\ntitle: Valid\n---\n",
        )
        .unwrap();
        watcher.rescan_paths(["valid.md"]).unwrap();
        let event = watcher
            .recv_timeout(Duration::ZERO)
            .unwrap()
            .expect("watcher should continue after skipping invalid YAML");
        assert_eq!(event.event_type, "mdbase.record.created");
        assert_eq!(event.payload["path"], "valid.md");
    }
}
