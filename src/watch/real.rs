use super::{PortableWatchEvent, WatchEvent};
use crate::record_load::RecordLoadOutcome;
use crate::runtime::{CollectionSnapshotResourceKind, OperationContext, ProviderError};
use crate::Collection;
use notify::{
    event::{CreateKind, MetadataKind, ModifyKind, RemoveKind},
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
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
    #[error("collection watcher has too many pending rescans; retry later")]
    RescanBackpressure,
    #[error("collection watcher reconciliation revision exhausted")]
    RevisionExhausted,
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
    pending_rescans: Arc<AtomicUsize>,
    next_rescan_id: Arc<AtomicU64>,
    invalidation_revision: Arc<AtomicU64>,
    epoch: Arc<WatcherEpoch>,
    #[cfg(test)]
    filesystem_callback: FilesystemCallback,
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct WatcherTestControl {
    commands: mpsc::Sender<WorkerInput>,
    pending_rescans: Arc<AtomicUsize>,
    invalidation_revision: Arc<AtomicU64>,
    epoch: Arc<WatcherEpoch>,
    filesystem_callback: FilesystemCallback,
}

#[cfg(test)]
impl WatcherTestControl {
    pub(crate) fn pending_rescan_count(&self) -> usize {
        self.pending_rescans.load(Ordering::Acquire)
    }

    pub(crate) fn set_invalidation_revision(&self, value: u64) {
        self.invalidation_revision.store(value, Ordering::Release);
    }

    pub(crate) fn invalidation_revision(&self) -> u64 {
        self.invalidation_revision.load(Ordering::Acquire)
    }

    pub(crate) fn invoke_installed_modify_callback(&self, path: &Path) {
        (self.filesystem_callback)(Ok(Event {
            kind: EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            paths: vec![path.to_path_buf()],
            attrs: Default::default(),
        }));
    }

    pub(crate) fn poison(&self) {
        poison_watcher(&self.epoch, &self.commands);
    }

    pub(crate) fn install_acknowledgement_linearization_hook(&self) -> LinearizationRace {
        self.epoch.install_hook(LinearizationPoint::Acknowledgement)
    }

    pub(crate) fn install_cache_commit_linearization_hook(&self) -> LinearizationRace {
        self.epoch.install_hook(LinearizationPoint::CacheCommit)
    }
}

type FilesystemCallback = Arc<dyn Fn(Result<Event, notify::Error>) + Send + Sync>;

pub(crate) struct WatcherEpoch {
    exhausted: AtomicBool,
    linearization: Mutex<()>,
    #[cfg(test)]
    hooks: LinearizationHooks,
}

impl WatcherEpoch {
    pub(crate) fn new() -> Self {
        Self {
            exhausted: AtomicBool::new(false),
            linearization: Mutex::new(()),
            #[cfg(test)]
            hooks: LinearizationHooks::default(),
        }
    }

    pub(crate) fn is_exhausted(&self) -> bool {
        self.exhausted.load(Ordering::Acquire)
    }

    pub(crate) fn linearize(&self) -> MutexGuard<'_, ()> {
        self.linearization
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    fn install_hook(&self, point: LinearizationPoint) -> LinearizationRace {
        let (reached_tx, reached_rx) = mpsc::sync_channel(0);
        let (resume_tx, resume_rx) = mpsc::sync_channel(0);
        *self.hooks.slot(point).lock().unwrap() = Some(LinearizationHook {
            reached: reached_tx,
            resume: resume_rx,
        });
        LinearizationRace {
            reached: reached_rx,
            resume: resume_tx,
        }
    }

    #[cfg(test)]
    pub(crate) fn run_hook(&self, point: LinearizationPoint) {
        let hook = self.hooks.slot(point).lock().unwrap().take();
        if let Some(hook) = hook {
            let _ = hook.reached.send(());
            let _ = hook.resume.recv();
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum LinearizationPoint {
    Waiter,
    Acknowledgement,
    CacheCommit,
}

#[cfg(test)]
#[derive(Default)]
struct LinearizationHooks {
    waiter: Mutex<Option<LinearizationHook>>,
    acknowledgement: Mutex<Option<LinearizationHook>>,
    cache_commit: Mutex<Option<LinearizationHook>>,
}

#[cfg(test)]
impl LinearizationHooks {
    fn slot(&self, point: LinearizationPoint) -> &Mutex<Option<LinearizationHook>> {
        match point {
            LinearizationPoint::Waiter => &self.waiter,
            LinearizationPoint::Acknowledgement => &self.acknowledgement,
            LinearizationPoint::CacheCommit => &self.cache_commit,
        }
    }
}

#[cfg(test)]
struct LinearizationHook {
    reached: mpsc::SyncSender<()>,
    resume: mpsc::Receiver<()>,
}

#[cfg(test)]
pub(crate) struct LinearizationRace {
    reached: mpsc::Receiver<()>,
    resume: mpsc::SyncSender<()>,
}

#[cfg(test)]
impl LinearizationRace {
    pub(crate) fn wait_until_reached(&self) {
        self.reached.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    pub(crate) fn resume(self) {
        self.resume.send(()).unwrap();
    }
}

#[derive(Clone, Copy, Debug)]
enum ReconciliationFailure {
    RevisionExhausted,
}

type ReconciliationResult = Result<Arc<ReconciliationOutcome>, ReconciliationFailure>;
type ReconciliationSender = mpsc::Sender<ReconciliationResult>;

struct PendingRescan {
    id: u64,
    ready: ReconciliationSender,
    ticket: Arc<RescanTicket>,
}

enum Command {
    Rescan(PendingRescan),
    RescanPaths(Vec<PathBuf>, PendingRescan),
    CancelRescan(u64),
    Acknowledge {
        outcome: Arc<ReconciliationOutcome>,
        active: Arc<AtomicBool>,
        ready: mpsc::Sender<Result<bool, ReconciliationFailure>>,
    },
    Stop,
}

pub(crate) struct ReconciliationOutcome {
    pub(crate) invalid_records: Arc<BTreeSet<String>>,
    pub(crate) removed_invalid_records: Arc<BTreeSet<String>>,
    revision: u64,
    pub(crate) epoch: Arc<WatcherEpoch>,
}

#[derive(Clone)]
pub(crate) struct ReconciliationToken {
    epoch: Arc<WatcherEpoch>,
    revision: u64,
}

impl ReconciliationToken {
    pub(crate) fn is_later_in_same_epoch_than(&self, previous: &Self) -> bool {
        Arc::ptr_eq(&self.epoch, &previous.epoch) && self.revision > previous.revision
    }

    pub(crate) fn is_exhausted(&self) -> bool {
        self.epoch.is_exhausted()
    }

    #[cfg(test)]
    pub(crate) fn for_test(epoch: Arc<WatcherEpoch>, revision: u64) -> Self {
        Self { epoch, revision }
    }
}

impl ReconciliationOutcome {
    pub(crate) fn token(&self) -> ReconciliationToken {
        ReconciliationToken {
            epoch: self.epoch.clone(),
            revision: self.revision,
        }
    }
}

/// Rescan requests are synchronous, but concurrent callers can enqueue before
/// the worker reaches them. Reserve a finite slot before sending so both the
/// command queue and retained waiter vector remain bounded.
const MAX_PENDING_RESCANS: usize = 64;
const MAX_PENDING_PATHS: usize = 4_096;

struct RescanTicket {
    pending: Arc<AtomicUsize>,
    cancelled: AtomicBool,
    released: AtomicBool,
}

impl RescanTicket {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    fn release(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            self.pending.fetch_sub(1, Ordering::Release);
        }
    }
}

impl Drop for RescanTicket {
    fn drop(&mut self) {
        self.release();
    }
}

enum WorkerInput {
    Command(Command),
    Filesystem(Result<Event, notify::Error>),
    RevisionExhausted,
}

impl CollectionWatcher {
    pub fn open(root: impl AsRef<Path>, debounce: Duration) -> Result<Self, WatchError> {
        Self::open_internal(root.as_ref(), debounce)
    }

    fn open_internal(root: &Path, debounce: Duration) -> Result<Self, WatchError> {
        // Acquire collection authority exactly once. The path retained below is
        // only the notify registration/display name; every snapshot read is
        // rooted in this held collection even if that name is replaced.
        let collection = Collection::open_for_observation(root)
            .map_err(|error| WatchError::Collection(collection_error(&error)))?;
        let root = root.to_path_buf();
        let initial = Snapshot::load(&collection)?;
        let (events_tx, events) = mpsc::channel();
        let (commands, command_rx) = mpsc::channel();
        let pending_rescans = Arc::new(AtomicUsize::new(0));
        let invalidation_revision = Arc::new(AtomicU64::new(0));
        let epoch = Arc::new(WatcherEpoch::new());
        let callback_root = root.clone();
        let callback_sender = commands.clone();
        let callback_revision = invalidation_revision.clone();
        let callback_epoch = epoch.clone();
        let filesystem_callback: FilesystemCallback = Arc::new(move |event| {
            enqueue_filesystem_callback(
                &callback_sender,
                &callback_root,
                &callback_revision,
                &callback_epoch,
                event,
            );
        });
        let worker_invalidation_revision = invalidation_revision.clone();
        let worker_epoch = epoch.clone();
        let worker_filesystem_callback = filesystem_callback.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("mdbase-watch".to_string())
            .spawn(move || {
                watch_loop(
                    root,
                    collection,
                    debounce,
                    initial,
                    WorkerChannels {
                        inputs: command_rx,
                        filesystem_callback: worker_filesystem_callback,
                        events: events_tx,
                        ready: ready_tx,
                        invalidation_revision: worker_invalidation_revision,
                        epoch: worker_epoch,
                    },
                )
            })
            .map_err(|error| WatchError::Collection(error.to_string()))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                events,
                commands,
                worker: Some(worker),
                pending_rescans,
                next_rescan_id: Arc::new(AtomicU64::new(1)),
                invalidation_revision,
                epoch,
                #[cfg(test)]
                filesystem_callback,
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
        self.rescan_observation().map(|_| ())
    }

    /// Complete a full comparison and return private maintenance hints that
    /// must not be translated into public record events.
    pub(crate) fn rescan_observation(&self) -> Result<Arc<ReconciliationOutcome>, WatchError> {
        let request = self.enqueue_reconciliation(None)?;
        request
            .receiver
            .recv()
            .map_err(|_| WatchError::Stopped)?
            .map_err(reconciliation_failure)
    }

    pub(crate) fn rescan_observation_with_context(
        &self,
        context: &OperationContext,
    ) -> Result<Arc<ReconciliationOutcome>, ProviderError> {
        let request = self.enqueue_reconciliation(None)?;
        self.wait_for_reconciliation(request, context)
    }

    pub(crate) fn rescan_paths_observation<I, P>(
        &self,
        paths: I,
    ) -> Result<Arc<ReconciliationOutcome>, WatchError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let request = self.enqueue_reconciliation_paths(paths)?;
        request
            .receiver
            .recv()
            .map_err(|_| WatchError::Stopped)?
            .map_err(reconciliation_failure)
    }

    pub(crate) fn rescan_paths_observation_with_context<I, P>(
        &self,
        paths: I,
        context: &OperationContext,
    ) -> Result<Arc<ReconciliationOutcome>, ProviderError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let request = self.enqueue_reconciliation_paths(paths)?;
        self.wait_for_reconciliation(request, context)
    }

    pub(crate) fn acknowledge_observation_with_context(
        &self,
        outcome: Arc<ReconciliationOutcome>,
        context: &OperationContext,
    ) -> Result<bool, ProviderError> {
        if outcome.epoch.is_exhausted() {
            return Err(WatchError::RevisionExhausted.into());
        }
        let (ready, receiver) = mpsc::channel();
        let active = Arc::new(AtomicBool::new(true));
        self.commands
            .send(WorkerInput::Command(Command::Acknowledge {
                outcome,
                active: active.clone(),
                ready,
            }))
            .map_err(|_| WatchError::Stopped)?;
        loop {
            let wait = match context.next_wait() {
                Ok(wait) => wait,
                Err(error) => {
                    active.store(false, Ordering::Release);
                    return Err(error);
                }
            };
            match receiver.recv_timeout(wait) {
                Ok(Ok(result)) => return Ok(result),
                Ok(Err(failure)) => return Err(reconciliation_failure(failure).into()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(WatchError::Stopped.into());
                }
            }
        }
    }

    fn enqueue_reconciliation_paths<I, P>(&self, paths: I) -> Result<PendingRequest, WatchError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        // Reserve before touching the iterator: a rejected 65th caller cannot
        // force evaluation or allocation through a lazy/adversarial source.
        let ticket = reserve_rescan_slot(self.pending_rescans.clone())?;
        let mut bounded = BTreeSet::new();
        let mut full = false;
        for path in paths {
            bounded.insert(path.into());
            if bounded.len() > MAX_PENDING_PATHS {
                bounded.clear();
                full = true;
                break;
            }
        }
        self.enqueue_reserved((!full).then(|| bounded.into_iter().collect()), ticket)
    }

    fn enqueue_reconciliation(
        &self,
        paths: Option<Vec<PathBuf>>,
    ) -> Result<PendingRequest, WatchError> {
        let ticket = reserve_rescan_slot(self.pending_rescans.clone())?;
        self.enqueue_reserved(paths, ticket)
    }

    fn enqueue_reserved(
        &self,
        paths: Option<Vec<PathBuf>>,
        ticket: Arc<RescanTicket>,
    ) -> Result<PendingRequest, WatchError> {
        let id = claim_rescan_id(&self.next_rescan_id, &self.epoch, &self.commands)?;
        let (ready, receiver) = mpsc::channel();
        let pending = PendingRescan {
            id,
            ready,
            ticket: ticket.clone(),
        };
        increment_revision(&self.invalidation_revision, &self.epoch, &self.commands)?;
        let command = match paths {
            Some(paths) => Command::RescanPaths(paths, pending),
            None => Command::Rescan(pending),
        };
        if self.commands.send(WorkerInput::Command(command)).is_err() {
            ticket.release();
            return Err(WatchError::Stopped);
        }
        Ok(PendingRequest {
            id,
            receiver,
            ticket,
        })
    }

    fn wait_for_reconciliation(
        &self,
        request: PendingRequest,
        context: &OperationContext,
    ) -> Result<Arc<ReconciliationOutcome>, ProviderError> {
        loop {
            let wait = match context.next_wait() {
                Ok(wait) => wait,
                Err(error) => {
                    request.ticket.cancel();
                    if self
                        .commands
                        .send(WorkerInput::Command(Command::CancelRescan(request.id)))
                        .is_err()
                    {
                        request.ticket.release();
                    }
                    return Err(error);
                }
            };
            match request.receiver.recv_timeout(wait) {
                Ok(Ok(outcome)) => return Ok(outcome),
                Ok(Err(failure)) => return Err(reconciliation_failure(failure).into()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    request.ticket.release();
                    return Err(WatchError::Stopped.into());
                }
            }
        }
    }

    /// Compare only the supplied record paths with the current snapshot.
    ///
    /// This is the preferred synchronization path after an in-process
    /// mutation. It preserves operation/event ordering without turning every
    /// write into an O(collection size) reload.
    #[cfg(test)]
    pub(crate) fn test_control(&self) -> WatcherTestControl {
        WatcherTestControl {
            commands: self.commands.clone(),
            pending_rescans: self.pending_rescans.clone(),
            invalidation_revision: self.invalidation_revision.clone(),
            epoch: self.epoch.clone(),
            filesystem_callback: self.filesystem_callback.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn pending_rescan_count(&self) -> usize {
        self.pending_rescans.load(Ordering::Acquire)
    }

    pub fn rescan_paths<I, P>(&self, paths: I) -> Result<(), WatchError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.rescan_paths_observation(paths).map(|_| ())
    }
}

struct PendingRequest {
    id: u64,
    receiver: mpsc::Receiver<ReconciliationResult>,
    ticket: Arc<RescanTicket>,
}

fn reconciliation_failure(failure: ReconciliationFailure) -> WatchError {
    match failure {
        ReconciliationFailure::RevisionExhausted => WatchError::RevisionExhausted,
    }
}

fn reserve_rescan_slot(pending: Arc<AtomicUsize>) -> Result<Arc<RescanTicket>, WatchError> {
    pending
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current < MAX_PENDING_RESCANS).then_some(current + 1)
        })
        .map_err(|_| WatchError::RescanBackpressure)?;
    Ok(Arc::new(RescanTicket {
        pending,
        cancelled: AtomicBool::new(false),
        released: AtomicBool::new(false),
    }))
}

