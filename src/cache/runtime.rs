//! Runtime-owned incremental maintenance of the rebuildable SQLite index.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::sync::{LazyLock, Mutex};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use super::{indexer, sqlite, CacheError};
use crate::runtime::{CanonicalChange, ChangeSet, CollectionGeneration};
use crate::Collection;

const GENERATION_KEY: &str = "runtime_generation";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UniqueConflictKind {
    Identity,
    Field {
        type_name: String,
        field_name: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UniqueConflict {
    pub kind: UniqueConflictKind,
    pub value: String,
    pub path: String,
}

/// Build one complete initial index and bind it to the runtime generation in
/// the same rebuildable transaction. This is the only ordinary full walk.
pub(crate) fn rebuild(
    collection: &Collection,
    generation: &CollectionGeneration,
) -> Result<(), CacheError> {
    let mut connection =
        sqlite::open_cache_db(&collection.root, &collection.settings.cache_folder)?;
    let files = collection.scan_collection_files_checked()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "DELETE FROM links; DELETE FROM file_types; DELETE FROM unique_values; DELETE FROM identity_values; DELETE FROM files; DELETE FROM meta;",
    )?;
    for absolute in files {
        let relative = absolute
            .strip_prefix(&collection.root)
            .map_err(|_| CacheError::OutsideRoot(absolute.display().to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        indexer::reindex_file(&transaction, collection, &absolute, &relative)?;
    }
    indexer::resolve_all_links(&transaction, collection)?;
    let _ = advance_generation(&transaction, generation)?;
    transaction.commit()?;
    Ok(())
}

/// Apply an exact runtime change set without scanning unrelated records, then
/// advance the cache generation atomically with those derived rows.
pub(crate) fn apply_changes(
    collection: &Collection,
    changes: &ChangeSet,
    generation: &CollectionGeneration,
) -> Result<(), CacheError> {
    let ChangeSet::Exact(batch) = changes else {
        return rebuild(collection, generation);
    };
    if batch
        .items()
        .iter()
        .any(|change| matches!(change, CanonicalChange::Resource(_)))
    {
        return rebuild(collection, generation);
    }

    let mut remove = BTreeSet::new();
    let mut reindex = BTreeSet::new();
    let mut resolution_unstable = false;
    for change in batch.items() {
        let CanonicalChange::Record(change) = change else {
            continue;
        };
        if let Some(from) = &change.from {
            remove.insert(from.as_str().to_string());
        }
        resolution_unstable |= !matches!(change.kind, crate::runtime::RecordChangeKind::Updated)
            || change.changed_fields.iter().any(|field| {
                field == "/title"
                    || field
                        == format!(
                            "/{}",
                            collection
                                .settings
                                .id_field
                                .replace('~', "~0")
                                .replace('/', "~1")
                        )
            });
        remove.insert(change.path.as_str().to_string());
        if collection.root.join(change.path.as_str()).is_file() {
            reindex.insert(change.path.as_str().to_string());
        }
    }

    let mut connection =
        sqlite::open_cache_db(&collection.root, &collection.settings.cache_folder)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for path in &remove {
        indexer::remove_file(&transaction, path)?;
    }
    for path in reindex {
        indexer::reindex_file(
            &transaction,
            collection,
            &collection.root.join(&path),
            &path,
        )?;
    }
    if resolution_unstable {
        indexer::resolve_all_links(&transaction, collection)?;
    } else {
        let sources = remove
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        indexer::resolve_links_for_sources(&transaction, collection, &sources)?;
    }
    let _ = advance_generation(&transaction, generation)?;
    transaction.commit()?;
    Ok(())
}

/// Apply capability-revalidated classified-invalid refreshes and genuine
/// removals in one transaction without publishing a record event or advancing
/// the runtime generation. Refresh rows contain only bounded path/file facts
/// and the canonical closed failure reason.
pub(crate) struct InvalidMaintenanceSeal {
    refresh: BTreeSet<String>,
    remove: BTreeSet<String>,
    expectations: BTreeMap<String, indexer::MaintenanceExpectation>,
    generation: CollectionGeneration,
    observation: crate::watch::ReconciliationToken,
    query_snapshot: String,
    schema_version: i64,
}

impl InvalidMaintenanceSeal {
    pub(crate) fn matches(
        &self,
        refresh: &BTreeSet<String>,
        remove: &BTreeSet<String>,
        generation: &CollectionGeneration,
        observation: &crate::watch::ReconciliationToken,
    ) -> bool {
        self.refresh == *refresh
            && self.remove == *remove
            && self.generation == *generation
            && observation.is_later_in_same_epoch_than(&self.observation)
    }

    pub(crate) fn filesystem_is_current(
        &self,
        collection: &Collection,
    ) -> Result<bool, CacheError> {
        for (path, expectation) in &self.expectations {
            if !indexer::maintenance_expectation_still_current(collection, path, expectation)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) fn cache_is_current(&self, collection: &Collection) -> Result<bool, CacheError> {
        let mut connection = sqlite::open_cache_db_read_only_existing(
            &collection.root,
            &collection.settings.cache_folder,
        )?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let schema_version =
            transaction.query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))?;
        if schema_version != self.schema_version {
            return Ok(false);
        }
        let generation = transaction
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [GENERATION_KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let query_snapshot = transaction
            .query_row(
                "SELECT value FROM meta WHERE key = 'query_snapshot'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if generation.as_deref() != Some(generation_value(&self.generation)?.as_str())
            || query_snapshot.as_deref() != Some(self.query_snapshot.as_str())
        {
            return Ok(false);
        }
        for (path, expectation) in &self.expectations {
            if !indexer::maintenance_cache_expectation_is_exact(&transaction, path, expectation)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

pub(crate) enum InvalidMaintenanceOutcome {
    Stale,
    Current,
    Applied(InvalidMaintenanceSeal),
}

impl std::fmt::Debug for InvalidMaintenanceOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Stale => "Stale",
            Self::Current => "Current",
            Self::Applied(_) => "Applied(..)",
        })
    }
}

impl PartialEq for InvalidMaintenanceOutcome {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Stale, Self::Stale) | (Self::Current, Self::Current)
        )
    }
}

