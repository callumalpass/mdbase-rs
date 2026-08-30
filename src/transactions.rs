//! Crash-recoverable multi-file collection transactions.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::api::CollectionPath;
use crate::operations::{
    atomic_create, atomic_write, ensure_no_symlink_components, ensure_safe_relative_path,
};
use crate::runtime::OperationContext;
use crate::{Collection, SpecProfile};

mod runtime;
pub(crate) use runtime::{
    ack_runtime_change_event, ack_runtime_resolution, attach_runtime_prepared,
    cancel_runtime_prepared, commit_runtime_prepared, list_unacked_runtime_events,
    prepare_runtime_transaction, reset_runtime_support_for_fork, resolve_runtime_claim,
    resolve_runtime_commit, settle_runtime_commit, RuntimeCommitAttempt, RuntimePrepareInput,
    RuntimePrepareOutcome, RuntimeResolution,
};
#[cfg(test)]
pub(crate) use runtime::{set_runtime_crash_point, set_runtime_settlement_delay};

pub(crate) type FileBaseline = BTreeMap<String, Vec<u8>>;

#[cfg(test)]
type PostCommitReplacements = BTreeMap<(PathBuf, String), Option<Vec<u8>>>;

#[cfg(test)]
fn post_commit_replacements() -> &'static std::sync::Mutex<PostCommitReplacements> {
    static REPLACEMENTS: std::sync::OnceLock<std::sync::Mutex<PostCommitReplacements>> =
        std::sync::OnceLock::new();
    REPLACEMENTS.get_or_init(Default::default)
}

#[cfg(test)]
fn deferred_cleanup_roots() -> &'static std::sync::Mutex<BTreeSet<PathBuf>> {
    static ROOTS: std::sync::OnceLock<std::sync::Mutex<BTreeSet<PathBuf>>> =
        std::sync::OnceLock::new();
    ROOTS.get_or_init(Default::default)
}

#[cfg(test)]
pub(crate) fn inject_cleanup_deferred(root: &Path) {
    deferred_cleanup_roots()
        .lock()
        .expect("deferred cleanup lock")
        .insert(root.to_path_buf());
}

#[cfg(test)]
pub(crate) fn inject_post_commit_replacement(root: &Path, path: &str, bytes: Option<Vec<u8>>) {
    post_commit_replacements()
        .lock()
        .expect("post-commit replacement lock")
        .insert((root.to_path_buf(), path.to_string()), bytes);
}

#[cfg(test)]
pub(super) fn apply_post_commit_hook(collection: &Collection) -> Result<(), TransactionError> {
    let mut all = post_commit_replacements()
        .lock()
        .expect("post-commit replacement lock");
    let keys = all
        .keys()
        .filter(|(root, _)| root == &collection.root)
        .cloned()
        .collect::<Vec<_>>();
    let replacements = keys
        .into_iter()
        .filter_map(|key| all.remove(&key).map(|value| (key.1, value)))
        .collect::<Vec<_>>();
    drop(all);
    for (path, replacement) in replacements {
        let target = CollectionPath::new(&path)?.under(&collection.root);
        match replacement {
            Some(bytes) => fs::write(target, bytes).expect("injected post-commit replacement"),
            None => fs::remove_file(target).expect("injected post-commit removal"),
        }
    }
    Ok(())
}

const TRANSACTIONS_DIR: &str = ".mdbase/transactions";
const JOURNAL_FILE: &str = "journal.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommittedFileFacts {
    pub size: u64,
    pub mtime: Option<String>,
}

impl CommittedFileFacts {
    pub(crate) fn attach_record_file(&self, file: &mut crate::api::RecordFile) {
        file.size = self.size;
        file.mtime = self.mtime.clone().unwrap_or_default();
    }
}

pub(crate) fn attach_committed_file_facts(
    result: &mut serde_json::Value,
    facts: &BTreeMap<String, CommittedFileFacts>,
) {
    if let Some(operations) = result
        .get_mut("operations")
        .and_then(serde_json::Value::as_array_mut)
    {
        for operation in operations {
            if let Some(item_result) = operation.get_mut("result") {
                attach_committed_file_facts(item_result, facts);
            }
        }
        return;
    }
    let Some(path) = result.get("path").and_then(serde_json::Value::as_str) else {
        return;
    };
    let Some(facts) = facts.get(path) else {
        return;
    };
    let Some(file) = result
        .get_mut("file")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    file.insert("size".to_string(), serde_json::Value::from(facts.size));
    file.insert(
        "mtime".to_string(),
        serde_json::Value::String(facts.mtime.clone().unwrap_or_default()),
    );
}

