//! Delete operation (§12.4).

use crate::errors::*;
use crate::Collection;

impl Collection {
    /// Delete a file (§12.4).
    pub fn delete(&self, input: &serde_json::Value) -> serde_json::Value {
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return op_error(INVALID_PATH, "path is required"),
        };
        let check_backlinks = input.get("check_backlinks").and_then(|v| v.as_bool()).unwrap_or(false);

        let full_path = self.root.join(path);
        if !full_path.exists() {
            return op_error(FILE_NOT_FOUND, &format!("File not found: {}", path));
        }

        // Concurrent modification detection
        if let Some(known_ms) = input.get("last_known_mtime").and_then(|v| v.as_u64()) {
            if let Ok(meta) = std::fs::metadata(&full_path) {
                if let Ok(mtime) = meta.modified() {
                    let current_ms = mtime.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64).unwrap_or(0);
                    if current_ms != known_ms {
                        return op_error(CONCURRENT_MODIFICATION,
                            &format!("File '{}' was modified externally", path));
                    }
                }
            }
        }

        // Check backlinks before deletion
        let mut broken_links: Vec<serde_json::Value> = Vec::new();
        if check_backlinks {
            let all_files = self.build_all_files_data();
            let bl_index = self.build_backlinks_index(&all_files);
            if let Some(sources) = bl_index.get(path) {
                for source in sources {
                    broken_links.push(serde_json::json!({
                        "path": source,
                    }));
                }
            }
        }

        if let Err(e) = std::fs::remove_file(&full_path) {
            return op_error("io_error", &format!("Failed to delete: {}", e));
        }

        let mut result = serde_json::json!({
            "path": path,
            "deleted": true,
        });
        if !broken_links.is_empty() {
            result["broken_links"] = serde_json::Value::Array(broken_links);
        }
        result
    }
}