/// Advance without wrap. Exhaustion permanently poisons this worker; recovery
/// requires dropping and recreating `CollectionWatcher`, which starts a fresh
/// watcher-lifetime revision epoch.
fn poison_watcher(epoch: &WatcherEpoch, sender: &mpsc::Sender<WorkerInput>) {
    let _linearized = epoch.linearize();
    if !epoch.exhausted.swap(true, Ordering::AcqRel) {
        // Unbounded channel send is non-blocking. Keep notification inside the
        // poison linearization point without making filesystem callbacks wait
        // for the worker to receive it.
        let _ = sender.send(WorkerInput::RevisionExhausted);
    }
}

fn claim_rescan_id(
    next: &AtomicU64,
    epoch: &WatcherEpoch,
    sender: &mpsc::Sender<WorkerInput>,
) -> Result<u64, WatchError> {
    let _linearized = epoch.linearize();
    if epoch.is_exhausted() {
        return Err(WatchError::RevisionExhausted);
    }
    match next.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        current.checked_add(1)
    }) {
        Ok(previous) => Ok(previous),
        Err(_) => {
            drop(_linearized);
            poison_watcher(epoch, sender);
            Err(WatchError::RevisionExhausted)
        }
    }
}

fn increment_revision(
    revision: &AtomicU64,
    epoch: &WatcherEpoch,
    sender: &mpsc::Sender<WorkerInput>,
) -> Result<u64, WatchError> {
    let _linearized = epoch.linearize();
    if epoch.is_exhausted() {
        return Err(WatchError::RevisionExhausted);
    }
    match revision.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        current.checked_add(1)
    }) {
        Ok(previous) => Ok(previous + 1),
        Err(_) => {
            drop(_linearized);
            poison_watcher(epoch, sender);
            Err(WatchError::RevisionExhausted)
        }
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
    filesystem_callback: FilesystemCallback,
    events: mpsc::Sender<WatchEvent>,
    ready: mpsc::SyncSender<Result<(), notify::Error>>,
    invalidation_revision: Arc<AtomicU64>,
    epoch: Arc<WatcherEpoch>,
}

