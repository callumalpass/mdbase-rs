//! Update operation (§12.3).

use crate::api::operations::{UpdateInput, UpdateOutput};
use crate::errors::*;
use crate::frontmatter::parser::{parse_document_for_rewrite, yaml_mapping_to_json};
use crate::frontmatter::serializer;
use crate::operations::{
    atomic_write, ensure_no_symlink_components, ensure_regular_record_file, mutation_record_path,
};
use crate::Collection;

pub(crate) struct PrevalidatedUpdate {
    pub expected_revision: String,
    pub raw_frontmatter: serde_json::Map<String, serde_json::Value>,
}

#[cfg(test)]
fn injected_publication_replacements(
) -> &'static std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf, std::path::PathBuf>> {
    static REPLACEMENTS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf, std::path::PathBuf>>,
    > = std::sync::OnceLock::new();
    REPLACEMENTS.get_or_init(Default::default)
}

#[cfg(test)]
fn injected_prevalidated_replacements(
) -> &'static std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf, std::path::PathBuf>> {
    static REPLACEMENTS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf, std::path::PathBuf>>,
    > = std::sync::OnceLock::new();
    REPLACEMENTS.get_or_init(Default::default)
}

#[cfg(test)]
pub(crate) fn inject_prevalidated_replacement(
    path: &std::path::Path,
    replacement: std::path::PathBuf,
) {
    injected_prevalidated_replacements()
        .lock()
        .expect("prevalidated replacement lock")
        .insert(path.to_path_buf(), replacement);
}

#[cfg(test)]
fn apply_injected_prevalidated_replacement(path: &std::path::Path) {
    let replacement = injected_prevalidated_replacements()
        .lock()
        .expect("prevalidated replacement lock")
        .remove(path);
    if let Some(replacement) = replacement {
        std::fs::rename(replacement, path).expect("injected prevalidated replacement");
    }
}

#[cfg(test)]
fn inject_publication_replacement(path: &std::path::Path, replacement: std::path::PathBuf) {
    injected_publication_replacements()
        .lock()
        .expect("publication replacement lock")
        .insert(path.to_path_buf(), replacement);
}

#[cfg(test)]
fn apply_injected_publication_replacement(path: &std::path::Path) {
    let replacement = injected_publication_replacements()
        .lock()
        .expect("publication replacement lock")
        .remove(path);
    if let Some(replacement) = replacement {
        std::fs::rename(replacement, path).expect("injected atomic publication replacement");
    }
}

impl Collection {
    /// Update a file (§12.3).
    pub fn update(&self, input: &serde_json::Value) -> serde_json::Value {
        self.update_prepared(input, None, true, None)
    }

    pub(crate) fn update_prevalidated(
        &self,
        input: &serde_json::Value,
        prepared: PrevalidatedUpdate,
    ) -> serde_json::Value {
        self.update_prepared(input, None, false, Some(prepared))
    }

    pub(crate) fn update_v03_prepared(
        &self,
        input: &serde_json::Value,
        membership: crate::v03::write_membership::ResolvedWriteMembership,
    ) -> serde_json::Value {
        self.update_prepared(input, Some(membership), true, None)
    }

