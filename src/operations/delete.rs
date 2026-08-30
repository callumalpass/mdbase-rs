//! Delete operation (§12.4).

use crate::api::operations::{DeleteInput, DeleteOutput};
use crate::api::{DeleteRequest, Revision};
use crate::errors::*;
use crate::mutation::{PlannedDelete, PreparationOptions, PreparedDelete};
use crate::Collection;

impl Collection {
    /// Legacy JSON delete edge. Canonical callers use the typed mutation service.
    pub fn delete(&self, input: &serde_json::Value) -> serde_json::Value {
        #[cfg(test)]
        crate::mutation::probe_legacy_parse();
        let input = match DeleteInput::parse(input) {
            Ok(parsed) => parsed,
            Err(error) => return error,
        };
        let path = match crate::operations::mutation_record_path_diagnostic(self, &input.path) {
            Ok(path) => path,
            Err(error) => return serde_json::json!({"error": error}),
        };
        let if_revision = match input
            .if_revision
            .as_deref()
            .map(Revision::parse)
            .transpose()
        {
            Ok(revision) => revision,
            Err(_) => {
                return op_error(
                    CONCURRENT_MODIFICATION,
                    &format!("File '{}' was modified externally", input.path),
                )
            }
        };
        let request = DeleteRequest {
            path,
            check_backlinks: input.check_backlinks,
            if_revision,
        };
        let prepared = match crate::mutation::prepare_delete(
            self,
            request,
            PreparationOptions {
                create_document: None,
                dry_run: input.dry_run,
            },
            input.last_known_mtime,
        ) {
            Ok(prepared) => prepared,
            Err(diagnostics) => return mutation_failure_json(diagnostics),
        };
        match self.delete_planned(prepared) {
            Ok(planned) => DeleteOutput {
                path: planned.path.to_string(),
                deleted: planned.deleted,
                dry_run: !planned.deleted,
                broken_links: planned.broken_links,
            }
            .into_json(),
            Err(failure) => mutation_failure_json(failure.diagnostics),
        }
    }

    pub(crate) fn delete_planned(
        &self,
        prepared: PreparedDelete,
    ) -> Result<PlannedDelete, crate::mutation::MutationFailure> {
        let PreparedDelete {
            request,
            dry_run,
            before_revision,
            before_frontmatter,
            before_body,
            types,
            broken_links,
            legacy_last_known_mtime,
        } = prepared;
        let path = request.path;
        let display_path = path.to_string();
        let full_path = path.under(&self.root);
        let loaded = crate::record_load::load_record(self, &display_path).map_err(|error| {
            crate::mutation::MutationFailure::operation(
                if error.kind() == std::io::ErrorKind::NotFound {
                    FILE_NOT_FOUND
                } else {
                    "file_read_failed"
                },
                if error.kind() == std::io::ErrorKind::NotFound {
                    format!("File not found: {display_path}")
                } else {
                    "Record could not be read.".to_string()
                },
            )
        })?;
        if loaded.facts().revision != before_revision
            || request
                .if_revision
                .as_ref()
                .is_some_and(|expected| expected.as_str() != loaded.facts().revision)
        {
            return Err(crate::mutation::MutationFailure::operation(
                CONCURRENT_MODIFICATION,
                format!("File '{display_path}' was modified externally"),
            ));
        }
        if let Some(known_ms) = legacy_last_known_mtime {
            let current_ms = std::fs::metadata(&full_path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as u64);
            if current_ms.is_some_and(|current| current != known_ms) {
                return Err(crate::mutation::MutationFailure::operation(
                    CONCURRENT_MODIFICATION,
                    format!("File '{display_path}' was modified externally"),
                ));
            }
        }

        if !dry_run {
            self.held_root()
                .remove_file(&path.to_path_buf())
                .map_err(|error| {
                    crate::mutation::MutationFailure::operation(
                        "io_error",
                        format!("Failed to delete: {error}"),
                    )
                })?;
        }

        Ok(PlannedDelete {
            path,
            before_revision,
            before_frontmatter,
            before_body,
            types,
            broken_links,
            deleted: !dry_run,
        })
    }
}

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
