use super::{PortableWatchEvent, WatchEvent};
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
    commands: mpsc::Sender<Command>,
    worker: Option<thread::JoinHandle<()>>,
}

enum Command {
    Rescan(mpsc::SyncSender<()>),
    Stop,
}

impl CollectionWatcher {
    pub fn open(root: impl AsRef<Path>, debounce: Duration) -> Result<Self, WatchError> {
        let root = root.as_ref().to_path_buf();
        let initial = Snapshot::load(&root)?;
        let (events_tx, events) = mpsc::channel();
        let (commands, command_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("mdbase-watch".to_string())
            .spawn(move || watch_loop(root, debounce, initial, command_rx, events_tx, ready_tx))
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
            .send(Command::Rescan(ready))
            .map_err(|_| WatchError::Stopped)?;
        receiver.recv().map_err(|_| WatchError::Stopped)
    }
}

impl Drop for CollectionWatcher {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn watch_loop(
    root: PathBuf,
    debounce: Duration,
    mut snapshot: Snapshot,
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<WatchEvent>,
    ready: mpsc::SyncSender<Result<(), notify::Error>>,
) {
    let (raw_tx, raw_rx) = mpsc::channel();
    let mut watcher: RecommendedWatcher = match notify::recommended_watcher(move |event| {
        let _ = raw_tx.send(event);
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
    let _ = ready.send(Ok(()));

    let tick = debounce
        .min(Duration::from_millis(50))
        .max(Duration::from_millis(5));
    // Compare once after the OS watch is installed. This closes the small gap
    // between the caller's initial snapshot and watcher registration.
    let mut deadline: Option<Instant> = Some(Instant::now());
    let mut pending_rescans = Vec::new();
    let mut sequence = 0_u64;

    loop {
        while let Ok(command) = commands.try_recv() {
            match command {
                Command::Stop => return,
                Command::Rescan(ready) => {
                    deadline = Some(Instant::now());
                    pending_rescans.push(ready);
                }
            }
        }

        match raw_rx.recv_timeout(tick) {
            Ok(Ok(event)) if invalidates_snapshot(&event) => {
                deadline = Some(Instant::now() + debounce)
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
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
            for ready in pending_rescans.drain(..) {
                let _ = ready.send(());
            }
        }
    }
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
            let bytes =
                std::fs::read(&path).map_err(|error| WatchError::Collection(error.to_string()))?;
            let read = collection.read(&json!({"path": relative}));
            let raw_frontmatter = read
                .get("raw_frontmatter")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let effective_frontmatter = read
                .get("frontmatter")
                .cloned()
                .unwrap_or_else(|| Value::Object(raw_frontmatter.clone()));
            let matched_types = read.get("types").cloned().unwrap_or_else(|| json!([]));
            records.insert(
                relative,
                RecordState {
                    revision: crate::v03::revision(&bytes),
                    raw_frontmatter,
                    effective_frontmatter,
                    types: matched_types,
                },
            );
        }
        Ok(Self {
            config_revision,
            types,
            records,
        })
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

        let mut deleted: BTreeSet<String> = self
            .records
            .keys()
            .filter(|path| !next.records.contains_key(*path))
            .cloned()
            .collect();
        let mut created: BTreeSet<String> = next
            .records
            .keys()
            .filter(|path| !self.records.contains_key(*path))
            .cloned()
            .collect();

        let deleted_paths = deleted.iter().cloned().collect::<Vec<_>>();
        for from in deleted_paths {
            let previous = &self.records[&from];
            let matches = created
                .iter()
                .filter(|to| next.records[*to].revision == previous.revision)
                .cloned()
                .collect::<Vec<_>>();
            if matches.len() == 1 {
                let to = matches[0].clone();
                let current = &next.records[&to];
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
            let previous = &self.records[&path];
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
            let current = &next.records[&path];
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
        for path in self.records.keys() {
            let (Some(previous), Some(current)) = (self.records.get(path), next.records.get(path))
            else {
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
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(60)).unwrap();

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
            .recv_timeout(Duration::from_millis(250))
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
}
