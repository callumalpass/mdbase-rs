//! Mtime-based staleness detection (S13.6).

use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

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
    let mut statement =
        conn.prepare("SELECT path, mtime_ns, parse_error, failure_reason FROM files")?;
    let rows = statement.query_map([], |row| {
        let parse_error = row.get::<_, i64>(2)? != 0;
        let failure_reason = row.get::<_, Option<String>>(3)?;
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            parse_error && failure_reason.is_none(),
        ))
    })?;
    for row in rows {
        let (path, mtime, legacy_unclassified) = row?;
        cached.insert(path, (mtime, legacy_unclassified));
    }

    let mut disk_paths = HashSet::with_capacity(files.len());
    let mut stale = Vec::new();
    for rel_path in files {
        disk_paths.insert(rel_path.clone());
        let filesystem_mtime = collection.held_root().modified_nanos(Path::new(rel_path))?;
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
