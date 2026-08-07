use super::{PortableWatchEvent, WatchEvent};
use crate::Collection;
use notify::{
    event::{MetadataKind, ModifyKind, RenameMode},
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;
use walkdir::WalkDir;

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

    loop {
        let current_time = Instant::now();
        let wait = deadline
            .map(|deadline| deadline.saturating_duration_since(current_time).min(tick))
            .unwrap_or(tick);
        match inputs.recv_timeout(wait) {
            Ok(WorkerInput::Command(Command::Stop)) => return,
            Ok(WorkerInput::Command(Command::Rescan(ready))) => {
                deadline = Some(Instant::now());
                full_rescan = true;
                pending_rescans.push(ready);
            }
            Ok(WorkerInput::Command(Command::RescanPaths(paths, ready))) => {
                deadline = Some(Instant::now());
                pending_paths.extend(paths);
                pending_rescans.push(ready);
            }
            Ok(WorkerInput::Filesystem(Ok(event))) if invalidates_snapshot(&event) => {
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
                    sequence += 1;
                    if events
                        .send(watch_error_event(sequence, error.to_string()))
                        .is_err()
                    {
                        return;
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
            pending_paths.clear();
            full_rescan = false;
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
    config_revision: String,
    types: BTreeMap<String, String>,
    records: BTreeMap<String, RecordState>,
    types_folder: String,
    contracts_folder: String,
    cache_folder: String,
    record_extensions: BTreeSet<String>,
}

#[derive(Clone)]
struct RecordState {
    revision: String,
    raw_frontmatter: Map<String, Value>,
    effective_frontmatter: Value,
    types: Value,
}

impl Snapshot {
    fn load(root: &Path) -> Result<Self, WatchError> {
        let collection = Collection::open(root)
            .map_err(|error| WatchError::Collection(collection_error(&error)))?;
        let config_revision = file_revision(&root.join("mdbase.yaml"))?;
        let types = type_revisions(root, &collection.settings.types_folder)?;
        let mut records = BTreeMap::new();
        let mut files = collection.scan_collection_files();
        files.sort();
        for path in files {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if let Some(record) = load_record(&collection, &relative)? {
                records.insert(relative, record);
            }
        }
        Ok(Self {
            config_revision,
            types,
            records,
            types_folder: collection.settings.types_folder.clone(),
            contracts_folder: collection.settings.contracts_folder.clone(),
            cache_folder: collection.settings.cache_folder.clone(),
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
        if event.paths.is_empty() || matches!(event.kind, EventKind::Any | EventKind::Other) {
            return None;
        }
        let mut records = BTreeSet::new();
        let mut may_change_record_tree = false;
        for path in &event.paths {
            let relative = path.strip_prefix(root).unwrap_or(path);
            let normalized = relative.to_string_lossy().replace('\\', "/");
            if normalized.is_empty() {
                return None;
            }
            if normalized == "mdbase.yaml"
                || normalized == self.types_folder
                || normalized.starts_with(&format!("{}/", self.types_folder))
                || normalized == self.contracts_folder
                || normalized.starts_with(&format!("{}/", self.contracts_folder))
            {
                return None;
            }
            if normalized == self.cache_folder
                || normalized.starts_with(&format!("{}/", self.cache_folder))
                || normalized == ".git"
                || normalized.starts_with(".git/")
                || normalized == "node_modules"
                || normalized.starts_with("node_modules/")
            {
                continue;
            }
            let extension = relative.extension().and_then(|value| value.to_str());
            if extension.is_some_and(|extension| self.record_extensions.contains(extension)) {
                records.insert(PathBuf::from(normalized));
            } else if matches!(
                event.kind,
                EventKind::Create(notify::event::CreateKind::Folder)
                    | EventKind::Remove(notify::event::RemoveKind::Folder)
                    | EventKind::Modify(ModifyKind::Name(
                        RenameMode::Any | RenameMode::To | RenameMode::Both | RenameMode::Other
                    ))
            ) {
                may_change_record_tree = true;
            }
        }
        if records.is_empty() && may_change_record_tree {
            None
        } else {
            Some(records)
        }
    }

    fn refresh_paths(
        &mut self,
        root: &Path,
        paths: &BTreeSet<PathBuf>,
    ) -> Result<Vec<PendingEvent>, WatchError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let collection = Collection::open(root)
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
        if self.config_revision != next.config_revision {
            events.push(PendingEvent::new(
                "mdbase.config.changed",
                json!({
                    "previous_revision": self.config_revision,
                    "revision": next.config_revision,
                }),
            ));
        }

        for path in self
            .types
            .keys()
            .chain(next.types.keys())
            .collect::<BTreeSet<_>>()
        {
            let before = self.types.get(path);
            let after = next.types.get(path);
            if before != after {
                events.push(PendingEvent::new(
                    "mdbase.type.changed",
                    json!({
                        "path": path,
                        "previous_revision": before,
                        "revision": after,
                    }),
                ));
            }
        }

        events.extend(record_events(&self.records, &next.records));

        events
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
    let raw_frontmatter = read
        .get("frontmatter")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
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

fn type_revisions(root: &Path, types_folder: &str) -> Result<BTreeMap<String, String>, WatchError> {
    let mut result = BTreeMap::new();
    let types_root = root.join(types_folder);
    if !types_root.exists() {
        return Ok(result);
    }
    for entry in WalkDir::new(&types_root).sort_by_file_name() {
        let entry = entry.map_err(|error| WatchError::Collection(error.to_string()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        result.insert(relative, file_revision(path)?);
    }
    Ok(result)
}

fn file_revision(path: &Path) -> Result<String, WatchError> {
    std::fs::read(path)
        .map(|bytes| crate::v03::revision(&bytes))
        .map_err(|error| WatchError::Collection(error.to_string()))
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