fn capture_committed_file_facts(
    collection: &Collection,
    entries: &[JournalEntry],
) -> Result<BTreeMap<String, CommittedFileFacts>, TransactionError> {
    let mut facts = BTreeMap::new();
    for entry in entries
        .iter()
        .filter(|entry| entry.after_revision.is_some())
    {
        let path = CollectionPath::new(&entry.path)?.under(&collection.root);
        let file = File::open(&path).map_err(|source| io_error(path.clone(), source))?;
        let metadata = file.metadata().map_err(|source| io_error(path, source))?;
        let mtime = metadata.modified().ok().map(|time| {
            let value: chrono::DateTime<chrono::Utc> = time.into();
            value.format("%Y-%m-%dT%H:%M:%SZ").to_string()
        });
        facts.insert(
            entry.path.clone(),
            CommittedFileFacts {
                size: metadata.len(),
                mtime,
            },
        );
    }
    Ok(facts)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitOutcome {
    pub cleanup_deferred: bool,
    pub file_facts: BTreeMap<String, CommittedFileFacts>,
}

#[derive(Debug, Error)]
pub(crate) enum TransactionError {
    #[error("transaction filesystem error at '{}': {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("transaction journal is invalid: {0}")]
    InvalidJournal(String),
    #[error("transaction path is unsafe: {0}")]
    UnsafePath(String),
    #[error("collection changed after batch preflight: {0}")]
    ConcurrentModification(String),
    #[error("transaction requires manual recovery: {0}")]
    ManualRecovery(String),
    #[error("host mutation claim was reused with different canonical input")]
    ClaimMismatch,
    #[error("runtime transaction metadata capacity is exhausted")]
    RuntimeCapacityExhausted,
    #[error("transaction did not cross its durable boundary: {code}")]
    OperationBoundary { code: &'static str },
    #[cfg(test)]
    #[error("simulated process interruption")]
    SimulatedCrash,
}

impl TransactionError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Io { .. } => "transaction_io_failed",
            Self::InvalidJournal(_) => "transaction_journal_invalid",
            Self::UnsafePath(_) => "path_traversal",
            Self::ConcurrentModification(_) => "concurrent_modification",
            Self::ManualRecovery(_) => "manual_recovery_required",
            Self::ClaimMismatch => "claim_mismatch",
            Self::RuntimeCapacityExhausted => "runtime_capacity_exhausted",
            Self::OperationBoundary { code } => code,
            #[cfg(test)]
            Self::SimulatedCrash => "simulated_crash",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepared,
    Committing,
    Committed,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TransactionScope {
    #[default]
    Records,
    Resources,
    SystemMigration,
}

#[derive(Debug, Deserialize, Serialize)]
struct Journal {
    version: u32,
    id: String,
    #[serde(default)]
    scope: TransactionScope,
    phase: Phase,
    applied: usize,
    entries: Vec<JournalEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct JournalEntry {
    path: String,
    before_revision: Option<String>,
    after_revision: Option<String>,
    stage_file: Option<String>,
    backup_file: Option<String>,
}

/// Stage and commit all changed files between one exact baseline and the
/// successfully preflighted shadow collection.
pub(crate) fn commit_shadow(
    collection: &Collection,
    baseline: &FileBaseline,
    desired: &FileBaseline,
) -> Result<CommitOutcome, TransactionError> {
    commit_shadow_controlled(
        collection,
        baseline,
        desired,
        TransactionScope::Records,
        None,
    )
}

pub(crate) fn commit_migration(
    collection: &Collection,
    baseline: &FileBaseline,
    desired: &FileBaseline,
) -> Result<CommitOutcome, TransactionError> {
    commit_shadow_controlled(
        collection,
        baseline,
        desired,
        TransactionScope::SystemMigration,
        None,
    )
}

fn commit_shadow_controlled(
    collection: &Collection,
    baseline: &FileBaseline,
    desired: &FileBaseline,
    scope: TransactionScope,
    _fail_after_applied: Option<usize>,
) -> Result<CommitOutcome, TransactionError> {
    // Keep transaction directories private until they are durable and retain
    // the same lock through commit and cleanup. Collection recovery takes this
    // lock before discovering journals, so it can never observe staging state
    // or a transaction directory another writer is removing.
    let _write_lock = WriteLock::acquire(collection)?;
    ensure_transaction_root(collection)?;
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

    let paths = baseline
        .keys()
        .chain(desired.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut entries = Vec::new();
    for path in paths {
        let before = baseline.get(&path);
        let after = desired.get(&path);
        if before == after {
            continue;
        }
        validate_entry_path(collection, &path, scope)?;
        let index = entries.len();
        let stage_file = after.map(|bytes| {
            let name = format!("stage/{index}");
            write_synced(&directory.join(&name), bytes)?;
            Ok::<_, TransactionError>(name)
        });
        let stage_file = match stage_file {
            Some(result) => Some(result?),
            None => None,
        };
        let backup_file = before.map(|bytes| {
            let name = format!("backup/{index}");
            write_synced(&directory.join(&name), bytes)?;
            Ok::<_, TransactionError>(name)
        });
        let backup_file = match backup_file {
            Some(result) => Some(result?),
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

    if entries.is_empty() {
        cleanup_transaction(&directory);
        return Ok(CommitOutcome {
            cleanup_deferred: false,
            file_facts: BTreeMap::new(),
        });
    }

    sync_dir(&directory.join("stage"))?;
    sync_dir(&directory.join("backup"))?;
    let mut journal = Journal {
        version: 1,
        id,
        scope,
        phase: Phase::Prepared,
        applied: 0,
        entries,
    };
    persist_journal(&directory, &journal)?;
    staging.durable = true;

    if let Err(error) = recheck_preconditions(collection, &journal) {
        cleanup_transaction(&directory);
        return Err(error);
    }
    journal.phase = Phase::Committing;
    persist_journal(&directory, &journal)?;

    let mut file_facts = BTreeMap::new();
    for index in 0..journal.entries.len() {
        apply_entry(
            collection,
            &directory,
            &journal.entries[index],
            journal.scope,
        )?;
        if journal.entries[index].after_revision.is_some() {
            let entry = &journal.entries[index];
            let path = CollectionPath::new(&entry.path)?.under(&collection.root);
            let file = File::open(&path).map_err(|source| io_error(path.clone(), source))?;
            let metadata = file.metadata().map_err(|source| io_error(path, source))?;
            let mtime = metadata.modified().ok().map(|time| {
                let value: chrono::DateTime<chrono::Utc> = time.into();
                value.format("%Y-%m-%dT%H:%M:%SZ").to_string()
            });
            file_facts.insert(
                entry.path.clone(),
                CommittedFileFacts {
                    size: metadata.len(),
                    mtime,
                },
            );
        }
        journal.applied = index + 1;
        persist_journal(&directory, &journal)?;
        #[cfg(test)]
        if _fail_after_applied == Some(journal.applied) {
            return Err(TransactionError::SimulatedCrash);
        }
    }

    journal.phase = Phase::Committed;
    persist_journal(&directory, &journal)?;
    #[cfg(test)]
    apply_post_commit_hook(collection)?;
    #[cfg(test)]
    let injected_cleanup_deferred = deferred_cleanup_roots()
        .lock()
        .expect("deferred cleanup lock")
        .remove(&collection.root);
    #[cfg(not(test))]
    let injected_cleanup_deferred = false;
    let cleanup_deferred = injected_cleanup_deferred || fs::remove_dir_all(&directory).is_err();
    if !cleanup_deferred {
        let _ = sync_dir(&collection.root.join(TRANSACTIONS_DIR));
    }
    Ok(CommitOutcome {
        cleanup_deferred,
        file_facts,
    })
}

/// Recover every durable transaction before a collection becomes available.
pub(crate) fn recover_pending(collection: &Collection) -> Result<bool, TransactionError> {
    ensure_no_symlink_components(&collection.root, TRANSACTIONS_DIR, SpecProfile::V03)
        .map_err(|error| TransactionError::UnsafePath(error.to_string()))?;
    let root = collection.root.join(TRANSACTIONS_DIR);
    if !root.exists() {
        return Ok(false);
    }
    // Discover transactions only while holding the same lock used to stage,
    // commit, and clean them up. Enumerating before locking leaves a stale path
    // if a concurrent writer completes while recovery is waiting.
    let _write_lock = WriteLock::acquire(collection)?;
    if !root.exists() {
        return Ok(false);
    }
    let mut directories = fs::read_dir(&root)
        .map_err(|source| io_error(root.clone(), source))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|source| io_error(root.clone(), source))
        })
        .collect::<Result<Vec<_>, _>>()?;
    directories.sort();
    let mut changed = false;
    for directory in directories {
        let metadata = fs::symlink_metadata(&directory)
            .map_err(|source| io_error(directory.clone(), source))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(TransactionError::ManualRecovery(format!(
                "'{}' is not a regular transaction directory",
                directory.display()
            )));
        }
        changed |= recover_one(collection, &directory)?;
    }
    Ok(changed)
}

fn recover_one(collection: &Collection, directory: &Path) -> Result<bool, TransactionError> {
    let journal_path = directory.join(JOURNAL_FILE);
    let bytes = fs::read(&journal_path).map_err(|source| io_error(journal_path.clone(), source))?;
    let version = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| value.get("version").and_then(serde_json::Value::as_u64));
    if version == Some(2) {
        return runtime::recover_runtime_one(collection, directory, &bytes);
    }
    let mut journal: Journal = serde_json::from_slice(&bytes)
        .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
    if journal.version != 1
        || directory.file_name().and_then(|name| name.to_str()) != Some(&journal.id)
    {
        return Err(TransactionError::InvalidJournal(format!(
            "identity mismatch in '{}'",
            journal_path.display()
        )));
    }
    if journal.applied > journal.entries.len() {
        return Err(TransactionError::InvalidJournal(
            "applied entry count exceeds journal length".to_string(),
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
    }

    match journal.phase {
        Phase::Prepared if journal.applied == 0 => {
            cleanup_transaction(directory);
            return Ok(true);
        }
        Phase::Committed => {
            cleanup_transaction(directory);
            return Ok(true);
        }
        Phase::Prepared | Phase::Committing => {}
    }
    for entry in &journal.entries {
        if let Some(stage_file) = &entry.stage_file {
            let staged = read_regular_file(&directory.join(stage_file))?;
            if Some(crate::v03::revision(&staged)) != entry.after_revision {
                return Err(TransactionError::InvalidJournal(format!(
                    "staged contents for '{}' do not match the journal",
                    entry.path
                )));
            }
        }
        if let Some(backup_file) = &entry.backup_file {
            let backup = read_regular_file(&directory.join(backup_file))?;
            if Some(crate::v03::revision(&backup)) != entry.before_revision {
                return Err(TransactionError::InvalidJournal(format!(
                    "backup contents for '{}' do not match the journal",
                    entry.path
                )));
            }
        }
    }

    journal.phase = Phase::Committing;
    persist_journal(directory, &journal)?;
    for index in 0..journal.entries.len() {
        let entry = &journal.entries[index];
        let current = current_revision(
            &collection
                .root
                .join(CollectionPath::new(&entry.path)?.to_path_buf()),
        )?;
        if current == entry.after_revision {
            journal.applied = journal.applied.max(index + 1);
            persist_journal(directory, &journal)?;
            continue;
        }
        if current != entry.before_revision {
            return Err(TransactionError::ManualRecovery(format!(
                "'{}' matches neither its before nor intended revision",
                entry.path
            )));
        }
        apply_entry(collection, directory, entry, journal.scope)?;
        journal.applied = index + 1;
        persist_journal(directory, &journal)?;
    }
    journal.phase = Phase::Committed;
    persist_journal(directory, &journal)?;
    cleanup_transaction(directory);
    Ok(true)
}

fn recheck_preconditions(
    collection: &Collection,
    journal: &Journal,
) -> Result<(), TransactionError> {
    for entry in &journal.entries {
        let path = CollectionPath::new(&entry.path)
            .map_err(|error| TransactionError::UnsafePath(error.to_string()))?
            .under(&collection.root);
        let current = current_revision(&path)?;
        if current != entry.before_revision {
            return Err(TransactionError::ConcurrentModification(entry.path.clone()));
        }
    }
    Ok(())
}

fn apply_entry(
    collection: &Collection,
    directory: &Path,
    entry: &JournalEntry,
    scope: TransactionScope,
) -> Result<(), TransactionError> {
    validate_entry_path(collection, &entry.path, scope)?;
    let path = CollectionPath::new(&entry.path)
        .map_err(|error| TransactionError::UnsafePath(error.to_string()))?
        .under(&collection.root);
    match &entry.stage_file {
        Some(stage_file) => {
            let staged_path = directory.join(stage_file);
            let bytes = read_regular_file(&staged_path)?;
            if crate::v03::revision(&bytes) != entry.after_revision.as_deref().unwrap_or_default() {
                return Err(TransactionError::InvalidJournal(format!(
                    "staged contents for '{}' do not match the journal",
                    entry.path
                )));
            }
            let result = if entry.before_revision.is_none() {
                atomic_create(&path, &bytes)
            } else {
                atomic_write(&path, &bytes)
            };
            result.map_err(|source| io_error(path.clone(), source))?;
        }
        None => {
            fs::remove_file(&path).map_err(|source| io_error(path.clone(), source))?;
            if let Some(parent) = path.parent() {
                sync_dir(parent)?;
            }
        }
    }
    Ok(())
}

fn validate_entry_path(
    collection: &Collection,
    path: &str,
    scope: TransactionScope,
) -> Result<(), TransactionError> {
    let logical = match scope {
        TransactionScope::Records => collection
            .validate_record_path(path)
            .map_err(|error| TransactionError::UnsafePath(error.to_string()))?,
        TransactionScope::Resources => {
            let logical = CollectionPath::new(path)
                .map_err(|error| TransactionError::UnsafePath(error.to_string()))?;
            let resource = Path::new(logical.as_str());
            let hidden_or_reserved = resource.components().any(|component| {
                let value = component.as_os_str().to_string_lossy();
                value.starts_with('.') || matches!(value.as_ref(), "node_modules" | "target")
            });
            let managed = matches!(
                logical.as_str(),
                "mdbase.yaml" | "mdbase.lock.yaml" | "mdbase.provisions.yaml"
            ) || resource.starts_with(&collection.settings.types_folder)
                || resource.starts_with(&collection.settings.contracts_folder)
                || matches!(
                    resource.extension().and_then(|value| value.to_str()),
                    Some("base" | "md")
                )
                || (resource.extension().and_then(|value| value.to_str()) == Some("json")
                    && resource.components().any(|component| {
                        matches!(component.as_os_str().to_str(), Some("schemas" | "_schemas"))
                    }));
            if hidden_or_reserved || !managed {
                return Err(TransactionError::UnsafePath(format!(
                    "'{path}' is not a managed collection resource"
                )));
            }
            logical
        }
        TransactionScope::SystemMigration => CollectionPath::new(path)
            .map_err(|error| TransactionError::UnsafePath(error.to_string()))?,
    };
    ensure_safe_relative_path(logical.as_str(), SpecProfile::V03)
        .map_err(|error| TransactionError::UnsafePath(error.to_string()))?;
    ensure_no_symlink_components(&collection.root, logical.as_str(), SpecProfile::V03)
        .map_err(|error| TransactionError::UnsafePath(error.to_string()))
}

fn ensure_transaction_root(collection: &Collection) -> Result<(), TransactionError> {
    ensure_no_symlink_components(&collection.root, TRANSACTIONS_DIR, SpecProfile::V03)
        .map_err(|error| TransactionError::UnsafePath(error.to_string()))?;
    let root = collection.root.join(TRANSACTIONS_DIR);
    fs::create_dir_all(&root).map_err(|source| io_error(root, source))
}

fn persist_journal(directory: &Path, journal: &Journal) -> Result<(), TransactionError> {
    let bytes = serde_json::to_vec_pretty(journal)
        .map_err(|error| TransactionError::InvalidJournal(error.to_string()))?;
    let path = directory.join(JOURNAL_FILE);
    atomic_write(&path, &bytes).map_err(|source| io_error(path, source))
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), TransactionError> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .map_err(|source| io_error(path.to_path_buf(), source))
}

fn read_regular_file(path: &Path) -> Result<Vec<u8>, TransactionError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| io_error(path.to_path_buf(), source))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(TransactionError::InvalidJournal(format!(
            "'{}' is not a regular transaction payload",
            path.display()
        )));
    }
    fs::read(path).map_err(|source| io_error(path.to_path_buf(), source))
}

