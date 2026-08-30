//! Rename planning and publication (§12.5).

mod body;
mod frontmatter;
#[cfg(test)]
pub(super) mod hooks;
mod link_rewrite;
mod planner;
mod publication;
#[cfg(all(test, feature = "legacy-collection-mutation"))]
mod tests;

pub(crate) use planner::ReferenceRewritePlan;

#[cfg(feature = "legacy-collection-mutation")]
use crate::api::operations::RenameInput;
#[cfg(feature = "legacy-collection-mutation")]
use crate::api::{RenameRequest, Revision};
use crate::errors::*;
#[cfg(feature = "legacy-collection-mutation")]
use crate::mutation::PreparationOptions;
use crate::mutation::{PlannedRecord, PlannedRename, PreparedRename};
use crate::operations::{
    atomic_rename_noclobber, atomic_write_in_prepared_parent,
    ensure_no_symlink_components_diagnostic, ensure_regular_record_file_diagnostic,
    prepare_record_parent_no_follow,
};
use crate::Collection;

#[cfg(test)]
use hooks::apply_injected_root_replacement;

impl Collection {
    #[cfg(feature = "legacy-collection-mutation")]
    pub(crate) fn rename_legacy(&self, input: &serde_json::Value) -> serde_json::Value {
        #[cfg(test)]
        crate::mutation::probe_legacy_parse();
        let input = match RenameInput::parse(input) {
            Ok(parsed) => parsed,
            Err(error) => return error,
        };
        let from = match crate::operations::mutation_record_path(self, &input.from) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let to = match crate::operations::mutation_record_path(self, &input.to) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let if_revision = match input
            .if_revision
            .as_deref()
            .map(Revision::parse)
            .transpose()
        {
            Ok(value) => value,
            Err(_) => {
                return crate::errors::op_error(
                    CONCURRENT_MODIFICATION,
                    &format!("File '{}' was modified externally", input.from),
                )
            }
        };
        let simulations = input
            .simulate_before_ref_update
            .into_iter()
            .map(|simulation| {
                crate::operations::mutation_record_path(self, &simulation.path)
                    .map(|path| (path, simulation.content))
            })
            .collect::<Result<Vec<_>, _>>();
        let simulations = match simulations {
            Ok(value) => value,
            Err(error) => return error,
        };
        for (path, _) in &simulations {
            if let Err(error) = crate::operations::ensure_no_symlink_components(
                &self.root,
                path.as_str(),
                self.spec_profile,
            ) {
                return error;
            }
        }
        let request = RenameRequest {
            from,
            to,
            update_refs: input
                .update_refs
                .unwrap_or(self.settings.rename_update_refs),
            if_revision,
            include_document: false,
        };
        let _write_lock = if input.dry_run {
            None
        } else {
            match crate::transactions::WriteLock::acquire(self) {
                Ok(lock) => Some(lock),
                Err(error) => return crate::errors::op_error(error.code(), &error.to_string()),
            }
        };
        let prepared = match crate::mutation::prepare_rename(
            self,
            request,
            PreparationOptions {
                create_document: None,
                dry_run: input.dry_run,
            },
            input.last_known_mtime,
            input.last_known_ref_mtimes,
            simulations,
        ) {
            Ok(prepared) => prepared,
            Err(diagnostics) => return mutation_failure_json(diagnostics),
        };
        match self.rename_planned(prepared) {
            Ok(planned) => planned_legacy_result(&planned),
            Err(failure) => mutation_failure_json(failure.diagnostics),
        }
    }

    pub(crate) fn rename_planned(
        &self,
        prepared: PreparedRename,
    ) -> Result<PlannedRename, crate::mutation::MutationFailure> {
        let PreparedRename {
            request,
            dry_run,
            source_revision,
            source_types,
            source_id,
            source_frontmatter,
            source_effective_frontmatter,
            source_body,
            source_bytes,
            reference_plans,
            warnings,
            mut reference_failures,
            legacy_ref_mtimes,
            legacy_simulations,
        } = prepared;
        let from = request.from.to_string();
        let to = request.to.to_string();
        let planned_reference_writes = reference_plans.clone();
        let references_affected = reference_plans
            .iter()
            .flat_map(|plan| plan.updates.iter().cloned())
            .collect::<Vec<_>>();
        let mut destination_bytes = reference_plans
            .iter()
            .find(|plan| plan.execution_path == to)
            .map(|plan| plan.output.as_bytes().to_vec())
            .unwrap_or_else(|| source_bytes.clone());

        if !dry_run {
            #[cfg(test)]
            apply_injected_root_replacement(&self.root);
            ensure_no_symlink_components_diagnostic(&self.root, &from, self.spec_profile)
                .map_err(crate::mutation::MutationFailure::diagnostic)?;
            ensure_no_symlink_components_diagnostic(&self.root, &to, self.spec_profile)
                .map_err(crate::mutation::MutationFailure::diagnostic)?;
            ensure_regular_record_file_diagnostic(&request.from.under(&self.root), &from)
                .map_err(crate::mutation::MutationFailure::diagnostic)?;
            if !self.rename_root_path_is_current() {
                return Err(crate::mutation::MutationFailure::operation(
                    CONCURRENT_MODIFICATION,
                    "Collection root was replaced during rename",
                ));
            }
            prepare_record_parent_no_follow(self, &request.to).map_err(|error| {
                crate::mutation::MutationFailure::operation(
                    "io_error",
                    format!("Failed to prepare target folder safely: {error}"),
                )
            })?;
            let current = crate::record_load::load_record_no_follow(self, &from)
                .ok()
                .flatten()
                .ok_or_else(|| {
                    crate::mutation::MutationFailure::operation(
                        CONCURRENT_MODIFICATION,
                        format!("File '{from}' was modified externally"),
                    )
                })?;
            if current.facts().revision != source_revision
                || request
                    .if_revision
                    .as_ref()
                    .is_some_and(|expected| expected.as_str() != current.facts().revision)
            {
                return Err(crate::mutation::MutationFailure::operation(
                    CONCURRENT_MODIFICATION,
                    format!("File '{from}' was modified externally"),
                ));
            }
            if !self.rename_root_path_is_current() {
                return Err(crate::mutation::MutationFailure::operation(
                    CONCURRENT_MODIFICATION,
                    "Collection root was replaced during rename",
                ));
            }
            atomic_rename_noclobber(
                &request.from.under(&self.root),
                &request.to.under(&self.root),
            )
            .map_err(|error| {
                crate::mutation::MutationFailure::operation(
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        PATH_CONFLICT
                    } else {
                        "io_error"
                    },
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        format!("Target already exists: {to}")
                    } else {
                        format!("Failed to rename: {error}")
                    },
                )
            })?;

