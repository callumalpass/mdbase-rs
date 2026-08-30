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
/// Maximum losing candidates retained as resolution evidence. Selection still
/// considers every candidate; this bound applies only to additive evidence.
pub const MAX_RESOLUTION_ALTERNATIVES: usize = MAX_RESOLUTION_CANDIDATES - 1;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionCandidateIdentity {
    pub record_id: String,
    pub path: String,
}

pub(crate) type RankedResolutionCandidate = ResolutionCandidateIdentity;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionReason {
    ConfiguredId,
    OnlyCandidate,
    ExactPath,
    SameDirectory,
    ShallowestPath,
    LexicalTieBreak,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RankedResolution {
    Missing,
    Resolved {
        record_id: String,
        path: String,
        reason: ResolutionReason,
        selected_kind: RecordResolutionKeyKind,
        candidate_count: usize,
        candidate_digest: String,
        alternatives: Vec<String>,
        alternative_candidates: Vec<ResolutionCandidateIdentity>,
    },
    Ambiguous {
        paths: Vec<String>,
    },
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
    let mut by_record_id = BTreeMap::<String, String>::new();
    for candidate in candidates {
        if candidate.record_id.is_empty() {
            return Err(invalid_candidate_error());
        }
        let path = CollectionPath::new(&candidate.path).map_err(|_| invalid_candidate_error())?;
        match by_record_id.entry(candidate.record_id.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(path.to_string());
            }
            std::collections::btree_map::Entry::Occupied(entry) if entry.get() != path.as_str() => {
                return Err(invalid_candidate_error());
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
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
    if candidates.len() > MAX_RESOLUTION_CANDIDATES {
        return Err(resolution_budget_error("resolution candidate"));
    }
    let candidate_count = candidates.len();

    if kind == RecordResolutionKeyKind::Basename {
        let reason = if candidates.len() == 1 {
            ResolutionReason::OnlyCandidate
        } else {
            let same_directory = candidates
                .iter()
                .filter(|candidate| {
                    candidate
                        .path
                        .rsplit_once('/')
                        .map_or("", |(parent, _)| parent)
                        == source_directory
                })
                .count();
            if same_directory == 1 {
                ResolutionReason::SameDirectory
            } else {
                let preferred_directory = same_directory > 0;
                let mut depths = candidates
                    .iter()
                    .filter(|candidate| {
                        !preferred_directory
                            || candidate
                                .path
                                .rsplit_once('/')
                                .map_or("", |(parent, _)| parent)
                                == source_directory
                    })
                    .map(|candidate| candidate.path.split('/').count())
                    .collect::<Vec<_>>();
                depths.sort_unstable();
                if depths.first() != depths.get(1) {
                    ResolutionReason::ShallowestPath
                } else {
                    ResolutionReason::LexicalTieBreak
                }
            }
        };
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
        let alternative_candidates = complete_alternative_candidates(candidates);
        let alternatives = alternative_candidates
            .iter()
            .map(|candidate| candidate.path.clone())
            .collect();
        let candidate_digest = digest_candidate_identities(kind, &winner, &alternative_candidates)?;
        return Ok(RankedResolution::Resolved {
            record_id: winner.record_id,
            path: winner.path,
            reason,
            selected_kind: kind,
            candidate_count,
            candidate_digest,
            alternatives,
            alternative_candidates,
        });
    }

    if candidates.len() == 1 {
        let winner = candidates.remove(0);
        let candidate_digest = digest_candidate_identities(kind, &winner, &[])?;
        let reason = match kind {
            RecordResolutionKeyKind::Id => ResolutionReason::ConfiguredId,
            RecordResolutionKeyKind::Path => ResolutionReason::ExactPath,
            RecordResolutionKeyKind::Basename => unreachable!(),
            RecordResolutionKeyKind::Title => ResolutionReason::OnlyCandidate,
        };
        Ok(RankedResolution::Resolved {
            record_id: winner.record_id,
            path: winner.path,
            reason,
            selected_kind: kind,
            candidate_count,
            candidate_digest,
            alternatives: Vec::new(),
            alternative_candidates: Vec::new(),
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

fn complete_alternative_candidates(
    mut candidates: Vec<RankedResolutionCandidate>,
) -> Vec<ResolutionCandidateIdentity> {
    candidates.sort_by(|left, right| {
        (left.path.as_str(), left.record_id.as_str())
            .cmp(&(right.path.as_str(), right.record_id.as_str()))
    });
    candidates
}

pub(crate) fn digest_candidate_identities(
    kind: RecordResolutionKeyKind,
    winner: &ResolutionCandidateIdentity,
    alternatives: &[ResolutionCandidateIdentity],
) -> Result<String, CatalogError> {
    use sha2::{Digest, Sha256};

    let ordered = std::iter::once(winner)
        .chain(alternatives)
        .map(|candidate| (candidate.record_id.as_str(), candidate.path.as_str()))
        .collect::<Vec<_>>();
    let canonical = serde_jcs::to_vec(&(kind, ordered)).map_err(|_| invalid_candidate_error())?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ResolutionReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_lookup: Option<ResolutionLookupKey>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub candidate_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternative_candidates: Vec<ResolutionCandidateIdentity>,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
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

    /// Validate additive selector evidence independently of the pre-resolution
    /// structural digest. This prevents an old or hand-crafted projection from
    /// claiming the current format while omitting bounded resolution evidence.
    pub fn resolution_evidence_is_valid(&self) -> bool {
        self.occurrences.iter().all(|resolved| {
            let alternatives_valid = resolved.alternatives.len() <= MAX_RESOLUTION_ALTERNATIVES
                && resolved
                    .alternatives
                    .windows(2)
                    .all(|pair| pair[0] < pair[1])
                && resolved
                    .alternatives
                    .iter()
                    .all(|path| CollectionPath::new(path).is_ok())
                && !resolved
                    .target_path
                    .as_ref()
                    .is_some_and(|winner| resolved.alternatives.iter().any(|path| path == winner));
            match resolved.resolution {
                StructuralResolution::Resolved => {
                    resolved.target_record_id.is_some()
                        && resolved
                            .target_path
                            .as_deref()
                            .is_some_and(|path| CollectionPath::new(path).is_ok())
                        && resolved.ambiguous_paths.is_empty()
                        && alternatives_valid
                        && resolved_reason_is_proven(&self.path, resolved)
                }
                _ => {
                    resolved.reason.is_none()
                        && resolved.selected_lookup.is_none()
                        && resolved.candidate_count == 0
                        && resolved.candidate_digest.is_none()
                        && resolved.alternatives.is_empty()
                        && resolved.alternative_candidates.is_empty()
                }
            }
        })
    }
}

fn collection_directory(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn resolved_reason_is_proven(source_path: &str, resolved: &ResolvedStructuralOccurrence) -> bool {
    let (Some(reason), Some(winner), Some(record_id), Some(lookup), Some(bound_digest)) = (
        resolved.reason,
        resolved.target_path.as_deref(),
        resolved.target_record_id.as_deref(),
        resolved.selected_lookup.as_ref(),
        resolved.candidate_digest.as_deref(),
    ) else {
        return false;
    };
    let winner_identity = ResolutionCandidateIdentity {
        record_id: record_id.to_string(),
        path: winner.to_string(),
    };
    let identities_are_canonical = resolved
        .alternative_candidates
        .windows(2)
        .all(|pair| (&pair[0].path, &pair[0].record_id) < (&pair[1].path, &pair[1].record_id));
    let identity_paths = resolved
        .alternative_candidates
        .iter()
        .map(|candidate| candidate.path.as_str())
        .collect::<Vec<_>>();
    let mut record_ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let identities_are_unique = std::iter::once(&winner_identity)
        .chain(&resolved.alternative_candidates)
        .all(|candidate| {
            !candidate.record_id.is_empty()
                && CollectionPath::new(&candidate.path).is_ok()
                && record_ids.insert(candidate.record_id.as_str())
                && paths.insert(candidate.path.as_str())
        });
    if resolved.candidate_count != resolved.alternative_candidates.len() + 1
        || resolved.candidate_count == 0
        || resolved.candidate_count > MAX_RESOLUTION_CANDIDATES
        || resolved
            .alternatives
            .iter()
            .map(String::as_str)
            .ne(identity_paths)
        || !identities_are_canonical
        || !identities_are_unique
        || digest_candidate_identities(
            lookup.kind,
            &winner_identity,
            &resolved.alternative_candidates,
        )
        .ok()
        .as_deref()
            != Some(bound_digest)
    {
        return false;
    }
    let source_directory = collection_directory(source_path);
    let depth = |path: &str| path.split('/').count();
    let simple_wikilink = matches!(
        resolved.occurrence.kind,
        StructuralLinkKind::Wikilink | StructuralLinkKind::WikilinkEmbed
    ) && !resolved.occurrence.relative
        && !resolved.occurrence.raw_target.contains('/')
        && std::path::Path::new(&resolved.occurrence.raw_target)
            .extension()
            .is_none();
    let basename_eligible = |path: &str| {
        std::path::Path::new(path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.eq_ignore_ascii_case(&resolved.occurrence.raw_target))
    };
    let ranked_candidates_are_eligible = basename_eligible(winner)
        && resolved
            .alternatives
            .iter()
            .all(|path| basename_eligible(path));

    match reason {
        ResolutionReason::ConfiguredId => {
            lookup.kind == RecordResolutionKeyKind::Id
                && lookup.value == resolved.occurrence.raw_target.to_lowercase()
                && resolved.candidate_count == 1
                && !record_id.is_empty()
        }
        ResolutionReason::OnlyCandidate => {
            matches!(
                lookup.kind,
                RecordResolutionKeyKind::Basename | RecordResolutionKeyKind::Title
            ) && resolved.candidate_count == 1
                && lookup.value == resolved.occurrence.raw_target.to_lowercase()
        }
        ResolutionReason::ExactPath => {
            let Some(target) = resolved.occurrence.normalized_target.as_deref() else {
                return false;
            };
            lookup.kind == RecordResolutionKeyKind::Path
                && resolved.candidate_count == 1
                && lookup.value == winner
                && !simple_wikilink
                && (winner == target
                    || (std::path::Path::new(target).extension().is_none()
                        && [".md", ".mdx"]
                            .iter()
                            .any(|extension| winner == format!("{target}{extension}"))))
        }
        ResolutionReason::SameDirectory => {
            lookup.kind == RecordResolutionKeyKind::Basename
                && lookup.value == resolved.occurrence.raw_target.to_lowercase()
                && resolved.candidate_count > 1
                && ranked_candidates_are_eligible
                && collection_directory(winner) == source_directory
                && resolved
                    .alternatives
                    .iter()
                    .all(|path| collection_directory(path) != source_directory)
        }
        ResolutionReason::ShallowestPath => {
            lookup.kind == RecordResolutionKeyKind::Basename
                && lookup.value == resolved.occurrence.raw_target.to_lowercase()
                && resolved.candidate_count > 1
                && ranked_candidates_are_eligible
                && collection_directory(winner) != source_directory
                && resolved.alternatives.iter().all(|path| {
                    collection_directory(path) != source_directory && depth(winner) < depth(path)
                })
        }
        ResolutionReason::LexicalTieBreak => {
            lookup.kind == RecordResolutionKeyKind::Basename
                && lookup.value == resolved.occurrence.raw_target.to_lowercase()
                && resolved.candidate_count > 1
                && ranked_candidates_are_eligible
                && resolved.alternatives.iter().all(|path| {
                    let winner_rank = (
                        collection_directory(winner) != source_directory,
                        depth(winner),
                        winner,
                    );
                    let loser_rank = (
                        collection_directory(path) != source_directory,
                        depth(path),
                        path.as_str(),
                    );
                    winner_rank < loser_rank
                })
                && resolved.alternatives.iter().any(|path| {
                    (
                        collection_directory(winner) != source_directory,
                        depth(winner),
                    ) == (collection_directory(path) != source_directory, depth(path))
                })
        }
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
                        reason: None,
                        selected_lookup: None,
                        candidate_count: 0,
                        candidate_digest: None,
                        alternatives: Vec::new(),
                        alternative_candidates: Vec::new(),
                    });
                }
                let selected = grouped
                    .get(&occurrence.ordinal)
                    .and_then(|priorities| priorities.first_key_value())
                    .map(|(priority, matches)| {
                        let selected_lookup = plan
                            .lookups
                            .iter()
                            .find(|lookup| lookup.occurrence_ordinal == occurrence.ordinal)
                            .and_then(|lookup| {
                                lookup
                                    .alternatives
                                    .iter()
                                    .find(|alternative| alternative.priority == *priority)
                            })
                            .cloned()
                            .ok_or_else(|| CatalogError {
                                code: "invalid_resolution_plan".to_string(),
                                message: "The relationship-resolution plan does not define the returned priority."
                                    .to_string(),
                            })?;
                        select_resolution_candidate(
                            &structure.path,
                            selected_lookup.kind,
                            matches
                                .iter()
                                .map(|(record_id, path)| RankedResolutionCandidate {
                                    record_id: record_id.clone(),
                                    path: path.clone(),
                                }),
                        )
                        .map(|resolution| (selected_lookup, resolution))
                    })
                    .transpose()?;
                let (selected_lookup, selected) = selected
                    .map(|(lookup, resolution)| (Some(lookup), resolution))
                    .unwrap_or((None, RankedResolution::Missing));
                Ok(match selected {
                    RankedResolution::Missing => ResolvedStructuralOccurrence {
                        occurrence,
                        resolution: StructuralResolution::Missing,
                        target_record_id: None,
                        target_path: None,
                        ambiguous_paths: Vec::new(),
                        reason: None,
                        selected_lookup: None,
                        candidate_count: 0,
                        candidate_digest: None,
                        alternatives: Vec::new(),
                        alternative_candidates: Vec::new(),
                    },
                    RankedResolution::Resolved {
                        record_id,
                        path,
                        reason,
                        selected_kind: _,
                        candidate_count,
                        candidate_digest,
                        alternatives,
                        alternative_candidates,
                    } => ResolvedStructuralOccurrence {
                        occurrence,
                        resolution: StructuralResolution::Resolved,
                        target_record_id: Some(record_id),
                        target_path: Some(path),
                        ambiguous_paths: Vec::new(),
                        reason: Some(reason),
                        selected_lookup,
                        candidate_count,
                        candidate_digest: Some(candidate_digest),
                        alternatives,
                        alternative_candidates,
                    },
                    RankedResolution::Ambiguous { paths } => ResolvedStructuralOccurrence {
                        occurrence,
                        resolution: StructuralResolution::Ambiguous,
                        target_record_id: None,
                        target_path: None,
                        ambiguous_paths: paths,
                        reason: None,
                        selected_lookup: None,
                        candidate_count: 0,
                        candidate_digest: None,
                        alternatives: Vec::new(),
                        alternative_candidates: Vec::new(),
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

    fn resolved_occurrence(
        body: &str,
        kind: RecordResolutionKeyKind,
        paths: &[&str],
    ) -> ResolvedStructuralOccurrence {
        let catalog = catalog();
        let structure = structure(body);
        let plan = catalog.plan_record_resolution(&structure).unwrap();
        let lookup = &plan.lookups[0];
        let alternative = lookup
            .alternatives
            .iter()
            .position(|alternative| alternative.kind == kind)
            .unwrap();
        let candidates = paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                candidate(
                    lookup,
                    alternative,
                    &format!("record-{index}"),
                    if path.is_empty() {
                        &lookup.alternatives[alternative].value
                    } else {
                        path
                    },
                )
            })
            .collect::<Vec<_>>();
        catalog
            .resolve_record_structure(&structure, &plan, &candidates)
            .unwrap()
            .occurrences
            .into_iter()
            .next()
            .unwrap()
    }

    fn rebind_fabricated_evidence(occurrence: &mut ResolvedStructuralOccurrence) {
        occurrence.alternative_candidates = occurrence
            .alternatives
            .iter()
            .enumerate()
            .map(|(index, path)| ResolutionCandidateIdentity {
                record_id: format!("fabricated-{index}"),
                path: path.clone(),
            })
            .collect();
        occurrence.alternative_candidates.sort_by(|left, right| {
            (&left.path, &left.record_id).cmp(&(&right.path, &right.record_id))
        });
        occurrence.alternatives = occurrence
            .alternative_candidates
            .iter()
            .map(|candidate| candidate.path.clone())
            .collect();
        occurrence.candidate_count = occurrence.alternative_candidates.len() + 1;
        let winner = ResolutionCandidateIdentity {
            record_id: occurrence.target_record_id.clone().unwrap(),
            path: occurrence.target_path.clone().unwrap(),
        };
        occurrence.candidate_digest = Some(
            digest_candidate_identities(
                occurrence.selected_lookup.as_ref().unwrap().kind,
                &winner,
                &occurrence.alternative_candidates,
            )
            .unwrap(),
        );
    }

    #[test]
    fn hostile_v6_reason_evidence_is_rejected_for_every_reason() {
        let mut configured = resolved_occurrence(
            "[[target]]",
            RecordResolutionKeyKind::Id,
            &["elsewhere/by-id.md"],
        );
        assert_eq!(configured.reason, Some(ResolutionReason::ConfiguredId));
        assert!(resolved_reason_is_proven("notes/source.md", &configured));
        configured.alternatives.push("other/target.md".to_string());
        rebind_fabricated_evidence(&mut configured);
        assert!(!resolved_reason_is_proven("notes/source.md", &configured));

        let mut only = resolved_occurrence(
            "[[target]]",
            RecordResolutionKeyKind::Title,
            &["elsewhere/by-title.md"],
        );
        assert_eq!(only.reason, Some(ResolutionReason::OnlyCandidate));
        assert!(resolved_reason_is_proven("notes/source.md", &only));
        only.alternatives.push("other/target.md".to_string());
        rebind_fabricated_evidence(&mut only);
        assert!(!resolved_reason_is_proven("notes/source.md", &only));

        let mut exact = resolved_occurrence(
            "[Target](../target.md)",
            RecordResolutionKeyKind::Path,
            &[""],
        );
        assert_eq!(exact.reason, Some(ResolutionReason::ExactPath));
        assert!(resolved_reason_is_proven("notes/source.md", &exact));
        exact.target_path = Some("fabricated.md".to_string());
        rebind_fabricated_evidence(&mut exact);
        assert!(!resolved_reason_is_proven("notes/source.md", &exact));

        let mut same = resolved_occurrence(
            "[[target]]",
            RecordResolutionKeyKind::Basename,
            &["notes/target.md", "z/target.md"],
        );
        assert_eq!(same.reason, Some(ResolutionReason::SameDirectory));
        assert!(resolved_reason_is_proven("notes/source.md", &same));
        same.alternatives = vec!["notes/target.md".to_string()];
        rebind_fabricated_evidence(&mut same);
        assert!(!resolved_reason_is_proven("notes/source.md", &same));

        let mut shallow = resolved_occurrence(
            "[[target]]",
            RecordResolutionKeyKind::Basename,
            &["a/target.md", "deep/nested/target.md"],
        );
        assert_eq!(shallow.reason, Some(ResolutionReason::ShallowestPath));
        assert!(resolved_reason_is_proven("notes/source.md", &shallow));
        shallow.alternatives = vec!["z/target.md".to_string()];
        rebind_fabricated_evidence(&mut shallow);
        assert!(!resolved_reason_is_proven("notes/source.md", &shallow));

        let mut lexical = resolved_occurrence(
            "[[target]]",
            RecordResolutionKeyKind::Basename,
            &["a/target.md", "z/target.md"],
        );
        assert_eq!(lexical.reason, Some(ResolutionReason::LexicalTieBreak));
        assert!(resolved_reason_is_proven("notes/source.md", &lexical));
        lexical.target_path = Some("z/target.md".to_string());
        lexical.alternatives = vec!["a/target.md".to_string()];
        rebind_fabricated_evidence(&mut lexical);
        assert!(!resolved_reason_is_proven("notes/source.md", &lexical));
    }

    #[test]
    fn candidate_identity_substitution_and_duplicates_fail_even_with_rebound_digest() {
        let valid = resolved_occurrence(
            "[[target]]",
            RecordResolutionKeyKind::Basename,
            &["a/target.md", "z/target.md"],
        );
        assert!(resolved_reason_is_proven("notes/source.md", &valid));

        let mut swapped = valid.clone();
        swapped.target_record_id = Some(valid.alternative_candidates[0].record_id.clone());
        swapped.candidate_digest = Some(
            digest_candidate_identities(
                swapped.selected_lookup.as_ref().unwrap().kind,
                &ResolutionCandidateIdentity {
                    record_id: swapped.target_record_id.clone().unwrap(),
                    path: swapped.target_path.clone().unwrap(),
                },
                &swapped.alternative_candidates,
            )
            .unwrap(),
        );
        assert!(!resolved_reason_is_proven("notes/source.md", &swapped));

        let mut duplicate_id = valid.clone();
        duplicate_id.alternative_candidates[0].record_id = valid.target_record_id.clone().unwrap();
        duplicate_id.candidate_digest = Some(
            digest_candidate_identities(
                duplicate_id.selected_lookup.as_ref().unwrap().kind,
                &ResolutionCandidateIdentity {
                    record_id: duplicate_id.target_record_id.clone().unwrap(),
                    path: duplicate_id.target_path.clone().unwrap(),
                },
                &duplicate_id.alternative_candidates,
            )
            .unwrap(),
        );
        assert!(!resolved_reason_is_proven("notes/source.md", &duplicate_id));

        let mut duplicate_path = valid.clone();
        duplicate_path.alternative_candidates[0].path = valid.target_path.clone().unwrap();
        duplicate_path.alternatives[0] = valid.target_path.clone().unwrap();
        duplicate_path.candidate_digest = Some(
            digest_candidate_identities(
                duplicate_path.selected_lookup.as_ref().unwrap().kind,
                &ResolutionCandidateIdentity {
                    record_id: duplicate_path.target_record_id.clone().unwrap(),
                    path: duplicate_path.target_path.clone().unwrap(),
                },
                &duplicate_path.alternative_candidates,
            )
            .unwrap(),
        );
        assert!(!resolved_reason_is_proven(
            "notes/source.md",
            &duplicate_path
        ));
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
        assert_eq!(
            resolved.occurrences[0]
                .selected_lookup
                .as_ref()
                .map(|lookup| lookup.kind),
            Some(RecordResolutionKeyKind::Id)
        );
        assert_eq!(resolved.occurrences[0].candidate_count, 1);
        assert!(resolved.occurrences[0]
            .candidate_digest
            .as_deref()
            .is_some_and(|digest| digest.starts_with("sha256:")));
    }

    #[test]
    fn shared_selector_reports_every_reason_and_sorted_bounded_alternatives() {
        let resolve = |kind, paths: &[&str]| {
            select_resolution_candidate(
                "notes/source.md",
                kind,
                paths.iter().map(|path| RankedResolutionCandidate {
                    record_id: (*path).to_string(),
                    path: (*path).to_string(),
                }),
            )
            .unwrap()
        };
        let evidence = |resolution| match resolution {
            RankedResolution::Resolved {
                reason,
                alternatives,
                ..
            } => (reason, alternatives),
            other => panic!("expected resolved evidence, got {other:?}"),
        };

        assert_eq!(
            evidence(resolve(RecordResolutionKeyKind::Id, &["a.md"])).0,
            ResolutionReason::ConfiguredId
        );
        assert_eq!(
            evidence(resolve(RecordResolutionKeyKind::Title, &["a.md"])).0,
            ResolutionReason::OnlyCandidate
        );
        assert_eq!(
            evidence(resolve(RecordResolutionKeyKind::Path, &["a.md"])).0,
            ResolutionReason::ExactPath
        );
        assert_eq!(
            evidence(resolve(
                RecordResolutionKeyKind::Basename,
                &["notes/a.md", "z/a.md"]
            ))
            .0,
            ResolutionReason::SameDirectory
        );
        assert_eq!(
            evidence(resolve(
                RecordResolutionKeyKind::Basename,
                &["deep/nested/a.md", "z/a.md"]
            ))
            .0,
            ResolutionReason::ShallowestPath
        );
        assert_eq!(
            evidence(resolve(
                RecordResolutionKeyKind::Basename,
                &["z/a.md", "a/a.md"]
            ))
            .0,
            ResolutionReason::LexicalTieBreak
        );

        let many = (0..MAX_RESOLUTION_ALTERNATIVES)
            .map(|index| format!("z{index:03}/a.md"))
            .chain(std::iter::once("a/a.md".to_string()))
            .collect::<Vec<_>>();
        let ranked = select_resolution_candidate(
            "notes/source.md",
            RecordResolutionKeyKind::Basename,
            many.into_iter().map(|path| RankedResolutionCandidate {
                record_id: path.clone(),
                path,
            }),
        )
        .unwrap();
        let (_, alternatives) = evidence(ranked);
        assert_eq!(alternatives.len(), MAX_RESOLUTION_ALTERNATIVES);
        assert!(alternatives.windows(2).all(|pair| pair[0] < pair[1]));

        let over_budget = (0..=MAX_RESOLUTION_CANDIDATES).map(|index| RankedResolutionCandidate {
            record_id: format!("record-{index}"),
            path: format!("candidate-{index}.md"),
        });
        assert_eq!(
            select_resolution_candidate(
                "notes/source.md",
                RecordResolutionKeyKind::Basename,
                over_budget,
            )
            .unwrap_err()
            .code,
            "relationship_budget_exceeded"
        );
    }

    #[test]
    fn invalid_candidates_are_diagnostics_not_missing() {
        let error = select_resolution_candidate(
            "notes/source.md",
            RecordResolutionKeyKind::Basename,
            [RankedResolutionCandidate {
                record_id: "bad".to_string(),
                path: "../outside.md".to_string(),
            }],
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid_resolution_candidate");
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