fn current_revision(path: &Path) -> Result<Option<String>, TransactionError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(crate::v03::revision(&bytes))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error(path.to_path_buf(), source)),
    }
}

#[cfg(not(windows))]
fn sync_dir(path: &Path) -> Result<(), TransactionError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error(path.to_path_buf(), source))
}

#[cfg(windows)]
fn sync_dir(_path: &Path) -> Result<(), TransactionError> {
    // std::fs cannot open directory handles with FILE_FLAG_BACKUP_SEMANTICS,
    // so the portable directory-fsync pattern fails with AccessDenied on
    // Windows. Every transaction payload and journal file is still synced
    // before its atomic rename; only the additional directory metadata flush
    // is unavailable through Rust's standard library.
    Ok(())
}

fn cleanup_transaction(directory: &Path) {
    if fs::remove_dir_all(directory).is_ok() {
        if let Some(parent) = directory.parent() {
            let _ = sync_dir(parent);
        }
    }
}

fn io_error(path: PathBuf, source: io::Error) -> TransactionError {
    TransactionError::Io { path, source }
}

pub(crate) struct WriteLock {
    file: File,
}

struct StagingGuard {
    directory: PathBuf,
    durable: bool,
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if !self.durable {
            cleanup_transaction(&self.directory);
        }
    }
}

