//! Rename with reference updates (§12.5).

mod body;
mod frontmatter;
#[cfg(test)]
pub(super) mod hooks;
mod link_rewrite;
mod planner;
mod publication;
#[cfg(test)]
mod tests;

use crate::api::operations::RenameInput;
use crate::errors::*;
use crate::operations::{
    atomic_rename_noclobber, atomic_write_in_prepared_parent, ensure_no_symlink_components,
    ensure_regular_record_file, ensure_revision, mutation_record_path,
    prepare_record_parent_no_follow,
};
use crate::Collection;

#[cfg(test)]
use hooks::apply_injected_root_replacement;

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
        let to_collection_path = match mutation_record_path(self, &to) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let to = to_collection_path.to_string();
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

        let snapshot = match self.capture_collection_snapshot(&crate::OperationCancellation::new())
        {
            Ok(snapshot) => snapshot,
            Err(error) => return op_error("collection_snapshot_failed", &error.to_string()),
        };

        let from_path = self.root.join(&from);
        let to_path = self.root.join(&to);
        if let Err(error) = ensure_regular_record_file(&from_path, &from) {
            return error;
        }
        let Some(source_entry) = snapshot.entry(&from) else {
            return op_error(FILE_NOT_FOUND, &format!("Source not found: {from}"));
        };
        if to_path.exists() {
            return op_error(PATH_CONFLICT, &format!("Target already exists: {to}"));
        }
        if if_revision
            .as_deref()
            .is_some_and(|expected| expected != source_entry.facts().revision)
        {
            return op_error(
                CONCURRENT_MODIFICATION,
                &format!("File '{from}' no longer matches the requested revision"),
            );
        }
        if let Some(known_ms) = last_known_mtime {
            let captured_ms = source_entry.facts().mtime_ns.max(0) as u64 / 1_000_000;
            if captured_ms != known_ms {
                return op_error(
                    CONCURRENT_MODIFICATION,
                    &format!("File '{from}' was modified externally"),
                );
            }
        }

        let source_id = source_entry
            .raw_frontmatter()
            .and_then(serde_json::Value::as_object)
            .and_then(|frontmatter| frontmatter.get(&self.settings.id_field))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let update_refs = update_refs.unwrap_or(self.settings.rename_update_refs);
        let mut warnings = Vec::new();
        let mut ref_update_failures = Vec::new();
        let plans = if update_refs {
            self.plan_reference_rewrites(
                &snapshot,
                &from,
                &to,
                &source_id,
                &mut warnings,
                &mut ref_update_failures,
            )
        } else {
            Vec::new()
        };
        #[cfg(test)]
        apply_injected_root_replacement(&self.root);

        if dry_run {
            let references_affected = plans
                .iter()
                .flat_map(|plan| plan.updates.iter().cloned())
                .collect::<Vec<_>>();
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
            if !ref_update_failures.is_empty() {
                result["error"] = serde_json::json!({
                    "code": RENAME_REF_UPDATE_FAILED,
                    "message": "Some reference updates could not be prepared",
                });
                result["partial_updates"] = serde_json::json!({"failed": ref_update_failures});
            }
            return result;
        }

        if !self.rename_root_path_is_current() {
            return op_error(
                CONCURRENT_MODIFICATION,
                "Collection root was replaced during rename",
            );
        }
        if let Err(error) = prepare_record_parent_no_follow(self, &to_collection_path) {
            return op_error(
                "io_error",
                &format!("Failed to prepare target folder safely: {error}"),
            );
        }
        if let Err(error) = ensure_revision(&from_path, &from, if_revision.as_deref()) {
            return error;
        }
        let current_source = match crate::record_load::load_record_no_follow(self, &from) {
            Ok(Some(current)) => current,
            Ok(None) | Err(_) => {
                return op_error(
                    CONCURRENT_MODIFICATION,
                    &format!("File '{from}' was modified externally"),
                )
            }
        };
        if current_source.facts().revision != source_entry.facts().revision {
            return op_error(
                CONCURRENT_MODIFICATION,
                &format!("File '{from}' was modified externally"),
            );
        }
        if let Err(error) = ensure_no_symlink_components(&self.root, &from, self.spec_profile) {
            return error;
        }
        // The publication primitive below is ambient for cross-platform
        // no-clobber semantics, so fence the public root again immediately
        // before it is invoked.
        if !self.rename_root_path_is_current() {
            return op_error(
                CONCURRENT_MODIFICATION,
                "Collection root was replaced during rename",
            );
        }
        if let Err(e) = atomic_rename_noclobber(&from_path, &to_path) {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                return op_error(PATH_CONFLICT, &format!("Target already exists: {to}"));
            }
            let error_str = e.to_string();
            if error_str.contains("NUL") || error_str.contains("null") {
                return op_error(INVALID_PATH, &format!("Invalid path: {e}"));
            }
            return op_error("io_error", &format!("Failed to rename: {e}"));
        }

        if update_refs {
            for sim in &simulate_before_ref_update {
                let full = self.root.join(&sim.path);
                let prepared = crate::api::CollectionPath::new(&sim.path)
                    .ok()
                    .filter(|path| {
                        self.rename_root_path_is_current()
                            && prepare_record_parent_no_follow(self, path).is_ok()
                    })
                    .is_some_and(|_| self.rename_root_path_is_current());
                if !prepared {
                    continue;
                }
                let _ = atomic_write_in_prepared_parent(&full, sim.content.as_bytes());
                if let Ok(meta) = std::fs::metadata(&full) {
                    if let Ok(cur) = meta.modified() {
                        let times = std::fs::FileTimes::new()
                            .set_modified(cur + std::time::Duration::from_secs(1));
                        if let Ok(file) = std::fs::File::options().write(true).open(&full) {
                            let _ = file.set_times(times);
                        }
                    }
                }
            }
        }

        let mut references_updated = Vec::new();
        if update_refs {
            self.execute_reference_rewrites(
                plans,
                &last_known_ref_mtimes,
                &mut references_updated,
                &mut ref_update_failures,
            );
        }

        let mut result = serde_json::json!({"from": from, "to": to});
        if !references_updated.is_empty() {
            result["references_updated"] = serde_json::Value::Array(references_updated);
        }
        if !warnings.is_empty() {
            result["warnings"] = serde_json::Value::Array(warnings);
        }
        if !ref_update_failures.is_empty() {
            result["error"] = serde_json::json!({
                "code": RENAME_REF_UPDATE_FAILED,
                "message": "Some reference updates failed",
            });
            result["partial_updates"] = serde_json::json!({"failed": ref_update_failures});
        }
        result
    }
}
