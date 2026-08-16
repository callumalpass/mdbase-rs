//! Production provider-neutral semantic projection for encrypted authorities.
//!
//! Exact Markdown is the canonical input and is deliberately absent from the
//! serialized projection. Authority-owned record/catalog/generation bindings are
//! applied outside this module.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::v03::Diagnostic;

use super::{
    CanonicalRecordInput, CatalogError, CompiledCatalog, RecordStructure, ResolutionCandidate,
    ResolvedRecordStructure, StructuralResolution, MAX_RESOLUTION_CANDIDATES,
};

pub const SEMANTIC_PROJECTION_FORMAT_VERSION: u32 = 3;
pub const SEMANTIC_PROJECTION_SCHEMA_VERSION: &str = "mdbase-semantic-projection-v3";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticProjectionFacts {
    pub schema_version: String,
    pub format_version: u32,
    pub semantic_engine_version: String,
    pub catalog_revision: String,
    pub path: String,
    pub types: Vec<String>,
    pub file: SemanticFileFacts,
    pub persisted_frontmatter: Map<String, Value>,
    pub effective_frontmatter: Map<String, Value>,
    pub diagnostics: Vec<Diagnostic>,
    pub resolution_keys: Vec<RecordResolutionKey>,
    pub semantic_complete: bool,
}

/// Projection facts awaiting collection-snapshot relationship resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreparedSemanticProjection {
    #[serde(flatten)]
    pub facts: SemanticProjectionFacts,
    pub structure: RecordStructure,
}