impl Eq for InvalidMaintenanceOutcome {}

pub(crate) fn apply_invalid_maintenance(
    collection: &Collection,
    refresh: &BTreeSet<String>,
    remove: &BTreeSet<String>,
    epoch: &crate::watch::WatcherEpoch,
    generation: &CollectionGeneration,
    observation: crate::watch::ReconciliationToken,
) -> Result<InvalidMaintenanceOutcome, CacheError> {
    if epoch.is_exhausted() {
        return Ok(InvalidMaintenanceOutcome::Stale);
    }
    if !refresh.is_disjoint(remove) {
        return Ok(InvalidMaintenanceOutcome::Stale);
    }
    if refresh.is_empty() && remove.is_empty() {
        #[cfg(test)]
        epoch.run_hook(crate::watch::LinearizationPoint::CacheCommit);
        let _linearized = epoch.linearize();
        return Ok(if epoch.is_exhausted() {
            InvalidMaintenanceOutcome::Stale
        } else {
            InvalidMaintenanceOutcome::Current
        });
    }
    let mut expected = BTreeMap::new();
    for path in refresh {
        let Some(expectation) = indexer::refresh_maintenance_expectation(collection, path)? else {
            return Ok(InvalidMaintenanceOutcome::Stale);
        };
        expected.insert(path.clone(), expectation);
    }
    for path in remove {
        if crate::record_load::load_record_no_follow(collection, path)?.is_some() {
            return Ok(InvalidMaintenanceOutcome::Stale);
        }
        expected.insert(path.clone(), indexer::MaintenanceExpectation::Absent);
    }
    let mut connection =
        sqlite::open_cache_db(&collection.root, &collection.settings.cache_folder)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for path in refresh {
        let Some(expectation) =
            indexer::refresh_invalid_file_no_follow(&transaction, collection, path)?
        else {
            return Ok(InvalidMaintenanceOutcome::Stale);
        };
        if expected.get(path) != Some(&expectation) {
            return Ok(InvalidMaintenanceOutcome::Stale);
        }
    }
    for path in remove {
        let Some(expectation) =
            indexer::remove_invalid_file_no_follow_if_absent(&transaction, collection, path)?
        else {
            return Ok(InvalidMaintenanceOutcome::Stale);
        };
        if expected.get(path) != Some(&expectation) {
            return Ok(InvalidMaintenanceOutcome::Stale);
        }
    }
    indexer::resolve_all_links(&transaction, collection)?;
    // Keep the same generation while rotating the derived query snapshot.
    // This write is also rolled back when final canonical revalidation fails.
    let query_snapshot = advance_generation(&transaction, generation)?;
    let schema_version =
        transaction.query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))?;
    run_maintenance_revalidation_hook(collection);
    // External writers do not participate in SQLite locking. Re-open every
    // path whose row changed at the final pre-commit boundary and roll back if
    // absence, invalid classification, reason, or byte revision changed.
    for (path, expectation) in &expected {
        if !indexer::maintenance_expectation_still_current(collection, path, expectation)? {
            return Ok(InvalidMaintenanceOutcome::Stale);
        }
    }
    // The hook is deliberately after the former atomic check and immediately
    // before the shared gate, making the old check-before-commit race
    // deterministic in tests.
    #[cfg(test)]
    epoch.run_hook(crate::watch::LinearizationPoint::CacheCommit);
    let _linearized = epoch.linearize();
    if epoch.is_exhausted() {
        return Ok(InvalidMaintenanceOutcome::Stale);
    }
    transaction.commit()?;
    Ok(InvalidMaintenanceOutcome::Applied(InvalidMaintenanceSeal {
        refresh: refresh.clone(),
        remove: remove.clone(),
        expectations: expected,
        generation: generation.clone(),
        observation,
        query_snapshot,
        schema_version,
    }))
}

