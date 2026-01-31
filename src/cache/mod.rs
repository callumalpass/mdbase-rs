//! SQLite cache (S13).

pub mod indexer;
pub mod sqlite;
pub mod staleness;

use crate::Collection;

impl Collection {
    /// Rebuild the cache (S13.3.4, S13.8).
    /// Opens (or creates) the SQLite cache database and reindexes every file
    /// in the collection.  Cache failure is silently swallowed so that the
    /// operation always returns success -- the cache is a transparent
    /// optimisation and must not block normal operations.
    pub fn cache_rebuild(&self) -> serde_json::Value {
        match sqlite::open_cache_db(&self.root, &self.settings.cache_folder) {
            Ok(conn) => {
                indexer::reindex_all(&conn, self);
            }
            Err(_) => {
                // Cache failure is non-fatal
            }
        }
        serde_json::json!({ "success": true })
    }

    /// Clear the cache (S13.3.5, S13.8).
    /// Removes the SQLite database file from disk.
    pub fn cache_clear(&self) -> serde_json::Value {
        let db_path = self.root.join(&self.settings.cache_folder).join("cache.db");
        if db_path.exists() {
            let _ = std::fs::remove_file(&db_path);
        }
        // Also try to remove WAL and SHM files that SQLite may leave behind
        let wal_path = self.root.join(&self.settings.cache_folder).join("cache.db-wal");
        let shm_path = self.root.join(&self.settings.cache_folder).join("cache.db-shm");
        if wal_path.exists() {
            let _ = std::fs::remove_file(&wal_path);
        }
        if shm_path.exists() {
            let _ = std::fs::remove_file(&shm_path);
        }
        serde_json::json!({ "success": true })
    }
}
