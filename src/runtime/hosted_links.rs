//! Link traversal for hosted queries that follow links (`asFile()`, `file.backlinks`).
//!
//! A hosted provider cannot hand a query the whole collection. Instead, for each candidate it
//! supplies the candidate's current projection and the bounded union of its incoming and
//! outgoing graph neighbors, as it does for Obsidian Bases. Projections carry each link's
//! resolution, made when the record was indexed, so traversal returns the targets the indexer
//! chose; links built inside an expression resolve among the supplied records only.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    CatalogError, SemanticProjection, StructuralResolution, StructuralSourceKind,
    MAX_HOSTED_BASE_RELATED_RECORDS,
};
use crate::expressions::evaluator::ResolvedFileData;
use crate::links::linked_files::{LinkedFiles, StoredLinkTargets};

/// The records one hosted candidate's links reach.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostedRelationshipNeighborhood {
    /// The candidate's own current projection, whose occurrences record where its links go.
    pub projection: Option<SemanticProjection>,
    /// Current projections of the candidate's incoming and outgoing neighbors. False positives
    /// are harmless; missing neighbors are not, which is what `complete` attests.
    #[serde(default)]
    pub related: Vec<SemanticProjection>,
    /// True only when the provider completed its bounded relationship lookup for this
    /// candidate. False never means an empty graph; evaluation fails closed.
    #[serde(default)]
    pub complete: bool,
}

pub(super) type HostedLinkGraph = (Arc<LinkedFiles>, Arc<HashMap<String, Vec<String>>>);

/// Link traversal data and backlinks for one candidate, from its neighborhood.
pub(super) fn hosted_link_graph(
    candidate: ResolvedFileData,
    neighborhood: Option<&HostedRelationshipNeighborhood>,
    is_current: impl Fn(&SemanticProjection) -> bool,
    id_field: &str,
) -> Result<HostedLinkGraph, CatalogError> {
    let Some(neighborhood) = neighborhood.filter(|neighborhood| neighborhood.complete) else {
        return Err(error(
            "hosted_collection_context_required",
            "This query follows links and requires the candidate's complete relationship neighborhood.",
        ));
    };
    if neighborhood.related.len() > MAX_HOSTED_BASE_RELATED_RECORDS {
        return Err(error(
            "hosted_relationship_budget_exceeded",
            "The candidate's relationship neighborhood exceeds the semantic budget.",
        ));
    }
    let own = neighborhood
        .projection
        .as_ref()
        .filter(|projection| projection.facts.path == candidate.path)
        .ok_or_else(|| {
            error(
                "hosted_projection_mismatch",
                "The relationship neighborhood does not belong to this candidate.",
            )
        })?;
    let projections = std::iter::once(own)
        .chain(&neighborhood.related)
        .collect::<Vec<_>>();
    if let Some(stale) = projections
        .iter()
        .find(|projection| !is_current(projection))
    {
        return Err(error(
            "hosted_projection_stale",
            &format!(
                "Link traversal requires a current projection for '{}'.",
                stale.facts.path
            ),
        ));
    }

    let mut stored = StoredLinkTargets::new();
    let mut backlinks = HashMap::<String, Vec<String>>::new();
    for projection in &projections {
        let source = &projection.facts.path;
        // Frontmatter links resolve with their field's target types, so they win over a body
        // link with the same text, as when the graph is built from records.
        let mut occurrences = projection.structure.occurrences.iter().collect::<Vec<_>>();
        occurrences.sort_by_key(|occurrence| {
            occurrence.occurrence.source_kind != StructuralSourceKind::Frontmatter
        });
        for occurrence in occurrences {
            let (StructuralResolution::Resolved, Some(target)) =
                (&occurrence.resolution, &occurrence.target_path)
            else {
                continue;
            };
            let text = occurrence.occurrence.raw_target.as_str();
            let key = text.split('#').next().unwrap_or(text).trim().to_string();
            stored
                .entry(source.clone())
                .or_default()
                .entry(key)
                .or_insert_with(|| target.clone());
            backlinks
                .entry(target.clone())
                .or_default()
                .push(source.clone());
        }
    }
    for sources in backlinks.values_mut() {
        sources.sort();
        sources.dedup();
    }

    let mut seen = std::collections::HashSet::from([candidate.path.clone()]);
    let mut files = vec![candidate];
    for projection in &neighborhood.related {
        if seen.insert(projection.facts.path.clone()) {
            files.push(ResolvedFileData {
                path: projection.facts.path.clone(),
                frontmatter: Value::Object(projection.facts.effective_frontmatter.clone()),
                body: String::new(),
            });
        }
    }
    Ok((
        Arc::new(LinkedFiles::new(files, stored, id_field, None)),
        Arc::new(backlinks),
    ))
}

