//! Runtime-owned incremental maintenance of the rebuildable SQLite index.

use std::collections::BTreeSet;

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
    advance_generation(&transaction, generation)?;
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
    advance_generation(&transaction, generation)?;
    transaction.commit()?;
    Ok(())
}

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
) -> Result<(), CacheError> {
    connection.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![GENERATION_KEY, generation_value(generation)?],
    )?;
    connection.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('query_snapshot', ?1)",
        [uuid::Uuid::new_v4().simple().to_string()],
    )?;
    Ok(())
}

fn generation_value(generation: &CollectionGeneration) -> Result<String, CacheError> {
    serde_json::to_string(generation).map_err(CacheError::from)
}