    fn update_prepared(
        &self,
        input: &serde_json::Value,
        membership: Option<crate::v03::write_membership::ResolvedWriteMembership>,
        validate_collection: bool,
        prevalidated: Option<PrevalidatedUpdate>,
    ) -> serde_json::Value {
        let input = match UpdateInput::parse(input) {
            Ok(parsed) => parsed,
            Err(err) => return err,
        };
        let UpdateInput {
            path,
            fields,
            body: new_body,
            document,
            last_known_mtime,
            if_revision,
        } = input;
        let path = match mutation_record_path(self, &path) {
            Ok(path) => path,
            Err(error) => return error,
        };
        if let Err(error) =
            ensure_no_symlink_components(&self.root, path.as_str(), self.spec_profile)
        {
            return error;
        }
        let _write_lock = match crate::transactions::WriteLock::acquire(self) {
            Ok(write_lock) => write_lock,
            Err(error) => return op_error(error.code(), &error.to_string()),
        };

        let new_body = new_body.as_deref();
        let full_path = path.under(&self.root);
        if let Err(error) = ensure_regular_record_file(&full_path, path.as_str()) {
            return error;
        }

        // Load bytes, metadata, and revision from one open handle. The loaded
        // revision is also the mandatory publication precondition, including
        // when a legacy caller omitted `if_revision`.
        #[cfg(test)]
        if prevalidated.is_some() {
            apply_injected_prevalidated_replacement(&full_path);
        }
        let loaded = match crate::record_load::load_record(self, path.as_str()) {
            Ok(loaded) => loaded,
            Err(error) => return op_error(FILE_NOT_FOUND, &format!("Failed to read: {error}")),
        };
        let loaded_facts = loaded.facts().clone();
        if let Some(known_ms) = last_known_mtime {
            let current_ms = loaded_facts.mtime_ns.max(0) as u64 / 1_000_000;
            if current_ms != known_ms {
                return op_error(
                    CONCURRENT_MODIFICATION,
                    &format!("File '{}' was modified externally", path.as_str()),
                );
            }
        }
        if prevalidated
            .as_ref()
            .is_some_and(|prepared| prepared.expected_revision != loaded_facts.revision)
            || if_revision
                .as_deref()
                .is_some_and(|expected| expected != loaded_facts.revision)
        {
            return op_error(
                CONCURRENT_MODIFICATION,
                &format!("File '{}' was modified externally", path.as_str()),
            );
        }
        let replacement = document.as_deref().map(parse_document_for_rewrite);
        let existing = match loaded {
            crate::record_load::RecordLoadOutcome::Parsed {
                document, layout, ..
            } => {
                let had_bom = layout.had_bom();
                Some((layout.into_parsed_document(&document), had_bom))
            }
            crate::record_load::RecordLoadOutcome::Invalid {
                state: crate::record_load::InvalidRecordState::Frontmatter { document: raw, .. },
                ..
            } => Some(parse_document_for_rewrite(&raw)),
            crate::record_load::RecordLoadOutcome::Invalid {
                state: crate::record_load::InvalidRecordState::InvalidUtf8,
                ..
            } if replacement.is_some() => None,
            crate::record_load::RecordLoadOutcome::Invalid {
                state: crate::record_load::InvalidRecordState::InvalidUtf8,
                ..
            } => return op_error(INVALID_FRONTMATTER, "Invalid frontmatter: invalid_utf8"),
        };
        let replacement_doc = replacement.as_ref().map(|(doc, _)| doc);
        let replacement_mapping = match replacement_doc
            .as_ref()
            .and_then(|doc| doc.frontmatter.as_ref())
        {
            Some(serde_yaml::Value::Mapping(mapping)) => Some(mapping.clone()),
            Some(_) => {
                return op_error(
                    INVALID_FRONTMATTER,
                    "Replacement document frontmatter must be a YAML mapping",
                )
            }
            None if document.is_some() => Some(serde_yaml::Mapping::new()),
            None => None,
        };

        let existing_mapping = match replacement_mapping.as_ref() {
            Some(mapping) => mapping.clone(),
            None => match existing
                .as_ref()
                .and_then(|(document, _)| document.frontmatter.as_ref())
            {
                Some(serde_yaml::Value::Mapping(mapping)) => mapping.clone(),
                _ => serde_yaml::Mapping::new(),
            },
        };

        // Merge fields
        let merged = if document.is_some() {
            existing_mapping.clone()
        } else {
            serializer::merge_fields(&existing_mapping, &fields, &self.settings.write_nulls)
        };
        let merged_json = yaml_mapping_to_json(&merged);

        let proposed_raw = prevalidated
            .as_ref()
            .map(|prepared| serde_json::Value::Object(prepared.raw_frontmatter.clone()))
            .unwrap_or_else(|| merged_json.clone());
        // Canonical v0.3 callers carry the membership frozen before lifecycle.
        // Legacy callers retain historical inference unchanged.
        let type_names = membership.as_ref().map_or_else(
            || self.determine_types_for_path(&proposed_raw, Some(path.as_str())),
            |membership| membership.types().to_vec(),
        );
        let mut merged_obj = proposed_raw.as_object().cloned().unwrap_or_default();
        let has_generated = type_names.iter().any(|type_name| {
            self.types.get(type_name).is_some_and(|definition| {
                definition
                    .fields
                    .values()
                    .any(|field| field.generated.is_some())
            })
        });
        let operation_snapshot = if prevalidated.is_none()
            && (has_generated
                || (validate_collection && self.settings.default_validation == "error"))
        {
            match self.capture_collection_snapshot(&crate::OperationCancellation::new()) {
                Ok(snapshot) => Some(snapshot),
                Err(error) => return op_error("collection_snapshot_failed", &error.to_string()),
            }
        } else {
            None
        };
        if prevalidated.is_none() && has_generated {
            let mut generated = crate::generated::GeneratedValueContext::from_snapshot(
                self,
                operation_snapshot
                    .as_ref()
                    .expect("generated updates capture one snapshot"),
            );
            if let Err(error) = generated.apply_generated(
                self,
                &mut merged_obj,
                &type_names,
                false,
                Some(path.as_str()),
            ) {
                return op_error(error.code(), &error.to_string());
            }
        }

        // Apply defaults for effective frontmatter
        let effective = self.coerce_types(
            &self.apply_defaults(&serde_json::Value::Object(merged_obj.clone()), &type_names),
            &type_names,
        );

        // Validate
        if validate_collection && self.settings.default_validation == "error" {
            let validation_frontmatter = if self.spec_profile == crate::SpecProfile::V03 {
                &serde_json::Value::Object(merged_obj.clone())
            } else {
                &effective
            };
            let mut validation = self.validate(validation_frontmatter, &type_names, path.as_str());
            let collection_snapshot = operation_snapshot
                .as_ref()
                .expect("validated updates capture one snapshot");

            // Cross-file uniqueness checks for update
            let uniqueness_issues =
                self.check_uniqueness(&effective, &type_names, path.as_str(), collection_snapshot);
            validation.issues.extend(uniqueness_issues.iter().cloned());
            if !uniqueness_issues.is_empty() {
                validation.valid = false;
            }

            if !validation.valid {
                return validation_failed_error(&validation.issues);
            }
        }

        // Build frontmatter for writing (honor write_defaults/write_nulls)
        let mut write_obj = merged_obj.clone();
        if self.spec_profile != crate::SpecProfile::V03 && self.settings.write_defaults {
            if let Some(eff_map) = effective.as_object() {
                for (k, v) in eff_map {
                    if !write_obj.contains_key(k) {
                        write_obj.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        if self.settings.write_nulls == "omit" && document.is_none() {
            write_obj.retain(|_, v| !v.is_null());
        }
        if let Some(membership) = &membership {
            if let Err(diagnostics) = membership.revalidate(self, &write_obj, path.as_str()) {
                return crate::v03::write_membership::diagnostics_error(diagnostics);
            }
        }

        // Write file
        let write_mapping = serializer::reconcile_json_object(&existing_mapping, &write_obj);
        let body = match replacement_doc.as_ref() {
            Some(candidate) => candidate.body.as_str(),
            None => new_body
                .or_else(|| {
                    existing
                        .as_ref()
                        .map(|(document, _)| document.body.as_str())
                })
                .unwrap_or_default(),
        };
        let output = if document.is_some() && replacement_mapping.as_ref() == Some(&write_mapping) {
            Ok(document.as_deref().unwrap_or_default().to_string())
        } else {
            // Restore the original UTF-8 BOM so round-trips stay byte-stable.
            let existing_had_bom = existing.as_ref().is_some_and(|(_, had_bom)| *had_bom);
            let had_bom = replacement
                .as_ref()
                .map_or(existing_had_bom, |(_, had_bom)| *had_bom);
            serializer::serialize_document_with_bom(had_bom, &write_mapping, body)
        };
        let output = match output {
            Ok(output) => output,
            Err(error) => {
                return op_error(FRONTMATTER_SERIALIZATION_FAILED, &error.to_string());
            }
        };

        if let Err(error) =
            ensure_no_symlink_components(&self.root, path.as_str(), self.spec_profile)
        {
            return error;
        }
        #[cfg(test)]
        apply_injected_publication_replacement(&full_path);

        // Reopen once at the publication boundary and compare a byte revision,
        // not only path metadata. This catches atomic replacements (including
        // forged same-mtime and invalid-UTF-8 files) without weakening the
        // existing cross-platform atomic replace and crash-safety semantics.
        let current_facts = match crate::record_load::load_record(self, path.as_str()) {
            Ok(current) => current.facts().clone(),
            Err(error) => {
                return op_error("io_error", &format!("Failed to revalidate record: {error}"));
            }
        };
        if current_facts.revision != loaded_facts.revision {
            return op_error(
                CONCURRENT_MODIFICATION,
                &format!("File '{}' was modified during operation", path.as_str()),
            );
        }
        if let Err(error) = atomic_write(&full_path, output.as_bytes()) {
            return op_error("io_error", &format!("Failed to write: {error}"));
        }

        // Collect warnings (deprecated fields, etc.)
        let mut result_warnings: Vec<serde_json::Value> = Vec::new();
        for type_name in &type_names {
            if let Some(type_def) = self.types.get(type_name) {
                if let Some(fields_obj) = fields.as_object() {
                    for field_name in fields_obj.keys() {
                        if let Some(field_def) = type_def.fields.get(field_name) {
                            if field_def.deprecated.is_some() {
                                result_warnings.push(serde_json::json!({
                                    "code": "deprecated_field",
                                    "message": format!("Field '{}' is deprecated", field_name),
                                    "field": field_name,
                                }));
                            }
                        }
                    }
                }
            }
        }

        // Evaluate computed fields for the returned result (not written to disk)
        let effective =
            self.evaluate_computed_fields(effective, &type_names, path.as_str(), Some(body));

        UpdateOutput {
            path: path.to_string(),
            frontmatter: effective,
            body: body.to_string(),
            warnings: result_warnings,
        }
        .into_json()
    }
}

#[cfg(test)]
mod tests {
    use super::inject_publication_replacement;
    use crate::Collection;
    use serde_json::json;
    use std::fs;

    fn collection() -> (tempfile::TempDir, Collection) {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
        )
        .unwrap();
        fs::write(
            root.path().join("record.md"),
            "---\ntitle: Original\n---\noriginal body\n",
        )
        .unwrap();
        let collection = Collection::open(root.path()).unwrap();
        (root, collection)
    }

    #[test]
    fn update_revalidation_rejects_same_mtime_atomic_replacement() {
        let (root, collection) = collection();
        let record = root.path().join("record.md");
        let original_mtime = fs::metadata(&record).unwrap().modified().unwrap();
        let replacement = root.path().join("replacement.tmp");
        let external = b"---\ntitle: External\n---\nexternal body\n";
        fs::write(&replacement, external).unwrap();
        fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(original_mtime))
            .unwrap();
        inject_publication_replacement(&record, replacement);

        let result = collection.update(&json!({
            "path": "record.md",
            "fields": {"title": "Ours"}
        }));
        assert_eq!(result["error"]["code"], "concurrent_modification");
        assert_eq!(fs::read(record).unwrap(), external);
    }

    #[test]
    fn invalid_utf8_replacement_cannot_be_repaired_over_stale_bytes() {
        let (root, collection) = collection();
        let record = root.path().join("record.md");
        let replacement = root.path().join("replacement.tmp");
        let external = b"external-\xff-invalid";
        fs::write(&replacement, external).unwrap();
        inject_publication_replacement(&record, replacement);

        let result = collection.update(&json!({
            "path": "record.md",
            "document": "---\ntitle: Repaired\n---\nrepaired\n"
        }));
        assert_eq!(result["error"]["code"], "concurrent_modification");
        assert_eq!(fs::read(record).unwrap(), external);
    }
}
