//! Closed, bounded link-resolution plan for provider-owned identity indexes.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::api::CollectionPath;

use super::{
    record_structure::digest_structure, CatalogError, CompiledCatalog, RecordResolutionKeyKind,
    RecordStructure, StructuralLinkKind, StructuralOccurrence, StructuralResolution,
};

pub const MAX_STRUCTURAL_OCCURRENCES: usize = 4_096;
pub const MAX_RESOLUTION_LOOKUPS: usize = 16_384;
pub const MAX_RESOLUTION_CANDIDATES: usize = 16_384;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordResolutionPlan {
    pub structure_schema_version: String,
    pub source_path: String,
    pub structural_digest: String,
    pub lookups: Vec<OccurrenceResolutionLookup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OccurrenceResolutionLookup {
    pub occurrence_ordinal: usize,
    pub alternatives: Vec<ResolutionLookupKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ResolutionLookupKey {
    pub priority: u16,
    pub kind: RecordResolutionKeyKind,
    pub value: String,
}

/// One exact indexed row returned by an authority for a planned lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionCandidate {
    pub occurrence_ordinal: usize,
    pub lookup: ResolutionLookupKey,
    pub record_id: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RankedResolutionCandidate {
    pub record_id: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RankedResolution {
    Missing,
    Resolved { record_id: String, path: String },
    Ambiguous { paths: Vec<String> },
}

pub(crate) fn select_resolution_candidate(
    source_path: &str,
    kind: RecordResolutionKeyKind,
    candidates: impl IntoIterator<Item = RankedResolutionCandidate>,
) -> Result<RankedResolution, CatalogError> {
    let source = CollectionPath::new(source_path).map_err(|_| invalid_candidate_error())?;
    let source_directory = source
        .as_str()
        .rsplit_once('/')
        .map_or("", |(parent, _)| parent);
    let mut by_path = BTreeMap::<String, String>::new();
    for candidate in candidates {
        if candidate.record_id.is_empty() {
            return Err(invalid_candidate_error());
        }
        let path = CollectionPath::new(&candidate.path).map_err(|_| invalid_candidate_error())?;
        match by_path.entry(path.to_string()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(candidate.record_id);
            }
            std::collections::btree_map::Entry::Occupied(entry)
                if entry.get() != &candidate.record_id =>
            {
                return Err(invalid_candidate_error());
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
    }
    let mut candidates = by_path
        .into_iter()
        .map(|(path, record_id)| RankedResolutionCandidate { record_id, path })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Ok(RankedResolution::Missing);
    }

    if kind == RecordResolutionKeyKind::Basename {
        candidates.sort_by(|left, right| {
            let left_directory = left.path.rsplit_once('/').map_or("", |(parent, _)| parent);
            let right_directory = right.path.rsplit_once('/').map_or("", |(parent, _)| parent);
            let left_rank = (
                left_directory != source_directory,
                left.path.split('/').count(),
                left.path.as_str(),
                left.record_id.as_str(),
            );
            let right_rank = (
                right_directory != source_directory,
                right.path.split('/').count(),
                right.path.as_str(),
                right.record_id.as_str(),
            );
            left_rank.cmp(&right_rank)
        });
        let winner = candidates.remove(0);
        return Ok(RankedResolution::Resolved {
            record_id: winner.record_id,
            path: winner.path,
        });
    }

    if candidates.len() == 1 {
        let winner = candidates.remove(0);
        Ok(RankedResolution::Resolved {
            record_id: winner.record_id,
            path: winner.path,
        })
    } else {
        Ok(RankedResolution::Ambiguous {
            paths: candidates
                .into_iter()
                .map(|candidate| candidate.path)
                .collect(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRecordStructure {
    pub schema_version: String,
    pub path: String,
    pub structural_digest: String,
    pub occurrences: Vec<ResolvedStructuralOccurrence>,
    pub body_tags: Vec<String>,
    pub body_links: Vec<String>,
    pub body_embeds: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedStructuralOccurrence {
    pub occurrence: StructuralOccurrence,
    pub resolution: StructuralResolution,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_record_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ambiguous_paths: Vec<String>,
}

impl ResolvedRecordStructure {
    /// Verify that deserialized structural facts still bind the digest produced
    /// before relationship resolution. Resolution outcomes are deliberately
    /// excluded; the exact occurrence spellings and query-visible body facts
    /// are not.
    pub fn structural_digest_is_valid(&self) -> bool {
        let structure = RecordStructure {
            schema_version: self.schema_version.clone(),
            path: self.path.clone(),
            occurrences: self
                .occurrences
                .iter()
                .map(|resolved| resolved.occurrence.clone())
                .collect(),
            body_tags: self.body_tags.clone(),
            body_links: self.body_links.clone(),
            body_embeds: self.body_embeds.clone(),
            structural_digest: String::new(),
        };
        digest_structure(&structure) == self.structural_digest
    }
}

impl CompiledCatalog {
    /// Compile exact lookup alternatives without exposing collection storage.
    pub fn plan_record_resolution(
        &self,
        structure: &RecordStructure,
    ) -> Result<RecordResolutionPlan, CatalogError> {
        if structure.occurrences.len() > MAX_STRUCTURAL_OCCURRENCES {
            return Err(resolution_budget_error("structural occurrence"));
        }
        let mut lookups = Vec::new();
        for occurrence in &structure.occurrences {
            if occurrence.resolution != StructuralResolution::Unresolved {
                continue;
            }
            let Some(target) = occurrence.normalized_target.as_deref() else {
                continue;
            };
            lookups.push(OccurrenceResolutionLookup {
                occurrence_ordinal: occurrence.ordinal,
                alternatives: self
                    .resolution_lookup_alternatives_for_occurrence(occurrence, target),
            });
        }
        let lookup_count = lookups
            .iter()
            .map(|lookup| lookup.alternatives.len())
            .sum::<usize>();
        if lookup_count > MAX_RESOLUTION_LOOKUPS {
            return Err(resolution_budget_error("resolution lookup"));
        }
        Ok(RecordResolutionPlan {
            structure_schema_version: structure.schema_version.clone(),
            source_path: structure.path.clone(),
            structural_digest: structure.structural_digest.clone(),
            lookups,
        })
    }

    fn resolution_lookup_alternatives_for_occurrence(
        &self,
        occurrence: &StructuralOccurrence,
        target: &str,
    ) -> Vec<ResolutionLookupKey> {
        let simple_wikilink = matches!(
            occurrence.kind,
            StructuralLinkKind::Wikilink | StructuralLinkKind::WikilinkEmbed
        ) && !occurrence.relative
            && !occurrence.raw_target.contains('/')
            && std::path::Path::new(&occurrence.raw_target)
                .extension()
                .is_none();
        if simple_wikilink {
            let simple = target.to_lowercase();
            return vec![
                ResolutionLookupKey {
                    priority: 0,
                    kind: RecordResolutionKeyKind::Id,
                    value: simple.clone(),
                },
                ResolutionLookupKey {
                    priority: 1,
                    kind: RecordResolutionKeyKind::Basename,
                    value: simple.clone(),
                },
                ResolutionLookupKey {
                    priority: 2,
                    kind: RecordResolutionKeyKind::Title,
                    value: simple,
                },
            ];
        }
        self.resolution_lookup_alternatives(target)
    }

    pub(crate) fn resolution_lookup_alternatives(&self, target: &str) -> Vec<ResolutionLookupKey> {
        let mut alternatives = vec![ResolutionLookupKey {
            priority: 0,
            kind: RecordResolutionKeyKind::Path,
            value: target.to_string(),
        }];
        if std::path::Path::new(target).extension().is_none() {
            for extension in self.record_extensions() {
                alternatives.push(ResolutionLookupKey {
                    priority: 1,
                    kind: RecordResolutionKeyKind::Path,
                    value: format!("{target}.{extension}"),
                });
            }
        }
        if !target.contains('/') {
            let simple = target.to_lowercase();
            alternatives.extend([
                ResolutionLookupKey {
                    priority: 2,
                    kind: RecordResolutionKeyKind::Id,
                    value: simple.clone(),
                },
                ResolutionLookupKey {
                    priority: 3,
                    kind: RecordResolutionKeyKind::Basename,
                    value: simple.clone(),
                },
                ResolutionLookupKey {
                    priority: 4,
                    kind: RecordResolutionKeyKind::Title,
                    value: simple,
                },
            ]);
        }
        alternatives.sort();
        alternatives.dedup();
        alternatives
    }

    /// Resolve authority-returned exact lookup rows under the compiled plan.
    pub fn resolve_record_structure(
        &self,
        structure: &RecordStructure,
        plan: &RecordResolutionPlan,
        candidates: &[ResolutionCandidate],
    ) -> Result<ResolvedRecordStructure, CatalogError> {
        if plan.structure_schema_version != structure.schema_version
            || plan.source_path != structure.path
            || plan.structural_digest != structure.structural_digest
        {
            return Err(CatalogError {
                code: "resolution_plan_mismatch".to_string(),
                message: "The relationship-resolution plan does not bind this record structure."
                    .to_string(),
            });
        }
        if candidates.len() > MAX_RESOLUTION_CANDIDATES {
            return Err(resolution_budget_error("resolution candidate"));
        }
        let mut planned_occurrences = BTreeSet::new();
        if plan
            .lookups
            .iter()
            .any(|lookup| !planned_occurrences.insert(lookup.occurrence_ordinal))
        {
            return Err(CatalogError {
                code: "invalid_resolution_plan".to_string(),
                message: "The relationship-resolution plan repeats an occurrence ordinal."
                    .to_string(),
            });
        }

        let planned = plan
            .lookups
            .iter()
            .map(|lookup| {
                (
                    lookup.occurrence_ordinal,
                    lookup.alternatives.iter().cloned().collect::<BTreeSet<_>>(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut grouped = BTreeMap::<usize, BTreeMap<u16, BTreeSet<(String, String)>>>::new();
        for candidate in candidates {
            if !planned
                .get(&candidate.occurrence_ordinal)
                .is_some_and(|lookups| lookups.contains(&candidate.lookup))
            {
                return Err(CatalogError {
                    code: "unplanned_resolution_candidate".to_string(),
                    message: "The authority returned a relationship candidate outside the closed lookup plan."
                        .to_string(),
                });
            }
            grouped
                .entry(candidate.occurrence_ordinal)
                .or_default()
                .entry(candidate.lookup.priority)
                .or_default()
                .insert((candidate.record_id.clone(), candidate.path.clone()));
        }

        let occurrences = structure
            .occurrences
            .iter()
            .cloned()
            .map(|occurrence| {
                if occurrence.resolution != StructuralResolution::Unresolved {
                    return Ok(ResolvedStructuralOccurrence {
                        resolution: occurrence.resolution,
                        occurrence,
                        target_record_id: None,
                        target_path: None,
                        ambiguous_paths: Vec::new(),
                    });
                }
                let selected = grouped
                    .get(&occurrence.ordinal)
                    .and_then(|priorities| priorities.first_key_value())
                    .map(|(priority, matches)| {
                        let kind = plan
                            .lookups
                            .iter()
                            .find(|lookup| lookup.occurrence_ordinal == occurrence.ordinal)
                            .and_then(|lookup| {
                                lookup
                                    .alternatives
                                    .iter()
                                    .find(|alternative| alternative.priority == *priority)
                            })
                            .map(|alternative| alternative.kind)
                            .ok_or_else(|| CatalogError {
                                code: "invalid_resolution_plan".to_string(),
                                message: "The relationship-resolution plan does not define the returned priority."
                                    .to_string(),
                            })?;
                        select_resolution_candidate(
                            &structure.path,
                            kind,
                            matches
                                .iter()
                                .map(|(record_id, path)| RankedResolutionCandidate {
                                    record_id: record_id.clone(),
                                    path: path.clone(),
                                }),
                        )
                    })
                    .transpose()?;
                Ok(match selected.unwrap_or(RankedResolution::Missing) {
                    RankedResolution::Missing => ResolvedStructuralOccurrence {
                        occurrence,
                        resolution: StructuralResolution::Missing,
                        target_record_id: None,
                        target_path: None,
                        ambiguous_paths: Vec::new(),
                    },
                    RankedResolution::Resolved { record_id, path } => {
                        ResolvedStructuralOccurrence {
                            occurrence,
                            resolution: StructuralResolution::Resolved,
                            target_record_id: Some(record_id),
                            target_path: Some(path),
                            ambiguous_paths: Vec::new(),
                        }
                    }
                    RankedResolution::Ambiguous { paths } => ResolvedStructuralOccurrence {
                        occurrence,
                        resolution: StructuralResolution::Ambiguous,
                        target_record_id: None,
                        target_path: None,
                        ambiguous_paths: paths,
                    },
                })
            })
            .collect::<Result<Vec<_>, CatalogError>>()?;

        Ok(ResolvedRecordStructure {
            schema_version: structure.schema_version.clone(),
            path: structure.path.clone(),
            structural_digest: structure.structural_digest.clone(),
            occurrences,
            body_tags: structure.body_tags.clone(),
            body_links: structure.body_links.clone(),
            body_embeds: structure.body_embeds.clone(),
        })
    }
}

fn invalid_candidate_error() -> CatalogError {
    CatalogError {
        code: "invalid_resolution_candidate".to_string(),
        message: "The authority returned an invalid relationship candidate.".to_string(),
    }
}

fn resolution_budget_error(kind: &str) -> CatalogError {
    CatalogError {
        code: "relationship_budget_exceeded".to_string(),
        message: format!("The record exceeds the bounded {kind} budget."),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::runtime::{CanonicalRecordInput, CatalogInput};

    fn catalog() -> CompiledCatalog {
        CompiledCatalog::compile(CatalogInput {
            resource_revision: "resources-1".to_string(),
            configuration_document:
                "spec_version: 0.3.0\nsettings:\n  record_extensions: [md, mdx]\n".to_string(),
            types: Vec::new(),
            contracts: Vec::new(),
        })
        .unwrap()
    }

    fn structure(body: &str) -> RecordStructure {
        catalog()
            .parse_record_structure(&CanonicalRecordInput {
                stable_id: None,
                path: "notes/source.md".to_string(),
                document: format!("---\n---\n{body}"),
                file_size: 0,
                file_mtime: None,
            })
            .unwrap()
    }

    fn candidate(
        lookup: &OccurrenceResolutionLookup,
        alternative: usize,
        record_id: &str,
        path: &str,
    ) -> ResolutionCandidate {
        ResolutionCandidate {
            occurrence_ordinal: lookup.occurrence_ordinal,
            lookup: lookup.alternatives[alternative].clone(),
            record_id: record_id.to_string(),
            path: path.to_string(),
        }
    }

    #[test]
    fn configured_id_wins_over_filename_match() {
        let catalog = catalog();
        let structure = structure("[[target]]");
        let plan = catalog.plan_record_resolution(&structure).unwrap();
        let lookup = &plan.lookups[0];
        let id = lookup
            .alternatives
            .iter()
            .position(|item| item.kind == RecordResolutionKeyKind::Id)
            .unwrap();
        let basename = lookup
            .alternatives
            .iter()
            .position(|item| item.kind == RecordResolutionKeyKind::Basename)
            .unwrap();
        let resolved = catalog
            .resolve_record_structure(
                &structure,
                &plan,
                &[
                    candidate(lookup, basename, "basename", "notes/target.md"),
                    candidate(lookup, id, "configured-id", "elsewhere/by-id.md"),
                ],
            )
            .unwrap();
        assert_eq!(
            resolved.occurrences[0].target_record_id.as_deref(),
            Some("configured-id")
        );
        assert_eq!(
            resolved.occurrences[0].resolution,
            StructuralResolution::Resolved
        );
    }

    #[test]
    fn extensionless_targets_always_plan_the_mandatory_markdown_path() {
        let catalog = catalog();
        let structure = structure("[[projects/mobile]]");
        let plan = catalog.plan_record_resolution(&structure).unwrap();
        assert!(plan.lookups[0].alternatives.iter().any(|lookup| {
            lookup.kind == RecordResolutionKeyKind::Path && lookup.value == "projects/mobile.md"
        }));
    }

    #[test]
    fn duplicate_configured_ids_are_ambiguous() {
        let catalog = catalog();
        let structure = structure("[[target]]");
        let plan = catalog.plan_record_resolution(&structure).unwrap();
        let lookup = &plan.lookups[0];
        let id = lookup
            .alternatives
            .iter()
            .position(|item| item.kind == RecordResolutionKeyKind::Id)
            .unwrap();
        let resolved = catalog
            .resolve_record_structure(
                &structure,
                &plan,
                &[
                    candidate(lookup, id, "a", "a/target.md"),
                    candidate(lookup, id, "b", "b/target.md"),
                ],
            )
            .unwrap();
        assert_eq!(
            resolved.occurrences[0].resolution,
            StructuralResolution::Ambiguous
        );
        assert_eq!(
            resolved.occurrences[0].ambiguous_paths,
            ["a/target.md", "b/target.md"]
        );
    }

    #[test]
    fn duplicate_basenames_use_same_directory_shortest_then_lexical_ranking() {
        let catalog = catalog();
        let structure = structure("[[target]]");
        let plan = catalog.plan_record_resolution(&structure).unwrap();
        let lookup = &plan.lookups[0];
        let basename = lookup
            .alternatives
            .iter()
            .position(|item| item.kind == RecordResolutionKeyKind::Basename)
            .unwrap();

        let resolve = |paths: &[(&str, &str)]| {
            let candidates = paths
                .iter()
                .map(|(record_id, path)| candidate(lookup, basename, record_id, path))
                .collect::<Vec<_>>();
            catalog
                .resolve_record_structure(&structure, &plan, &candidates)
                .unwrap()
                .occurrences[0]
                .target_path
                .clone()
                .unwrap()
        };

        assert_eq!(
            resolve(&[
                ("deep", "deep/nested/target.md"),
                ("same", "notes/target.md")
            ]),
            "notes/target.md"
        );
        assert_eq!(
            resolve(&[("deep", "deep/nested/target.md"), ("short", "a/target.md")]),
            "a/target.md"
        );
        assert_eq!(
            resolve(&[("beta", "beta/target.md"), ("alpha", "alpha/target.md")]),
            "alpha/target.md"
        );
        assert_eq!(
            resolve(&[("alpha", "alpha/target.md"), ("beta", "beta/target.md")]),
            "alpha/target.md"
        );
    }

    #[test]
    fn missing_external_and_malformed_are_explicit() {
        let catalog = catalog();
        let structure = structure("[[missing]] [web](https://example.com) [[broken");
        let plan = catalog.plan_record_resolution(&structure).unwrap();
        let resolved = catalog
            .resolve_record_structure(&structure, &plan, &[])
            .unwrap();
        assert!(resolved
            .occurrences
            .iter()
            .any(|item| item.resolution == StructuralResolution::Missing));
        assert!(resolved
            .occurrences
            .iter()
            .any(|item| item.resolution == StructuralResolution::External));
        assert!(resolved
            .occurrences
            .iter()
            .any(|item| item.resolution == StructuralResolution::Malformed));
    }

    #[test]
    fn duplicate_occurrence_lookups_in_a_plan_fail_closed() {
        let catalog = catalog();
        let structure = structure("[[target]]");
        let mut plan = catalog.plan_record_resolution(&structure).unwrap();
        plan.lookups.push(plan.lookups[0].clone());

        let error = catalog
            .resolve_record_structure(&structure, &plan, &[])
            .unwrap_err();
        assert_eq!(error.code, "invalid_resolution_plan");
    }

    #[test]
    fn authority_cannot_inject_an_unplanned_candidate() {
        let catalog = catalog();
        let structure = structure("[[target]]");
        let plan = catalog.plan_record_resolution(&structure).unwrap();
        let error = catalog
            .resolve_record_structure(
                &structure,
                &plan,
                &[ResolutionCandidate {
                    occurrence_ordinal: plan.lookups[0].occurrence_ordinal,
                    lookup: ResolutionLookupKey {
                        priority: 99,
                        kind: RecordResolutionKeyKind::Title,
                        value: "other".to_string(),
                    },
                    record_id: "injected".to_string(),
                    path: "other.md".to_string(),
                }],
            )
            .unwrap_err();
        assert_eq!(error.code, "unplanned_resolution_candidate");
    }

    #[test]
    fn plan_is_closed_and_serializable() {
        let catalog = catalog();
        let structure = structure("[[target]]");
        let plan = catalog.plan_record_resolution(&structure).unwrap();
        let value = serde_json::to_value(&plan).unwrap();
        assert_eq!(value["source_path"], json!("notes/source.md"));
        assert_eq!(plan.lookups[0].alternatives.len(), 3);
    }
}
