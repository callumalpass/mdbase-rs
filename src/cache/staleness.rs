//! Mtime-based staleness detection (S13.6).

use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub(crate) struct CacheChanges {
    pub stale: Vec<PathBuf>,
    pub deleted: Vec<String>,
}

/// Compare one filesystem scan with the cache using one bulk SQLite read.
///
/// The previous implementation issued a SQLite lookup per collection file and
/// then performed another filesystem existence pass for deletions. That made a
/// no-op freshness check disproportionately expensive for paginated queries.
pub(crate) fn find_changes(conn: &Connection, root: &Path, files: &[PathBuf]) -> CacheChanges {
    let mut cached = HashMap::<String, i64>::new();
    let mut statement = match conn.prepare("SELECT path, mtime_ns FROM files") {
        Ok(statement) => statement,
        Err(_) => return CacheChanges::default(),
    };
    let rows = match statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    }) {
        Ok(rows) => rows,
        Err(_) => return CacheChanges::default(),
    };
    for (path, mtime) in rows.flatten() {
        cached.insert(path, mtime);
    }

    let mut disk_paths = HashSet::with_capacity(files.len());
    let mut stale = Vec::new();
    for file_path in files {
        let rel_path = match file_path.strip_prefix(root) {
            Ok(path) => path.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        disk_paths.insert(rel_path.clone());
        let filesystem_mtime = std::fs::metadata(file_path)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos() as i64)
            .unwrap_or(0);
        if cached.get(&rel_path).copied() != Some(filesystem_mtime) {
            stale.push(file_path.clone());
        }
    }
    let deleted = cached
        .into_keys()
        .filter(|path| !disk_paths.contains(path))
        .collect();
    CacheChanges { stale, deleted }
}

/// Compare filesystem mtimes against cached `mtime_ns` and return paths that
/// are stale (file is newer than what the cache recorded).
///
/// `files` should be a list of *absolute* file paths already discovered on disk.
#[allow(dead_code)]
pub(crate) fn find_stale(conn: &Connection, root: &Path, files: &[PathBuf]) -> Vec<PathBuf> {
    let mut stale = Vec::new();
    for file_path in files {
        let rel_path = match file_path.strip_prefix(root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };

        // Read filesystem mtime
        let fs_mtime_ns = match std::fs::metadata(file_path) {
            Ok(meta) => {
                use std::time::UNIX_EPOCH;
                meta.modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as i64)
                    .unwrap_or(0)
            }
            Err(_) => continue, // can't stat => skip
        };

        // Look up cached mtime
        let cached_mtime: Option<i64> = conn
            .query_row(
                "SELECT mtime_ns FROM files WHERE path = ?1",
                rusqlite::params![rel_path],
                |row| row.get(0),
            )
            .ok();

        match cached_mtime {
            Some(cm) if cm == fs_mtime_ns => {} // up to date
            _ => stale.push(file_path.clone()), // missing or different
        }
    }
    stale
}

/// Return the set of relative paths present in the cache but **not** on disk.
#[allow(dead_code)]
pub(crate) fn find_deleted(conn: &Connection, root: &Path) -> Vec<String> {
    let mut stmt = match conn.prepare("SELECT path FROM files") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map([], |row| row.get::<_, String>(0)) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut deleted = Vec::new();
    for rel_path in rows.flatten() {
        let abs = root.join(&rel_path);
        if !abs.exists() {
            deleted.push(rel_path);
        }
    }
    deleted
}
