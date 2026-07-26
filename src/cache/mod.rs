//! SQLite cache (S13).

pub mod indexer;
pub mod sqlite;
pub mod staleness;

use crate::Collection;
use thiserror::Error;

/// Failure while reading or maintaining the derived collection index.
///
/// Query execution treats this as a reason to use the authoritative Markdown
/// files. Explicit cache-management commands surface it to the caller.
#[derive(Debug, Error)]
pub(crate) enum CacheError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("collection path is outside the configured root: {0}")]
    OutsideRoot(String),
}

impl Collection {
    /// Rebuild the cache (S13.3.4, S13.8).
    /// Opens (or creates) the SQLite cache database and reindexes every file.
    ///
    /// The cache remains an optional optimization, but an explicit rebuild
    /// command reports failure instead of claiming that an incomplete index is
    /// healthy.
    pub fn cache_rebuild(&self) -> serde_json::Value {
        let result = sqlite::open_cache_db(&self.root, &self.settings.cache_folder)
            .map_err(CacheError::from)
            .and_then(|mut connection| indexer::reindex_all(&mut connection, self));
        match result {
            Ok(()) => serde_json::json!({ "success": true }),
            Err(error) => serde_json::json!({
                "success": false,
                "error": {
                    "code": "cache_rebuild_failed",
                    "message": error.to_string(),
                }
            }),
        }
    }

    /// Clear the cache (S13.3.5, S13.8).
    /// Removes the SQLite database file from disk.
    pub fn cache_clear(&self) -> serde_json::Value {
        let cache_root = self.root.join(&self.settings.cache_folder);
        for name in ["cache.db", "cache.db-wal", "cache.db-shm"] {
            let path = cache_root.join(name);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return serde_json::json!({
                        "success": false,
                        "error": {
                            "code": "cache_clear_failed",
                            "message": format!("Failed to remove '{}': {error}", path.display()),
                        }
                    });
                }
            }
        }
        serde_json::json!({ "success": true })
    }
}
