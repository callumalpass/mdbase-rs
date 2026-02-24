//! Mtime-based staleness detection (S13.6).

use rusqlite::Connection;
use std::path::{Path, PathBuf};

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