impl WriteLock {
    pub(crate) fn acquire(collection: &Collection) -> Result<Self, TransactionError> {
        let (file, path) = Self::open(collection)?;
        file.lock_exclusive()
            .map_err(|source| io_error(path, source))?;
        Ok(Self { file })
    }

    pub(crate) fn acquire_context(
        collection: &Collection,
        context: &OperationContext,
    ) -> Result<Self, TransactionError> {
        let (file, path) = Self::open(collection)?;
        loop {
            context
                .check()
                .map_err(|error| TransactionError::OperationBoundary { code: error.code() })?;
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self { file }),
                Err(error) if lock_is_contended(&error) => {
                    std::thread::sleep(
                        context
                            .deadline()
                            .remaining()
                            .min(std::time::Duration::from_millis(10)),
                    );
                }
                Err(source) => return Err(io_error(path, source)),
            }
        }
    }

    fn open(collection: &Collection) -> Result<(File, PathBuf), TransactionError> {
        ensure_no_symlink_components(
            &collection.root,
            ".mdbase/write.lock",
            collection.spec_profile,
        )
        .map_err(|error| TransactionError::UnsafePath(error.to_string()))?;
        let lock_directory = collection.root.join(".mdbase");
        fs::create_dir_all(&lock_directory).map_err(|source| io_error(lock_directory, source))?;
        let path = collection.root.join(".mdbase/write.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| io_error(path.clone(), source))?;
        Ok((file, path))
    }
}