            if request.update_refs {
                for (path, content) in legacy_simulations {
                    if prepare_record_parent_no_follow(self, &path).is_ok() {
                        let _ = atomic_write_in_prepared_parent(
                            &path.under(&self.root),
                            content.as_bytes(),
                        );
                    }
                }
            }
            let planned_destination = destination_bytes.clone();
            let mut references_updated = Vec::new();
            if request.update_refs {
                self.execute_reference_rewrites(
                    reference_plans,
                    &legacy_ref_mtimes,
                    &mut references_updated,
                    &mut reference_failures,
                );
            }
            if reference_failures
                .iter()
                .any(|failure| failure.get("path").and_then(serde_json::Value::as_str) == Some(&to))
            {
                destination_bytes = source_bytes;
            } else {
                destination_bytes = planned_destination;
            }
            let destination_body = planned_body(&destination_bytes, &source_body);
            return Ok(PlannedRename {
                from: request.from,
                to: request.to.clone(),
                source_revision: source_revision.clone(),
                source_types: source_types.clone(),
                source_id,
                destination: PlannedRecord {
                    path: request.to,
                    types: source_types,
                    frontmatter: source_frontmatter,
                    effective_frontmatter: source_effective_frontmatter,
                    body: destination_body,
                    bytes: destination_bytes,
                    diagnostics: Vec::new(),
                    before_revision: Some(source_revision),
                    include_document: request.include_document,
                },
                reference_plans: planned_reference_writes,
                references_affected,
                references_updated,
                warnings,
                reference_failures,
                dry_run: false,
            });
        }

        let destination_body = planned_body(&destination_bytes, &source_body);
        Ok(PlannedRename {
            from: request.from,
            to: request.to.clone(),
            source_revision: source_revision.clone(),
            source_types: source_types.clone(),
            source_id,
            destination: PlannedRecord {
                path: request.to,
                types: source_types,
                frontmatter: source_frontmatter,
                effective_frontmatter: source_effective_frontmatter,
                body: destination_body,
                bytes: destination_bytes,
                diagnostics: Vec::new(),
                before_revision: Some(source_revision),
                include_document: request.include_document,
            },
            reference_plans: planned_reference_writes,
            references_affected,
            references_updated: Vec::new(),
            warnings,
            reference_failures,
            dry_run: true,
        })
    }
}

fn planned_body(bytes: &[u8], fallback: &str) -> String {
    std::str::from_utf8(bytes)
        .ok()
        .map(crate::frontmatter::parser::parse_document_for_rewrite)
        .map(|(document, _)| document.body)
        .unwrap_or_else(|| fallback.to_string())
}

#[cfg(feature = "legacy-collection-mutation")]
fn planned_legacy_result(planned: &PlannedRename) -> serde_json::Value {
    let mut result = if planned.dry_run {
        serde_json::json!({
            "from": planned.from,
            "to": planned.to,
            "dry_run": true,
            "would_rename": true,
        })
    } else {
        serde_json::json!({"from": planned.from, "to": planned.to})
    };
    let references = if planned.dry_run {
        &planned.references_affected
    } else {
        &planned.references_updated
    };
    if !references.is_empty() {
        result[if planned.dry_run {
            "references_affected"
        } else {
            "references_updated"
        }] = serde_json::Value::Array(references.clone());
    }
    if !planned.warnings.is_empty() {
        result["warnings"] = serde_json::Value::Array(planned.warnings.clone());
    }
    if !planned.reference_failures.is_empty() {
        result["error"] = serde_json::json!({
            "code": RENAME_REF_UPDATE_FAILED,
            "message": if planned.dry_run {
                "Some reference updates could not be prepared"
            } else {
                "Some reference updates failed"
            },
        });
        result["partial_updates"] = serde_json::json!({"failed": planned.reference_failures});
    }
    result
}

#[cfg(feature = "legacy-collection-mutation")]
fn mutation_failure_json(diagnostics: Vec<crate::diagnostic::Diagnostic>) -> serde_json::Value {
    if diagnostics.len() == 1 {
        serde_json::json!({"error": diagnostics.into_iter().next().unwrap()})
    } else {
        serde_json::json!({"error": {
            "code": VALIDATION_FAILED,
            "message": "Validation failed",
            "issues": diagnostics,
        }})
    }
}
