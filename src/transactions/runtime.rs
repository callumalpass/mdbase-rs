use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[cfg(test)]
use super::apply_post_commit_hook;
use super::{
    apply_entry, attach_committed_file_facts, capture_committed_file_facts, cleanup_transaction,
    current_revision, ensure_transaction_root, io_error, read_regular_file, sync_dir,
    validate_entry_path, write_synced, FileBaseline, JournalEntry, StagingGuard, TransactionError,
    TransactionScope, WriteLock, JOURNAL_FILE, TRANSACTIONS_DIR,
};
use crate::runtime::{
    CanonicalChange, ChangeBatch, ChangeBatchDescriptor, ChangeEventId, ChangeWatermark,
    CollectionGeneration, CommitId, HostClaimId, OperationContext,
};
use crate::{diagnostic::Diagnostic, v03::OperationResult, Collection};

const RUNTIME_JOURNAL_VERSION: u32 = 2;
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
    operation_result: OperationResult,
    change_descriptor: ChangeBatchDescriptor,
    changes: Vec<CanonicalChange>,
    event_id: ChangeEventId,
    generation: Option<CollectionGeneration>,
    watermark: Option<ChangeWatermark>,
    rejection: Option<OperationResult>,
    resolution_acked: bool,
    event_acked: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RuntimePrepareOutcome {
    NoMutation(OperationResult),
    Prepared(CommitId),
    Existing(RuntimeResolution),
}

pub(crate) struct RuntimePrepareInput<'a> {
    pub baseline: &'a FileBaseline,
    pub desired: &'a FileBaseline,
    pub claim: &'a HostClaimId,
    pub mutation_digest: &'a str,
    pub result: &'a OperationResult,
    pub changes: &'a ChangeBatch,
    pub event_id: &'a ChangeEventId,
}

#[derive(Debug)]
pub(crate) enum RuntimeCommitAttempt {
    Committed(RuntimeResolution),
    RejectedBeforeCommit(RuntimeResolution),
    AlreadyCancelled,
    SettlementRequired(RuntimeSettlement),
    SettlementPending(RuntimeSettlement),
    NeedsManualRecovery(CommitId),
}

pub(crate) struct RuntimeSettlement {
    commit_id: CommitId,
    write_lock: Option<WriteLock>,
}

impl RuntimeSettlement {
    pub(crate) fn commit_id(&self) -> &CommitId {
        &self.commit_id
    }
}

