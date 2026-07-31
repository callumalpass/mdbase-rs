//! Rename with reference updates (§12.5).

mod body;
mod frontmatter;
mod link_rewrite;

use crate::api::operations::RenameInput;
use crate::errors::*;
use crate::frontmatter::parser::{parse_document, yaml_mapping_to_json};
use crate::operations::{
    atomic_rename_noclobber, ensure_no_symlink_components, ensure_regular_record_file,
    ensure_revision, mutation_record_path,
};
use crate::Collection;

impl Collection {
    /// Rename a file (§12.5).
    pub fn rename(&self, input: &serde_json::Value) -> serde_json::Value {
        let input = match RenameInput::parse(input) {
            Ok(parsed) => parsed,
            Err(err) => return err,
        };
        let RenameInput {
            from,
            to,
            update_refs,
            dry_run,
            last_known_mtime,
            if_revision,
            mut simulate_before_ref_update,
            last_known_ref_mtimes,
        } = input;
        let from = match mutation_record_path(self, &from) {
            Ok(path) => path.to_string(),
            Err(error) => return error,
        };
        let to = match mutation_record_path(self, &to) {
            Ok(path) => path.to_string(),
            Err(error) => return error,
        };
        if let Err(error) = ensure_no_symlink_components(&self.root, &from, self.spec_profile) {
            return error;
        }
        if let Err(error) = ensure_no_symlink_components(&self.root, &to, self.spec_profile) {
            return error;
        }
        for simulated in &mut simulate_before_ref_update {
            simulated.path = match mutation_record_path(self, &simulated.path) {
                Ok(path) => path.to_string(),
                Err(error) => return error,
            };
            if let Err(error) =
                ensure_no_symlink_components(&self.root, &simulated.path, self.spec_profile)
            {
                return error;
            }
        }
        let _write_lock = if dry_run {
            None
        } else {
            match crate::transactions::WriteLock::acquire(self) {
                Ok(write_lock) => Some(write_lock),
                Err(error) => return op_error(error.code(), &error.to_string()),
            }
        };

        let from_path = self.root.join(&from);
        let to_path = self.root.join(&to);

        if let Err(error) = ensure_regular_record_file(&from_path, &from) {
            return error;
        }
        if !from_path.exists() {
            return op_error(FILE_NOT_FOUND, &format!("Source not found: {}", from));
        }

        if to_path.exists() {
            return op_error(PATH_CONFLICT, &format!("Target already exists: {}", to));
        }

        if let Err(error) = ensure_revision(&from_path, &from, if_revision.as_deref()) {
            return error;
        }

        // A dry run must not create the destination folder.
        if !dry_run {
            if let Some(parent) = to_path.parent() {
                if let Err(error) = std::fs::create_dir_all(parent) {
                    return op_error(
                        "io_error",
                        &format!("Failed to create target folder: {error}"),
                    );
                }
            }
        }
        if let Err(error) = ensure_no_symlink_components(&self.root, &to, self.spec_profile) {
            return error;
        }

        // Concurrent modification detection for source file
        if let Some(known_ms) = last_known_mtime {
            if let Ok(meta) = std::fs::metadata(&from_path) {
                if let Ok(mtime) = meta.modified() {
                    let current_ms = mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    if current_ms != known_ms {
                        return op_error(
                            CONCURRENT_MODIFICATION,
                            &format!("File '{}' was modified externally", from),
                        );
                    }
                }
            }
        }

        // Read the source file's id before rename (for id-stability check)
        let source_id = std::fs::read_to_string(&from_path)
            .ok()
            .and_then(|content| {
                let doc = parse_document(&content);
                if let Some(serde_yaml::Value::Mapping(m)) = &doc.frontmatter {
                    let json = yaml_mapping_to_json(m);
                    json.get(&self.settings.id_field)
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            });

        let update_refs = update_refs.unwrap_or(self.settings.rename_update_refs);
        if dry_run {
            let mut references_affected: Vec<serde_json::Value> = Vec::new();
            let mut warnings: Vec<serde_json::Value> = Vec::new();
            let mut ignored_failures: Vec<serde_json::Value> = Vec::new();
            if update_refs {
                self.update_references_after_rename_with_mtime(
                    &from,
                    &to,
                    &source_id,
                    &mut references_affected,
                    &mut warnings,
                    &mut ignored_failures,
                    &std::collections::HashMap::new(),
                    &std::collections::HashMap::new(),
                    true,
                );
            }
            let mut result = serde_json::json!({
                "from": from,
                "to": to,
                "dry_run": true,
                "would_rename": true,
            });
            if !references_affected.is_empty() {
                result["references_affected"] = serde_json::Value::Array(references_affected);
            }
            if !warnings.is_empty() {
                result["warnings"] = serde_json::Value::Array(warnings);
            }
            return result;
        }

        if let Err(error) = ensure_revision(&from_path, &from, if_revision.as_deref()) {
            return error;
        }
        if let Err(error) = ensure_no_symlink_components(&self.root, &from, self.spec_profile) {
            return error;
        }
        if let Err(e) = atomic_rename_noclobber(&from_path, &to_path) {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                return op_error(PATH_CONFLICT, &format!("Target already exists: {to}"));
            }
            let error_str = e.to_string();
            if error_str.contains("NUL") || error_str.contains("null") {
                return op_error(INVALID_PATH, &format!("Invalid path: {}", e));
            }
            return op_error("io_error", &format!("Failed to rename: {}", e));
        }

        // Determine if we should update references
        let mut references_updated: Vec<serde_json::Value> = Vec::new();
        let mut warnings: Vec<serde_json::Value> = Vec::new();
        let mut ref_update_failures: Vec<serde_json::Value> = Vec::new();

        if update_refs {
            // Apply simulate.external_modify with timing: before_ref_update
            // This is passed as "simulate_before_ref_update" in the input
            for sim in &simulate_before_ref_update {
                let full = self.root.join(&sim.path);
                if let Some(parent) = full.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = crate::operations::atomic_write(&full, sim.content.as_bytes());
                // Always bump mtime forward by 1 second to guarantee it
                // differs from the pre-simulate value recorded by the
                // test runner, regardless of filesystem granularity.
                if let Ok(meta) = std::fs::metadata(&full) {
                    if let Ok(cur) = meta.modified() {
                        let bumped = cur + std::time::Duration::from_secs(1);
                        let times = std::fs::FileTimes::new().set_modified(bumped);
                        if let Ok(f) = std::fs::File::options().write(true).open(&full) {
                            let _ = f.set_times(times);
                        }
                    }
                }
            }

            // Record mtimes of all collection files before updating refs
            let files = self.scan_collection_files();
            let mut file_mtimes: std::collections::HashMap<String, std::time::SystemTime> =
                std::collections::HashMap::new();
            for file_path in &files {
                if let Ok(meta) = std::fs::metadata(file_path) {
                    if let Ok(mtime) = meta.modified() {
                        let rel = file_path
                            .strip_prefix(&self.root)
                            .map(|p| p.to_string_lossy().to_string().replace('\\', "/"))
                            .unwrap_or_default();
                        file_mtimes.insert(rel, mtime);
                    }
                }
            }

            // Also check for last_known_ref_mtimes provided by the test runner
            let ref_mtime_overrides = last_known_ref_mtimes.clone();

            self.update_references_after_rename_with_mtime(
                &from,
                &to,
                &source_id,
                &mut references_updated,
                &mut warnings,
                &mut ref_update_failures,
                &file_mtimes,
                &ref_mtime_overrides,
                false,
            );
        }

        let mut result = serde_json::json!({
            "from": from,
            "to": to,
        });
        if !references_updated.is_empty() {
            result["references_updated"] = serde_json::Value::Array(references_updated);
        }
        if !warnings.is_empty() {
            result["warnings"] = serde_json::Value::Array(warnings);
        }
        if !ref_update_failures.is_empty() {
            result["error"] = serde_json::json!({
                "code": RENAME_REF_UPDATE_FAILED,
                "message": "Some reference updates failed due to concurrent modification",
            });
            result["partial_updates"] = serde_json::json!({
                "failed": ref_update_failures,
            });
        }
        result
    }