fn lock_is_contended(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        // LockFileEx reports these Win32 lock races without mapping them to
        // ErrorKind::WouldBlock in every supported Rust toolchain.
        matches!(error.raw_os_error(), Some(32 | 33))
    }
    #[cfg(not(windows))]
    {
        false
    }
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

impl From<crate::api::CollectionPathError> for TransactionError {
    fn from(error: crate::api::CollectionPathError) -> Self {
        Self::UnsafePath(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collection() -> (tempfile::TempDir, Collection) {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        fs::write(root.path().join("a.md"), "old-a\n").unwrap();
        fs::write(root.path().join("b.md"), "old-b\n").unwrap();
        let collection = Collection::open(root.path()).unwrap();
        (root, collection)
    }

    fn versions() -> (FileBaseline, FileBaseline) {
        (
            BTreeMap::from([
                ("a.md".to_string(), b"old-a\n".to_vec()),
                ("b.md".to_string(), b"old-b\n".to_vec()),
            ]),
            BTreeMap::from([
                ("a.md".to_string(), b"new-a\n".to_vec()),
                ("b.md".to_string(), b"new-b\n".to_vec()),
            ]),
        )
    }

    #[test]
    fn open_completes_an_interrupted_commit() {
        let (root, collection) = collection();
        let (before, after) = versions();
        let error = commit_shadow_controlled(
            &collection,
            &before,
            &after,
            TransactionScope::Records,
            Some(1),
        )
        .unwrap_err();
        assert!(
            matches!(&error, TransactionError::SimulatedCrash),
            "unexpected transaction error: {error:?}"
        );
        assert_eq!(fs::read(root.path().join("a.md")).unwrap(), b"new-a\n");
        assert_eq!(fs::read(root.path().join("b.md")).unwrap(), b"old-b\n");

        drop(collection);
        Collection::open(root.path()).expect("recovery should complete");
        assert_eq!(fs::read(root.path().join("a.md")).unwrap(), b"new-a\n");
        assert_eq!(fs::read(root.path().join("b.md")).unwrap(), b"new-b\n");
        assert_eq!(
            fs::read_dir(root.path().join(TRANSACTIONS_DIR))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn recovery_fails_closed_after_an_unrelated_external_edit() {
        let (root, collection) = collection();
        let (before, after) = versions();
        let error = commit_shadow_controlled(
            &collection,
            &before,
            &after,
            TransactionScope::Records,
            Some(1),
        )
        .unwrap_err();
        assert!(
            matches!(&error, TransactionError::SimulatedCrash),
            "unexpected transaction error: {error:?}"
        );
        fs::write(root.path().join("b.md"), "external\n").unwrap();

        drop(collection);
        let error = match Collection::open(root.path()) {
            Ok(_) => panic!("external edit should require manual recovery"),
            Err(error) => error,
        };
        assert_eq!(error["error"]["code"], "manual_recovery_required");
        assert_eq!(fs::read(root.path().join("a.md")).unwrap(), b"new-a\n");
        assert_eq!(fs::read(root.path().join("b.md")).unwrap(), b"external\n");
        assert_eq!(
            fs::read_dir(root.path().join(TRANSACTIONS_DIR))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn commit_rechecks_the_exact_preflight_baseline() {
        let (root, collection) = collection();
        let (before, after) = versions();
        fs::write(root.path().join("b.md"), "external\n").unwrap();

        let error = commit_shadow(&collection, &before, &after).unwrap_err();
        assert!(
            matches!(
                &error,
                TransactionError::ConcurrentModification(ref path) if path == "b.md"
            ),
            "unexpected transaction error: {error:?}"
        );
        assert_eq!(fs::read(root.path().join("a.md")).unwrap(), b"old-a\n");
        assert_eq!(fs::read(root.path().join("b.md")).unwrap(), b"external\n");
        assert_eq!(
            fs::read_dir(root.path().join(TRANSACTIONS_DIR))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn open_completes_an_interrupted_system_migration_before_loading_types() {
        let root = tempfile::tempdir().unwrap();
        let legacy_config = b"spec_version: 0.2.0\n".to_vec();
        fs::write(root.path().join("mdbase.yaml"), &legacy_config).unwrap();
        let collection = Collection::open(root.path()).unwrap();
        let canonical_type = br#"---
kind: mdbase.type
name: task
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    properties: {}
---
"#
        .to_vec();
        let before = BTreeMap::from([("mdbase.yaml".to_string(), legacy_config)]);
        let after = BTreeMap::from([
            ("mdbase.yaml".to_string(), b"spec_version: 0.3.0\n".to_vec()),
            ("_types/task.md".to_string(), canonical_type),
        ]);

        let error = commit_shadow_controlled(
            &collection,
            &before,
            &after,
            TransactionScope::SystemMigration,
            Some(1),
        )
        .unwrap_err();
        assert!(
            matches!(&error, TransactionError::SimulatedCrash),
            "unexpected transaction error: {error:?}"
        );
        assert!(root.path().join("_types/task.md").is_file());
        assert!(fs::read_to_string(root.path().join("mdbase.yaml"))
            .unwrap()
            .contains("0.2.0"));

        drop(collection);
        let recovered = Collection::open(root.path()).expect("migration recovery should complete");
        assert_eq!(recovered.spec_profile, SpecProfile::V03);
        assert!(recovered.types.contains_key("task"));
        assert!(fs::read_to_string(root.path().join("mdbase.yaml"))
            .unwrap()
            .contains("0.3.0"));
    }
}
