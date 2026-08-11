use std::fs;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{
    ChangeBatch, ChangeEventId, ChangeEventIdentity, ChangeFeed, ChangeFeedBaseline,
    ChangeFeedOwnerId, ChangeFeedTransfer, ChangeFeedTransferId, ChangeFeedTransferIntent,
    ChangeFeedTransferReceipt, ChangeOrigin, ChangeSet, ChangeWatermark, CollectionGeneration,
    CommitId, OperationContext, ProviderError, RebuildReason, RuntimeChangeEvent,
    RuntimeChangeEventPage,
};
use crate::runtime::CanonicalChange;
use crate::transactions::RuntimeResolution;
use crate::Collection;

const FEED_VERSION: u32 = 1;
const RUNTIME_DIRECTORY: &str = ".mdbase/runtime";
const FEED_FILE: &str = "change-feed.json";
const MAX_UNACKED_EVENTS: usize = 100_000;
const MAX_FEED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PAGE_ITEMS: usize = 256;

#[derive(Debug, Deserialize, Serialize)]
struct FeedJournal {
    version: u32,
    owner: Option<String>,
    fencing_epoch: u64,
    acknowledged_through: u64,
    head: u64,
    baseline_through: Option<u64>,
    events: Vec<StoredEvent>,
    pending_transfer: Option<StoredTransfer>,
    last_acknowledged_transfer_id: Option<ChangeFeedTransferId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredEvent {
    id: ChangeEventId,
    watermark: u64,
    generation: CollectionGeneration,
    changes: StoredChangeSet,
    origin: ChangeOrigin,
    commit_id: Option<CommitId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum StoredChangeSet {
    Exact(Vec<CanonicalChange>),
    CollectionWide { reason: RebuildReason },
}

#[derive(Debug, Deserialize, Serialize)]
struct StoredTransfer {
    id: ChangeFeedTransferId,
    current: String,
    next: String,
    expected_acked_through: u64,
    fencing_epoch: u64,
}

pub(crate) struct ReconciledFeed {
    pub(crate) watermark: ChangeWatermark,
    pub(crate) generation: CollectionGeneration,
}

pub(crate) struct BaselinePlan {
    pub(crate) baseline: ChangeFeedBaseline,
    pub(crate) commits: Vec<CommitId>,
    pub(crate) needs_commit: bool,
}

pub(crate) fn reconcile(
    collection: &Collection,
    initial_generation: CollectionGeneration,
    context: &OperationContext,
) -> Result<ReconciledFeed, ProviderError> {
    context.check()?;
    let mut journal = read_or_default(collection)?;
    validate(&journal)?;
    let mut generation = initial_generation;
    let resolutions = crate::transactions::list_unacked_runtime_events(collection, context)
        .map_err(super::filesystem::transaction_error)?;
    let mut changed = !feed_path(collection).exists();
    for resolution in resolutions {
        context.check()?;
        let RuntimeResolution::Committed {
            commit_id,
            generation: committed_generation,
            watermark,
            event_id,
            changes,
            ..
        } = resolution
        else {
            continue;
        };
        if journal.events.iter().any(|event| event.id == event_id) {
            continue;
        }
        if journal
            .events
            .iter()
            .any(|event| event.watermark == watermark.get() && event.id != event_id)
        {
            return Err(ProviderError::Transaction {
                code: "change_feed_corrupt",
                message: "two durable events claim one change watermark".to_string(),
            });
        }
        generation = if committed_generation.runtime_epoch() == generation.runtime_epoch() {
            if committed_generation.sequence() > generation.sequence() {
                committed_generation
            } else {
                generation
            }
        } else {
            generation.successor()?
        };
        journal.head = journal.head.max(watermark.get());
        journal.events.push(StoredEvent {
            id: event_id,
            watermark: watermark.get(),
            generation: generation.clone(),
            changes: StoredChangeSet::Exact(changes.items().to_vec()),
            origin: ChangeOrigin::RecoveryReconciliation,
            commit_id: Some(commit_id),
        });
        changed = true;
    }
    journal.events.sort_by_key(|event| event.watermark);
    validate(&journal)?;
    if changed {
        persist(collection, &journal)?;
    }
    Ok(ReconciledFeed {
        watermark: ChangeWatermark::from_stored(journal.head),
        generation,
    })
}

pub(crate) fn ensure_capacity(collection: &Collection) -> Result<(), ProviderError> {
    let journal = read_or_default(collection)?;
    if journal.events.len() >= MAX_UNACKED_EVENTS.saturating_sub(1) {
        return Err(ProviderError::ChangeFeedCapacityExhausted);
    }
    match fs::metadata(feed_path(collection)) {
        Ok(metadata) if metadata.len() >= MAX_FEED_BYTES => {
            Err(ProviderError::ChangeFeedCapacityExhausted)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(feed_io(error)),
    }
}

pub(crate) fn append_known(
    collection: &Collection,
    outcome: &super::ExecutionOutcome,
) -> Result<(), ProviderError> {
    let identity = outcome
        .change_event
        .as_ref()
        .ok_or_else(|| ProviderError::Transaction {
            code: "change_event_missing",
            message: "committed mutation is missing its durable event identity".to_string(),
        })?;
    let commit_id = outcome
        .commit_id
        .as_ref()
        .ok_or_else(|| ProviderError::Transaction {
            code: "commit_identity_missing",
            message: "committed mutation is missing its transaction identity".to_string(),
        })?;
    let ChangeSet::Exact(changes) = &outcome.changes else {
        return Err(ProviderError::Transaction {
            code: "known_change_set_missing",
            message: "committed mutation must retain exact canonical changes".to_string(),
        });
    };
    let mut journal = read_or_default(collection)?;
    validate(&journal)?;
    if let Some(existing) = journal.events.iter().find(|event| event.id == identity.id) {
        if existing.watermark == identity.watermark.get()
            && existing.commit_id.as_ref() == Some(commit_id)
        {
            return Ok(());
        }
        return Err(ProviderError::Transaction {
            code: "change_feed_corrupt",
            message: "change event identity was reused with different metadata".to_string(),
        });
    }
    if identity.watermark.get() != journal.head.saturating_add(1) {
        return Err(ProviderError::Transaction {
            code: "change_feed_order_conflict",
            message: "known mutation event is not the next durable watermark".to_string(),
        });
    }
    if journal.events.len() >= MAX_UNACKED_EVENTS.saturating_sub(1) {
        return Err(ProviderError::ChangeFeedCapacityExhausted);
    }
    journal.head = identity.watermark.get();
    journal.events.push(StoredEvent {
        id: identity.id.clone(),
        watermark: identity.watermark.get(),
        generation: outcome.generation.clone(),
        changes: StoredChangeSet::Exact(changes.items().to_vec()),
        origin: ChangeOrigin::KnownMutation,
        commit_id: Some(commit_id.clone()),
    });
    persist(collection, &journal)
}

pub(crate) fn append_external(
    collection: &Collection,
    event: &RuntimeChangeEvent,
) -> Result<(), ProviderError> {
    let mut journal = read_or_default(collection)?;
    validate(&journal)?;
    if event.identity.watermark.get() != journal.head.saturating_add(1) {
        return Err(ProviderError::Transaction {
            code: "change_feed_order_conflict",
            message: "filesystem observation is not the next durable watermark".to_string(),
        });
    }
    if journal.events.len() >= MAX_UNACKED_EVENTS {
        return Err(ProviderError::ChangeFeedCapacityExhausted);
    }
    let changes = match &event.changes {
        ChangeSet::Exact(_batch)
            if journal.events.len() >= MAX_UNACKED_EVENTS.saturating_sub(1) =>
        {
            StoredChangeSet::CollectionWide {
                reason: RebuildReason::ExternalChangeUncertain,
            }
        }
        ChangeSet::Exact(batch) => StoredChangeSet::Exact(batch.items().to_vec()),
        ChangeSet::CollectionWide { reason } => StoredChangeSet::CollectionWide { reason: *reason },
        ChangeSet::None => return Ok(()),
    };
    journal.head = event.identity.watermark.get();
    journal.events.push(StoredEvent {
        id: event.identity.id.clone(),
        watermark: event.identity.watermark.get(),
        generation: event.generation.clone(),
        changes,
        origin: event.origin,
        commit_id: None,
    });
    persist(collection, &journal)
}

pub(crate) fn open(
    collection: &Collection,
    owner: &ChangeFeedOwnerId,
) -> Result<ChangeFeed, ProviderError> {
    let mut journal = read_or_default(collection)?;
    match journal.owner.as_deref() {
        None => journal.owner = Some(owner.as_str().to_string()),
        Some(current) if current == owner.as_str() => {}
        Some(_) => return Err(ProviderError::ChangeFeedOwned),
    }
    journal.fencing_epoch = journal
        .fencing_epoch
        .checked_add(1)
        .ok_or(ProviderError::WatermarkExhausted)?;
    persist(collection, &journal)?;
    Ok(ChangeFeed {
        owner: owner.clone(),
        fencing_epoch: journal.fencing_epoch,
    })
}

pub(crate) fn baseline_plan(
    collection: &Collection,
    feed: &ChangeFeed,
) -> Result<BaselinePlan, ProviderError> {
    let journal = checked_journal(collection, feed)?;
    let through = journal.baseline_through.unwrap_or(journal.head);
    Ok(BaselinePlan {
        baseline: ChangeFeedBaseline {
            fencing_epoch: journal.fencing_epoch,
            acknowledged_through: ChangeWatermark::from_stored(through),
            feed_head: ChangeWatermark::from_stored(journal.head),
        },
        commits: journal
            .events
            .iter()
            .filter_map(|event| event.commit_id.clone())
            .collect(),
        needs_commit: journal.baseline_through.is_none(),
    })
}

pub(crate) fn commit_baseline(
    collection: &Collection,
    feed: &ChangeFeed,
) -> Result<ChangeFeedBaseline, ProviderError> {
    let mut journal = checked_journal(collection, feed)?;
    let through = match journal.baseline_through {
        Some(through) => through,
        None => {
            let through = journal.head;
            journal.baseline_through = Some(through);
            journal.acknowledged_through = through;
            journal.events.clear();
            persist(collection, &journal)?;
            through
        }
    };
    Ok(ChangeFeedBaseline {
        fencing_epoch: journal.fencing_epoch,
        acknowledged_through: ChangeWatermark::from_stored(through),
        feed_head: ChangeWatermark::from_stored(journal.head),
    })
}

pub(crate) fn read(
    collection: &Collection,
    feed: &ChangeFeed,
    after: Option<ChangeWatermark>,
    limit: NonZeroUsize,
    context: &OperationContext,
) -> Result<RuntimeChangeEventPage, ProviderError> {
    context.check()?;
    let journal = checked_journal(collection, feed)?;
    let after = after
        .map(ChangeWatermark::get)
        .unwrap_or(journal.acknowledged_through);
    if after < journal.acknowledged_through {
        return Err(ProviderError::ChangeFeedRetentionGap);
    }
    if after >= journal.head {
        return Ok(RuntimeChangeEventPage {
            events: Vec::new(),
            next: None,
            feed_head: ChangeWatermark::from_stored(journal.head),
        });
    }
    let limit = limit.get().min(MAX_PAGE_ITEMS);
    let stored = journal
        .events
        .iter()
        .filter(|event| event.watermark > after)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    let last = stored.last().map(|event| event.watermark);
    let next = last
        .filter(|watermark| *watermark < journal.head)
        .map(ChangeWatermark::from_stored);
    let mut events = Vec::with_capacity(stored.len());
    for event in stored {
        context.check()?;
        events.push(runtime_event(event)?);
    }
    Ok(RuntimeChangeEventPage {
        events,
        next,
        feed_head: ChangeWatermark::from_stored(journal.head),
    })
}

pub(crate) fn ack(
    collection: &Collection,
    feed: &ChangeFeed,
    through: ChangeWatermark,
) -> Result<Vec<CommitId>, ProviderError> {
    let mut journal = checked_journal(collection, feed)?;
    if through.get() > journal.head {
        return Err(ProviderError::InvalidChangeFeedAck);
    }
    if through.get() <= journal.acknowledged_through {
        return Ok(Vec::new());
    }
    let mut commits = Vec::new();
    journal.events.retain(|event| {
        if event.watermark <= through.get() {
            if let Some(commit_id) = &event.commit_id {
                commits.push(commit_id.clone());
            }
            false
        } else {
            true
        }
    });
    journal.acknowledged_through = through.get();
    persist(collection, &journal)?;
    Ok(commits)
}

pub(crate) fn commits_through(
    collection: &Collection,
    feed: &ChangeFeed,
    through: ChangeWatermark,
) -> Result<Vec<CommitId>, ProviderError> {
    let journal = checked_journal(collection, feed)?;
    if through.get() > journal.head {
        return Err(ProviderError::InvalidChangeFeedAck);
    }
    if through.get() <= journal.acknowledged_through {
        return Ok(Vec::new());
    }
    Ok(journal
        .events
        .iter()
        .filter(|event| event.watermark <= through.get())
        .filter_map(|event| event.commit_id.clone())
        .collect())
}

pub(crate) fn transfer(
    collection: &Collection,
    intent: &ChangeFeedTransferIntent,
) -> Result<ChangeFeedTransfer, ProviderError> {
    let mut journal = read_or_default(collection)?;
    if let Some(stored) = &journal.pending_transfer {
        if stored.id == intent.id
            && stored.current == intent.current.as_str()
            && stored.next == intent.next.as_str()
            && stored.expected_acked_through == intent.expected_acked_through.get()
            && journal.owner.as_deref() == Some(intent.next.as_str())
        {
            return Ok(transfer_result(&journal, intent.id.clone()));
        }
        return Err(ProviderError::ChangeFeedTransferMismatch);
    }
    if journal.owner.as_deref() != Some(intent.current.as_str())
        || journal.acknowledged_through != intent.expected_acked_through.get()
    {
        return Err(ProviderError::ChangeFeedTransferMismatch);
    }
    journal.fencing_epoch = journal
        .fencing_epoch
        .checked_add(1)
        .ok_or(ProviderError::WatermarkExhausted)?;
    journal.owner = Some(intent.next.as_str().to_string());
    journal.pending_transfer = Some(StoredTransfer {
        id: intent.id.clone(),
        current: intent.current.as_str().to_string(),
        next: intent.next.as_str().to_string(),
        expected_acked_through: intent.expected_acked_through.get(),
        fencing_epoch: journal.fencing_epoch,
    });
    persist(collection, &journal)?;
    Ok(transfer_result(&journal, intent.id.clone()))
}

pub(crate) fn ack_transfer(
    collection: &Collection,
    id: &ChangeFeedTransferId,
) -> Result<(), ProviderError> {
    let mut journal = read_or_default(collection)?;
    match &journal.pending_transfer {
        Some(transfer) if &transfer.id == id => {
            journal.pending_transfer = None;
            journal.last_acknowledged_transfer_id = Some(id.clone());
            persist(collection, &journal)
        }
        _ if journal.last_acknowledged_transfer_id.as_ref() == Some(id) => Ok(()),
        _ => Err(ProviderError::ChangeFeedTransferMismatch),
    }
}

fn transfer_result(journal: &FeedJournal, id: ChangeFeedTransferId) -> ChangeFeedTransfer {
    let owner = ChangeFeedOwnerId::from_stored(
        journal
            .owner
            .clone()
            .expect("a transferred feed has an owner"),
    );
    ChangeFeedTransfer {
        feed: ChangeFeed {
            owner,
            fencing_epoch: journal.fencing_epoch,
        },
        receipt: ChangeFeedTransferReceipt {
            id,
            fencing_epoch: journal.fencing_epoch,
            acknowledged_through: ChangeWatermark::from_stored(journal.acknowledged_through),
            feed_head: ChangeWatermark::from_stored(journal.head),
        },
    }
}

fn runtime_event(event: StoredEvent) -> Result<RuntimeChangeEvent, ProviderError> {
    let changes = match event.changes {
        StoredChangeSet::Exact(changes) => ChangeSet::Exact(ChangeBatch::new(changes)?),
        StoredChangeSet::CollectionWide { reason } => ChangeSet::CollectionWide { reason },
    };
    Ok(RuntimeChangeEvent {
        identity: ChangeEventIdentity {
            id: event.id,
            watermark: ChangeWatermark::from_stored(event.watermark),
        },
        generation: event.generation,
        changes,
        origin: event.origin,
        commit_id: event.commit_id,
    })
}

fn checked_journal(
    collection: &Collection,
    feed: &ChangeFeed,
) -> Result<FeedJournal, ProviderError> {
    let journal = read_or_default(collection)?;
    if journal.owner.as_deref() != Some(feed.owner.as_str())
        || journal.fencing_epoch != feed.fencing_epoch
    {
        return Err(ProviderError::ChangeFeedFenced);
    }
    validate(&journal)?;
    Ok(journal)
}

fn validate(journal: &FeedJournal) -> Result<(), ProviderError> {
    if journal.version != FEED_VERSION
        || journal.acknowledged_through > journal.head
        || journal
            .baseline_through
            .is_some_and(|through| through > journal.head)
    {
        return Err(corrupt("invalid version or acknowledged position"));
    }
    let mut previous = journal.acknowledged_through;
    for event in &journal.events {
        if event.watermark != previous.saturating_add(1) || event.watermark > journal.head {
            return Err(corrupt("event watermarks are not strictly ordered"));
        }
        previous = event.watermark;
    }
    if journal.events.last().map(|event| event.watermark)
        != (journal.head > journal.acknowledged_through).then_some(journal.head)
    {
        return Err(corrupt("feed head is not retained in the event sequence"));
    }
    Ok(())
}

fn read_or_default(collection: &Collection) -> Result<FeedJournal, ProviderError> {
    let path = feed_path(collection);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| corrupt(&error.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FeedJournal {
            version: FEED_VERSION,
            owner: None,
            fencing_epoch: 0,
            acknowledged_through: 0,
            head: 0,
            baseline_through: None,
            events: Vec::new(),
            pending_transfer: None,
            last_acknowledged_transfer_id: None,
        }),
        Err(error) => Err(feed_io(error)),
    }
}

fn persist(collection: &Collection, journal: &FeedJournal) -> Result<(), ProviderError> {
    validate(journal)?;
    let path = feed_path(collection);
    let parent = path.parent().expect("feed path has a parent");
    fs::create_dir_all(parent).map_err(feed_io)?;
    let bytes = serde_json::to_vec_pretty(journal).map_err(|error| corrupt(&error.to_string()))?;
    if bytes.len() as u64 > MAX_FEED_BYTES {
        return Err(ProviderError::ChangeFeedCapacityExhausted);
    }
    crate::operations::atomic_write(&path, &bytes).map_err(feed_io)
}

fn feed_path(collection: &Collection) -> PathBuf {
    collection.root.join(RUNTIME_DIRECTORY).join(FEED_FILE)
}

fn corrupt(message: &str) -> ProviderError {
    ProviderError::Transaction {
        code: "change_feed_corrupt",
        message: message.to_string(),
    }
}

fn feed_io(error: std::io::Error) -> ProviderError {
    ProviderError::Transaction {
        code: "change_feed_io_failed",
        message: error.to_string(),
    }
}