impl std::fmt::Debug for RuntimeSettlement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeSettlement")
            .field("commit_id", &self.commit_id)
            .finish_non_exhaustive()
    }
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
        result: OperationResult,
        generation: CollectionGeneration,
        watermark: ChangeWatermark,
        event_id: ChangeEventId,
        changes: ChangeBatch,
    },
    RejectedBeforeCommit {
        commit_id: CommitId,
        rejection: OperationResult,
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
        result,
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
        return Ok(RuntimePrepareOutcome::NoMutation(result.clone()));
    }

    let id = uuid::Uuid::new_v4().simple().to_string();
    let directory = collection.root.join(TRANSACTIONS_DIR).join(&id);
    fs::create_dir_all(directory.join("stage"))
        .map_err(|source| io_error(directory.join("stage"), source))?;
    let mut staging = StagingGuard {
        directory: directory.clone(),
        durable: false,
    };
    fs::create_dir_all(directory.join("backup"))
        .map_err(|source| io_error(directory.join("backup"), source))?;

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
                write_synced(&directory.join(&name), bytes)?;
                Some(name)
            }
            None => None,
        };
        let backup_file = match before {
            Some(bytes) => {
                let name = format!("backup/{index}");
                write_synced(&directory.join(&name), bytes)?;
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
    sync_dir(&directory.join("stage"))?;
    sync_dir(&directory.join("backup"))?;
    let journal = RuntimeJournal {
        version: RUNTIME_JOURNAL_VERSION,
        id: id.clone(),
        scope,
        phase: RuntimePhase::Prepared,
        applied: 0,
        entries,
        host_claim: claim.as_str().to_string(),
        mutation_digest: mutation_digest.to_string(),
        operation_result: result.clone(),
        change_descriptor: changes.descriptor().clone(),
        changes: changes.items().to_vec(),
        event_id: event_id.clone(),
        generation: None,
        watermark: None,
        rejection: None,
        resolution_acked: false,
        event_acked: false,
    };
    persist_runtime_journal(&directory, &journal)?;
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
    let write_lock = WriteLock::acquire_context(collection, context)?;
    context_check(context)?;
    let directory = transaction_directory(collection, id);
    let mut journal = read_runtime_journal(&directory)?;
    match journal.phase {
        RuntimePhase::Prepared => {}
        RuntimePhase::Committing => {
            return Ok(RuntimeCommitAttempt::SettlementPending(RuntimeSettlement {
                commit_id: id.clone(),
                write_lock: Some(write_lock),
            }))
        }
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
        journal.rejection = Some(conflict_result(&path));
        release_payloads(&directory, &mut journal);
        persist_runtime_journal(&directory, &journal)?;
        return Ok(RuntimeCommitAttempt::RejectedBeforeCommit(resolution(
            &journal,
        )?));
    }

    journal.generation = Some(generation.clone());
    journal.watermark = Some(watermark);
    journal.phase = RuntimePhase::Committing;
    persist_runtime_journal(&directory, &journal)?;
    simulate_runtime_crash(&journal.id, 1)?;
    Ok(RuntimeCommitAttempt::SettlementRequired(
        RuntimeSettlement {
            commit_id: id.clone(),
            write_lock: Some(write_lock),
        },
    ))
}

pub(crate) fn settle_runtime_commit(
    collection: &Collection,
    settlement: &mut RuntimeSettlement,
) -> Result<RuntimeResolution, TransactionError> {
    let directory = transaction_directory(collection, settlement.commit_id());
    let mut journal = read_runtime_journal(&directory)?;
    let result = match journal.phase {
        RuntimePhase::Committing => match settle(collection, &directory, &mut journal) {
            Ok(()) => resolution(&journal),
            Err(error) => {
                #[cfg(test)]
                if matches!(error, TransactionError::SimulatedCrash) {
                    settlement.write_lock.take();
                    return Err(error);
                }
                journal.phase = RuntimePhase::NeedsManualRecovery;
                let _ = persist_runtime_journal(&directory, &journal);
                Err(error)
            }
        },
        _ => resolution(&journal),
    };
    settlement.write_lock.take();
    result
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
    let mut journal = read_runtime_journal(&directory)?;
    if journal.phase == RuntimePhase::Prepared {
        journal.phase = RuntimePhase::CancelledBeforeCommit;
        release_payloads(&directory, &mut journal);
        persist_runtime_journal(&directory, &journal)?;
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
    if !directory.exists() {
        return Ok(None);
    }
    read_runtime_journal(&directory).and_then(|journal| resolution(&journal).map(Some))
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
    let root = collection.root.join(TRANSACTIONS_DIR);
    let mut directories = match fs::read_dir(&root) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error(root, source)),
    };
    directories.sort();
    let mut resolutions = Vec::new();
    for directory in directories {
        context_check(context)?;
        let path = directory.join(JOURNAL_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(io_error(path, source)),
        };
        let value = serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
        if value.get("version").and_then(serde_json::Value::as_u64) != Some(2) {
            continue;
        }
        let journal: RuntimeJournal = serde_json::from_value(value)
            .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
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
    let root = collection.root.join(TRANSACTIONS_DIR);
    let mut directories = match fs::read_dir(&root) {
        Ok(entries) => entries
            .map(|entry| {
                entry
                    .map(|entry| entry.path())
                    .map_err(|source| io_error(root.clone(), source))
            })
            .collect::<Result<Vec<_>, _>>()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_error(root, source)),
    };
    directories.sort();
    for directory in directories {
        let metadata = fs::symlink_metadata(&directory)
            .map_err(|source| io_error(directory.clone(), source))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(TransactionError::ManualRecovery(format!(
                "'{}' is not a regular transaction directory",
                directory.display()
            )));
        }
        let journal_path = directory.join(JOURNAL_FILE);
        let bytes =
            fs::read(&journal_path).map_err(|source| io_error(journal_path.clone(), source))?;
        let version = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| value.get("version").and_then(serde_json::Value::as_u64));
        if version != Some(u64::from(RUNTIME_JOURNAL_VERSION)) {
            return Err(TransactionError::ManualRecovery(format!(
                "non-runtime transaction '{}' remained after collection recovery",
                directory.display()
            )));
        }
        let mut journal: RuntimeJournal = serde_json::from_slice(&bytes)
            .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
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
        fs::remove_dir_all(&directory).map_err(|source| io_error(directory.clone(), source))?;
    }
    sync_dir(&root)?;
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
    if !directory.exists() {
        return Ok(());
    }
    let mut journal = read_runtime_journal(&directory)?;
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
        cleanup_transaction(&directory);
    } else {
        persist_runtime_journal(&directory, &journal)?;
    }
    Ok(())
}