fn watch_loop(
    root: PathBuf,
    collection: Collection,
    debounce: Duration,
    mut snapshot: Snapshot,
    channels: WorkerChannels,
) {
    let WorkerChannels {
        inputs,
        filesystem_callback,
        events,
        ready,
        invalidation_revision,
        epoch,
    } = channels;
    // `RecommendedWatcher` and test control both invoke this exact shared
    // callable instance. Tests never reconstruct the callback pipeline.
    let installed_callback = filesystem_callback.clone();
    let mut watcher: RecommendedWatcher =
        match notify::recommended_watcher(move |event: Result<Event, notify::Error>| {
            installed_callback(event);
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
    let mut snapshot_revision = invalidation_revision.load(Ordering::Acquire);
    let mut maintained_invalid_records = snapshot.invalid_records.clone();
    // Close the gap between the caller's initial snapshot and OS watch
    // registration before reporting readiness. Hosts can now treat `open` as
    // a stable boundary instead of triggering an additional full rescan.
    let mut startup_refresh_failure = None;
    match Snapshot::load(&collection) {
        Ok(mut next) => {
            next.retain_classified_invalid(&snapshot);
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
            Ok(WorkerInput::Command(Command::Rescan(pending))) => {
                if pending.ticket.cancelled.load(Ordering::Acquire) {
                    pending.ticket.release();
                } else {
                    schedule_relevant_refresh(
                        &mut deadline,
                        &mut retry_backoff,
                        last_refresh_diagnostic.is_some(),
                        Duration::ZERO,
                    );
                    full_rescan = true;
                    pending_rescans.push(pending);
                }
            }
            Ok(WorkerInput::Command(Command::RescanPaths(paths, pending))) => {
                if pending.ticket.cancelled.load(Ordering::Acquire) {
                    pending.ticket.release();
                } else {
                    schedule_relevant_refresh(
                        &mut deadline,
                        &mut retry_backoff,
                        last_refresh_diagnostic.is_some(),
                        Duration::ZERO,
                    );
                    merge_pending_paths(&mut pending_paths, &mut full_rescan, paths);
                    pending_rescans.push(pending);
                }
            }
            Ok(WorkerInput::Command(Command::CancelRescan(id))) => {
                if let Some(index) = pending_rescans.iter().position(|pending| pending.id == id) {
                    pending_rescans.swap_remove(index).ticket.release();
                }
            }
            Ok(WorkerInput::Command(Command::Acknowledge {
                outcome,
                active,
                ready,
            })) => {
                let preliminarily_current = active.load(Ordering::Acquire)
                    && !epoch.is_exhausted()
                    && !outcome.epoch.is_exhausted()
                    && outcome.revision == snapshot_revision
                    && outcome.revision == invalidation_revision.load(Ordering::Acquire);
                #[cfg(test)]
                if preliminarily_current {
                    epoch.run_hook(LinearizationPoint::Acknowledgement);
                }
                let _linearized = epoch.linearize();
                let current = preliminarily_current
                    && active.load(Ordering::Acquire)
                    && !epoch.is_exhausted()
                    && !outcome.epoch.is_exhausted()
                    && outcome.revision == snapshot_revision
                    && outcome.revision == invalidation_revision.load(Ordering::Acquire);
                if current {
                    maintained_invalid_records = outcome.invalid_records.clone();
                    let _ = ready.send(Ok(true));
                } else if epoch.is_exhausted() || outcome.epoch.is_exhausted() {
                    let _ = ready.send(Err(ReconciliationFailure::RevisionExhausted));
                } else {
                    let _ = ready.send(Ok(false));
                }
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
                        Some(paths) => {
                            merge_pending_paths(&mut pending_paths, &mut full_rescan, paths)
                        }
                        None => {
                            pending_paths.clear();
                            full_rescan = true;
                        }
                    }
                    let refresh_is_failing = last_refresh_diagnostic.is_some();
                    if !(pathless && refresh_is_failing) {
                        schedule_relevant_refresh(
                            &mut deadline,
                            &mut retry_backoff,
                            refresh_is_failing,
                            if pending_rescans.is_empty() {
                                debounce
                            } else {
                                Duration::ZERO
                            },
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
            Ok(WorkerInput::RevisionExhausted) => {
                fail_pending_rescans(&mut pending_rescans);
                pending_paths.clear();
                full_rescan = false;
                deadline = None;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        if epoch.is_exhausted() {
            fail_pending_rescans(&mut pending_rescans);
            pending_paths.clear();
            full_rescan = false;
            deadline = None;
        }

        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            deadline = None;
            let refresh_started = Instant::now();
            let refresh_mode = if full_rescan { "full" } else { "incremental" };
            let refresh_path_count = pending_paths.len();
            let refresh_revision = invalidation_revision.load(Ordering::Acquire);
            let mut next = snapshot.clone();
            let refreshed = if full_rescan {
                Snapshot::load(&collection).map(|mut candidate| {
                    candidate.retain_classified_invalid(&snapshot);
                    let diff = snapshot.diff(&candidate);
                    next = candidate;
                    diff
                })
            } else {
                next.refresh_paths(&collection, &pending_paths)
            };
            if epoch.is_exhausted() {
                fail_pending_rescans(&mut pending_rescans);
                pending_paths.clear();
                full_rescan = false;
                deadline = None;
                continue;
            }
            match refreshed {
                Ok(changes)
                    if refresh_revision == invalidation_revision.load(Ordering::Acquire) =>
                {
                    snapshot = next;
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
                    snapshot_revision = refresh_revision;
                    let removed_invalid_records = maintained_invalid_records
                        .iter()
                        .filter(|path| {
                            !snapshot.invalid_records.contains(*path)
                                && !snapshot.records.contains_key(*path)
                        })
                        .cloned()
                        .collect();
                    let reconciliation = Arc::new(ReconciliationOutcome {
                        invalid_records: snapshot.invalid_records.clone(),
                        removed_invalid_records: Arc::new(removed_invalid_records),
                        revision: snapshot_revision,
                        epoch: epoch.clone(),
                    });
                    complete_pending_rescans(&mut pending_rescans, reconciliation, &epoch);
                }
                Ok(_) => {
                    // An invalidation was enqueued while bytes were being
                    // observed. Discard the candidate and reconcile a bounded
                    // full snapshot before completing any waiter.
                    pending_paths.clear();
                    full_rescan = true;
                    deadline = Some(Instant::now());
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

fn merge_pending_paths(
    pending: &mut BTreeSet<PathBuf>,
    full_rescan: &mut bool,
    paths: impl IntoIterator<Item = PathBuf>,
) {
    if *full_rescan {
        return;
    }
    for path in paths {
        pending.insert(path);
        if pending.len() > MAX_PENDING_PATHS {
            pending.clear();
            *full_rescan = true;
            return;
        }
    }
}

fn take_released_pending_rescans(pending: &mut Vec<PendingRescan>) -> Vec<PendingRescan> {
    let released = std::mem::take(pending);
    // A received result is the synchronous completion boundary. Empty the
    // worker's waiter state and release every bounded-queue permit before any
    // caller can observe that boundary, including when several requests were
    // coalesced into one reconciliation.
    for pending in &released {
        pending.ticket.release();
    }
    released
}

fn fail_pending_rescans(pending: &mut Vec<PendingRescan>) {
    for pending in take_released_pending_rescans(pending) {
        if !pending.ticket.cancelled.load(Ordering::Acquire) {
            let _ = pending
                .ready
                .send(Err(ReconciliationFailure::RevisionExhausted));
        }
    }
}

fn complete_pending_rescans(
    pending: &mut Vec<PendingRescan>,
    outcome: Arc<ReconciliationOutcome>,
    epoch: &WatcherEpoch,
) {
    #[cfg(test)]
    epoch.run_hook(LinearizationPoint::Waiter);
    let _linearized = epoch.linearize();
    let result = if epoch.is_exhausted() {
        Err(ReconciliationFailure::RevisionExhausted)
    } else {
        Ok(outcome)
    };
    for pending in take_released_pending_rescans(pending) {
        if !pending.ticket.cancelled.load(Ordering::Acquire) {
            // Arc cloning is O(1), and unbounded send cannot block while the
            // poison linearization gate is held.
            let _ = pending.ready.send(result.clone());
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
fn enqueue_filesystem_callback(
    sender: &mpsc::Sender<WorkerInput>,
    root: &Path,
    revision: &AtomicU64,
    epoch: &WatcherEpoch,
    event: Result<Event, notify::Error>,
) {
    if event
        .as_ref()
        .is_ok_and(|event| enqueues_reconciliation(root, event))
    {
        let _ = increment_revision(revision, epoch, sender);
    }
    // Even after exhaustion, enqueue the backend event so the worker can
    // release resources and consume callback traffic. It can no longer fence
    // or complete reconciliation until the watcher is recreated.
    let _ = sender.send(WorkerInput::Filesystem(event));
}

fn enqueues_reconciliation(root: &Path, event: &Event) -> bool {
    invalidates_snapshot(event)
        && (event.paths.is_empty()
            || event.paths.iter().any(|path| {
                let Ok(relative) = path.strip_prefix(root) else {
                    return true;
                };
                let normalized = relative.to_string_lossy().replace('\\', "/");
                normalized.is_empty() || !crate::record_path::has_hidden_component(&normalized)
            }))
}

fn invalidates_snapshot(event: &Event) -> bool {
    !matches!(
        event.kind,
        EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime))
    )
}

fn watch_error_event(sequence: u64, _provider_message: String) -> WatchEvent {
    WatchEvent {
        event_type: "mdbase.collection.invalidated".to_string(),
        sequence,
        occurred_at: now(),
        payload: json!({
            "diagnostic": {
                "severity": "error",
                "code": "collection_reload_failed",
                "message": "Collection reconciliation failed and will retry.",
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
    invalid_records: Arc<BTreeSet<String>>,
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
    fn load(held: &Collection) -> Result<Self, WatchError> {
        // Reload configuration/resources through a clone of the already-held
        // capability. Never reacquire through the notify display path.
        let collection = held
            .reopen_held(false)
            .map_err(|error| WatchError::Collection(collection_error(&error)))?;
        let observed = collection
            .snapshot_for_watcher()
            .map_err(|error| WatchError::Collection(error.to_string()))?;
        let invalid_records = Arc::new(observed.invalid_records);
        let canonical = observed.snapshot;
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
            invalid_records,
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

    fn retain_classified_invalid(&mut self, previous: &Self) {
        for path in self.invalid_records.iter() {
            if let Some(record) = previous.records.get(path) {
                self.records.insert(path.clone(), record.clone());
            }
        }
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
        collection: &Collection,
        paths: &BTreeSet<PathBuf>,
    ) -> Result<Vec<PendingEvent>, WatchError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut before = BTreeMap::new();
        let mut replacements = Vec::new();
        for path in paths {
            let relative = path.to_string_lossy().replace('\\', "/");
            if let Some(record) = self.records.get(&relative) {
                before.insert(relative.clone(), record.clone());
            }
            replacements.push((relative.clone(), load_record(collection, &relative)?));
        }
        for (path, outcome) in replacements {
            match outcome {
                WatchRecordLoad::Parsed(record) => {
                    Arc::make_mut(&mut self.invalid_records).remove(&path);
                    self.records.insert(path, record);
                }
                WatchRecordLoad::Invalid => {
                    Arc::make_mut(&mut self.invalid_records).insert(path);
                }
                WatchRecordLoad::Absent => {
                    Arc::make_mut(&mut self.invalid_records).remove(&path);
                    self.records.remove(&path);
                }
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

enum WatchRecordLoad {
    Parsed(RecordState),
    Invalid,
    Absent,
}

fn load_record(collection: &Collection, path: &str) -> Result<WatchRecordLoad, WatchError> {
    if collection.is_excluded(path) || !collection.is_valid_extension(path) {
        return Ok(WatchRecordLoad::Absent);
    }
    let Some(outcome) = crate::record_load::load_record_no_follow(collection, path)
        .map_err(|error| WatchError::Collection(error.to_string()))?
    else {
        return Ok(WatchRecordLoad::Absent);
    };
    let invalid_reason = outcome.reason();
    match outcome {
        RecordLoadOutcome::Parsed {
            facts,
            raw_frontmatter,
            effective_frontmatter,
            document,
            layout,
            type_names,
            ..
        } => Ok(WatchRecordLoad::Parsed(RecordState {
            revision: facts.revision,
            raw_frontmatter: raw_frontmatter.as_object().cloned().unwrap_or_default(),
            effective_frontmatter,
            types: json!(type_names),
            body: layout.body(&document).to_string(),
        })),
        // Watch events have no invalid-record representation. Keep classified
        // invalidity distinct from absence so an existing parsed state is not
        // converted into a synthetic deletion/checkpoint advancement.
        RecordLoadOutcome::Invalid { .. } => {
            debug_assert!(invalid_reason.is_some());
            Ok(WatchRecordLoad::Invalid)
        }
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

    fn test_pending_rescan(
        watcher: &CollectionWatcher,
        ready: ReconciliationSender,
    ) -> PendingRescan {
        let ticket = reserve_rescan_slot(watcher.pending_rescans.clone()).unwrap();
        let id = watcher.next_rescan_id.fetch_add(1, Ordering::AcqRel);
        increment_revision(
            &watcher.invalidation_revision,
            &watcher.epoch,
            &watcher.commands,
        )
        .unwrap();
        PendingRescan { id, ready, ticket }
    }

    fn bounded_rescan(watcher: &CollectionWatcher, paths: Option<&[&str]>) {
        let (ready, receiver) = mpsc::channel();
        let pending = test_pending_rescan(watcher, ready);
        let command = match paths {
            Some(paths) => Command::RescanPaths(paths.iter().map(PathBuf::from).collect(), pending),
            None => Command::Rescan(pending),
        };
        watcher
            .commands
            .send(WorkerInput::Command(command))
            .expect("watcher worker remains available");
        receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("reconciliation completes within its bounded test budget")
            .expect("reconciliation succeeds");
    }

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
            invalid_records: Arc::new(BTreeSet::new()),
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
        let collection = Collection::open_for_observation(directory.path()).unwrap();
        let snapshot = Snapshot::load(&collection).unwrap();
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

        let collection = Collection::open_for_observation(directory.path()).unwrap();
        let observed = Snapshot::load(&collection).unwrap();
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

    #[cfg(unix)]
    #[test]
    fn watcher_root_replacement_never_reads_or_emits_replacement_records_or_resources() {
        let parent = tempfile::tempdir().unwrap();
        let display = parent.path().join("collection");
        let held_name = parent.path().join("held-original");
        fs::create_dir(&display).unwrap();
        fs::create_dir(display.join("_types")).unwrap();
        fs::write(
            display.join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        fs::write(display.join("original.md"), "---\ntitle: Original\n---\n").unwrap();
        fs::write(
            display.join("_types/original.md"),
            "---\nname: original\nfields: {}\n---\n",
        )
        .unwrap();
        let watcher = CollectionWatcher::open(&display, Duration::from_millis(10)).unwrap();

        fs::rename(&display, &held_name).unwrap();
        fs::create_dir(&display).unwrap();
        fs::create_dir(display.join("_types")).unwrap();
        // Deliberately invalid replacement configuration proves a path reopen
        // would fail before it could even inspect the replacement payloads.
        fs::write(display.join("mdbase.yaml"), "not: [valid\n").unwrap();
        fs::write(
            display.join("replacement.md"),
            "---\ntitle: Replacement\n---\nreplacement body\n",
        )
        .unwrap();
        fs::write(
            display.join("_types/replacement.md"),
            "---\nname: replacement\nfields: {}\n---\n",
        )
        .unwrap();

        let control = watcher.test_control();
        control.invoke_installed_modify_callback(&display.join("replacement.md"));
        watcher.rescan().unwrap();
        assert!(watcher
            .recv_timeout(Duration::from_millis(200))
            .unwrap()
            .is_none());

        // The original authority remains live and readable after displacement.
        fs::write(
            held_name.join("original.md"),
            "---\ntitle: Held changed\n---\n",
        )
        .unwrap();
        watcher.rescan().unwrap();
        let event = watcher
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .expect("held original change event");
        assert_eq!(event.event_type, "mdbase.record.modified");
        assert_eq!(event.payload["path"], "original.md");
        assert!(!event.payload.to_string().contains("Replacement"));
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
        let (waiter, waiter_rx) = mpsc::channel();
        let pending = test_pending_rescan(&watcher, waiter);
        watcher
            .commands
            .send(WorkerInput::Command(Command::RescanPaths(
                vec![PathBuf::from("tracked.md")],
                pending,
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
            .expect("retained waiter completes after retry")
            .expect("retained reconciliation succeeds");
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
        let (waiter, waiter_rx) = mpsc::channel();
        let pending = test_pending_rescan(&watcher, waiter);
        watcher
            .commands
            .send(WorkerInput::Command(Command::Rescan(pending)))
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
            .expect("retained waiter completes after recovery")
            .expect("retained reconciliation succeeds");
    }

    #[test]
    fn pending_rescan_slots_enforce_finite_boundary_and_recover_capacity() {
        let pending = Arc::new(AtomicUsize::new(0));
        let mut permits = (0..MAX_PENDING_RESCANS)
            .map(|_| reserve_rescan_slot(pending.clone()).unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            reserve_rescan_slot(pending.clone()),
            Err(WatchError::RescanBackpressure)
        ));
        permits.pop();
        let replacement = reserve_rescan_slot(pending.clone()).unwrap();
        assert_eq!(pending.load(Ordering::Acquire), MAX_PENDING_RESCANS);
        drop(replacement);
        drop(permits);
        assert_eq!(pending.load(Ordering::Acquire), 0);

        pending.store(MAX_PENDING_RESCANS, Ordering::Release);
        let (_events_tx, events) = mpsc::channel();
        let (commands, inputs) = mpsc::channel();
        let worker = thread::spawn(move || {
            while !matches!(
                inputs.recv(),
                Ok(WorkerInput::Command(Command::Stop)) | Err(_)
            ) {}
        });
        let watcher = CollectionWatcher {
            events,
            commands,
            worker: Some(worker),
            pending_rescans: pending,
            next_rescan_id: Arc::new(AtomicU64::new(1)),
            invalidation_revision: Arc::new(AtomicU64::new(0)),
            epoch: Arc::new(WatcherEpoch::new()),
            filesystem_callback: Arc::new(|_| {}),
        };
        assert!(matches!(
            watcher.rescan(),
            Err(WatchError::RescanBackpressure)
        ));
        let lazy = std::iter::from_fn(|| -> Option<PathBuf> {
            panic!("rejected request must not enumerate")
        });
        assert!(matches!(
            watcher.rescan_paths(lazy),
            Err(WatchError::RescanBackpressure)
        ));
    }

    #[test]
    fn pending_path_union_overflow_coalesces_to_bounded_full_rescan() {
        let mut pending = (0..MAX_PENDING_PATHS)
            .map(|index| PathBuf::from(format!("pending-{index}.md")))
            .collect::<BTreeSet<_>>();
        let mut full = false;
        merge_pending_paths(&mut pending, &mut full, [PathBuf::from("overflow.md")]);
        assert!(full);
        assert!(pending.is_empty());
    }

    #[test]
    fn over_limit_path_iterator_is_bounded_and_converges_via_full_rescan() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_secs(60)).unwrap();
        fs::write(
            directory.path().join("visible.md"),
            "---\ntitle: Visible\n---\n",
        )
        .unwrap();
        let enumerated = Arc::new(AtomicUsize::new(0));
        let observed = enumerated.clone();
        let paths = (0..(MAX_PENDING_PATHS * 2)).map(move |index| {
            observed.fetch_add(1, Ordering::Relaxed);
            PathBuf::from(format!("irrelevant-{index}.md"))
        });
        watcher.rescan_paths(paths).unwrap();
        assert_eq!(enumerated.load(Ordering::Relaxed), MAX_PENDING_PATHS + 1);
        let event = watcher
            .recv_timeout(Duration::ZERO)
            .unwrap()
            .expect("overflow must converge through one full rescan");
        assert_eq!(event.event_type, "mdbase.record.created");
        assert_eq!(event.payload["path"], "visible.md");
    }

    #[test]
    fn coalesced_waiters_share_one_immutable_reconciliation_outcome() {
        let (first_tx, first_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let first_ticket = reserve_rescan_slot(count.clone()).unwrap();
        let second_ticket = reserve_rescan_slot(count.clone()).unwrap();
        let mut pending = vec![
            PendingRescan {
                id: 1,
                ready: first_tx,
                ticket: first_ticket,
            },
            PendingRescan {
                id: 2,
                ready: second_tx,
                ticket: second_ticket,
            },
        ];
        let epoch = Arc::new(WatcherEpoch::new());
        let outcome = Arc::new(ReconciliationOutcome {
            invalid_records: Arc::new(BTreeSet::from(["invalid.md".to_string()])),
            removed_invalid_records: Arc::new(BTreeSet::new()),
            revision: 1,
            epoch: epoch.clone(),
        });
        complete_pending_rescans(&mut pending, outcome.clone(), &epoch);
        let first = first_rx.recv().unwrap().unwrap();
        let second = second_rx.recv().unwrap().unwrap();
        assert!(Arc::ptr_eq(&outcome, &first));
        assert!(Arc::ptr_eq(&first, &second));
        assert!(pending.is_empty());
    }

    #[test]
    fn waiter_success_and_poison_have_one_linearized_order() {
        let epoch = Arc::new(WatcherEpoch::new());
        let (commands, _inputs) = mpsc::channel();
        let (ready, receiver) = mpsc::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let ticket = reserve_rescan_slot(count.clone()).unwrap();
        let pending = PendingRescan {
            id: 1,
            ready,
            ticket,
        };
        let outcome = Arc::new(ReconciliationOutcome {
            invalid_records: Arc::new(BTreeSet::new()),
            removed_invalid_records: Arc::new(BTreeSet::new()),
            revision: 1,
            epoch: epoch.clone(),
        });
        let race = epoch.install_hook(LinearizationPoint::Waiter);
        let worker_epoch = epoch.clone();
        let completion = thread::spawn(move || {
            let mut pending = vec![pending];
            complete_pending_rescans(&mut pending, outcome, &worker_epoch);
        });
        race.wait_until_reached();
        poison_watcher(&epoch, &commands);
        race.resume();
        completion.join().unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)),
            Ok(Err(ReconciliationFailure::RevisionExhausted))
        ));
        assert_eq!(count.load(Ordering::Acquire), 0);

        // The opposite legal order remains possible: a send completed under
        // the gate before poison is a valid pre-poison success.
        let epoch = Arc::new(WatcherEpoch::new());
        let (ready, receiver) = mpsc::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let mut pending = vec![PendingRescan {
            id: 2,
            ready,
            ticket: reserve_rescan_slot(count).unwrap(),
        }];
        let outcome = Arc::new(ReconciliationOutcome {
            invalid_records: Arc::new(BTreeSet::new()),
            removed_invalid_records: Arc::new(BTreeSet::new()),
            revision: 1,
            epoch: epoch.clone(),
        });
        complete_pending_rescans(&mut pending, outcome, &epoch);
        assert!(receiver.recv().unwrap().is_ok());
        poison_watcher(&epoch, &commands);
    }

    #[test]
    fn acknowledgement_cannot_succeed_after_poison_begins() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(5)).unwrap();
        let outcome = watcher.rescan_observation().unwrap();
        let control = watcher.test_control();
        let race = control.install_acknowledgement_linearization_hook();
        let (ready, receiver) = mpsc::channel();
        watcher
            .commands
            .send(WorkerInput::Command(Command::Acknowledge {
                outcome,
                active: Arc::new(AtomicBool::new(true)),
                ready,
            }))
            .unwrap();
        race.wait_until_reached();
        control.poison();
        race.resume();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)),
            Ok(Err(ReconciliationFailure::RevisionExhausted))
        ));
    }

    #[test]
    fn rescan_id_exhaustion_poisons_and_fails_an_already_pending_waiter() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("tracked.md"),
            "---\ntitle: Before\n---\n",
        )
        .unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(5)).unwrap();
        crate::operations::set_record_open_failure(
            directory.path(),
            "tracked.md",
            Some(std::io::ErrorKind::Interrupted),
        );
        fs::write(
            directory.path().join("tracked.md"),
            "---\ntitle: After\n---\n",
        )
        .unwrap();
        let pending = watcher.enqueue_reconciliation(None).unwrap();
        watcher.next_rescan_id.store(u64::MAX, Ordering::Release);

        assert!(matches!(
            watcher.rescan(),
            Err(WatchError::RevisionExhausted)
        ));
        assert!(matches!(
            pending.receiver.recv_timeout(Duration::from_secs(2)),
            Ok(Err(ReconciliationFailure::RevisionExhausted))
        ));
        assert!(watcher.epoch.is_exhausted());
        assert_eq!(watcher.next_rescan_id.load(Ordering::Acquire), u64::MAX);
        assert_eq!(watcher.pending_rescan_count(), 0);
        crate::operations::set_record_open_failure(directory.path(), "tracked.md", None);
    }

    #[test]
    fn callback_revision_exhaustion_fails_pending_and_future_reconciliation_without_wrap() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("tracked.md"),
            "---\ntitle: Before\n---\n",
        )
        .unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_millis(5)).unwrap();
        let stale = watcher.rescan_observation().unwrap();

        crate::operations::set_record_open_failure(
            directory.path(),
            "tracked.md",
            Some(std::io::ErrorKind::Interrupted),
        );
        fs::write(
            directory.path().join("tracked.md"),
            "---\ntitle: After\n---\n",
        )
        .unwrap();
        let pending = watcher.enqueue_reconciliation(None).unwrap();
        watcher
            .invalidation_revision
            .store(u64::MAX, Ordering::Release);
        (watcher.filesystem_callback)(Ok(Event {
            kind: EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            paths: vec![directory.path().join("tracked.md")],
            attrs: Default::default(),
        }));

        assert!(matches!(
            pending.receiver.recv_timeout(Duration::from_secs(2)),
            Ok(Err(ReconciliationFailure::RevisionExhausted))
        ));
        assert_eq!(watcher.pending_rescan_count(), 0);
        assert_eq!(
            watcher.invalidation_revision.load(Ordering::Acquire),
            u64::MAX
        );
        assert!(watcher.epoch.is_exhausted());
        assert!(matches!(
            watcher.acknowledge_observation_with_context(stale, &OperationContext::legacy()),
            Err(ProviderError::Watch(WatchError::RevisionExhausted))
        ));
        assert!(matches!(
            watcher.rescan(),
            Err(WatchError::RevisionExhausted)
        ));
        crate::operations::set_record_open_failure(directory.path(), "tracked.md", None);
    }

    #[test]
    fn invalidation_enqueued_after_observation_rejects_stale_acknowledgement() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        fs::write(directory.path().join("invalid.md"), b"bad\xffutf8\n").unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_secs(60)).unwrap();
        let outcome = watcher.rescan_observation().unwrap();
        increment_revision(
            &watcher.invalidation_revision,
            &watcher.epoch,
            &watcher.commands,
        )
        .unwrap();
        assert!(!watcher
            .acknowledge_observation_with_context(outcome, &OperationContext::legacy())
            .unwrap());
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

    fn assert_valid_to_invalid_retains_state(invalid_document: &[u8], reason: &str, full: bool) {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("tracked.md"),
            "---\ntitle: Before\n---\nOriginal\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("sibling.md"),
            "---\ntitle: Sibling Before\n---\n",
        )
        .unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_secs(60)).unwrap();

        fs::write(directory.path().join("tracked.md"), invalid_document).unwrap();
        if full {
            bounded_rescan(&watcher, None);
        } else {
            bounded_rescan(&watcher, Some(&["tracked.md"]));
        }
        assert!(
            watcher.recv_timeout(Duration::ZERO).unwrap().is_none(),
            "classified {reason} must not emit synthetic deletion"
        );

        let collection = Collection::open(directory.path()).unwrap();
        let query = collection
            .v03_operations()
            .unwrap()
            .query(&json!({"frontmatter_mode": "persisted"}));
        let stub = query.result["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["path"] == "tracked.md")
            .expect("query retains invalid stub");
        assert!(stub.get("frontmatter").is_none());
        assert!(stub.get("body").is_none());
        assert!(stub["file"]["revision"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
        assert!(query.diagnostics.iter().any(|diagnostic| {
            diagnostic.path.as_deref() == Some("tracked.md")
                && diagnostic.details.as_ref().unwrap()["reason"] == reason
        }));

        fs::write(
            directory.path().join("sibling.md"),
            "---\ntitle: Sibling After\n---\n",
        )
        .unwrap();
        if full {
            bounded_rescan(&watcher, None);
        } else {
            bounded_rescan(&watcher, Some(&["sibling.md"]));
        }
        let sibling = watcher
            .recv_timeout(Duration::ZERO)
            .unwrap()
            .expect("unrelated sibling remains reconcilable");
        assert_eq!(sibling.event_type, "mdbase.record.modified");
        assert_eq!(sibling.payload["path"], "sibling.md");

        fs::write(
            directory.path().join("tracked.md"),
            "---\ntitle: Repaired\n---\nVisible\n",
        )
        .unwrap();
        if full {
            bounded_rescan(&watcher, None);
        } else {
            bounded_rescan(&watcher, Some(&["tracked.md"]));
        }
        let repaired = watcher
            .recv_timeout(Duration::ZERO)
            .unwrap()
            .expect("repair converges from retained prior state");
        assert_eq!(repaired.event_type, "mdbase.record.modified");
        assert_eq!(repaired.payload["path"], "tracked.md");
        assert_eq!(repaired.payload["before"]["title"], "Before");
        assert_eq!(repaired.payload["after"]["title"], "Repaired");
    }

    #[test]
    fn valid_to_classified_invalid_full_refresh_retains_state_until_modified_repair() {
        for (document, reason) in [
            (b"bad\xffutf8\n".as_slice(), "invalid_utf8"),
            (
                b"---\ntitle: One\ntitle: Two\n---\n".as_slice(),
                "invalid_yaml",
            ),
            (
                b"---\n- nonmapping\n---\n".as_slice(),
                "non_mapping_frontmatter",
            ),
        ] {
            assert_valid_to_invalid_retains_state(document, reason, true);
        }
    }

    #[test]
    fn valid_to_classified_invalid_incremental_refresh_retains_state_until_modified_repair() {
        for (document, reason) in [
            (b"bad\xffutf8\n".as_slice(), "invalid_utf8"),
            (
                b"---\ntitle: One\ntitle: Two\n---\n".as_slice(),
                "invalid_yaml",
            ),
            (
                b"---\n- nonmapping\n---\n".as_slice(),
                "non_mapping_frontmatter",
            ),
        ] {
            assert_valid_to_invalid_retains_state(document, reason, false);
        }
    }

    #[test]
    fn invalid_utf8_full_reconciliation_is_bounded_and_repair_converges() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("healthy.md"),
            "---\ntitle: Before\n---\nBody\n",
        )
        .unwrap();
        fs::write(directory.path().join("broken.md"), b"bad\xffutf8\n").unwrap();

        let watcher = CollectionWatcher::open(directory.path(), Duration::from_secs(60))
            .expect("classified invalid UTF-8 must not prevent watcher startup");
        bounded_rescan(&watcher, None);
        assert!(watcher.recv_timeout(Duration::ZERO).unwrap().is_none());

        let collection = Collection::open(directory.path()).unwrap();
        let query = collection
            .v03_operations()
            .unwrap()
            .query(&json!({"frontmatter_mode": "persisted"}));
        let broken = query.result["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["path"] == "broken.md")
            .expect("invalid query stub");
        assert!(broken["file"]["revision"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
        assert!(broken.get("body").is_none());
        assert!(broken.get("frontmatter").is_none());
        assert_eq!(
            query.diagnostics[0].details.as_ref().unwrap()["reason"],
            "invalid_utf8"
        );

        fs::write(
            directory.path().join("healthy.md"),
            "---\ntitle: After\n---\nBody\n",
        )
        .unwrap();
        bounded_rescan(&watcher, None);
        let sibling = watcher
            .recv_timeout(Duration::ZERO)
            .unwrap()
            .expect("healthy sibling event");
        assert_eq!(sibling.event_type, "mdbase.record.modified");
        assert_eq!(sibling.payload["path"], "healthy.md");
        assert_eq!(sibling.payload["after"]["title"], "After");

        fs::write(
            directory.path().join("broken.md"),
            "---\ntitle: Repaired\n---\nVisible\n",
        )
        .unwrap();
        bounded_rescan(&watcher, None);
        let repaired = watcher
            .recv_timeout(Duration::ZERO)
            .unwrap()
            .expect("repair convergence event");
        assert_eq!(repaired.event_type, "mdbase.record.created");
        assert_eq!(repaired.payload["path"], "broken.md");
        assert_eq!(repaired.payload["after"]["title"], "Repaired");
    }

    #[test]
    fn invalid_utf8_incremental_reconciliation_is_bounded_and_does_not_wedge_siblings() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        let watcher = CollectionWatcher::open(directory.path(), Duration::from_secs(60)).unwrap();

        fs::write(directory.path().join("broken.md"), b"bad\xffutf8\n").unwrap();
        bounded_rescan(&watcher, Some(&["broken.md"]));
        assert!(watcher.recv_timeout(Duration::ZERO).unwrap().is_none());

        fs::write(
            directory.path().join("healthy.md"),
            "---\ntitle: Healthy\n---\n",
        )
        .unwrap();
        bounded_rescan(&watcher, Some(&["healthy.md"]));
        let sibling = watcher
            .recv_timeout(Duration::ZERO)
            .unwrap()
            .expect("sibling creation after invalid record");
        assert_eq!(sibling.event_type, "mdbase.record.created");
        assert_eq!(sibling.payload["path"], "healthy.md");

        fs::write(
            directory.path().join("broken.md"),
            "---\ntitle: Repaired\n---\n",
        )
        .unwrap();
        bounded_rescan(&watcher, Some(&["broken.md"]));
        let repaired = watcher
            .recv_timeout(Duration::ZERO)
            .unwrap()
            .expect("incremental repair convergence");
        assert_eq!(repaired.event_type, "mdbase.record.created");
        assert_eq!(repaired.payload["path"], "broken.md");
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
