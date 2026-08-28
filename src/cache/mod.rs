//! SQLite cache (S13).

pub mod indexer;
pub(crate) mod runtime;
pub mod sqlite;
pub mod staleness;

use crate::Collection;
use std::time::Duration;
#[cfg(any(test, windows))]
use std::time::Instant;
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
    #[error(transparent)]
    Scan(#[from] crate::snapshot::CollectionScanError),
    #[error("collection path is outside the configured root: {0}")]
    OutsideRoot(String),
    #[error("collection operation cancelled")]
    Cancelled,
}

impl Collection {
    /// Rebuild the cache (S13.3.4, S13.8).
    /// Opens (or creates) the SQLite cache database and reindexes every file.
    ///
    /// The cache remains an optional optimization, but an explicit rebuild
    /// command reports failure instead of claiming that an incomplete index is
    /// healthy.
    pub fn cache_rebuild(&self) -> serde_json::Value {
        let result = sqlite::lock_cache_lifecycle_exclusive(
            &self.root,
            &self.settings.cache_folder,
            Duration::from_secs(1),
        )
        .map_err(CacheError::from)
        .and_then(|_lifecycle| {
            sqlite::open_cache_db(&self.root, &self.settings.cache_folder)
                .map_err(CacheError::from)
                .and_then(|mut connection| indexer::reindex_all(&mut connection, self))
        });
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
        let _lifecycle = match sqlite::lock_cache_lifecycle_exclusive(
            &self.root,
            &self.settings.cache_folder,
            Duration::from_secs(1),
        ) {
            Ok(guard) => guard,
            Err(error) => {
                return serde_json::json!({
                    "success": false,
                    "error": {
                        "code": "cache_clear_failed",
                        "message": format!(
                            "Failed to lock cache lifecycle at '{}': {error}",
                            cache_root.display()
                        ),
                    }
                });
            }
        };
        for name in ["cache.db", "cache.db-wal", "cache.db-shm"] {
            let path = cache_root.join(name);
            match remove_cache_file(&path) {
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

#[cfg(not(windows))]
fn remove_cache_file(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

#[cfg(windows)]
fn remove_cache_file(path: &std::path::Path) -> std::io::Result<()> {
    remove_cache_file_bounded(path, Duration::from_secs(1), |path| {
        std::fs::remove_file(path)
    })
}

#[cfg(any(test, windows))]
fn remove_cache_file_bounded(
    path: &std::path::Path,
    timeout: Duration,
    mut remove: impl FnMut(&std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut first_error = None;
    loop {
        match remove(path) {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                ) =>
            {
                if first_error.is_none() {
                    first_error = Some(error);
                }
                let now = Instant::now();
                if now >= deadline {
                    return Err(first_error.expect("removal failure was recorded"));
                }
                std::thread::sleep(Duration::from_millis(10).min(deadline - now));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_clear_sharing_retry_is_bounded_and_preserves_platform_error() {
        let path = std::path::Path::new("cache.db");
        let mut attempts = 0;
        remove_cache_file_bounded(path, Duration::from_millis(100), |_| {
            attempts += 1;
            if attempts < 3 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "deterministic sharing violation",
                ))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(attempts, 3);

        let mut attempt = 0;
        let error = remove_cache_file_bounded(path, Duration::from_millis(15), |_| {
            attempt += 1;
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("retained sqlite handle {attempt}"),
            ))
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "retained sqlite handle 1");
    }

    #[test]
    fn cache_lifecycle_exclusive_waits_for_retained_shared_guard() {
        use std::sync::mpsc;

        let directory = tempfile::tempdir().unwrap();
        let shared = sqlite::lock_cache_lifecycle_shared(
            directory.path(),
            ".cache",
            Duration::from_millis(100),
        )
        .unwrap();
        let root = directory.path().to_path_buf();
        let waiter_root = root.clone();
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        let waiter = std::thread::spawn(move || {
            let result = sqlite::lock_cache_lifecycle_exclusive(
                &waiter_root,
                ".cache",
                Duration::from_millis(500),
            );
            result_sender.send(result).unwrap();
        });

        let evidence_deadline = Instant::now() + Duration::from_secs(2);
        while sqlite::cache_lifecycle_waiter_count(&root, ".cache") == 0 {
            assert!(
                Instant::now() < evidence_deadline,
                "exclusive acquisition never entered in-process contention"
            );
            std::thread::yield_now();
        }
        assert!(matches!(
            result_receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        drop(shared);
        result_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        waiter.join().unwrap();
        assert!(directory
            .path()
            .join(".cache/cache.lifecycle.lock")
            .is_file());
    }
}