/// Complete persistable semantic projection with explicit relationship outcomes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticProjection {
    #[serde(flatten)]
    pub facts: SemanticProjectionFacts,
    pub structure: ResolvedRecordStructure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticFileFacts {
    pub path: String,
    pub name: String,
    pub basename: String,
    pub extension: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordResolutionKeyKind {
    Path,
    Basename,
    Id,
    Title,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RecordResolutionKey {
    pub kind: RecordResolutionKeyKind,
    pub value: String,
}

impl CompiledCatalog {
    /// Generate one full semantic projection from exact Markdown.
    ///
    /// This method performs no storage access and never serializes body prose or
    /// the exact document into the returned value. A non-zero supplied file size
    /// must agree with the canonical UTF-8 document bytes.
    pub fn project_record(
        &self,
        record: &CanonicalRecordInput,
    ) -> Result<PreparedSemanticProjection, CatalogError> {
        let exact_size = record.document.len() as u64;
        if record.file_size != 0 && record.file_size != exact_size {
            return Err(CatalogError {
                code: "record_size_mismatch".to_string(),
                message: format!(
                    "Record '{}' declared {} bytes but its exact UTF-8 document contains {} bytes.",
                    record.path, record.file_size, exact_size
                ),
            });
        }

        let classified = self.classify_record(record)?;
        let read = self.read_record(&serde_json::json!({"path": record.path}), record);
        let structure = self.parse_record_structure(record)?;
        let semantic_complete = read.valid
            && classified.frontmatter_error.is_none()
            && structure
                .occurrences
                .iter()
                .all(|occurrence| occurrence.resolution != StructuralResolution::Malformed);

        let effective_frontmatter = if read.valid {
            read.result
                .get("effective_frontmatter")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default()
        } else {
            Map::new()
        };
        let mut diagnostics = read.diagnostics;
        collect_result_diagnostics(&read.result, &mut diagnostics);
        if let Some(message) = classified.frontmatter_error.as_ref() {
            diagnostics.push(Diagnostic::error(
                "frontmatter_parse_failed",
                message.clone(),
                Some(record.path.clone()),
            ));
        }
        for occurrence in structure
            .occurrences
            .iter()
            .filter(|occurrence| occurrence.resolution == StructuralResolution::Malformed)
        {
            diagnostics.push(Diagnostic::error(
                "malformed_record_relationship",
                "A structural relationship has malformed Markdown syntax.",
                Some(record.path.clone()),
            ));
            if occurrence.source_kind == super::StructuralSourceKind::Frontmatter {
                if let Some(diagnostic) = diagnostics.last_mut() {
                    diagnostic.field.clone_from(&occurrence.field);
                }
            }
        }
        sort_diagnostics(&mut diagnostics);

        let mut types = classified.types;
        types
            .iter_mut()
            .for_each(|name| *name = name.to_lowercase());
        types.sort();
        types.dedup();
        let file = file_facts(&record.path, exact_size, record.file_mtime.clone());
        let resolution_keys = resolution_keys(&file, &effective_frontmatter, self.id_field());

        Ok(PreparedSemanticProjection {
            facts: SemanticProjectionFacts {
                schema_version: SEMANTIC_PROJECTION_SCHEMA_VERSION.to_string(),
                format_version: SEMANTIC_PROJECTION_FORMAT_VERSION,
                semantic_engine_version: env!("CARGO_PKG_VERSION").to_string(),
                catalog_revision: self.resource_revision().to_string(),
                path: record.path.clone(),
                types,
                file,
                persisted_frontmatter: classified.frontmatter,
                effective_frontmatter,
                diagnostics,
                resolution_keys,
                semantic_complete,
            },
            structure,
        })
    }

    /// Bind an authority-fetched relationship result to a prepared projection.
    pub fn finalize_projection(
        &self,
        prepared: PreparedSemanticProjection,
        resolved: ResolvedRecordStructure,
    ) -> Result<SemanticProjection, CatalogError> {
        if resolved.schema_version != prepared.structure.schema_version
            || resolved.path != prepared.structure.path
            || resolved.structural_digest != prepared.structure.structural_digest
            || resolved
                .occurrences
                .iter()
                .any(|occurrence| occurrence.resolution == StructuralResolution::Unresolved)
        {
            return Err(CatalogError {
                code: "projection_resolution_mismatch".to_string(),
                message: "Resolved relationships do not bind the prepared semantic projection."
                    .to_string(),
            });
        }
        Ok(SemanticProjection {
            facts: prepared.facts,
            structure: resolved,
        })
    }

    /// Canonically resolve and finalize a caller-bounded exact snapshot without
    /// consulting provider indexes. This is the fail-safe seam for an authority
    /// whose persisted projection or relationship graph is stale or absent.
    /// The caller owns record/byte limits; mdbase-rs additionally enforces its
    /// closed relationship-candidate ceiling across the whole batch.
    pub fn finalize_projection_batch(
        &self,
        records: Vec<(String, PreparedSemanticProjection)>,
    ) -> Result<Vec<(String, SemanticProjection)>, CatalogError> {
        let mut identities =
            BTreeMap::<(super::RecordResolutionKeyKind, String), Vec<(String, String)>>::new();
        for (record_id, prepared) in &records {
            for key in &prepared.facts.resolution_keys {
                identities
                    .entry((key.kind, key.value.clone()))
                    .or_default()
                    .push((record_id.clone(), prepared.facts.path.clone()));
            }
        }
        for values in identities.values_mut() {
            values.sort();
            values.dedup();
        }

        let mut candidate_count = 0_usize;
        let mut finalized = Vec::with_capacity(records.len());
        for (record_id, prepared) in records {
            let plan = self.plan_record_resolution(&prepared.structure)?;
            let mut candidates = Vec::new();
            for lookup in &plan.lookups {
                for alternative in &lookup.alternatives {
                    let Some(matches) =
                        identities.get(&(alternative.kind, alternative.value.clone()))
                    else {
                        continue;
                    };
                    candidate_count = candidate_count.saturating_add(matches.len());
                    if candidate_count > MAX_RESOLUTION_CANDIDATES {
                        return Err(CatalogError {
                            code: "relationship_resolution_budget_exceeded".to_string(),
                            message: "The exact fallback snapshot exceeded its bounded relationship-candidate budget."
                                .to_string(),
                        });
                    }
                    candidates.extend(matches.iter().map(|(target_id, path)| {
                        ResolutionCandidate {
                            occurrence_ordinal: lookup.occurrence_ordinal,
                            lookup: alternative.clone(),
                            record_id: target_id.clone(),
                            path: path.clone(),
                        }
                    }));
                }
            }
            let resolved =
                self.resolve_record_structure(&prepared.structure, &plan, &candidates)?;
            finalized.push((record_id, self.finalize_projection(prepared, resolved)?));
        }
        Ok(finalized)
    }
}

impl SemanticProjection {
    pub fn canonical_json(&self) -> serde_json::Result<Vec<u8>> {
        serde_jcs::to_vec(self)
    }

    pub fn canonical_digest(&self) -> serde_json::Result<String> {
        Ok(format!(
            "sha256:{:x}",
            Sha256::digest(self.canonical_json()?)
        ))
    }

    pub fn serialized_bytes(&self) -> serde_json::Result<usize> {
        self.canonical_json().map(|value| value.len())
    }
}

fn file_facts(path: &str, size: u64, mtime: Option<String>) -> SemanticFileFacts {
    let file_name = Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string();
    let extension = Path::new(&file_name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string();
    let basename = Path::new(&file_name)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string();
    SemanticFileFacts {
        path: path.to_string(),
        name: file_name,
        basename,
        extension,
        size,
        mtime,
    }
}

fn resolution_keys(
    file: &SemanticFileFacts,
    effective_frontmatter: &Map<String, Value>,
    id_field: &str,
) -> Vec<RecordResolutionKey> {
    let mut keys = vec![
        RecordResolutionKey {
            kind: RecordResolutionKeyKind::Path,
            value: file.path.clone(),
        },
        RecordResolutionKey {
            kind: RecordResolutionKeyKind::Basename,
            value: file.basename.to_ascii_lowercase(),
        },
    ];
    if let Some(id) = effective_frontmatter.get(id_field).and_then(Value::as_str) {
        keys.push(RecordResolutionKey {
            kind: RecordResolutionKeyKind::Id,
            value: id.to_ascii_lowercase(),
        });
    }
    if let Some(title) = effective_frontmatter.get("title").and_then(Value::as_str) {
        keys.push(RecordResolutionKey {
            kind: RecordResolutionKeyKind::Title,
            value: title.to_ascii_lowercase(),
        });
    }
    keys.sort();
    keys.dedup();
    keys
}

fn collect_result_diagnostics(result: &Value, diagnostics: &mut Vec<Diagnostic>) {
    let values = result
        .pointer("/validation/issues")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            result
                .get("warnings")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        );
    for value in values {
        let Some(code) = value.get("code").and_then(Value::as_str) else {
            continue;
        };
        diagnostics.push(Diagnostic {
            severity: value
                .get("severity")
                .and_then(Value::as_str)
                .unwrap_or("warning")
                .to_string(),
            code: code.to_string(),
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(code)
                .to_string(),
            path: value
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_string),
            field: value
                .get("field")
                .and_then(Value::as_str)
                .map(str::to_string),
            type_name: value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string),
            schema_location: value
                .get("schema_location")
                .and_then(Value::as_str)
                .map(str::to_string),
            details: value.get("details").cloned(),
        });
    }
}