pub(super) fn recover_runtime_one(
    collection: &Collection,
    directory: &Path,
    bytes: &[u8],
) -> Result<bool, TransactionError> {
    let mut journal: RuntimeJournal = serde_json::from_slice(bytes)
        .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
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
                let _ = persist_runtime_journal(directory, &journal);
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
        let path = crate::api::CollectionPath::new(&entry.path)?.under(&collection.root);
        let current = current_revision(&path)?;
        if current == entry.after_revision {
            if journal.applied < index + 1 {
                journal.applied = index + 1;
                persist_runtime_journal(directory, journal)?;
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
        persist_runtime_journal(directory, journal)?;
        simulate_runtime_crash(&journal.id, 3)?;
    }
    let file_facts = capture_committed_file_facts(collection, &journal.entries)?;
    attach_committed_file_facts(&mut journal.operation_result.result, &file_facts);
    journal.phase = RuntimePhase::Committed;
    persist_runtime_journal(directory, journal)?;
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
        let path = crate::api::CollectionPath::new(&entry.path)?.under(&collection.root);
        if current_revision(&path)? != entry.before_revision {
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
    let root = collection.root.join(TRANSACTIONS_DIR);
    let mut directories = match fs::read_dir(&root) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error(root, source)),
    };
    directories.sort();
    let claimed = journal
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>();
    for directory in directories {
        if directory.file_name().and_then(|name| name.to_str()) == Some(journal.id.as_str()) {
            continue;
        }
        let bytes = match fs::read(directory.join(JOURNAL_FILE)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(io_error(directory.join(JOURNAL_FILE), source)),
        };
        let value = serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
        if value.get("version").and_then(serde_json::Value::as_u64) != Some(2) {
            continue;
        }
        let other: RuntimeJournal = serde_json::from_value(value)
            .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
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
    if journal.version != RUNTIME_JOURNAL_VERSION
        || directory.file_name().and_then(|name| name.to_str()) != Some(&journal.id)
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
            let staged = read_regular_file(&directory.join(stage_file))?;
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

fn ensure_capacity(collection: &Collection, changes: &ChangeBatch) -> Result<(), TransactionError> {
    if changes.descriptor().count > MAX_RUNTIME_CHANGE_ITEMS {
        return Err(TransactionError::RuntimeCapacityExhausted);
    }
    let metadata = serde_json::to_vec(changes.items())
        .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
    if metadata.len() > MAX_RUNTIME_METADATA_BYTES {
        return Err(TransactionError::RuntimeCapacityExhausted);
    }
    let root = collection.root.join(TRANSACTIONS_DIR);
    let active = match fs::read_dir(&root) {
        Ok(entries) => entries.filter_map(Result::ok).count(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(source) => return Err(io_error(root, source)),
    };
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
    let root = collection.root.join(TRANSACTIONS_DIR);
    let mut directories = match fs::read_dir(&root) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error(root, source)),
    };
    directories.sort();
    for directory in directories {
        let bytes = match fs::read(directory.join(JOURNAL_FILE)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(io_error(directory.join(JOURNAL_FILE), source)),
        };
        let value = serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
        if value.get("version").and_then(serde_json::Value::as_u64) != Some(2) {
            continue;
        }
        let journal: RuntimeJournal = serde_json::from_value(value)
            .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
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
            result: journal.operation_result.clone(),
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
            rejection: journal.rejection.clone().ok_or_else(|| {
                TransactionError::InvalidJournal("commit rejection missing".to_string())
            })?,
        },
        RuntimePhase::CancelledBeforeCommit => {
            RuntimeResolution::CancelledBeforeCommit { commit_id }
        }
        RuntimePhase::NeedsManualRecovery => RuntimeResolution::NeedsManualRecovery { commit_id },
    })
}

fn release_payloads(directory: &Path, journal: &mut RuntimeJournal) {
    let _ = fs::remove_dir_all(directory.join("stage"));
    let _ = fs::remove_dir_all(directory.join("backup"));
    journal.changes.clear();
    journal.change_descriptor = ChangeBatch::new(Vec::new())
        .expect("an empty change batch is valid")
        .descriptor()
        .clone();
    for entry in &mut journal.entries {
        entry.stage_file = None;
        entry.backup_file = None;
    }
    journal.entries.clear();
}

fn conflict_result(path: &str) -> OperationResult {
    OperationResult {
        valid: false,
        result: serde_json::json!({}),
        diagnostics: vec![Diagnostic::error(
            "concurrent_modification",
            format!("Collection changed after mutation preparation at '{path}'."),
            Some(path.to_string()),
        )],
    }
}

fn commit_id(journal: &RuntimeJournal) -> CommitId {
    CommitId::from_stored(journal.id.clone())
}

fn transaction_directory(collection: &Collection, id: &CommitId) -> PathBuf {
    collection.root.join(TRANSACTIONS_DIR).join(id.as_str())
}

