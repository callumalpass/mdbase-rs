use crate::Collection;

#[cfg(test)]
use super::hooks::{apply_injected_reference_removal, take_injected_reference_open_failure};
use super::planner::ReferenceRewritePlan;

impl Collection {
    pub(crate) fn execute_reference_rewrites(
        &self,
        plans: Vec<ReferenceRewritePlan>,
        mtime_overrides: &std::collections::HashMap<String, u64>,
        references_updated: &mut Vec<serde_json::Value>,
        failures: &mut Vec<serde_json::Value>,
    ) {
        for plan in plans {
            let full_path = self.root.join(&plan.execution_path);
            #[cfg(test)]
            apply_injected_reference_removal(&full_path);
            #[cfg(test)]
            let injected_open_failure = take_injected_reference_open_failure(&full_path);
            #[cfg(test)]
            if injected_open_failure {
                crate::operations::set_record_open_failure(
                    &self.root,
                    &plan.execution_path,
                    Some(std::io::ErrorKind::PermissionDenied),
                );
            }
            let current_result =
                crate::record_load::load_record_no_follow(self, &plan.execution_path);
            #[cfg(test)]
            if injected_open_failure {
                crate::operations::set_record_open_failure(&self.root, &plan.execution_path, None);
            }
            let current = match current_result {
                Ok(Some(current)) => current,
                Ok(None) => {
                    failures.push(serde_json::json!({
                        "path": plan.execution_path,
                        "reason": "io_error",
                        "message": "Reference record is no longer available",
                    }));
                    continue;
                }
                Err(error) => {
                    failures.push(serde_json::json!({
                        "path": plan.execution_path,
                        "reason": "io_error",
                        "message": error.to_string(),
                    }));
                    continue;
                }
            };
            if current.facts().revision != plan.expected_revision {
                failures.push(serde_json::json!({
                    "path": plan.execution_path,
                    "reason": "concurrent_modification",
                }));
                continue;
            }
            let mtime_conflict =
                if let Some(override_ms) = mtime_overrides.get(&plan.execution_path) {
                    current.facts().mtime_ns.max(0) as u64 / 1_000_000 != *override_ms
                } else {
                    current.facts().mtime_ns != plan.expected_mtime_ns
                };
            if mtime_conflict {
                failures.push(serde_json::json!({
                    "path": plan.execution_path,
                    "reason": "concurrent_modification",
                }));
                continue;
            }
            if !self.rename_root_path_is_current() {
                failures.push(serde_json::json!({
                    "path": plan.execution_path,
                    "reason": "io_error",
                    "message": "Collection root was replaced during rename",
                }));
                continue;
            }
            if let Err(error) = crate::operations::atomic_write_in_prepared_parent(
                &full_path,
                plan.output.as_bytes(),
            ) {
                failures.push(serde_json::json!({
                    "path": plan.execution_path,
                    "reason": "io_error",
                    "message": error.to_string(),
                }));
                continue;
            }
            references_updated.extend(plan.updates);
        }
    }

    pub(crate) fn rename_root_path_is_current(&self) -> bool {
        let held = self
            .root_capability()
            .and_then(|directory| same_file::Handle::from_file(directory.into_std_file()));
        let current = same_file::Handle::from_path(&self.root);
        matches!((held, current), (Ok(held), Ok(current)) if held == current)
    }
}
