use std::collections::BTreeSet;
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[cfg(test)]
use super::apply_post_commit_hook;
use super::{
    apply_entry, capture_committed_file_facts, cleanup_transaction, current_revision,
    ensure_transaction_root, io_error, read_regular_file, sync_dir, validate_entry_path,
    write_synced, FileBaseline, JournalEntry, StagingGuard, TransactionError, TransactionScope,
    WriteLock, JOURNAL_FILE, TRANSACTIONS_DIR,
};
use crate::runtime::{
    CanonicalChange, CanonicalOperationFamily, CanonicalOperationOutcome, CanonicalOperationValue,
    ChangeBatch, ChangeBatchDescriptor, ChangeEventId, ChangeWatermark, CollectionGeneration,
    CommitId, HostClaimId, LegacyRecoveredV03Value, OperationContext, OperationKind,
    RecordChangeKind, ResourceChangeKind,
};
use crate::{v03::OperationResult, Collection};

const RUNTIME_JOURNAL_VERSION: u32 = 3;
const LEGACY_RUNTIME_JOURNAL_VERSION: u32 = 2;
const MAX_ACTIVE_RUNTIME_TRANSACTIONS: usize = 128;
const MAX_RUNTIME_CHANGE_ITEMS: usize = 100_000;
const MAX_RUNTIME_METADATA_BYTES: usize = 16 * 1024 * 1024;

#[cfg(test)]
static RUNTIME_SETTLEMENT_DELAYS: std::sync::Mutex<Vec<(String, u64)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
static RUNTIME_CRASH_POINT: std::sync::Mutex<Option<(String, u8)>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_runtime_settlement_delay(id: &CommitId, delay: std::time::Duration) {
    let mut configured = RUNTIME_SETTLEMENT_DELAYS.lock().unwrap();
    configured.retain(|candidate| candidate.0 != id.as_str());
    if !delay.is_zero() {
        configured.push((
            id.as_str().to_string(),
            delay.as_millis().min(u128::from(u64::MAX)) as u64,
        ));
    }
}