fn read_runtime_journal(directory: &Path) -> Result<RuntimeJournal, TransactionError> {
    let path = directory.join(JOURNAL_FILE);
    let bytes = fs::read(&path).map_err(|source| io_error(path, source))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| TransactionError::InvalidJournal(error.to_string()))
}

fn persist_runtime_journal(
    directory: &Path,
    journal: &RuntimeJournal,
) -> Result<(), TransactionError> {
    let bytes = serde_json::to_vec_pretty(journal)
        .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
    let path = directory.join(JOURNAL_FILE);
    crate::operations::atomic_write(&path, &bytes).map_err(|source| io_error(path, source))
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
        CanonicalFieldChangeSet, CanonicalTypeSet, OperationDeadline, RecordChange,
        RecordChangeKind,
    };
    use crate::OperationCancellation;
    use std::collections::BTreeMap;
    use std::time::Duration;

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
        let result = crate::v03::OperationResult {
            valid: true,
            result: serde_json::json!({}),
            diagnostics: Vec::new(),
        };
        let outcome = prepare_runtime_transaction(
            collection,
            RuntimePrepareInput {
                baseline: &baseline,
                desired: &desired,
                claim: &claim,
                mutation_digest: claim.as_str(),
                result: &result,
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

    fn short_context(cancellation: &OperationCancellation) -> OperationContext {
        OperationContext::new(
            cancellation,
            OperationDeadline::after(Duration::from_millis(25)),
        )
    }

    /// A durable commit keeps the cross-process write boundary until canonical
    /// settlement finishes. Otherwise two conditional writers can both take a
    /// commit point against the same old revision and leave one journal needing
    /// manual recovery.
    #[test]
    fn a_committed_but_unsettled_path_blocks_the_next_writer_before_its_commit_point() {
        let (root, collection) = collection();
        let first = prepare(&collection, "a.md", b"old-a\n", b"new-a\n");
        let second = prepare(&collection, "a.md", b"old-a\n", b"other-a\n");

        let mut settlement = match commit(&collection, &first) {
            RuntimeCommitAttempt::SettlementRequired(settlement) => settlement,
            other => panic!("the first writer must take the commit point, got {other:?}"),
        };
        assert_eq!(fs::read(collection.root.join("a.md")).unwrap(), b"old-a\n");

        let cancellation = OperationCancellation::new();
        let blocked = commit_runtime_prepared(
            &collection,
            &second,
            &CollectionGeneration::initial(),
            ChangeWatermark::from_stored(1),
            &short_context(&cancellation),
        );
        assert!(matches!(
            blocked,
            Err(TransactionError::OperationBoundary {
                code: "operation_deadline"
            })
        ));

        settle_runtime_commit(&collection, &mut settlement).unwrap();
        match commit(&collection, &second) {
            RuntimeCommitAttempt::RejectedBeforeCommit(_) => {}
            other => panic!("the stale second writer must lose cleanly, got {other:?}"),
        }
        assert_eq!(fs::read(collection.root.join("a.md")).unwrap(), b"new-a\n");
        drop(collection);
        Collection::open(root.path()).expect("the collection must remain openable");
    }

    /// The collection write boundary also serializes unrelated paths while a
    /// live commit settles; once settlement finishes, the unrelated writer can
    /// proceed normally.
    #[test]
    fn a_live_settlement_temporarily_blocks_an_unrelated_path() {
        let (root, collection) = collection();
        fs::write(root.path().join("b.md"), "old-b\n").unwrap();
        let first = prepare(&collection, "a.md", b"old-a\n", b"new-a\n");
        let second = prepare(&collection, "b.md", b"old-b\n", b"new-b\n");
        let mut first_settlement = match commit(&collection, &first) {
            RuntimeCommitAttempt::SettlementRequired(settlement) => settlement,
            other => panic!("the first writer must take the commit point, got {other:?}"),
        };

        let cancellation = OperationCancellation::new();
        assert!(matches!(
            commit_runtime_prepared(
                &collection,
                &second,
                &CollectionGeneration::initial(),
                ChangeWatermark::from_stored(1),
                &short_context(&cancellation),
            ),
            Err(TransactionError::OperationBoundary {
                code: "operation_deadline"
            })
        ));

        settle_runtime_commit(&collection, &mut first_settlement).unwrap();
        let mut second_settlement = match commit(&collection, &second) {
            RuntimeCommitAttempt::SettlementRequired(settlement) => settlement,
            other => panic!("the unrelated writer must proceed after settlement, got {other:?}"),
        };
        settle_runtime_commit(&collection, &mut second_settlement).unwrap();
        assert_eq!(fs::read(collection.root.join("b.md")).unwrap(), b"new-b\n");
    }
}
