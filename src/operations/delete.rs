//! Delete operation (§12.4).

use crate::api::operations::{DeleteInput, DeleteOutput};
use crate::errors::*;
use crate::operations::{ensure_no_symlink_components, ensure_revision, ensure_safe_relative_path};
use crate::Collection;

impl Collection {
    /// Delete a file (§12.4).
    pub fn delete(&self, input: &serde_json::Value) -> serde_json::Value {
        let input = match DeleteInput::parse(input) {
            Ok(parsed) => parsed,
            Err(err) => return err,
        };
        if let Err(error) = ensure_safe_relative_path(&input.path, self.spec_profile) {
            return error;
        }
        if let Err(error) = ensure_no_symlink_components(&self.root, &input.path, self.spec_profile)
        {
            return error;
        }
        let path = input.path;
        let check_backlinks = input.check_backlinks;
        let dry_run = input.dry_run;

        let full_path = self.root.join(&path);
        if !full_path.exists() {
            return op_error(FILE_NOT_FOUND, &format!("File not found: {}", path));
        }
        if let Err(error) = ensure_revision(&full_path, &path, input.if_revision.as_deref()) {
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
                            &format!("File '{}' was modified externally", path),
                        );
                    }
                }
            }
        }

        // Check backlinks before deletion
        let mut broken_links: Vec<serde_json::Value> = Vec::new();
        if check_backlinks {
            let all_files = self.build_all_files_data();
            let bl_index = self.build_backlinks_index(&all_files);
            if let Some(sources) = bl_index.get(&path) {
                for source in sources {
                    broken_links.push(serde_json::json!({
                        "path": source,
                    }));
                }
            }
        }

        if !dry_run {
            if let Err(error) = ensure_revision(&full_path, &path, input.if_revision.as_deref()) {
                return error;
            }
            if let Err(error) = ensure_no_symlink_components(&self.root, &path, self.spec_profile) {
                return error;
            }
            if let Err(e) = std::fs::remove_file(&full_path) {
                return op_error("io_error", &format!("Failed to delete: {}", e));
            }
        }

        DeleteOutput {
            path,
            deleted: !dry_run,
            dry_run,
            broken_links,
        }
        .into_json()
    }
}