#[cfg(test)]
pub(crate) fn set_runtime_crash_point(id: &CommitId, point: u8) {
    *RUNTIME_CRASH_POINT.lock().unwrap() = Some((id.as_str().to_string(), point));
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RuntimePhase {
    Prepared,
    Committing,
    Committed,
    RejectedBeforeCommit,
    CancelledBeforeCommit,
    NeedsManualRecovery,
}

#[derive(Debug, Deserialize, Serialize)]
struct RuntimeJournal {
    version: u32,
    id: String,
    scope: TransactionScope,
    phase: RuntimePhase,
    applied: usize,
    entries: Vec<JournalEntry>,
    host_claim: String,
    mutation_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation_outcome: Option<CanonicalOperationOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation_result: Option<OperationResult>,
    change_descriptor: ChangeBatchDescriptor,
    changes: Vec<CanonicalChange>,
    event_id: ChangeEventId,
    generation: Option<CollectionGeneration>,
    watermark: Option<ChangeWatermark>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation_rejection: Option<CanonicalOperationOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rejection: Option<OperationResult>,
    resolution_acked: bool,
    event_acked: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RuntimePrepareOutcome {
    NoMutation(CanonicalOperationOutcome),
    Prepared(CommitId),
    Existing(RuntimeResolution),
}

pub(crate) struct RuntimePrepareInput<'a> {
    pub baseline: &'a FileBaseline,
    pub desired: &'a FileBaseline,
    pub claim: &'a HostClaimId,
    pub mutation_digest: &'a str,
    pub operation: &'a CanonicalOperationOutcome,
    pub changes: &'a ChangeBatch,
    pub event_id: &'a ChangeEventId,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RuntimeCommitAttempt {
    Committed(RuntimeResolution),
    RejectedBeforeCommit(RuntimeResolution),
    AlreadyCancelled,
    SettlementRequired(CommitId),
    SettlementPending(CommitId),
    NeedsManualRecovery(CommitId),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RuntimeResolution {
    Prepared {
        commit_id: CommitId,
    },
    Committing {
        commit_id: CommitId,
    },
    Committed {
        commit_id: CommitId,
        result: CanonicalOperationOutcome,
        generation: CollectionGeneration,
        watermark: ChangeWatermark,
        event_id: ChangeEventId,
        changes: ChangeBatch,
    },
    RejectedBeforeCommit {
        commit_id: CommitId,
        rejection: CanonicalOperationOutcome,
    },
    CancelledBeforeCommit {
        commit_id: CommitId,
    },
    NeedsManualRecovery {
        commit_id: CommitId,
    },
}

pub(crate) fn prepare_runtime_transaction(
    collection: &Collection,
    input: RuntimePrepareInput<'_>,
    context: &OperationContext,
) -> Result<RuntimePrepareOutcome, TransactionError> {
    let RuntimePrepareInput {
        baseline,
        desired,
        claim,
        mutation_digest,
        operation,
        changes,
        event_id,
    } = input;
    context_check(context)?;
    let _write_lock = WriteLock::acquire_context(collection, context)?;
    context_check(context)?;
    ensure_transaction_root(collection)?;

    if let Some(existing) = find_by_claim(collection, claim.as_str())? {
        if existing.mutation_digest != mutation_digest {
            return Err(TransactionError::ClaimMismatch);
        }
        return Ok(match existing.phase {
            RuntimePhase::Prepared => RuntimePrepareOutcome::Prepared(commit_id(&existing)),
            _ => RuntimePrepareOutcome::Existing(resolution(&existing)?),
        });
    }

    ensure_capacity(collection, changes)?;
    let scope = runtime_scope(changes)?;
    let paths = baseline
        .keys()
        .chain(desired.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let changed_paths = paths
        .into_iter()
        .filter(|path| baseline.get(path) != desired.get(path))
        .collect::<Vec<_>>();
    if changed_paths.is_empty() {
        return Ok(RuntimePrepareOutcome::NoMutation(operation.clone()));
    }

    let id = uuid::Uuid::new_v4().simple().to_string();
    let directory = PathBuf::from(TRANSACTIONS_DIR).join(&id);
    collection
        .held_root()
        .create_dir_all(&directory.join("stage"))
        .map_err(|source| io_error(collection.root.join(&directory).join("stage"), source))?;
    let mut staging = StagingGuard {
        root: collection.held_root().clone(),
        directory: directory.clone(),
        durable: false,
    };
    collection
        .held_root()
        .create_dir_all(&directory.join("backup"))
        .map_err(|source| io_error(collection.root.join(&directory).join("backup"), source))?;

    let mut entries = Vec::with_capacity(changed_paths.len());
    for path in changed_paths {
        context_check(context)?;
        validate_entry_path(collection, &path, scope)?;
        let before = baseline.get(&path);
        let after = desired.get(&path);
        let index = entries.len();
        let stage_file = match after {
            Some(bytes) => {
                let name = format!("stage/{index}");
                write_synced(collection, &directory.join(&name), bytes)?;
                Some(name)
            }
            None => None,
        };
        let backup_file = match before {
            Some(bytes) => {
                let name = format!("backup/{index}");
                write_synced(collection, &directory.join(&name), bytes)?;
                Some(name)
            }
            None => None,
        };
        entries.push(JournalEntry {
            path,
            before_revision: before.map(|bytes| crate::v03::revision(bytes)),
            after_revision: after.map(|bytes| crate::v03::revision(bytes)),
            stage_file,
            backup_file,
        });
    }

    context_check(context)?;
    sync_dir(collection, &directory.join("stage"))?;
    sync_dir(collection, &directory.join("backup"))?;
    let journal = RuntimeJournal {
        version: RUNTIME_JOURNAL_VERSION,
        id: id.clone(),
        scope,
        phase: RuntimePhase::Prepared,
        applied: 0,
        entries,
        host_claim: claim.as_str().to_string(),
        mutation_digest: mutation_digest.to_string(),
        operation_outcome: Some(operation.clone()),
        operation_result: None,
        change_descriptor: changes.descriptor().clone(),
        changes: changes.items().to_vec(),
        event_id: event_id.clone(),
        generation: None,
        watermark: None,
        operation_rejection: None,
        rejection: None,
        resolution_acked: false,
        event_acked: false,
    };
    validate_journal_operation_family(&journal)?;
    persist_runtime_journal(collection, &directory, &journal)?;
    staging.durable = true;
    Ok(RuntimePrepareOutcome::Prepared(CommitId::from_stored(id)))
}

pub(crate) fn attach_runtime_prepared(
    collection: &Collection,
    claim: &HostClaimId,
    context: &OperationContext,
) -> Result<Option<CommitId>, TransactionError> {
    context_check(context)?;
    let _lock = WriteLock::acquire_context(collection, context)?;
    context_check(context)?;
    Ok(find_by_claim(collection, claim.as_str())?
        .filter(|journal| journal.phase == RuntimePhase::Prepared)
        .map(|journal| commit_id(&journal)))
}

pub(crate) fn commit_runtime_prepared(
    collection: &Collection,
    id: &CommitId,
    generation: &CollectionGeneration,
    watermark: ChangeWatermark,
    context: &OperationContext,
) -> Result<RuntimeCommitAttempt, TransactionError> {
    context_check(context)?;
    let _lock = WriteLock::acquire_context(collection, context)?;
    context_check(context)?;
    let directory = transaction_directory(collection, id);
    let mut journal = read_runtime_journal(collection, &directory)?;
    match journal.phase {
        RuntimePhase::Prepared => {}
        RuntimePhase::Committing => return Ok(RuntimeCommitAttempt::SettlementPending(id.clone())),
        RuntimePhase::Committed => {
            return Ok(RuntimeCommitAttempt::Committed(resolution(&journal)?))
        }
        RuntimePhase::RejectedBeforeCommit => {
            return Ok(RuntimeCommitAttempt::RejectedBeforeCommit(resolution(
                &journal,
            )?))
        }
        RuntimePhase::CancelledBeforeCommit => return Ok(RuntimeCommitAttempt::AlreadyCancelled),
        RuntimePhase::NeedsManualRecovery => {
            return Ok(RuntimeCommitAttempt::NeedsManualRecovery(id.clone()))
        }
    }

    if let Some(path) = first_precondition_conflict(collection, &journal)? {
        journal.phase = RuntimePhase::RejectedBeforeCommit;
        journal.operation_rejection = Some(conflict_result(&journal, &path)?);
        release_payloads(collection, &directory, &mut journal);
        persist_runtime_journal(collection, &directory, &journal)?;
        return Ok(RuntimeCommitAttempt::RejectedBeforeCommit(resolution(
            &journal,
        )?));
    }

    journal.generation = Some(generation.clone());
    journal.watermark = Some(watermark);
    journal.phase = RuntimePhase::Committing;
    persist_runtime_journal(collection, &directory, &journal)?;
    simulate_runtime_crash(&journal.id, 1)?;
    Ok(RuntimeCommitAttempt::SettlementRequired(id.clone()))
}

pub(crate) fn settle_runtime_commit(
    collection: &Collection,
    id: &CommitId,
) -> Result<RuntimeResolution, TransactionError> {
    let _lock = WriteLock::acquire(collection)?;
    let directory = transaction_directory(collection, id);
    let mut journal = read_runtime_journal(collection, &directory)?;
    match journal.phase {
        RuntimePhase::Committing => match settle(collection, &directory, &mut journal) {
            Ok(()) => resolution(&journal),
            Err(error) => {
                #[cfg(test)]
                if matches!(error, TransactionError::SimulatedCrash) {
                    return Err(error);
                }
                journal.phase = RuntimePhase::NeedsManualRecovery;
                let _ = persist_runtime_journal(collection, &directory, &journal);
                Err(error)
            }
        },
        _ => resolution(&journal),
    }
}

pub(crate) fn cancel_runtime_prepared(
    collection: &Collection,
    id: &CommitId,
    context: &OperationContext,
) -> Result<RuntimeResolution, TransactionError> {
    context_check(context)?;
    let _lock = WriteLock::acquire_context(collection, context)?;
    context_check(context)?;
    let directory = transaction_directory(collection, id);
    let mut journal = read_runtime_journal(collection, &directory)?;
    if journal.phase == RuntimePhase::Prepared {
        journal.phase = RuntimePhase::CancelledBeforeCommit;
        release_payloads(collection, &directory, &mut journal);
        persist_runtime_journal(collection, &directory, &journal)?;
    }
    resolution(&journal)
}

pub(crate) fn resolve_runtime_commit(
    collection: &Collection,
    id: &CommitId,
    context: &OperationContext,
) -> Result<Option<RuntimeResolution>, TransactionError> {
    context_check(context)?;
    let _lock = WriteLock::acquire_context(collection, context)?;
    context_check(context)?;
    let directory = transaction_directory(collection, id);
    if collection.held_root().open_dir(&directory).is_err() {
        return Ok(None);
    }
    read_runtime_journal(collection, &directory).and_then(|journal| resolution(&journal).map(Some))
}

pub(crate) fn resolve_runtime_claim(
    collection: &Collection,
    claim: &HostClaimId,
    context: &OperationContext,
) -> Result<Option<(CommitId, RuntimeResolution)>, TransactionError> {
    context_check(context)?;
    let _lock = WriteLock::acquire_context(collection, context)?;
    context_check(context)?;
    find_by_claim(collection, claim.as_str())?
        .map(|journal| {
            let id = commit_id(&journal);
            resolution(&journal).map(|resolution| (id, resolution))
        })
        .transpose()
}

pub(crate) fn ack_runtime_resolution(
    collection: &Collection,
    id: &CommitId,
    context: &OperationContext,
) -> Result<(), TransactionError> {
    update_ack(collection, id, context, true)
}

pub(crate) fn ack_runtime_change_event(
    collection: &Collection,
    id: &CommitId,
    context: &OperationContext,
) -> Result<(), TransactionError> {
    update_ack(collection, id, context, false)
}

pub(crate) fn list_unacked_runtime_events(
    collection: &Collection,
    context: &OperationContext,
) -> Result<Vec<RuntimeResolution>, TransactionError> {
    context_check(context)?;
    let _lock = WriteLock::acquire_context(collection, context)?;
    context_check(context)?;
    let directories = transaction_directories(collection)?;
    let mut resolutions = Vec::new();
    for directory in directories {
        context_check(context)?;
        let path = directory.join(JOURNAL_FILE);
        let bytes = match collection.held_root().read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(io_error(collection.root.join(&path), source)),
        };
        let value = serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
        if !runtime_journal_version(&value) {
            continue;
        }
        let journal = decode_runtime_journal(value)?;
        validate_journal(collection, &directory, &journal)?;
        if journal.phase == RuntimePhase::Committed && !journal.event_acked {
            resolutions.push(resolution(&journal)?);
        }
    }
    Ok(resolutions)
}

/// Remove copied runtime transaction support after collection recovery has
/// made canonical Markdown stable. A fork receives a new application identity,
/// so old host claims and acknowledgement obligations must not cross into it.
pub(crate) fn reset_runtime_support_for_fork(
    collection: &Collection,
    context: &OperationContext,
) -> Result<(), TransactionError> {
    context_check(context)?;
    let _lock = WriteLock::acquire_context(collection, context)?;
    context_check(context)?;
    let directories = transaction_directories(collection)?;
    for directory in directories {
        let journal_path = directory.join(JOURNAL_FILE);
        let bytes = collection
            .held_root()
            .read(&journal_path)
            .map_err(|source| io_error(collection.root.join(&journal_path), source))?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
        if !runtime_journal_version(&value) {
            return Err(TransactionError::ManualRecovery(format!(
                "non-runtime transaction '{}' remained after collection recovery",
                directory.display()
            )));
        }
        let mut journal = decode_runtime_journal(value)?;
        validate_journal(collection, &directory, &journal)?;
        if journal.phase == RuntimePhase::Committing {
            settle(collection, &directory, &mut journal)?;
        }
        if journal.phase == RuntimePhase::NeedsManualRecovery {
            return Err(TransactionError::ManualRecovery(format!(
                "runtime transaction '{}' must be repaired before the collection can be forked",
                journal.id
            )));
        }
        collection
            .held_root()
            .remove_dir_all(&directory)
            .map_err(|source| io_error(collection.root.join(&directory), source))?;
    }
    sync_dir(collection, Path::new(TRANSACTIONS_DIR))?;
    Ok(())
}

fn update_ack(
    collection: &Collection,
    id: &CommitId,
    context: &OperationContext,
    resolution_ack: bool,
) -> Result<(), TransactionError> {
    context_check(context)?;
    let _lock = WriteLock::acquire_context(collection, context)?;
    context_check(context)?;
    let directory = transaction_directory(collection, id);
    if collection.held_root().open_dir(&directory).is_err() {
        return Ok(());
    }
    let mut journal = read_runtime_journal(collection, &directory)?;
    if resolution_ack {
        journal.resolution_acked = true;
    } else {
        journal.event_acked = true;
    }
    let removable = match journal.phase {
        RuntimePhase::Committed => journal.resolution_acked && journal.event_acked,
        RuntimePhase::RejectedBeforeCommit | RuntimePhase::CancelledBeforeCommit => {
            journal.resolution_acked
        }
        _ => false,
    };
    if removable {
        cleanup_transaction(collection, &directory);
    } else {
        persist_runtime_journal(collection, &directory, &journal)?;
    }
    Ok(())
}

pub(super) fn recover_runtime_one(
    collection: &Collection,
    directory: &Path,
    bytes: &[u8],
) -> Result<bool, TransactionError> {
    let value = serde_json::from_slice(bytes)
        .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
    let mut journal = decode_runtime_journal(value)?;
    validate_journal(collection, directory, &journal)?;
    match journal.phase {
        RuntimePhase::Prepared
        | RuntimePhase::Committed
        | RuntimePhase::RejectedBeforeCommit
        | RuntimePhase::CancelledBeforeCommit => Ok(false),
        RuntimePhase::NeedsManualRecovery => Err(TransactionError::ManualRecovery(format!(
            "runtime transaction '{}' requires manual recovery",
            journal.id
        ))),
        RuntimePhase::Committing => match settle(collection, directory, &mut journal) {
            Ok(()) => Ok(true),
            Err(error) => {
                #[cfg(test)]
                if matches!(error, TransactionError::SimulatedCrash) {
                    return Err(error);
                }
                journal.phase = RuntimePhase::NeedsManualRecovery;
                let _ = persist_runtime_journal(collection, directory, &journal);
                Err(error)
            }
        },
    }
}

fn settle(
    collection: &Collection,
    directory: &Path,
    journal: &mut RuntimeJournal,
) -> Result<(), TransactionError> {
    #[cfg(test)]
    {
        let delay = RUNTIME_SETTLEMENT_DELAYS
            .lock()
            .unwrap()
            .iter()
            .filter(|candidate| candidate.0 == journal.id)
            .map(|candidate| candidate.1)
            .next()
            .unwrap_or(0);
        if delay > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }
    }
    validate_journal(collection, directory, journal)?;
    for index in 0..journal.entries.len() {
        let entry = &journal.entries[index];
        let path = crate::api::CollectionPath::new(&entry.path)?.to_path_buf();
        let current = current_revision(collection, &path)?;
        if current == entry.after_revision {
            if journal.applied < index + 1 {
                journal.applied = index + 1;
                persist_runtime_journal(collection, directory, journal)?;
            }
            continue;
        }
        if current != entry.before_revision {
            return Err(TransactionError::ManualRecovery(format!(
                "'{}' matches neither its before nor intended revision",
                entry.path
            )));
        }
        apply_entry(collection, directory, entry, journal.scope)?;
        simulate_runtime_crash(&journal.id, 2)?;
        journal.applied = index + 1;
        persist_runtime_journal(collection, directory, journal)?;
        simulate_runtime_crash(&journal.id, 3)?;
    }
    let file_facts = capture_committed_file_facts(collection, &journal.entries)?;
    journal_operation_mut(journal)?.attach_committed_file_facts(&file_facts);
    journal.phase = RuntimePhase::Committed;
    persist_runtime_journal(collection, directory, journal)?;
    #[cfg(test)]
    apply_post_commit_hook(collection)?;
    simulate_runtime_crash(&journal.id, 4)
}

#[cfg(test)]
fn simulate_runtime_crash(id: &str, point: u8) -> Result<(), TransactionError> {
    let mut configured = RUNTIME_CRASH_POINT.lock().unwrap();
    if configured
        .as_ref()
        .is_some_and(|candidate| candidate.0 == id && candidate.1 == point)
    {
        *configured = None;
        return Err(TransactionError::SimulatedCrash);
    }
    Ok(())
}

#[cfg(not(test))]
fn simulate_runtime_crash(_id: &str, _point: u8) -> Result<(), TransactionError> {
    Ok(())
}

fn first_precondition_conflict(
    collection: &Collection,
    journal: &RuntimeJournal,
) -> Result<Option<String>, TransactionError> {
    for entry in &journal.entries {
        let path = crate::api::CollectionPath::new(&entry.path)?.to_path_buf();
        if current_revision(collection, &path)? != entry.before_revision {
            return Ok(Some(entry.path.clone()));
        }
    }
    first_unsettled_conflict(collection, journal)
}

/// A path already owned by a transaction that passed its commit point but has
/// not settled.
///
/// Settlement runs after the commit lock is released, so between the two the
/// working tree still shows the old revision while the new one is already
/// committed. Checking only the working tree therefore lets a second writer
/// commit against a baseline that is about to be overwritten. Both then hold
/// commit points for the same path, and whichever settles second finds a
/// revision matching neither its before nor its intended state — reported as
/// `manual_recovery_required`, which strands the journal and fails every later
/// `Collection::open`.
///
/// Rejecting here keeps that outcome reachable only by genuine external edits,
/// and makes a lost race an ordinary `concurrent_modification` before any
/// commit point is taken.
fn first_unsettled_conflict(
    collection: &Collection,
    journal: &RuntimeJournal,
) -> Result<Option<String>, TransactionError> {
    let directories = transaction_directories(collection)?;
    let claimed = journal
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>();
    for directory in directories {
        if directory.file_name().and_then(|name| name.to_str()) == Some(journal.id.as_str()) {
            continue;
        }
        let journal_path = directory.join(JOURNAL_FILE);
        let bytes = match collection.held_root().read(&journal_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(io_error(collection.root.join(journal_path), source)),
        };
        let value = serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
        if !runtime_journal_version(&value) {
            continue;
        }
        let other = decode_runtime_journal(value)?;
        // Only a committed-but-unsettled transaction owns paths it has not yet
        // written. A prepared one has taken no commit point and loses the race
        // itself; a settled one is already visible in the working tree.
        if other.phase != RuntimePhase::Committing {
            continue;
        }
        if let Some(entry) = other
            .entries
            .iter()
            .find(|entry| claimed.contains(entry.path.as_str()))
        {
            return Ok(Some(entry.path.clone()));
        }
    }
    Ok(None)
}

fn validate_journal(
    collection: &Collection,
    directory: &Path,
    journal: &RuntimeJournal,
) -> Result<(), TransactionError> {
    if !matches!(
        journal.version,
        LEGACY_RUNTIME_JOURNAL_VERSION | RUNTIME_JOURNAL_VERSION
    ) || directory.file_name().and_then(|name| name.to_str()) != Some(&journal.id)
        || journal.applied > journal.entries.len()
    {
        return Err(TransactionError::InvalidJournal(format!(
            "runtime transaction identity or progress mismatch in '{}'",
            directory.display()
        )));
    }
    let batch = ChangeBatch::new(journal.changes.clone())
        .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
    if batch.descriptor() != &journal.change_descriptor {
        return Err(TransactionError::InvalidJournal(
            "runtime change batch does not match its descriptor".to_string(),
        ));
    }
    validate_journal_operation_family(journal)?;
    for (index, entry) in journal.entries.iter().enumerate() {
        validate_entry_path(collection, &entry.path, journal.scope)?;
        let expected_stage = entry
            .after_revision
            .as_ref()
            .map(|_| format!("stage/{index}"));
        let expected_backup = entry
            .before_revision
            .as_ref()
            .map(|_| format!("backup/{index}"));
        if entry.stage_file.as_deref() != expected_stage.as_deref()
            || entry.backup_file.as_deref() != expected_backup.as_deref()
        {
            return Err(TransactionError::InvalidJournal(format!(
                "payload paths for '{}' do not match its journal position",
                entry.path
            )));
        }
        if let Some(stage_file) = &entry.stage_file {
            let staged = read_regular_file(collection, &directory.join(stage_file))?;
            if Some(crate::v03::revision(&staged)) != entry.after_revision {
                return Err(TransactionError::InvalidJournal(format!(
                    "staged contents for '{}' do not match the journal",
                    entry.path
                )));
            }
        }
    }
    match journal.phase {
        RuntimePhase::Prepared
        | RuntimePhase::RejectedBeforeCommit
        | RuntimePhase::CancelledBeforeCommit => {
            if journal.generation.is_some() || journal.watermark.is_some() {
                return Err(TransactionError::InvalidJournal(
                    "a pre-commit state contains assigned ordering metadata".to_string(),
                ));
            }
        }
        RuntimePhase::Committing | RuntimePhase::Committed | RuntimePhase::NeedsManualRecovery => {
            if journal.generation.is_none() || journal.watermark.is_none() {
                return Err(TransactionError::InvalidJournal(
                    "a post-boundary state is missing ordering metadata".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_journal_operation_family(journal: &RuntimeJournal) -> Result<(), TransactionError> {
    if journal.version == LEGACY_RUNTIME_JOURNAL_VERSION {
        return Ok(());
    }
    if journal.changes.is_empty() {
        return Err(TransactionError::InvalidJournal(
            "v3 mutation journal has no canonical changes".to_string(),
        ));
    }
    let operation = journal.operation_outcome.as_ref().ok_or_else(|| {
        TransactionError::InvalidJournal("v3 canonical operation outcome missing".to_string())
    })?;
    let CanonicalOperationFamily::Operation(kind) = operation.family() else {
        return Err(TransactionError::InvalidJournal(
            "mutation journal contains a non-operation canonical family".to_string(),
        ));
    };
    if matches!(operation.value(), CanonicalOperationValue::WireOnly(_)) {
        return Err(TransactionError::InvalidJournal(
            "mutation journal contains a wire-only outcome".to_string(),
        ));
    }
    if let Some(rejection) = &journal.operation_rejection {
        if rejection.family() != CanonicalOperationFamily::Operation(kind)
            || matches!(rejection.value(), CanonicalOperationValue::WireOnly(_))
        {
            return Err(TransactionError::InvalidJournal(
                "commit rejection operation does not match the transaction operation".to_string(),
            ));
        }
    }

    let exact = match journal.scope {
        TransactionScope::Records => match kind {
            OperationKind::Create => journal.changes.iter().all(|change| {
                matches!(change, CanonicalChange::Record(record) if record.kind == RecordChangeKind::Created)
            }),
            OperationKind::Update => journal.changes.iter().all(|change| {
                matches!(change, CanonicalChange::Record(record) if record.kind == RecordChangeKind::Updated)
            }),
            OperationKind::Delete => journal.changes.iter().all(|change| {
                matches!(change, CanonicalChange::Record(record) if record.kind == RecordChangeKind::Deleted)
            }),
            OperationKind::Rename => {
                let renamed = journal.changes.iter().filter(|change| {
                    matches!(change, CanonicalChange::Record(record) if record.kind == RecordChangeKind::Renamed)
                }).count();
                renamed == 1 && journal.changes.iter().all(|change| {
                    matches!(change, CanonicalChange::Record(record) if matches!(record.kind, RecordChangeKind::Renamed | RecordChangeKind::Updated))
                })
            }
            OperationKind::Batch => journal
                .changes
                .iter()
                .all(|change| matches!(change, CanonicalChange::Record(_))),
            _ => false,
        },
        TransactionScope::Resources => validate_resource_operation(kind, &journal.changes),
        TransactionScope::SystemMigration => false,
    };
    if !exact {
        return Err(TransactionError::InvalidJournal(
            "canonical operation does not match the transaction change family".to_string(),
        ));
    }
    Ok(())
}

fn validate_resource_operation(kind: OperationKind, changes: &[CanonicalChange]) -> bool {
    changes.iter().all(|change| {
        let CanonicalChange::Resource(resource) = change else {
            return false;
        };
        match kind {
            OperationKind::CreateViewSource => {
                resource.kind == ResourceChangeKind::ViewSource
                    && resource.before_revision.is_none()
                    && resource.after_revision.is_some()
            }
            OperationKind::UpdateViewSource => {
                resource.kind == ResourceChangeKind::ViewSource
                    && resource.before_revision.is_some()
                    && resource.after_revision.is_some()
            }
            OperationKind::DeleteViewSource => {
                resource.kind == ResourceChangeKind::ViewSource
                    && resource.before_revision.is_some()
                    && resource.after_revision.is_none()
            }
            OperationKind::CreateType => {
                resource.kind == ResourceChangeKind::TypeDefinition
                    && resource.before_revision.is_none()
                    && resource.after_revision.is_some()
            }
            OperationKind::UpdateType => {
                resource.kind == ResourceChangeKind::TypeDefinition
                    && resource.before_revision.is_some()
                    && resource.after_revision.is_some()
            }
            OperationKind::ApplyTypePack | OperationKind::ApplyCollectionSetup => true,
            _ => false,
        }
    })
}

fn ensure_capacity(collection: &Collection, changes: &ChangeBatch) -> Result<(), TransactionError> {
    if changes.descriptor().count > MAX_RUNTIME_CHANGE_ITEMS {
        return Err(TransactionError::RuntimeCapacityExhausted);
    }
    let metadata = serde_json::to_vec(changes.items())
        .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
    if metadata.len() > MAX_RUNTIME_METADATA_BYTES {
        return Err(TransactionError::RuntimeCapacityExhausted);
    }
    let active = transaction_directories(collection)?.len();
    if active >= MAX_ACTIVE_RUNTIME_TRANSACTIONS {
        return Err(TransactionError::RuntimeCapacityExhausted);
    }
    Ok(())
}

fn runtime_scope(changes: &ChangeBatch) -> Result<TransactionScope, TransactionError> {
    let records = changes
        .items()
        .iter()
        .any(|change| matches!(change, CanonicalChange::Record(_)));
    let resources = changes
        .items()
        .iter()
        .any(|change| matches!(change, CanonicalChange::Resource(_)));
    match (records, resources) {
        (true, false) => Ok(TransactionScope::Records),
        (false, true) => Ok(TransactionScope::Resources),
        (false, false) => Err(TransactionError::InvalidJournal(
            "runtime mutation has no canonical changes".to_string(),
        )),
        (true, true) => Err(TransactionError::InvalidJournal(
            "runtime mutation mixes record and resource scopes".to_string(),
        )),
    }
}

fn find_by_claim(
    collection: &Collection,
    claim: &str,
) -> Result<Option<RuntimeJournal>, TransactionError> {
    let directories = transaction_directories(collection)?;
    for directory in directories {
        let journal_path = directory.join(JOURNAL_FILE);
        let bytes = match collection.held_root().read(&journal_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(io_error(collection.root.join(journal_path), source)),
        };
        let value = serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
        if !runtime_journal_version(&value) {
            continue;
        }
        let journal = decode_runtime_journal(value)?;
        validate_journal(collection, &directory, &journal)?;
        if journal.host_claim == claim {
            return Ok(Some(journal));
        }
    }
    Ok(None)
}

fn resolution(journal: &RuntimeJournal) -> Result<RuntimeResolution, TransactionError> {
    let commit_id = commit_id(journal);
    Ok(match journal.phase {
        RuntimePhase::Prepared => RuntimeResolution::Prepared { commit_id },
        RuntimePhase::Committing => RuntimeResolution::Committing { commit_id },
        RuntimePhase::Committed => RuntimeResolution::Committed {
            commit_id,
            result: journal_operation(journal)?,
            generation: journal.generation.clone().ok_or_else(|| {
                TransactionError::InvalidJournal("committed generation missing".to_string())
            })?,
            watermark: journal.watermark.ok_or_else(|| {
                TransactionError::InvalidJournal("committed watermark missing".to_string())
            })?,
            event_id: journal.event_id.clone(),
            changes: ChangeBatch::new(journal.changes.clone())
                .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?,
        },
        RuntimePhase::RejectedBeforeCommit => RuntimeResolution::RejectedBeforeCommit {
            commit_id,
            rejection: journal_rejection(journal)?,
        },
        RuntimePhase::CancelledBeforeCommit => {
            RuntimeResolution::CancelledBeforeCommit { commit_id }
        }
        RuntimePhase::NeedsManualRecovery => RuntimeResolution::NeedsManualRecovery { commit_id },
    })
}

fn release_payloads(collection: &Collection, directory: &Path, journal: &mut RuntimeJournal) {
    let _ = collection
        .held_root()
        .remove_dir_all(&directory.join("stage"));
    let _ = collection
        .held_root()
        .remove_dir_all(&directory.join("backup"));
    // Canonical change evidence is retained after payload cleanup so recovery
    // can continue to authenticate the operation family.
    for entry in &mut journal.entries {
        entry.stage_file = None;
        entry.backup_file = None;
    }
    journal.entries.clear();
}

fn journal_operation(
    journal: &RuntimeJournal,
) -> Result<CanonicalOperationOutcome, TransactionError> {
    match journal.version {
        RUNTIME_JOURNAL_VERSION => journal.operation_outcome.clone().ok_or_else(|| {
            TransactionError::InvalidJournal("v3 runtime operation outcome missing".to_string())
        }),
        LEGACY_RUNTIME_JOURNAL_VERSION => {
            let result = journal.operation_result.clone().ok_or_else(|| {
                TransactionError::InvalidJournal("v2 runtime operation result missing".to_string())
            })?;
            match legacy_operation_kind(journal, &result) {
                Some(kind) => CanonicalOperationOutcome::recover_v03(kind, result)
                    .map_err(|error| TransactionError::InvalidJournal(error.to_string())),
                None => Ok(legacy_recovered_v03(result)),
            }
        }
        _ => Err(TransactionError::InvalidJournal(
            "unsupported runtime journal version".to_string(),
        )),
    }
}

fn journal_operation_mut(
    journal: &mut RuntimeJournal,
) -> Result<&mut CanonicalOperationOutcome, TransactionError> {
    if journal.operation_outcome.is_none() {
        journal.operation_outcome = Some(journal_operation(journal)?);
        journal.operation_result = None;
        journal.version = RUNTIME_JOURNAL_VERSION;
    }
    journal.operation_outcome.as_mut().ok_or_else(|| {
        TransactionError::InvalidJournal("runtime operation outcome missing".to_string())
    })
}

fn journal_rejection(
    journal: &RuntimeJournal,
) -> Result<CanonicalOperationOutcome, TransactionError> {
    match journal.version {
        RUNTIME_JOURNAL_VERSION => journal.operation_rejection.clone().ok_or_else(|| {
            TransactionError::InvalidJournal("v3 commit rejection missing".to_string())
        }),
        LEGACY_RUNTIME_JOURNAL_VERSION => {
            let rejection = journal.rejection.clone().ok_or_else(|| {
                TransactionError::InvalidJournal("v2 commit rejection missing".to_string())
            })?;
            match journal_operation(journal)?.value.kind() {
                Some(kind) => CanonicalOperationOutcome::recover_v03(kind, rejection)
                    .map_err(|error| TransactionError::InvalidJournal(error.to_string())),
                None => Ok(legacy_recovered_v03(rejection)),
            }
        }
        _ => Err(TransactionError::InvalidJournal(
            "unsupported runtime journal version".to_string(),
        )),
    }
}

/// Construct an ambiguous v0.3 envelope only inside the transaction runtime's
/// version-2 compatibility path. Checked serde and v3 persistence reject it.
fn legacy_recovered_v03(result: OperationResult) -> CanonicalOperationOutcome {
    CanonicalOperationOutcome {
        valid: result.valid,
        diagnostics: result.diagnostics.iter().cloned().map(Into::into).collect(),
        value: CanonicalOperationValue::LegacyRecoveredV03(
            LegacyRecoveredV03Value::from_transaction_recovery(result),
        ),
    }
}

/// Version-2 journals did not persist an operation discriminator. This is the
/// sole backward-read compatibility path; version-3 journals never use it.
fn legacy_operation_kind(
    journal: &RuntimeJournal,
    result: &OperationResult,
) -> Option<OperationKind> {
    if journal.scope == TransactionScope::Resources {
        return None;
    }
    if result.result.get("operations").is_some() {
        return Some(OperationKind::Batch);
    }
    if result.result.get("from").is_some() && result.result.get("to").is_some() {
        return Some(OperationKind::Rename);
    }
    if result.result.get("deleted").is_some() || result.result.get("would_delete").is_some() {
        return Some(OperationKind::Delete);
    }
    let mut kinds = journal.changes.iter().filter_map(|change| match change {
        CanonicalChange::Record(change) => Some(change.kind),
        CanonicalChange::Resource(_) => None,
    });
    let first = kinds.next()?;
    if kinds.any(|kind| kind != first) {
        return None;
    }
    match first {
        crate::runtime::RecordChangeKind::Created => Some(OperationKind::Create),
        crate::runtime::RecordChangeKind::Updated => Some(OperationKind::Update),
        crate::runtime::RecordChangeKind::Deleted => Some(OperationKind::Delete),
        crate::runtime::RecordChangeKind::Renamed => Some(OperationKind::Rename),
    }
}

fn runtime_journal_version(value: &serde_json::Value) -> bool {
    matches!(
        value.get("version").and_then(serde_json::Value::as_u64),
        Some(version) if version == u64::from(LEGACY_RUNTIME_JOURNAL_VERSION)
            || version == u64::from(RUNTIME_JOURNAL_VERSION)
    )
}

fn decode_runtime_journal(
    mut value: serde_json::Value,
) -> Result<RuntimeJournal, TransactionError> {
    recover_phase4_definition_discriminators(&mut value);
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            TransactionError::InvalidJournal("runtime journal version missing".into())
        })?;
    let phase = value
        .get("phase")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| TransactionError::InvalidJournal("runtime journal phase missing".into()))?;
    let present = |field: &str| value.get(field).is_some_and(|item| !item.is_null());
    let canonical = present("operation_outcome");
    let legacy = present("operation_result");
    let canonical_rejection = present("operation_rejection");
    let legacy_rejection = present("rejection");
    let rejected = phase == "rejected_before_commit";

    let valid_shape = match u32::try_from(version).ok() {
        Some(LEGACY_RUNTIME_JOURNAL_VERSION) => {
            legacy && !canonical && !canonical_rejection && legacy_rejection == rejected
        }
        Some(RUNTIME_JOURNAL_VERSION) => {
            canonical && !legacy && !legacy_rejection && canonical_rejection == rejected
        }
        _ => false,
    };
    if !valid_shape {
        return Err(TransactionError::InvalidJournal(
            "runtime journal outcome fields do not match its version and phase".to_string(),
        ));
    }
    serde_json::from_value(value)
        .map_err(|error| TransactionError::InvalidJournal(error.to_string()))
}

/// Phase 4 persisted definition families without their assess/apply
/// discriminator. A durable resource mutation provides sufficient context to
/// recover only the apply form; generic public outcome serde never guesses.
fn recover_phase4_definition_discriminators(value: &mut serde_json::Value) {
    if value.get("version").and_then(serde_json::Value::as_u64)
        != Some(u64::from(RUNTIME_JOURNAL_VERSION))
        || value.get("scope").and_then(serde_json::Value::as_str) != Some("resources")
        || !value
            .get("changes")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|changes| {
                !changes.is_empty()
                    && changes.iter().all(|change| {
                        change.get("target").and_then(serde_json::Value::as_str) == Some("resource")
                    })
            })
    {
        return;
    }
    for field in ["operation_outcome", "operation_rejection"] {
        let Some(discriminator) = value
            .get_mut(field)
            .and_then(|outcome| outcome.get_mut("value"))
            .and_then(|outcome| outcome.get_mut("operation"))
        else {
            continue;
        };
        match discriminator.as_str() {
            Some("type_pack") => *discriminator = serde_json::json!("apply_type_pack"),
            Some("collection_setup") => {
                *discriminator = serde_json::json!("apply_collection_setup")
            }
            _ => {}
        }
    }
}

fn conflict_result(
    journal: &RuntimeJournal,
    path: &str,
) -> Result<CanonicalOperationOutcome, TransactionError> {
    let kind = journal_operation(journal)?.value.kind().ok_or_else(|| {
        TransactionError::InvalidJournal(
            "a version-3 prepared transaction has no semantic operation identity".to_string(),
        )
    })?;
    Ok(CanonicalOperationOutcome::invalid(
        kind,
        vec![crate::api::Diagnostic {
            severity: crate::api::Severity::Error,
            code: crate::api::DiagnosticCode::new("concurrent_modification"),
            message: format!("Collection changed after mutation preparation at '{path}'."),
            path: Some(path.to_string()),
            field: None,
            type_name: None,
            schema_location: None,
            details: None,
        }],
    ))
}

fn commit_id(journal: &RuntimeJournal) -> CommitId {
    CommitId::from_stored(journal.id.clone())
}

fn transaction_directories(collection: &Collection) -> Result<Vec<PathBuf>, TransactionError> {
    collection
        .held_root()
        .child_directories(Path::new(TRANSACTIONS_DIR))
        .map_err(|source| io_error(collection.root.join(TRANSACTIONS_DIR), source))
}

fn transaction_directory(_collection: &Collection, id: &CommitId) -> PathBuf {
    PathBuf::from(TRANSACTIONS_DIR).join(id.as_str())
}

fn read_runtime_journal(
    collection: &Collection,
    directory: &Path,
) -> Result<RuntimeJournal, TransactionError> {
    let path = directory.join(JOURNAL_FILE);
    let bytes = collection
        .held_root()
        .read(&path)
        .map_err(|source| io_error(collection.root.join(&path), source))?;
    let value = serde_json::from_slice(&bytes)
        .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
    let journal = decode_runtime_journal(value)?;
    validate_journal(collection, directory, &journal)?;
    Ok(journal)
}

fn persist_runtime_journal(
    collection: &Collection,
    directory: &Path,
    journal: &RuntimeJournal,
) -> Result<(), TransactionError> {
    validate_journal(collection, directory, journal)?;
    let bytes = serde_json::to_vec_pretty(journal)
        .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
    let path = directory.join(JOURNAL_FILE);
    collection
        .held_root()
        .atomic_write(&path, &bytes)
        .map_err(|source| io_error(collection.root.join(path), source))
}

fn context_check(context: &OperationContext) -> Result<(), TransactionError> {
    context
        .check()
        .map_err(|error| TransactionError::OperationBoundary { code: error.code() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        CanonicalFieldChangeSet, CanonicalTypeSet, RecordChange, RecordChangeKind,
    };
    use std::collections::BTreeMap;

    fn collection() -> (tempfile::TempDir, Collection) {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        fs::write(root.path().join("a.md"), "old-a\n").unwrap();
        let collection = Collection::open(root.path()).unwrap();
        (root, collection)
    }

    fn change(path: &str, before: &[u8], after: &[u8]) -> ChangeBatch {
        ChangeBatch::new(vec![CanonicalChange::Record(RecordChange {
            kind: RecordChangeKind::Updated,
            path: crate::api::CollectionPath::new(path).unwrap(),
            from: None,
            before_revision: Some(
                crate::api::Revision::parse(crate::v03::revision(before)).unwrap(),
            ),
            after_revision: Some(crate::api::Revision::parse(crate::v03::revision(after)).unwrap()),
            before_types: CanonicalTypeSet::new([]),
            after_types: CanonicalTypeSet::new([]),
            changed_fields: CanonicalFieldChangeSet::new([]).unwrap(),
            body_changed: true,
        })])
        .unwrap()
    }

    fn prepare(collection: &Collection, path: &str, before: &[u8], after: &[u8]) -> CommitId {
        let baseline: FileBaseline = BTreeMap::from([(path.to_string(), before.to_vec())]);
        let desired: FileBaseline = BTreeMap::from([(path.to_string(), after.to_vec())]);
        let claim = HostClaimId::generate();
        let event_id = ChangeEventId::generate();
        let changes = change(path, before, after);
        let operation = CanonicalOperationOutcome::invalid(OperationKind::Update, Vec::new());
        let outcome = prepare_runtime_transaction(
            collection,
            RuntimePrepareInput {
                baseline: &baseline,
                desired: &desired,
                claim: &claim,
                mutation_digest: claim.as_str(),
                operation: &operation,
                changes: &changes,
                event_id: &event_id,
            },
            &OperationContext::legacy(),
        )
        .unwrap();
        match outcome {
            RuntimePrepareOutcome::Prepared(id) => id,
            other => panic!("expected a prepared transaction, got {other:?}"),
        }
    }

    fn commit(collection: &Collection, id: &CommitId) -> RuntimeCommitAttempt {
        commit_runtime_prepared(
            collection,
            id,
            &CollectionGeneration::initial(),
            ChangeWatermark::from_stored(1),
            &OperationContext::legacy(),
        )
        .unwrap()
    }

    fn journal_value(collection: &Collection, id: &CommitId) -> serde_json::Value {
        let directory = transaction_directory(collection, id);
        serde_json::from_slice(
            &collection
                .held_root()
                .read(directory.join(JOURNAL_FILE))
                .unwrap(),
        )
        .unwrap()
    }

    fn write_journal_value(collection: &Collection, id: &CommitId, value: &serde_json::Value) {
        let directory = transaction_directory(collection, id);
        collection
            .held_root()
            .atomic_write(
                &directory.join(JOURNAL_FILE),
                &serde_json::to_vec_pretty(value).unwrap(),
            )
            .unwrap();
    }

    #[test]
    fn malicious_version_and_phase_outcome_field_combinations_fail_closed() {
        let (_root, collection) = collection();
        let id = prepare(&collection, "a.md", b"old-a\n", b"new-a\n");
        let canonical = journal_value(&collection, &id);
        let legacy_result = serde_json::to_value(
            serde_json::from_value::<CanonicalOperationOutcome>(
                canonical["operation_outcome"].clone(),
            )
            .unwrap()
            .to_v03(),
        )
        .unwrap();

        let mut fixtures = Vec::new();
        let mut v3_with_legacy = canonical.clone();
        v3_with_legacy["operation_result"] = legacy_result.clone();
        fixtures.push(v3_with_legacy);
        let mut v2_with_canonical = canonical.clone();
        v2_with_canonical["version"] = serde_json::json!(LEGACY_RUNTIME_JOURNAL_VERSION);
        v2_with_canonical["operation_result"] = legacy_result;
        fixtures.push(v2_with_canonical);
        let mut prepared_with_rejection = canonical.clone();
        prepared_with_rejection["operation_rejection"] =
            prepared_with_rejection["operation_outcome"].clone();
        fixtures.push(prepared_with_rejection);
        let mut rejected_without_rejection = canonical;
        rejected_without_rejection["phase"] = serde_json::json!("rejected_before_commit");
        fixtures.push(rejected_without_rejection);

        for fixture in fixtures {
            assert!(decode_runtime_journal(fixture).is_err());
        }
    }

    #[test]
    fn malicious_v3_operation_change_and_rejection_families_fail_closed() {
        let (_root, collection) = collection();
        let id = prepare(&collection, "a.md", b"old-a\n", b"new-a\n");
        let original = journal_value(&collection, &id);
        let directory = transaction_directory(&collection, &id);

        let mut wrong_change_family = original.clone();
        wrong_change_family["operation_outcome"]["value"]["operation"] =
            serde_json::json!("delete");
        write_journal_value(&collection, &id, &wrong_change_family);
        assert!(read_runtime_journal(&collection, &directory).is_err());

        let mut wire = original.clone();
        wire["operation_outcome"] = serde_json::to_value(
            CanonicalOperationOutcome::validation_wire(OperationResult {
                valid: true,
                result: serde_json::json!({"valid": true}),
                diagnostics: Vec::new(),
            }),
        )
        .unwrap();
        write_journal_value(&collection, &id, &wire);
        assert!(read_runtime_journal(&collection, &directory).is_err());

        let mut cursor = original.clone();
        cursor["operation_outcome"] =
            serde_json::to_value(CanonicalOperationOutcome::cursor_release(
                crate::runtime::CursorReleaseOutcome { released: true },
            ))
            .unwrap();
        write_journal_value(&collection, &id, &cursor);
        assert!(read_runtime_journal(&collection, &directory).is_err());

        let mut rejection = original;
        rejection["phase"] = serde_json::json!("rejected_before_commit");
        rejection["operation_rejection"] = rejection["operation_outcome"].clone();
        rejection["operation_rejection"]["value"]["operation"] = serde_json::json!("delete");
        write_journal_value(&collection, &id, &rejection);
        assert!(read_runtime_journal(&collection, &directory).is_err());
    }

    #[test]
    fn definition_journals_preserve_apply_and_reject_assess_or_swapped_rejection() {
        let (_root, collection) = collection();
        let id = prepare(&collection, "a.md", b"old-a\n", b"new-a\n");
        let directory = transaction_directory(&collection, &id);
        let mut journal = read_runtime_journal(&collection, &directory).unwrap();
        journal.scope = TransactionScope::Resources;
        journal.changes = vec![CanonicalChange::Resource(crate::runtime::ResourceChange {
            kind: ResourceChangeKind::Other,
            path: crate::api::CollectionPath::new("resource.bin").unwrap(),
            before_revision: None,
            after_revision: crate::api::Revision::parse("sha256:after").ok(),
        })];
        let definition = |kind| {
            CanonicalOperationOutcome::recover_v03(
                kind,
                OperationResult {
                    valid: false,
                    result: serde_json::json!({}),
                    diagnostics: Vec::new(),
                },
            )
            .unwrap()
        };
        journal.operation_outcome = Some(definition(OperationKind::ApplyTypePack));
        assert!(validate_journal_operation_family(&journal).is_ok());
        journal.operation_outcome = Some(definition(OperationKind::AssessTypePack));
        assert!(validate_journal_operation_family(&journal).is_err());
        journal.operation_outcome = Some(definition(OperationKind::ApplyTypePack));
        journal.operation_rejection = Some(definition(OperationKind::AssessTypePack));
        assert!(validate_journal_operation_family(&journal).is_err());

        let mut phase4 = journal_value(&collection, &id);
        phase4["scope"] = serde_json::json!("resources");
        phase4["changes"] = serde_json::json!([{
            "target": "resource",
            "change": {
                "kind": "other", "path": "resource.bin",
                "before_revision": null, "after_revision": "sha256:after"
            }
        }]);
        phase4["operation_outcome"] =
            serde_json::to_value(definition(OperationKind::ApplyCollectionSetup)).unwrap();
        phase4["operation_outcome"]["value"]["operation"] = serde_json::json!("collection_setup");
        let recovered = decode_runtime_journal(phase4).unwrap();
        assert_eq!(
            recovered.operation_outcome.unwrap().operation_kind(),
            Some(OperationKind::ApplyCollectionSetup)
        );
    }

    #[test]
    fn version_two_identity_uses_exact_evidence_and_never_resource_path_guessing() {
        let (_root, collection) = collection();
        let id = prepare(&collection, "a.md", b"old-a\n", b"new-a\n");
        let directory = transaction_directory(&collection, &id);
        let mut journal = read_runtime_journal(&collection, &directory).unwrap();
        let empty = OperationResult {
            valid: true,
            result: serde_json::json!({}),
            diagnostics: Vec::new(),
        };
        assert_eq!(
            legacy_operation_kind(&journal, &empty),
            Some(OperationKind::Update)
        );

        journal.changes = vec![CanonicalChange::Record(RecordChange {
            kind: RecordChangeKind::Created,
            path: crate::api::CollectionPath::new("a.md").unwrap(),
            from: None,
            before_revision: None,
            after_revision: crate::api::Revision::parse("sha256:after").ok(),
            before_types: CanonicalTypeSet::new([]),
            after_types: CanonicalTypeSet::new([]),
            changed_fields: CanonicalFieldChangeSet::new([]).unwrap(),
            body_changed: true,
        })];
        assert_eq!(
            legacy_operation_kind(&journal, &empty),
            Some(OperationKind::Create)
        );
        for (result, expected) in [
            (
                serde_json::json!({"path":"a.md","deleted":true}),
                OperationKind::Delete,
            ),
            (
                serde_json::json!({"from":"a.md","to":"b.md"}),
                OperationKind::Rename,
            ),
            (serde_json::json!({"operations":[]}), OperationKind::Batch),
        ] {
            assert_eq!(
                legacy_operation_kind(
                    &journal,
                    &OperationResult {
                        valid: true,
                        result,
                        diagnostics: Vec::new()
                    },
                ),
                Some(expected),
            );
        }
        journal.scope = TransactionScope::Resources;
        assert_eq!(legacy_operation_kind(&journal, &empty), None);
        journal.scope = TransactionScope::Records;
        journal.changes.clear();
        assert_eq!(legacy_operation_kind(&journal, &empty), None);
        assert!(matches!(
            legacy_recovered_v03(empty).value,
            crate::runtime::CanonicalOperationValue::LegacyRecoveredV03(_)
        ));
    }

    #[test]
    fn malformed_version_three_outcome_fails_closed() {
        let (_root, collection) = collection();
        let id = prepare(&collection, "a.md", b"old-a\n", b"new-a\n");
        let directory = transaction_directory(&collection, &id);
        let mut value: serde_json::Value = serde_json::from_slice(
            &collection
                .held_root()
                .read(directory.join(JOURNAL_FILE))
                .unwrap(),
        )
        .unwrap();
        value["operation_outcome"]["valid"] = serde_json::json!(true);
        collection
            .held_root()
            .atomic_write(
                &directory.join(JOURNAL_FILE),
                &serde_json::to_vec_pretty(&value).unwrap(),
            )
            .unwrap();

        assert!(matches!(
            resolve_runtime_commit(&collection, &id, &OperationContext::legacy()),
            Err(TransactionError::InvalidJournal(message))
                if message.contains("requires a semantic value")
        ));
    }

    #[test]
    fn version_two_operation_result_journal_is_read_as_typed_outcome() {
        let (_root, collection) = collection();
        let id = prepare(&collection, "a.md", b"old-a\n", b"new-a\n");
        let directory = transaction_directory(&collection, &id);
        let mut value: serde_json::Value = serde_json::from_slice(
            &collection
                .held_root()
                .read(directory.join(JOURNAL_FILE))
                .unwrap(),
        )
        .unwrap();
        assert!(value.get("operation_outcome").is_some());
        assert!(value.get("operation_result").is_none());
        let v3_outcome = value["operation_outcome"].clone();
        let typed: CanonicalOperationOutcome = serde_json::from_value(v3_outcome.clone()).unwrap();
        assert_eq!(serde_json::to_value(&typed).unwrap(), v3_outcome);
        let expected_v03 = typed.to_v03();
        value["version"] = serde_json::json!(LEGACY_RUNTIME_JOURNAL_VERSION);
        value["operation_result"] = serde_json::to_value(typed.to_v03()).unwrap();
        value.as_object_mut().unwrap().remove("operation_outcome");
        collection
            .held_root()
            .atomic_write(
                &directory.join(JOURNAL_FILE),
                &serde_json::to_vec_pretty(&value).unwrap(),
            )
            .unwrap();

        let resolved = resolve_runtime_commit(&collection, &id, &OperationContext::legacy())
            .unwrap()
            .unwrap();
        assert!(matches!(resolved, RuntimeResolution::Prepared { .. }));
        let recovered =
            journal_operation(&read_runtime_journal(&collection, &directory).unwrap()).unwrap();
        assert!(matches!(
            recovered.value,
            crate::runtime::CanonicalOperationValue::Update(None)
        ));
        assert_eq!(recovered.to_v03(), expected_v03);
    }

    /// Settlement runs after the commit lock is released, so for a window the
    /// working tree still shows the old revision of a path that is already
    /// committed. A second writer that checked only the working tree committed
    /// too, and then could not settle: its path matched neither its before nor
    /// its intended revision, which was reported as `manual_recovery_required`
    /// and stranded the journal so every later open of the collection failed.
    #[test]
    fn a_committed_but_unsettled_path_rejects_the_next_writer_before_its_commit_point() {
        let (_root, collection) = collection();
        let first = prepare(&collection, "a.md", b"old-a\n", b"new-a\n");
        let second = prepare(&collection, "a.md", b"old-a\n", b"other-a\n");

        // The first writer commits and does not settle: the working tree still
        // reads `old-a`, which is exactly the stale view that misled the second.
        assert!(matches!(
            commit(&collection, &first),
            RuntimeCommitAttempt::SettlementRequired(_)
        ));
        assert_eq!(fs::read(collection.root.join("a.md")).unwrap(), b"old-a\n");

        match commit(&collection, &second) {
            RuntimeCommitAttempt::RejectedBeforeCommit(_) => {}
            other => panic!("the second writer must lose cleanly, got {other:?}"),
        }

        // The loser took no commit point, so the winner still settles and the
        // collection stays openable rather than requiring manual recovery.
        settle_runtime_commit(&collection, &first).unwrap();
        assert_eq!(fs::read(collection.root.join("a.md")).unwrap(), b"new-a\n");
        drop(collection);
        Collection::open(_root.path()).expect("the collection must remain openable");
    }

    /// The rejection is specific to paths a committed transaction owns, so an
    /// unrelated path still commits while settlement is outstanding.
    #[test]
    fn an_unsettled_transaction_does_not_block_an_unrelated_path() {
        let (root, collection) = collection();
        fs::write(root.path().join("b.md"), "old-b\n").unwrap();
        let first = prepare(&collection, "a.md", b"old-a\n", b"new-a\n");
        let second = prepare(&collection, "b.md", b"old-b\n", b"new-b\n");
        assert!(matches!(
            commit(&collection, &first),
            RuntimeCommitAttempt::SettlementRequired(_)
        ));
        assert!(matches!(
            commit(&collection, &second),
            RuntimeCommitAttempt::SettlementRequired(_)
        ));
    }
}
