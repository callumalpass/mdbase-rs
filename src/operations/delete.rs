//! Delete operation (§12.4).

use crate::api::operations::{DeleteInput, DeleteOutput};
use crate::errors::*;
use crate::operations::{
    ensure_no_symlink_components, ensure_regular_record_file, ensure_revision,
    mutation_record_path, sync_directory,
};
use crate::Collection;

impl Collection {
    /// Delete a file (§12.4).
    pub fn delete(&self, input: &serde_json::Value) -> serde_json::Value {
        let input = match DeleteInput::parse(input) {
            Ok(parsed) => parsed,
            Err(err) => return err,
        };
        let path = match mutation_record_path(self, &input.path) {
            Ok(path) => path,
            Err(error) => return error,
        };
        if let Err(error) =
            ensure_no_symlink_components(&self.root, path.as_str(), self.spec_profile)
        {
            return error;
        }
        let check_backlinks = input.check_backlinks;
        let dry_run = input.dry_run;
        let _write_lock = if dry_run {
            None
        } else {
            match crate::transactions::WriteLock::acquire(self) {
                Ok(write_lock) => Some(write_lock),
                Err(error) => return op_error(error.code(), &error.to_string()),
            }
        };

        let full_path = path.under(&self.root);
        if let Err(error) = ensure_regular_record_file(&full_path, path.as_str()) {
            return error;
        }
        if let Err(error) = ensure_revision(&full_path, path.as_str(), input.if_revision.as_deref())
        {
            return error;
        }

        // Concurrent modification detection
        if let Some(known_ms) = input.last_known_mtime {
            if let Ok(meta) = std::fs::metadata(&full_path) {
                if let Ok(mtime) = meta.modified() {
                    let current_ms = mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    if current_ms != known_ms {
                        return op_error(
                            CONCURRENT_MODIFICATION,
                            &format!("File '{}' was modified externally", path.as_str()),
                        );
                    }
                }
            }
        }

        // Check backlinks before deletion
        let mut broken_links: Vec<serde_json::Value> = Vec::new();
        if check_backlinks {
            let bl_index = match self.build_authoritative_backlinks_index() {
                Ok(index) => index,
                Err(error) => return op_error("collection_snapshot_failed", &error.to_string()),
            };
            if let Some(sources) = bl_index.get(path.as_str()) {
                for source in sources {
                    broken_links.push(serde_json::json!({
                        "path": source,
                    }));
                }
            }
        }

        if !dry_run {
            if let Err(error) =
                ensure_revision(&full_path, path.as_str(), input.if_revision.as_deref())
            {
                return error;
            }
            if let Err(error) =
                ensure_no_symlink_components(&self.root, path.as_str(), self.spec_profile)
            {
                return error;
            }
            if let Err(e) = std::fs::remove_file(&full_path) {
                return op_error("io_error", &format!("Failed to delete: {}", e));
            }
            if let Some(parent) = full_path.parent() {
                if let Err(error) = sync_directory(parent) {
                    return op_error(
                        "io_error",
                        &format!("Failed to make deletion durable: {error}"),
                    );
                }
            }
        }

        DeleteOutput {
            path: path.to_string(),
            deleted: !dry_run,
            dry_run,
            broken_links,
        }
        .into_json()
    }
}