fn sort_diagnostics(diagnostics: &mut [Diagnostic]) {
    diagnostics.sort_by(|left, right| {
        left.code
            .cmp(&right.code)
            .then_with(|| left.severity.cmp(&right.severity))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.field.cmp(&right.field))
            .then_with(|| left.type_name.cmp(&right.type_name))
            .then_with(|| left.schema_location.cmp(&right.schema_location))
            .then_with(|| left.message.cmp(&right.message))
    });
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::runtime::{CatalogInput, ResolvedTypeResource};

    fn catalog() -> CompiledCatalog {
        CompiledCatalog::compile(CatalogInput {
            resource_revision: "resources-1".to_string(),
            configuration_document:
                "spec_version: 0.3.0\nsettings:\n  id_field: uid\n  default_validation: warn\n"
                    .to_string(),
            types: vec![ResolvedTypeResource {
                path: "_types/note.md".to_string(),
                revision: "type-1".to_string(),
                definition: json!({
                    "kind": "mdbase.type",
                    "name": "note",
                    "version": 1,
                    "match": {"path_glob": "notes/*.md"},
                    "schema": {"dialect": "json-schema-2020-12", "value": {
                        "type": "object",
                        "properties": {"status": {"type": "string", "default": "open"}}
                    }},
                    "collection": {"read_defaults": {"status": "open"}}
                }),
                schema: json!({
                    "type": "object",
                    "properties": {"status": {"type": "string", "default": "open"}}
                }),
            }],
            contracts: Vec::new(),
        })
        .unwrap()
    }

    fn input(document: &str) -> CanonicalRecordInput {
        CanonicalRecordInput {
            stable_id: Some("record-1".to_string()),
            path: "notes/one.md".to_string(),
            document: document.to_string(),
            file_size: document.len() as u64,
            file_mtime: None,
        }
    }

    fn projection(document: &str) -> SemanticProjection {
        let catalog = catalog();
        let prepared = catalog.project_record(&input(document)).unwrap();
        let plan = catalog.plan_record_resolution(&prepared.structure).unwrap();
        let resolved = catalog
            .resolve_record_structure(&prepared.structure, &plan, &[])
            .unwrap();
        catalog.finalize_projection(prepared, resolved).unwrap()
    }

    #[test]
    fn projection_contains_full_semantics_and_structure_without_exact_body() {
        let document = "---\nuid: note-1\ntitle: One\nproject: '[[projects/main]]'\n---\nsecret-body-prose [[two#part|Two]] #focus\n";
        let projection = projection(document);

        assert!(projection.facts.semantic_complete);
        assert_eq!(projection.facts.types, ["note"]);
        assert_eq!(projection.facts.effective_frontmatter["status"], "open");
        assert_eq!(projection.structure.body_tags, ["focus"]);
        assert!(projection
            .facts
            .resolution_keys
            .contains(&RecordResolutionKey {
                kind: RecordResolutionKeyKind::Id,
                value: "note-1".to_string(),
            }));
        let serialized = String::from_utf8(projection.canonical_json().unwrap()).unwrap();
        assert!(!serialized.contains("secret-body-prose"));
        assert!(!serialized.contains("---"));
        assert_eq!(projection.canonical_digest().unwrap().len(), 71);
    }

    #[test]
    fn projection_rejects_mismatched_authority_size() {
        let mut record = input("body\n");
        record.file_size += 1;
        let error = catalog().project_record(&record).unwrap_err();
        assert_eq!(error.code, "record_size_mismatch");
    }

    #[test]
    fn malformed_relationship_is_incomplete_and_diagnostic() {
        let projection = projection("[[broken\n");
        assert!(!projection.facts.semantic_complete);
        assert!(projection
            .facts
            .diagnostics
            .iter()
            .any(|item| item.code == "malformed_record_relationship"));
    }

    #[test]
    fn digest_changes_with_structure_but_not_unrelated_body_prose() {
        let first = projection("alpha [[two]]\n");
        let second = projection("gamma [[two]]\n");
        let changed = projection("gamma [[six]]\n");
        assert_eq!(
            first.structure.structural_digest,
            second.structure.structural_digest
        );
        assert_ne!(
            second.structure.structural_digest,
            changed.structure.structural_digest
        );
        assert_eq!(
            first.canonical_digest().unwrap(),
            second.canonical_digest().unwrap()
        );
    }

    #[test]
    fn exact_fallback_batch_resolves_relationships_without_provider_indexes() {
        let catalog = catalog();
        let source_document = "---\nuid: source\n---\n[[projects/mobile]]\n";
        let target_document = "---\nuid: mobile\ntitle: Mobile\n---\n";
        let source = catalog
            .project_record(&CanonicalRecordInput {
                stable_id: Some("source-id".to_string()),
                path: "notes/source.md".to_string(),
                document: source_document.to_string(),
                file_size: source_document.len() as u64,
                file_mtime: None,
            })
            .unwrap();
        let target = catalog
            .project_record(&CanonicalRecordInput {
                stable_id: Some("target-id".to_string()),
                path: "projects/mobile.md".to_string(),
                document: target_document.to_string(),
                file_size: target_document.len() as u64,
                file_mtime: None,
            })
            .unwrap();

        let finalized = catalog
            .finalize_projection_batch(vec![
                ("source-id".to_string(), source),
                ("target-id".to_string(), target),
            ])
            .unwrap();
        let source = &finalized[0].1;
        assert_eq!(
            source.structure.occurrences[0].target_record_id.as_deref(),
            Some("target-id")
        );
        assert_eq!(
            source.structure.occurrences[0].resolution,
            StructuralResolution::Resolved
        );
    }
}
