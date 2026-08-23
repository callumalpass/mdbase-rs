use super::{PortableWatchEvent, WatchEvent};
use crate::runtime::CollectionSnapshotResourceKind;
use crate::Collection;
use notify::{
    event::{MetadataKind, ModifyKind},
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
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
    let mut deadline = Some(Instant::now() + debounce);
    let mut pending_rescans = Vec::new();
    let mut pending_paths = BTreeSet::new();
    let mut full_rescan = true;
    let mut last_refresh_error: Option<(String, Instant)> = None;
    let mut refresh_retry_delay = Duration::from_millis(250);

    loop {
        let current_time = Instant::now();
        let wait = deadline
            .map(|deadline| deadline.saturating_duration_since(current_time).min(tick))
            .unwrap_or(tick);
        match inputs.recv_timeout(wait) {
            Ok(WorkerInput::Command(Command::Stop)) => return,
            Ok(WorkerInput::Command(Command::Rescan(ready))) => {
                deadline = Some(Instant::now());
                refresh_retry_delay = Duration::from_millis(250);
                full_rescan = true;
                pending_rescans.push(ready);
            }
            Ok(WorkerInput::Command(Command::RescanPaths(paths, ready))) => {
                deadline = Some(Instant::now());
                refresh_retry_delay = Duration::from_millis(250);
                pending_paths.extend(paths);
                pending_rescans.push(ready);
            }
            Ok(WorkerInput::Filesystem(Ok(event))) if invalidates_snapshot(&event) => {
                refresh_retry_delay = Duration::from_millis(250);
                let invalidation = snapshot.invalidation_paths(&root, &event);
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
                deadline = Some(Instant::now() + debounce)
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
            let refresh_succeeded = refreshed.is_ok();
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
                }
                Err(error) => {
                    // A transient symlink replacement or atomic-save gap must not
                    // consume the recovery request. Keep reconciling, but back
                    // off and coalesce identical diagnostics so a broken path
                    // cannot spin the worker or flood its consumer.
                    full_rescan = true;
                    let message = error.to_string();
                    let should_report = last_refresh_error.as_ref().is_none_or(|(previous, at)| {
                        previous != &message || at.elapsed() >= Duration::from_secs(1)
                    });
                    if should_report {
                        sequence += 1;
                        if events
                            .send(watch_error_event(sequence, message.clone()))
                            .is_err()
                        {
                            return;
                        }
                        last_refresh_error = Some((message, Instant::now()));
                    }
                    deadline = Some(Instant::now() + refresh_retry_delay);
                    refresh_retry_delay = (refresh_retry_delay * 2).min(Duration::from_secs(5));
                }
            }
            if refresh_succeeded {
                last_refresh_error = None;
                refresh_retry_delay = Duration::from_millis(250);
            }
            if watch_profile_enabled() {
                eprintln!(
                    "mdbase_watch refresh mode={} record_paths={} elapsed_us={}",
                    refresh_mode,
                    refresh_path_count,
                    refresh_started.elapsed().as_micros(),
                );
            }
            if refresh_succeeded {
                pending_paths.clear();
                full_rescan = false;
            }
            for ready in pending_rescans.drain(..) {
                let _ = ready.send(());
            }
        }
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
        // Rename notifications are intentionally conservative. In particular,
        // directory renames often carry no record extension, so returning an
        // empty incremental set would permanently miss the moved subtree.
        if matches!(event.kind, EventKind::Modify(ModifyKind::Name(_))) {
            return None;
        }
        if event.paths.is_empty() || matches!(event.kind, EventKind::Any | EventKind::Other) {
            return None;
        }
        let mut records = BTreeSet::new();
        for path in &event.paths {
            let relative = path.strip_prefix(root).unwrap_or(path);
            let normalized = relative.to_string_lossy().replace('\\', "/");
            if normalized.is_empty() {
                return None;
            }
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
            if self.is_ignored_path(&normalized) {
                continue;
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
    let deleted_paths = deleted.iter().cloned().collect::<Vec<_>>();
    for from in deleted_paths {
        let previous = &before[&from];
        let matches = created
            .iter()
            .filter(|to| after[*to].revision == previous.revision)
            .cloned()
            .collect::<Vec<_>>();
        if matches.len() == 1 {
            let to = matches[0].clone();
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
    let full_path = collection.root.join(path);
    match std::fs::symlink_metadata(&full_path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(WatchError::Collection(error.to_string())),
    }
    let read = collection.read(&json!({"path": path}));
    let revision = match read.get("revision").and_then(Value::as_str) {
        Some(revision) => revision.to_string(),
        None if is_invalid_yaml_frontmatter(&read) => return Ok(None),
        None => return Err(WatchError::Collection(collection_error(&read))),
    };
    let canonical = collection
        .snapshot_record(path)
        .map_err(|error| WatchError::Collection(error.to_string()))?;
    let raw_frontmatter = canonical.frontmatter;
    let effective_frontmatter = read
        .get("effective_frontmatter")
        .cloned()
        .unwrap_or_else(|| Value::Object(raw_frontmatter.clone()));
    let types = read.get("types").cloned().unwrap_or_else(|| json!([]));
    let body = canonical.body;
    Ok(Some(RecordState {
        revision,
        raw_frontmatter,
        effective_frontmatter,
        types,
        body,
    }))
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
    use notify::event::{AccessKind, AccessMode, CreateKind, DataChange, RemoveKind};
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
    fn directory_rename_forces_a_full_rescan() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        let snapshot = Snapshot::load(directory.path()).unwrap();
        let event = Event::new(EventKind::Modify(ModifyKind::Name(
            notify::event::RenameMode::Both,
        )))
        .add_path(directory.path().join("old"))
        .add_path(directory.path().join("new"));
        assert_eq!(snapshot.invalidation_paths(directory.path(), &event), None);
    }

    #[cfg(unix)]
    #[test]
    fn real_watcher_recovers_after_directory_rename_and_symlink_episode() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        fs::create_dir(directory.path().join("old")).unwrap();
        fs::write(
            directory.path().join("old/note.md"),
            "---\ntitle: Before\n---\n",
        )
        .unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(40)).unwrap();

        fs::rename(directory.path().join("old"), directory.path().join("new")).unwrap();
        watcher.rescan().unwrap();
        let renamed = watcher
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .expect("directory rename must converge through a full rescan");
        assert_eq!(renamed.event_type, "mdbase.record.renamed");
        assert_eq!(renamed.payload["to"], "new/note.md");

        let note = directory.path().join("new/note.md");
        let target = directory.path().join("target.md");
        fs::rename(&note, &target).unwrap();
        symlink("target.md", &note).unwrap();
        watcher.rescan_paths(["new/note.md"]).unwrap();
        while watcher.recv_timeout(Duration::ZERO).unwrap().is_some() {}
        fs::remove_file(&note).unwrap();
        fs::rename(&target, &note).unwrap();
        fs::write(&note, "---\ntitle: After\n---\n").unwrap();
        watcher.rescan_paths(["new/note.md"]).unwrap();
        watcher.rescan().unwrap();
        let recovered = (0..8)
            .filter_map(|_| watcher.recv_timeout(Duration::from_millis(500)).unwrap())
            .find(|event| {
                matches!(
                    event.event_type.as_str(),
                    "mdbase.record.created" | "mdbase.record.modified"
                )
            })
            .expect("ordinary edits must ingest after symlink replacement");
        assert_eq!(recovered.payload["after"]["title"], "After");
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
