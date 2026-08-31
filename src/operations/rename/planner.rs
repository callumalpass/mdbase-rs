use crate::errors::FRONTMATTER_SERIALIZATION_FAILED;
use crate::frontmatter::serializer;
use crate::Collection;

pub(super) struct ReferenceRewritePlan {
    pub(super) execution_path: String,
    pub(super) expected_revision: String,
    pub(super) expected_mtime_ns: i64,
    pub(super) output: String,
    pub(super) updates: Vec<serde_json::Value>,
}

impl Collection {
    pub(super) fn plan_reference_rewrites(
        &self,
        snapshot: &crate::snapshot::AuthoritativeCollectionSnapshot,
        from: &str,
        to: &str,
        source_id: &Option<String>,
        warnings: &mut Vec<serde_json::Value>,
        failures: &mut Vec<serde_json::Value>,
    ) -> Vec<ReferenceRewritePlan> {
        let from_stem = stem(from);
        let to_stem = stem(to);
        let from_no_ext = without_markdown_extension(from);
        let to_no_ext = without_markdown_extension(to);
        let resolution_index = snapshot.link_resolution_index(self);
        let mut plans = Vec::new();

        for entry in snapshot.entries() {
            let captured_path = entry.relative_path();
            let execution_path = if captured_path == from {
                to
            } else {
                captured_path
            };
            let Some(content) = entry.outcome().document() else {
                // Invalid UTF-8 remains byte-opaque and is never rewritten.
                continue;
            };
            let Some(layout) = outcome_layout(entry.outcome()) else {
                continue;
            };
            let doc = layout.clone().into_parsed_document(content);
            let source_dir = std::path::Path::new(execution_path)
                .parent()
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_default();
            let mut fm_changed = false;
            let mut pending_updates = Vec::new();
            let mut fm_yaml = match &doc.frontmatter {
                Some(value @ serde_yaml::Value::Mapping(_)) => Some(value.clone()),
                _ => None,
            };
            if let Some(frontmatter) = fm_yaml.as_mut() {
                self.update_fm_links(
                    frontmatter,
                    from,
                    to,
                    &from_stem,
                    &to_stem,
                    from_no_ext,
                    to_no_ext,
                    &source_dir,
                    execution_path,
                    source_id,
                    entry.type_names(),
                    &resolution_index,
                    &mut fm_changed,
                    &mut pending_updates,
                    warnings,
                );
            }

            let mut new_body = doc.body.clone();
            let body_changed = self.update_body_links(
                &mut new_body,
                from,
                to,
                &from_stem,
                &to_stem,
                from_no_ext,
                to_no_ext,
                &source_dir,
                execution_path,
                source_id.as_deref(),
                &resolution_index,
            );
            if body_changed {
                pending_updates.push(serde_json::json!({
                    "path": execution_path,
                    "location": "body",
                }));
            }
            if !fm_changed && !body_changed {
                continue;
            }

            let output = if fm_changed {
                let mapping = fm_yaml
                    .as_ref()
                    .and_then(serde_yaml::Value::as_mapping)
                    .expect("changed frontmatter is a mapping");
                serializer::serialize_document_with_bom(layout.had_bom(), mapping, &new_body)
            } else if doc.has_frontmatter {
                // Preserve malformed/nonmapping or untouched mapping frontmatter
                // exactly for body-only rewrites.
                let body_offset = content
                    .len()
                    .checked_sub(doc.body.len())
                    .expect("snapshot body is an exact document suffix");
                let mut output = String::with_capacity(body_offset + new_body.len());
                output.push_str(&content[..body_offset]);
                output.push_str(&new_body);
                Ok(output)
            } else {
                serializer::serialize_document_with_bom(
                    layout.had_bom(),
                    &serde_yaml::Mapping::new(),
                    &new_body,
                )
            };
            let output = match output {
                Ok(output) => output,
                Err(error) => {
                    failures.push(serde_json::json!({
                        "path": execution_path,
                        "reason": FRONTMATTER_SERIALIZATION_FAILED,
                        "message": error.to_string(),
                    }));
                    continue;
                }
            };
            plans.push(ReferenceRewritePlan {
                execution_path: execution_path.to_string(),
                expected_revision: entry.facts().revision.clone(),
                expected_mtime_ns: entry.facts().mtime_ns,
                output,
                updates: pending_updates,
            });
        }
        plans
    }
}

fn stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("")
        .to_string()
}

fn without_markdown_extension(path: &str) -> &str {
    path.strip_suffix(".md")
        .or_else(|| path.strip_suffix(".mdx"))
        .unwrap_or(path)
}

fn outcome_layout(
    outcome: &crate::record_load::RecordLoadOutcome,
) -> Option<&crate::frontmatter::parser::ParsedDocumentLayout> {
    match outcome {
        crate::record_load::RecordLoadOutcome::Parsed { layout, .. } => Some(layout),
        crate::record_load::RecordLoadOutcome::Invalid {
            state: crate::record_load::InvalidRecordState::Frontmatter { layout, .. },
            ..
        } => Some(layout),
        crate::record_load::RecordLoadOutcome::Invalid {
            state: crate::record_load::InvalidRecordState::InvalidUtf8,
            ..
        } => None,
    }
}
