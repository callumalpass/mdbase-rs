//! Mtime-based staleness detection (S13.6).

use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::CacheError;
use crate::Collection;

#[derive(Debug, Default)]
pub(crate) struct CacheChanges {
    pub stale: Vec<String>,
    pub deleted: Vec<String>,
}

/// Compare one capability-relative filesystem scan with the cache using one
/// bulk SQLite read. Both the filesystem facts and the derived SQLite store are
/// rooted in private held authorities rather than the collection display name.
pub(crate) fn find_changes(
    conn: &Connection,
    collection: &Collection,
    files: &[String],
) -> Result<CacheChanges, CacheError> {
    let mut cached = HashMap::<String, (i64, bool)>::new();
    let mut statement = conn.prepare(
        "SELECT path, mtime_ns, parse_error, failure_reason, source_revision FROM files",
    )?;
    let rows = statement.query_map([], |row| {
        let parse_error = row.get::<_, i64>(2)? != 0;
        let failure_reason = row.get::<_, Option<String>>(3)?;
        // Cache migrations created empty tokens in pre-revision stores. Reindex
        // those rows once; remove this migration test when those caches expire.
        let missing_revision = row.get::<_, String>(4)?.is_empty();
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            (parse_error && failure_reason.is_none()) || missing_revision,
        ))
    })?;
    for row in rows {
        let (path, mtime, needs_refresh) = row?;
        cached.insert(path, (mtime, needs_refresh));
    }

    let mut disk_paths = HashSet::with_capacity(files.len());
    let mut stale = Vec::new();
    let mtimes = collection.held_root().modified_nanos_many(files)?;
    for (rel_path, filesystem_mtime) in files.iter().zip(mtimes) {
        disk_paths.insert(rel_path.clone());
        if !matches!(cached.get(rel_path), Some((mtime, false)) if *mtime == filesystem_mtime) {
            stale.push(rel_path.clone());
        }
    }
    let deleted = cached
        .into_keys()
        .filter(|path| !disk_paths.contains(path))
        .collect();
    Ok(CacheChanges { stale, deleted })
}

/// Compatibility helpers retained for cache tests and older internal callers.
#[allow(dead_code)]
pub(crate) fn find_stale(
    conn: &Connection,
    collection: &Collection,
    files: &[String],
) -> Vec<PathBuf> {
    find_changes(conn, collection, files)
        .map(|changes| changes.stale.into_iter().map(PathBuf::from).collect())
        .unwrap_or_default()
}

#[allow(dead_code)]
pub(crate) fn find_deleted(
    conn: &Connection,
    collection: &Collection,
    files: &[String],
) -> Vec<String> {
    find_changes(conn, collection, files)
        .map(|changes| changes.deleted)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[test]
    fn missing_cache_revision_is_reindexed_before_required_output() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  default_validation: off\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("one.md"),
            "---\ntitle: cached\n---\nbody\n",
        )
        .unwrap();
        let collection = crate::Collection::open(root.path()).unwrap();
        assert_eq!(collection.cache_rebuild()["success"], true);
        let db = rusqlite::Connection::open(
            collection
                .held_root()
                .cache_storage_path()
                .join(".mdbase/cache.db"),
        )
        .unwrap();
        db.execute("UPDATE files SET source_revision = ''", [])
            .unwrap();
        drop(db);
        let typed = collection.typed().unwrap();
        let query = typed
            .query(crate::api::QueryRequest {
                output: Some(crate::api::QueryOutput::Metadata),
                ..Default::default()
            })
            .unwrap();
        let read = typed
            .read(crate::api::ReadRequest::new("one.md").unwrap())
            .unwrap();
        assert_eq!(
            query.value.records[0]["revision"],
            read.value.revision.as_str()
        );
    }
}