#[cfg(test)]
type MaintenanceReplacementHooks = BTreeMap<PathBuf, (String, Vec<u8>)>;

#[cfg(test)]
static MAINTENANCE_REVALIDATION_HOOKS: LazyLock<Mutex<MaintenanceReplacementHooks>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

#[cfg(test)]
pub(crate) fn set_maintenance_revalidation_replacement(
    collection_root: &std::path::Path,
    path: &str,
    bytes: Vec<u8>,
) {
    MAINTENANCE_REVALIDATION_HOOKS
        .lock()
        .unwrap()
        .insert(collection_root.to_path_buf(), (path.to_string(), bytes));
}

#[cfg(test)]
fn run_maintenance_revalidation_hook(collection: &Collection) {
    if let Some((path, bytes)) = MAINTENANCE_REVALIDATION_HOOKS
        .lock()
        .unwrap()
        .remove(&collection.root)
    {
        std::fs::write(collection.root.join(path), bytes).unwrap();
    }
}

#[cfg(not(test))]
fn run_maintenance_revalidation_hook(_collection: &Collection) {}

pub(crate) fn matches_generation(
    collection: &Collection,
    generation: &CollectionGeneration,
) -> Result<bool, CacheError> {
    let connection = sqlite::open_cache_db(&collection.root, &collection.settings.cache_folder)?;
    let stored = connection
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [GENERATION_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(stored.as_deref() == Some(generation_value(generation)?.as_str()))
}

pub(crate) fn uniqueness_conflicts(
    collection: &Collection,
    frontmatter: &serde_json::Value,
    type_names: &[String],
    exclude_path: &str,
) -> Result<Vec<UniqueConflict>, CacheError> {
    let connection = sqlite::open_cache_db(&collection.root, &collection.settings.cache_folder)?;
    let mut conflicts = Vec::new();
    if let Some(value) = frontmatter
        .get(&collection.settings.id_field)
        .and_then(indexer::canonical_unique_value)
    {
        if let Some(path) = connection
            .query_row(
                "SELECT path FROM identity_values WHERE value = ?1",
                [&value],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            if path != exclude_path {
                conflicts.push(UniqueConflict {
                    kind: UniqueConflictKind::Identity,
                    value,
                    path,
                });
            }
        }
    }

    for type_name in type_names {
        let Some(type_definition) = collection.types.get(type_name) else {
            continue;
        };
        let mut fields = type_definition
            .fields
            .iter()
            .filter(|(_, field)| field.unique)
            .map(|(name, _)| name.clone())
            .collect::<BTreeSet<_>>();
        fields.extend(
            type_definition
                .v03_frontmatter
                .as_ref()
                .and_then(|value| value.pointer("/collection/unique"))
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|rule| rule.get("field"))
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string),
        );
        for field_name in fields {
            let Some(value) = crate::field_references::get_value(frontmatter, &field_name)
                .and_then(indexer::canonical_unique_value)
            else {
                continue;
            };
            let path = connection
                .query_row(
                    "SELECT path FROM unique_values WHERE type_name = ?1 AND field_name = ?2 AND value = ?3",
                    rusqlite::params![type_name, field_name, value],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(path) = path.filter(|path| path != exclude_path) {
                conflicts.push(UniqueConflict {
                    kind: UniqueConflictKind::Field {
                        type_name: type_name.clone(),
                        field_name,
                    },
                    value,
                    path,
                });
            }
        }
    }
    Ok(conflicts)
}

fn advance_generation(
    connection: &Connection,
    generation: &CollectionGeneration,
) -> Result<String, CacheError> {
    connection.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![GENERATION_KEY, generation_value(generation)?],
    )?;
    let query_snapshot = uuid::Uuid::new_v4().simple().to_string();
    connection.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('query_snapshot', ?1)",
        [&query_snapshot],
    )?;
    Ok(query_snapshot)
}

fn generation_value(generation: &CollectionGeneration) -> Result<String, CacheError> {
    serde_json::to_string(generation).map_err(CacheError::from)
}