    /// Update references in all collection files after a rename (with mtime checking).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update_references_after_rename_with_mtime(
        &self,
        from: &str,
        to: &str,
        source_id: &Option<String>,
        references_updated: &mut Vec<serde_json::Value>,
        warnings: &mut Vec<serde_json::Value>,
        ref_update_failures: &mut Vec<serde_json::Value>,
        recorded_mtimes: &std::collections::HashMap<String, std::time::SystemTime>,
        mtime_overrides: &std::collections::HashMap<String, u64>,
        dry_run: bool,
    ) {
        let from_stem = std::path::Path::new(from)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let to_stem = std::path::Path::new(to)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let from_no_ext = from
            .strip_suffix(".md")
            .or_else(|| from.strip_suffix(".mdx"))
            .unwrap_or(from);
        let to_no_ext = to
            .strip_suffix(".md")
            .or_else(|| to.strip_suffix(".mdx"))
            .unwrap_or(to);
        let ambiguous_stem_counts = self.collect_wikilink_stem_counts(from, !dry_run);

        let files = self.scan_collection_files();

        for file_path in &files {
            let rel_path = match file_path.strip_prefix(&self.root) {
                Ok(p) => p.to_string_lossy().to_string().replace('\\', "/"),
                Err(_) => continue,
            };

            // Skip the old path (doesn't exist anymore)
            if rel_path == from {
                continue;
            }

            let content = match std::fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let doc = parse_document(&content);
            let source_dir = std::path::Path::new(&rel_path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();

            let mut fm_changed = false;
            let mut body_changed = false;
            let mut pending_updates: Vec<serde_json::Value> = Vec::new();
            let mut fm_yaml = match &doc.frontmatter {
                Some(v @ serde_yaml::Value::Mapping(_)) => Some(v.clone()),
                _ => None,
            };

            // Update frontmatter link fields (if mapping frontmatter exists).
            if let Some(fm) = fm_yaml.as_mut() {
                self.update_fm_links(
                    fm,
                    from,
                    to,
                    &from_stem,
                    &to_stem,
                    from_no_ext,
                    to_no_ext,
                    &source_dir,
                    &rel_path,
                    source_id,
                    &ambiguous_stem_counts,
                    &mut fm_changed,
                    &mut pending_updates,
                    warnings,
                );
            }

            // Update body links
            let mut new_body = doc.body.clone();
            if self.update_body_links(
                &mut new_body,
                from,
                to,
                &from_stem,
                &to_stem,
                from_no_ext,
                to_no_ext,
                &source_dir,
            ) {
                body_changed = true;
                pending_updates.push(serde_json::json!({
                    "path": rel_path,
                    "location": "body",
                }));
            }

            // Write back if changed
            if fm_changed || body_changed {
                if dry_run {
                    references_updated.extend(pending_updates);
                    continue;
                }
                // Check for concurrent modification before writing
                let mtime_conflict = if let Some(&override_ms) = mtime_overrides.get(&rel_path) {
                    // Use test-provided override mtime
                    if let Ok(meta) = std::fs::metadata(file_path) {
                        if let Ok(current) = meta.modified() {
                            let current_ms = current
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                            current_ms != override_ms
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else if let Some(recorded) = recorded_mtimes.get(&rel_path) {
                    // Use recorded mtime from before ref updates
                    if let Ok(meta) = std::fs::metadata(file_path) {
                        if let Ok(current) = meta.modified() {
                            current != *recorded
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };

                if mtime_conflict {
                    ref_update_failures.push(serde_json::json!({
                        "path": rel_path,
                        "reason": "concurrent_modification",
                    }));
                    continue;
                }

                let fm_to_write: Option<&serde_yaml::Value> = if fm_changed {
                    fm_yaml.as_ref()
                } else {
                    match &doc.frontmatter {
                        Some(v @ serde_yaml::Value::Mapping(_)) => Some(v),
                        _ => None,
                    }
                };

                let output = if let Some(new_fm) = fm_to_write {
                    let mut output = String::new();
                    output.push_str("---\n");
                    let yaml_str = serde_yaml::to_string(new_fm).unwrap_or_default();
                    output.push_str(&yaml_str);
                    if !yaml_str.ends_with('\n') {
                        output.push('\n');
                    }
                    output.push_str("---\n");
                    if !new_body.is_empty() {
                        output.push_str(&new_body);
                        if !new_body.ends_with('\n') {
                            output.push('\n');
                        }
                    }
                    output
                } else {
                    let mut output = new_body;
                    if !output.is_empty() && !output.ends_with('\n') {
                        output.push('\n');
                    }
                    output
                };
                if let Err(e) = crate::operations::atomic_write(file_path, output.as_bytes()) {
                    ref_update_failures.push(serde_json::json!({
                        "path": rel_path,
                        "reason": "io_error",
                        "message": e.to_string(),
                    }));
                    continue;
                }
                references_updated.extend(pending_updates);
            }
        }
    }
}