fn error(code: &str, message: &str) -> CatalogError {
    CatalogError {
        code: code.to_string(),
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::HostedRelationshipNeighborhood;
    use crate::runtime::{
        CanonicalRecordInput, CatalogInput, CompiledCatalog, ResolutionCandidate,
        ResolvedTypeResource, SemanticProjection,
    };

    fn record_type(name: &str, folder: &str) -> ResolvedTypeResource {
        let schema = json!({"type": "object", "properties": {
            "title": {"type": "string"}, "project": {"type": "string"}
        }});
        ResolvedTypeResource {
            path: format!("_types/{name}.md"),
            revision: format!("{name}-1"),
            definition: json!({
                "kind": "mdbase.type", "name": name, "version": 1,
                "match": {"path_glob": format!("{folder}/*.md")},
                "schema": {"dialect": "json-schema-2020-12", "value": schema.clone()}
            }),
            schema,
        }
    }

    fn catalog() -> CompiledCatalog {
        CompiledCatalog::compile(CatalogInput {
            resource_revision: "catalog-links".to_string(),
            configuration_document: "spec_version: 0.3.0\n".to_string(),
            types: vec![
                record_type("task", "tasks"),
                record_type("project", "projects"),
            ],
            contracts: Vec::new(),
        })
        .unwrap()
    }

    fn record(path: &str, frontmatter: &str) -> CanonicalRecordInput {
        CanonicalRecordInput {
            stable_id: Some(path.to_string()),
            path: path.to_string(),
            document: format!("---\n{frontmatter}\n---\n"),
            file_size: 0,
            file_mtime: None,
        }
    }

    /// The record's projection, with each link resolved to `target` as the indexer would.
    fn projection(
        catalog: &CompiledCatalog,
        record: &CanonicalRecordInput,
        target: &str,
    ) -> SemanticProjection {
        let prepared = catalog.project_record(record).unwrap();
        let plan = catalog.plan_record_resolution(&prepared.structure).unwrap();
        let candidates = plan
            .lookups
            .iter()
            .filter_map(|lookup| {
                Some(ResolutionCandidate {
                    occurrence_ordinal: lookup.occurrence_ordinal,
                    lookup: lookup
                        .alternatives
                        .iter()
                        .find(|alternative| alternative.value == target)?
                        .clone(),
                    record_id: target.to_string(),
                    path: target.to_string(),
                })
            })
            .collect::<Vec<_>>();
        let resolved = catalog
            .resolve_record_structure(&prepared.structure, &plan, &candidates)
            .unwrap();
        catalog.finalize_projection(prepared, resolved).unwrap()
    }

    fn fixture() -> (CompiledCatalog, CanonicalRecordInput, CanonicalRecordInput) {
        (
            catalog(),
            record("tasks/a.md", "project: '[[projects/alpha]]'"),
            record("projects/alpha.md", "title: Alpha"),
        )
    }

    #[test]
    fn follows_a_candidate_link_through_its_neighborhood() {
        let (catalog, task, alpha) = fixture();
        let plan = catalog
            .compile_hosted_query(
                &json!({"types": ["task"], "where": "project.asFile().title == 'Alpha'"}),
            )
            .unwrap();
        let neighborhood = HostedRelationshipNeighborhood {
            projection: Some(projection(&catalog, &task, "projects/alpha.md")),
            related: vec![projection(&catalog, &alpha, "")],
            complete: true,
        };
        let evaluation = catalog
            .evaluate_hosted_residual_with_neighborhood(&plan, &task, None, Some(&neighborhood))
            .unwrap();
        assert!(evaluation.matched, "{:?}", evaluation.diagnostics);
    }

    #[test]
    fn reads_backlinks_from_neighbors() {
        let (catalog, task, alpha) = fixture();
        let plan = catalog
            .compile_hosted_query(
                &json!({"types": ["project"], "where": "file.backlinks.size() == 1"}),
            )
            .unwrap();
        let neighborhood = HostedRelationshipNeighborhood {
            projection: Some(projection(&catalog, &alpha, "")),
            related: vec![projection(&catalog, &task, "projects/alpha.md")],
            complete: true,
        };
        let evaluation = catalog
            .evaluate_hosted_residual_with_neighborhood(&plan, &alpha, None, Some(&neighborhood))
            .unwrap();
        assert!(evaluation.matched, "{:?}", evaluation.diagnostics);
    }

    #[test]
    fn fails_closed_without_the_candidate_s_complete_neighborhood() {
        let (catalog, task, alpha) = fixture();
        let plan = catalog
            .compile_hosted_query(&json!({"types": ["task"], "where": "project.asFile() != null"}))
            .unwrap();
        let own = projection(&catalog, &task, "projects/alpha.md");
        let incomplete = HostedRelationshipNeighborhood {
            projection: Some(own.clone()),
            related: Vec::new(),
            complete: false,
        };
        let foreign = HostedRelationshipNeighborhood {
            projection: Some(projection(&catalog, &alpha, "")),
            related: vec![own],
            complete: true,
        };
        for (neighborhood, code) in [
            (None, "hosted_collection_context_required"),
            (Some(&incomplete), "hosted_collection_context_required"),
            (Some(&foreign), "hosted_projection_mismatch"),
        ] {
            let error = catalog
                .evaluate_hosted_residual_with_neighborhood(&plan, &task, None, neighborhood)
                .unwrap_err();
            assert_eq!(error.code, code);
        }
    }
}
