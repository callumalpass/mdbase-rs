//! Provider-neutral Obsidian Base planning and projection evaluation.
//!
//! The exact `.base` document is a point input. The returned plan contains only
//! its parsed semantic program and immutable invocation bindings. Providers feed
//! one current projection plus a bounded relationship neighborhood at a time;
//! exact Markdown and body prose are never accepted by the evaluator.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::expressions::evaluator::resolve_execution_timezone;
use crate::v03::{Diagnostic, OperationResult};
use crate::views::{
    base_uses_backlinks, combined_filter_matches, evaluate_property, is_configured_obsidian_source,
    stable_named_view_ids, validate_base_expressions, BasesEvaluationContext, BasesFile, BasesLink,
    BasesTimezone, ObsidianBaseDocument, ObsidianBaseView, ViewReferenceInput,
};
use crate::OperationCancellation;

use super::{CanonicalRecordInput, CatalogError, CompiledCatalog, SemanticProjection};

pub const HOSTED_BASE_PLAN_VERSION: u32 = 1;
pub const MAX_HOSTED_BASE_RELATED_RECORDS: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedBaseRequirements {
    pub backlinks: bool,
    pub outgoing_relationships: bool,
    pub link_resolution: bool,
    pub query_context: bool,
    pub max_relationship_depth: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedBasePlan {
    pub plan_version: u32,
    pub catalog_revision: String,
    pub view_path: String,
    pub view_id: String,
    pub view_revision: String,
    pub document: ObsidianBaseDocument,
    pub view: ObsidianBaseView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_types: Vec<String>,
    #[serde(default)]
    pub offset: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_page_size: Option<u64>,
    pub requirements: HostedBaseRequirements,
    pub invocation_digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum HostedBasePlanning {
    Planned { plan: Box<HostedBasePlan> },
    Invalid { result: OperationResult },
}

/// Projection inputs for evaluating one candidate. `related` is the bounded
/// union of incoming/outgoing graph neighbors and planned resolution hits. It
/// may contain false positives. mdbase-rs reconstructs the exact relationship
/// view before evaluating the canonical Base expression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedBaseRecordContext {
    pub projection: SemanticProjection,
    #[serde(default)]
    pub related: Vec<SemanticProjection>,
    /// True only when the provider completed the plan's bounded relationship
    /// and resolution lookup protocol for this candidate. False never means an
    /// empty graph; it means evaluation must fail closed.
    #[serde(default)]
    pub relationship_neighborhood_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_context: Option<SemanticProjection>,
    pub operation_clock: String,
    pub max_expression_steps: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedBaseRow {
    pub path: String,
    pub file: Value,
    pub effective_frontmatter: Map<String, Value>,
    pub types: Vec<String>,
    pub values: Map<String, Value>,
    /// Canonical values needed by the provider's bounded top-K reducer. These
    /// are not response fields and may be discarded after page selection.
    pub sort_values: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_value: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum HostedBaseEvaluation {
    Included { row: Box<HostedBaseRow> },
    Excluded { diagnostics: Vec<Diagnostic> },
}

impl CompiledCatalog {
    /// Compile one exact Obsidian Base resource into a closed, digest-bound
    /// semantic plan. No collection records are enumerated.
    pub fn plan_hosted_obsidian_base(
        &self,
        input: &Value,
        view_record: &CanonicalRecordInput,
        allowed_types: &[String],
    ) -> Result<HostedBasePlanning, CatalogError> {
        let mut request = match serde_json::from_value::<ViewReferenceInput>(input.clone()) {
            Ok(request) => request,
            Err(error) => {
                return Ok(invalid(
                    "invalid_request",
                    format!("View execution input is invalid: {error}"),
                    None,
                ))
            }
        };
        if input.get("context") == Some(&Value::Null) {
            request.context = Some(None);
        }
        if request.render {
            return Ok(invalid(
                "unsupported_presentation",
                "This provider supports headless view execution.",
                Some(request.path),
            ));
        }
        if request.path != view_record.path || !request.path.ends_with(".base") {
            return Ok(invalid(
                "view_not_found",
                "The requested Obsidian Base does not match the supplied exact resource.",
                Some(request.path),
            ));
        }
        if !is_configured_obsidian_source(self.collection(), &request.path) {
            return Ok(invalid(
                "view_not_found",
                "The requested Obsidian Base is not enabled by collection configuration.",
                Some(request.path),
            ));
        }
        let document = match serde_yaml::from_str::<ObsidianBaseDocument>(&view_record.document) {
            Ok(document) => document,
            Err(error) => {
                return Ok(invalid(
                    "invalid_view",
                    format!("Could not parse Obsidian Base: {error}"),
                    Some(request.path),
                ))
            }
        };
        let ids = stable_named_view_ids(&document.views);
        let Some(view) = ids
            .iter()
            .zip(&document.views)
            .find_map(|(id, view)| (id == &request.view_id).then_some(view.clone()))
        else {
            return Ok(invalid(
                "view_not_found",
                format!("Named view '{}' was not found.", request.view_id),
                Some(request.path),
            ));
        };
        let diagnostics = validate_base_expressions(&document, &view, &request.path);
        if diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == "error")
        {
            return Ok(HostedBasePlanning::Invalid {
                result: OperationResult {
                    valid: false,
                    result: json!({}),
                    diagnostics,
                },
            });
        }
        let configured_timezone = self.collection().settings.timezone.as_deref();
        let timezone =
            match resolve_execution_timezone(request.timezone.as_deref(), configured_timezone) {
                Ok(timezone) => timezone.map(ToString::to_string),
                Err(error) => return Ok(invalid("invalid_timezone", error, None)),
            };
        if let Err(error) = BasesTimezone::from_setting(timezone.as_deref()) {
            return Ok(invalid(
                "invalid_config",
                error,
                Some("mdbase.yaml".to_string()),
            ));
        }
        let context_path = request
            .context
            .as_ref()
            .and_then(|context| context.as_ref())
            .map(|context| context.path.clone());
        let relationships = base_uses_relationships(&document, &view);
        let backlinks = base_uses_backlinks(&document, &view);
        let mut allowed_types = allowed_types
            .iter()
            .map(|value| value.to_ascii_lowercase())
            .collect::<Vec<_>>();
        allowed_types.sort();
        allowed_types.dedup();
        if !allowed_types.is_empty() && relationships {
            return Ok(invalid(
                "scope_denied",
                "Cross-record Obsidian Base traversal is unavailable to a scoped capability.",
                Some(request.path),
            ));
        }
        let view_revision = format!(
            "sha256:{:x}",
            Sha256::digest(view_record.document.as_bytes())
        );
        let suggested_page_size = request.limit.or(view.limit);
        let mut plan = HostedBasePlan {
            plan_version: HOSTED_BASE_PLAN_VERSION,
            catalog_revision: self.resource_revision().to_string(),
            view_path: request.path,
            view_id: request.view_id,
            view_revision,
            document,
            view,
            context_path,
            timezone,
            allowed_types,
            offset: request.offset.unwrap_or(0),
            suggested_page_size,
            requirements: HostedBaseRequirements {
                backlinks,
                outgoing_relationships: relationships,
                link_resolution: relationships,
                query_context: request
                    .context
                    .as_ref()
                    .is_some_and(|context| context.is_some()),
                max_relationship_depth: u8::from(relationships),
            },
            invocation_digest: String::new(),
        };
        plan.invocation_digest = digest_plan(&plan)?;
        Ok(HostedBasePlanning::Planned {
            plan: Box::new(plan),
        })
    }
}

impl HostedBasePlan {
    pub fn order_arity(&self) -> usize {
        self.view.sort.len()
    }

    pub fn validate_integrity(&self) -> Result<(), CatalogError> {
        if self.plan_version != HOSTED_BASE_PLAN_VERSION
            || self.invocation_digest != digest_plan(self)?
        {
            return Err(CatalogError {
                code: "hosted_base_plan_mismatch".to_string(),
                message: "Stored Obsidian Base plan failed its version or digest binding."
                    .to_string(),
            });
        }
        Ok(())
    }

    /// Evaluate one current semantic projection using only provider-supplied,
    /// bounded derived state. A missing required neighborhood fails closed.
    pub fn evaluate_record(
        &self,
        input: &HostedBaseRecordContext,
    ) -> Result<HostedBaseEvaluation, CatalogError> {
        self.evaluate_record_inner(input, None)
    }

    /// Evaluate with a cooperative token checked at every expression AST node.
    /// Providers can run this method on a blocking worker and cancel the token
    /// when the owning request future is dropped.
    pub fn evaluate_record_with_cancellation(
        &self,
        input: &HostedBaseRecordContext,
        cancellation: &OperationCancellation,
    ) -> Result<HostedBaseEvaluation, CatalogError> {
        self.evaluate_record_inner(input, Some(cancellation.clone()))
    }

    fn evaluate_record_inner(
        &self,
        input: &HostedBaseRecordContext,
        cancellation: Option<OperationCancellation>,
    ) -> Result<HostedBaseEvaluation, CatalogError> {
        self.validate_integrity()?;
        if input.related.len() > MAX_HOSTED_BASE_RELATED_RECORDS {
            return Err(CatalogError {
                code: "hosted_base_relationship_budget_exceeded".to_string(),
                message: "Obsidian Base relationship neighborhood exceeds the semantic budget."
                    .to_string(),
            });
        }
        if (self.requirements.backlinks
            || self.requirements.outgoing_relationships
            || self.requirements.link_resolution)
            && !input.relationship_neighborhood_complete
        {
            return Err(CatalogError {
                code: "hosted_base_relationship_state_incomplete".to_string(),
                message: "Obsidian Base relationship evaluation requires a complete bounded neighborhood."
                    .to_string(),
            });
        }
        validate_projection(self, &input.projection)?;
        for projection in &input.related {
            validate_projection(self, projection)?;
        }
        if let Some(context) = &input.query_context {
            validate_projection(self, context)?;
            if !self.allowed_types.is_empty()
                && !context.facts.types.iter().any(|actual| {
                    self.allowed_types
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(actual))
                })
            {
                return Err(CatalogError {
                    code: "scope_denied".to_string(),
                    message: "The Base context is outside this capability's record scope."
                        .to_string(),
                });
            }
        }
        if !self.allowed_types.is_empty()
            && !input.projection.facts.types.iter().any(|actual| {
                self.allowed_types
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(actual))
            })
        {
            return Ok(HostedBaseEvaluation::Excluded {
                diagnostics: Vec::new(),
            });
        }
        match (&self.context_path, &input.query_context) {
            (Some(expected), Some(context)) if expected == &context.facts.path => {}
            (Some(_), _) => {
                return Err(CatalogError {
                    code: "hosted_base_context_mismatch".to_string(),
                    message: "The supplied Base context does not bind the planned context path."
                        .to_string(),
                })
            }
            (None, Some(_)) => {
                return Err(CatalogError {
                    code: "hosted_base_context_mismatch".to_string(),
                    message: "An unplanned Base context was supplied.".to_string(),
                })
            }
            (None, None) => {}
        }

        let mut projections = Vec::with_capacity(input.related.len().saturating_add(2));
        projections.push(&input.projection);
        projections.extend(input.related.iter());
        if let Some(context) = &input.query_context {
            projections.push(context);
        }
        projections.sort_by(|left, right| left.facts.path.cmp(&right.facts.path));
        projections.dedup_by(|left, right| left.facts.path == right.facts.path);
        let mut files = projections
            .iter()
            .map(|projection| projection_file(projection))
            .collect::<Vec<_>>();
        populate_backlinks(&mut files, &projections);
        let link_resolutions = Arc::new(link_resolutions(&projections));
        let files = Arc::new(files);
        let file = files
            .iter()
            .find(|file| file.path == input.projection.facts.path)
            .cloned()
            .ok_or_else(|| CatalogError {
                code: "hosted_base_projection_mismatch".to_string(),
                message: "Candidate file facts are absent from the evaluation context.".to_string(),
            })?;
        let this_file = input.query_context.as_ref().and_then(|context| {
            files
                .iter()
                .find(|file| file.path == context.facts.path)
                .cloned()
        });
        let timezone =
            BasesTimezone::from_setting(self.timezone.as_deref()).map_err(|message| {
                CatalogError {
                    code: "invalid_timezone".to_string(),
                    message,
                }
            })?;
        let context = BasesEvaluationContext {
            note: input.projection.facts.effective_frontmatter.clone(),
            file: file.clone(),
            this_file,
            files,
            formulas: Arc::new(self.document.formulas.clone()),
            property_types: Arc::new(BTreeMap::new()),
            link_resolutions,
            now: Some(input.operation_clock.clone()),
            timezone,
            work_limit: Some(usize::try_from(input.max_expression_steps).unwrap_or(usize::MAX)),
            cancellation,
        };
        let matched = match combined_filter_matches(
            self.document.filters.as_ref(),
            self.view.filters.as_ref(),
            &context,
        ) {
            Ok(matched) => matched,
            Err(error) if error == crate::views::BASES_WORK_BUDGET_EXCEEDED => {
                return Err(CatalogError {
                    code: "hosted_base_operator_budget_exceeded".to_string(),
                    message: error,
                })
            }
            Err(error) if error == crate::views::BASES_OPERATION_CANCELLED => {
                return Err(CatalogError {
                    code: "operation_cancelled".to_string(),
                    message: error,
                })
            }
            Err(error) => {
                return Ok(HostedBaseEvaluation::Excluded {
                    diagnostics: vec![Diagnostic {
                        severity: "warning".to_string(),
                        code: "expression_evaluation_error".to_string(),
                        message: error,
                        path: Some(input.projection.facts.path.clone()),
                        field: Some("filters".to_string()),
                        type_name: None,
                        schema_location: None,
                        details: Some(json!({"dialect": "obsidian.bases"})),
                    }],
                })
            }
        };
        if !matched {
            return Ok(HostedBaseEvaluation::Excluded {
                diagnostics: Vec::new(),
            });
        }
        let mut properties = self.view.order.iter().cloned().collect::<BTreeSet<_>>();
        properties.extend(self.view.sort.iter().map(|sort| sort.property.clone()));
        properties.extend(
            self.view
                .group_by
                .iter()
                .map(|group| group.property().to_string()),
        );
        let mut computed = Map::new();
        for property in properties {
            let value = match evaluate_property(&property, &context) {
                Ok(value) => value,
                Err(error) if error == crate::views::BASES_WORK_BUDGET_EXCEEDED => {
                    return Err(CatalogError {
                        code: "hosted_base_operator_budget_exceeded".to_string(),
                        message: error,
                    })
                }
                Err(error) if error == crate::views::BASES_OPERATION_CANCELLED => {
                    return Err(CatalogError {
                        code: "operation_cancelled".to_string(),
                        message: error,
                    })
                }
                Err(_) => Value::Null,
            };
            computed.insert(property.clone(), value);
        }
        let mut value_properties = self.view.order.clone();
        if let Some(group) = &self.view.group_by {
            if !value_properties
                .iter()
                .any(|value| value == group.property())
            {
                value_properties.push(group.property().to_string());
            }
        }
        let values = value_properties
            .into_iter()
            .map(|property| {
                let value = computed.get(&property).cloned().unwrap_or(Value::Null);
                (property, value)
            })
            .collect::<Map<_, _>>();
        let group_value = self.view.group_by.as_ref().map(|group| {
            computed
                .get(group.property())
                .cloned()
                .unwrap_or(Value::Null)
        });
        let file_value = json!({
            "path": file.path,
            "name": file.name,
            "basename": file.basename,
            "folder": file.folder,
            "ext": file.extension,
            "size": file.size,
            "mtime": file.mtime,
            "ctime": file.ctime,
            "tags": file.tags,
        });
        Ok(HostedBaseEvaluation::Included {
            row: Box::new(HostedBaseRow {
                path: input.projection.facts.path.clone(),
                file: file_value,
                effective_frontmatter: input.projection.facts.effective_frontmatter.clone(),
                types: input.projection.facts.types.clone(),
                values,
                sort_values: computed,
                group_value,
            }),
        })
    }

    /// Stable semantic key used by a provider cursor. Path remains the final
    /// deterministic tie-breaker and is stored separately by Connect.
    pub fn row_order_values(&self, row: &HostedBaseRow) -> Vec<Value> {
        self.view
            .sort
            .iter()
            .map(|sort| {
                row.sort_values
                    .get(&sort.property)
                    .cloned()
                    .unwrap_or(Value::Null)
            })
            .collect()
    }

    pub fn compare_rows(&self, left: &HostedBaseRow, right: &HostedBaseRow) -> std::cmp::Ordering {
        for sort in &self.view.sort {
            let comparison = compare_json(
                left.sort_values.get(&sort.property).unwrap_or(&Value::Null),
                right
                    .sort_values
                    .get(&sort.property)
                    .unwrap_or(&Value::Null),
            );
            if !comparison.is_eq() {
                return if sort.direction.eq_ignore_ascii_case("DESC") {
                    comparison.reverse()
                } else {
                    comparison
                };
            }
        }
        left.path.cmp(&right.path)
    }

    pub fn compare_row_to_boundary(
        &self,
        row: &HostedBaseRow,
        boundary_values: &[Value],
        boundary_path: &str,
    ) -> Result<std::cmp::Ordering, CatalogError> {
        if boundary_values.len() != self.view.sort.len() {
            return Err(CatalogError {
                code: "hosted_base_cursor_mismatch".to_string(),
                message: "Obsidian Base cursor keyset does not match its semantic plan."
                    .to_string(),
            });
        }
        for (sort, (left, right)) in self
            .view
            .sort
            .iter()
            .zip(self.row_order_values(row).iter().zip(boundary_values))
        {
            let comparison = compare_json(left, right);
            if !comparison.is_eq() {
                return Ok(if sort.direction.eq_ignore_ascii_case("DESC") {
                    comparison.reverse()
                } else {
                    comparison
                });
            }
        }
        Ok(row.path.as_str().cmp(boundary_path))
    }

    pub fn groups(&self, rows: &[HostedBaseRow]) -> Option<Vec<Value>> {
        let group = self.view.group_by.as_ref()?;
        let mut grouped = BTreeMap::<String, (Value, u64)>::new();
        for row in rows {
            let value = row.group_value.clone().unwrap_or(Value::Null);
            let key = serde_jcs::to_string(&value).unwrap_or_default();
            let entry = grouped.entry(key).or_insert((value, 0));
            entry.1 = entry.1.saturating_add(1);
        }
        let mut grouped = grouped.into_values().collect::<Vec<_>>();
        grouped.sort_by(|left, right| compare_json(&left.0, &right.0));
        if group.direction().eq_ignore_ascii_case("DESC") {
            grouped.reverse();
        }
        Some(
            grouped
                .into_iter()
                .map(|(value, count)| {
                    json!({
                        "values": {group.property(): value},
                        "count": count,
                        "summaries": {},
                    })
                })
                .collect(),
        )
    }
}

fn validate_projection(
    plan: &HostedBasePlan,
    projection: &SemanticProjection,
) -> Result<(), CatalogError> {
    if projection.facts.catalog_revision != plan.catalog_revision
        || !projection.structure.structural_digest_is_valid()
    {
        return Err(CatalogError {
            code: "hosted_base_projection_stale".to_string(),
            message: "Obsidian Base evaluation requires a current, integrity-bound projection."
                .to_string(),
        });
    }
    Ok(())
}

fn projection_file(projection: &SemanticProjection) -> BasesFile {
    let facts = &projection.facts.file;
    let mut tags = frontmatter_tags(&projection.facts.effective_frontmatter);
    for tag in &projection.structure.body_tags {
        if !tags.contains(tag) {
            tags.push(tag.clone());
        }
    }
    let resolved = projection
        .structure
        .occurrences
        .iter()
        .filter_map(|occurrence| {
            Some((
                occurrence.occurrence.raw_target.clone(),
                match occurrence.resolution {
                    super::StructuralResolution::Resolved => occurrence.target_path.clone(),
                    super::StructuralResolution::Missing
                    | super::StructuralResolution::Ambiguous
                    | super::StructuralResolution::UnsafeTraversal
                    | super::StructuralResolution::External
                    | super::StructuralResolution::Malformed => None,
                    super::StructuralResolution::Unresolved => return None,
                },
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let links = projection
        .structure
        .body_links
        .iter()
        .map(|path| BasesLink {
            path: path.clone(),
            resolved_path: resolved.get(path).cloned(),
            ..Default::default()
        })
        .collect();
    let embeds = projection
        .structure
        .body_embeds
        .iter()
        .map(|path| BasesLink {
            path: path.clone(),
            resolved_path: resolved.get(path).cloned(),
            ..Default::default()
        })
        .collect();
    BasesFile {
        path: facts.path.clone(),
        name: facts.name.clone(),
        basename: facts.basename.clone(),
        folder: Path::new(&facts.path)
            .parent()
            .and_then(|path| path.to_str())
            .unwrap_or_default()
            .replace('\\', "/"),
        extension: facts.extension.clone(),
        size: facts.size,
        properties: projection.facts.effective_frontmatter.clone(),
        tags,
        links,
        embeds,
        backlinks: Vec::new(),
        ctime: None,
        mtime: facts.mtime.clone(),
    }
}

fn populate_backlinks(files: &mut [BasesFile], projections: &[&SemanticProjection]) {
    let incoming = projections
        .iter()
        .flat_map(|source| {
            source
                .structure
                .occurrences
                .iter()
                .filter_map(|occurrence| {
                    occurrence.target_path.as_ref().map(|target| {
                        (
                            target.clone(),
                            BasesLink {
                                path: source.facts.path.clone(),
                                resolved_path: Some(Some(source.facts.path.clone())),
                                ..Default::default()
                            },
                        )
                    })
                })
        })
        .fold(
            BTreeMap::<String, Vec<BasesLink>>::new(),
            |mut map, (target, link)| {
                let links = map.entry(target).or_default();
                if !links.iter().any(|existing| existing.path == link.path) {
                    links.push(link);
                }
                map
            },
        );
    for file in files {
        file.backlinks = incoming.get(&file.path).cloned().unwrap_or_default();
        file.backlinks
            .sort_by(|left, right| left.path.cmp(&right.path));
    }
}

fn link_resolutions(projections: &[&SemanticProjection]) -> BTreeMap<String, Option<String>> {
    let mut candidates = BTreeMap::<String, BTreeSet<String>>::new();
    for projection in projections {
        for key in &projection.facts.resolution_keys {
            candidates
                .entry(key.value.clone())
                .or_default()
                .insert(projection.facts.path.clone());
            candidates
                .entry(key.value.to_ascii_lowercase())
                .or_default()
                .insert(projection.facts.path.clone());
        }
        for occurrence in &projection.structure.occurrences {
            if let Some(target) = &occurrence.target_path {
                candidates
                    .entry(occurrence.occurrence.raw_target.clone())
                    .or_default()
                    .insert(target.clone());
                if let Some(normalized) = &occurrence.occurrence.normalized_target {
                    candidates
                        .entry(normalized.clone())
                        .or_default()
                        .insert(target.clone());
                }
            }
        }
    }
    candidates
        .into_iter()
        .map(|(key, values)| {
            let value = (values.len() == 1)
                .then(|| values.into_iter().next())
                .flatten();
            (key, value)
        })
        .collect()
}

fn frontmatter_tags(frontmatter: &Map<String, Value>) -> Vec<String> {
    match frontmatter.get("tags") {
        Some(Value::String(value)) => vec![value.trim_start_matches('#').to_string()],
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(|value| value.trim_start_matches('#').to_string())
            .collect(),
        _ => Vec::new(),
    }
}

fn compare_json(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Greater,
        (_, Value::Null) => std::cmp::Ordering::Less,
        (Value::Number(left), Value::Number(right)) => left
            .as_f64()
            .partial_cmp(&right.as_f64())
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(left), Value::String(right)) => left.cmp(right),
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        _ => serde_jcs::to_string(left)
            .unwrap_or_default()
            .cmp(&serde_jcs::to_string(right).unwrap_or_default()),
    }
}

fn base_uses_relationships(document: &ObsidianBaseDocument, view: &ObsidianBaseView) -> bool {
    let mut relevant = document.clone();
    relevant.views.clear();
    let serialized = serde_json::to_string(&(relevant, view)).unwrap_or_default();
    [
        "backlinks",
        "file.links",
        "file.embeds",
        "asFile",
        "asLink",
        "file(",
    ]
    .iter()
    .any(|needle| serialized.contains(needle))
}

fn digest_plan(plan: &HostedBasePlan) -> Result<String, CatalogError> {
    let mut unsigned = plan.clone();
    unsigned.invocation_digest.clear();
    let bytes = serde_jcs::to_vec(&unsigned).map_err(|error| CatalogError {
        code: "hosted_base_plan_failed".to_string(),
        message: format!("Obsidian Base plan could not be canonicalized: {error}"),
    })?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn invalid(
    code: impl Into<String>,
    message: impl Into<String>,
    path: Option<String>,
) -> HostedBasePlanning {
    HostedBasePlanning::Invalid {
        result: OperationResult {
            valid: false,
            result: json!({}),
            diagnostics: vec![Diagnostic::error(code, message, path)],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        CatalogInput, PreparedSemanticProjection, ResolvedRecordStructure, ResolvedTypeResource,
    };

    fn catalog() -> CompiledCatalog {
        CompiledCatalog::compile(CatalogInput {
            resource_revision: "catalog:base-test".to_string(),
            configuration_document: "spec_version: 0.3.0\nsettings:\n  timezone: Australia/Melbourne\nx-obsidian:\n  bases:\n    include: ['**/*.base']\n"
                .to_string(),
            types: vec![ResolvedTypeResource {
                path: "_types/task.md".to_string(),
                revision: "task:1".to_string(),
                definition: json!({
                    "kind": "mdbase.type",
                    "name": "task",
                    "version": 1,
                    "match": {"path_glob": "tasks/*.md"},
                    "schema": {"dialect": "json-schema-2020-12", "value": {"type": "object"}}
                }),
                schema: json!({"type": "object"}),
            }],
            contracts: Vec::new(),
        })
        .unwrap()
    }

    fn project(catalog: &CompiledCatalog, path: &str, document: &str) -> SemanticProjection {
        let prepared = catalog
            .project_record(&CanonicalRecordInput {
                stable_id: Some(path.to_string()),
                path: path.to_string(),
                document: document.to_string(),
                file_size: document.len() as u64,
                file_mtime: Some("2026-08-16T00:00:00Z".to_string()),
            })
            .unwrap();
        finalize_without_links(catalog, prepared)
    }

    fn finalize_without_links(
        catalog: &CompiledCatalog,
        prepared: PreparedSemanticProjection,
    ) -> SemanticProjection {
        let resolved = ResolvedRecordStructure {
            schema_version: prepared.structure.schema_version.clone(),
            path: prepared.structure.path.clone(),
            structural_digest: prepared.structure.structural_digest.clone(),
            occurrences: prepared
                .structure
                .occurrences
                .iter()
                .cloned()
                .map(|occurrence| super::super::ResolvedStructuralOccurrence {
                    occurrence,
                    resolution: super::super::StructuralResolution::Missing,
                    target_record_id: None,
                    target_path: None,
                    ambiguous_paths: Vec::new(),
                })
                .collect(),
            body_tags: prepared.structure.body_tags.clone(),
            body_links: prepared.structure.body_links.clone(),
            body_embeds: prepared.structure.body_embeds.clone(),
        };
        catalog.finalize_projection(prepared, resolved).unwrap()
    }

    fn finalize_to_target(
        catalog: &CompiledCatalog,
        prepared: PreparedSemanticProjection,
        target_path: &str,
    ) -> SemanticProjection {
        let resolved = ResolvedRecordStructure {
            schema_version: prepared.structure.schema_version.clone(),
            path: prepared.structure.path.clone(),
            structural_digest: prepared.structure.structural_digest.clone(),
            occurrences: prepared
                .structure
                .occurrences
                .iter()
                .cloned()
                .map(|occurrence| super::super::ResolvedStructuralOccurrence {
                    occurrence,
                    resolution: super::super::StructuralResolution::Resolved,
                    target_record_id: Some("project:mobile".to_string()),
                    target_path: Some(target_path.to_string()),
                    ambiguous_paths: Vec::new(),
                })
                .collect(),
            body_tags: prepared.structure.body_tags.clone(),
            body_links: prepared.structure.body_links.clone(),
            body_embeds: prepared.structure.body_embeds.clone(),
        };
        catalog.finalize_projection(prepared, resolved).unwrap()
    }

    #[test]
    fn plans_and_evaluates_tasknotes_base_from_projection_only() {
        let catalog = catalog();
        let source = r#"filters:
  and:
    - 'file.hasTag("task")'
formulas:
  urgency: 'if(priority == "high", 2, 1)'
views:
  - type: tasknotesTaskList
    name: Open tasks
    filters:
      and:
        - 'status != "done"'
    order: [status, formula.urgency, file.name]
    sort:
      - property: formula.urgency
        direction: DESC
"#;
        let planned = catalog
            .plan_hosted_obsidian_base(
                &json!({"path": "TaskNotes/Views/tasks.base", "view": "open-tasks"}),
                &CanonicalRecordInput {
                    stable_id: Some("base:1".to_string()),
                    path: "TaskNotes/Views/tasks.base".to_string(),
                    document: source.to_string(),
                    file_size: source.len() as u64,
                    file_mtime: None,
                },
                &[],
            )
            .unwrap();
        let HostedBasePlanning::Planned { plan } = planned else {
            panic!("expected hosted Base plan")
        };
        plan.validate_integrity().unwrap();
        assert!(!plan.requirements.backlinks);
        let projection = project(
            &catalog,
            "tasks/high.md",
            "---\nstatus: todo\npriority: high\ntags: [task]\n---\n# urgent\n",
        );
        let budget_error = plan
            .evaluate_record(&HostedBaseRecordContext {
                projection: projection.clone(),
                related: Vec::new(),
                relationship_neighborhood_complete: false,
                query_context: None,
                operation_clock: "2026-08-16T00:00:00Z".to_string(),
                max_expression_steps: 1,
            })
            .unwrap_err();
        assert_eq!(budget_error.code, "hosted_base_operator_budget_exceeded");
        let cancellation = OperationCancellation::new();
        cancellation.cancel();
        let cancelled = plan
            .evaluate_record_with_cancellation(
                &HostedBaseRecordContext {
                    projection: projection.clone(),
                    related: Vec::new(),
                    relationship_neighborhood_complete: false,
                    query_context: None,
                    operation_clock: "2026-08-16T00:00:00Z".to_string(),
                    max_expression_steps: 10_000,
                },
                &cancellation,
            )
            .unwrap_err();
        assert_eq!(cancelled.code, "operation_cancelled");
        let evaluated = plan
            .evaluate_record(&HostedBaseRecordContext {
                projection,
                related: Vec::new(),
                relationship_neighborhood_complete: false,
                query_context: None,
                operation_clock: "2026-08-16T00:00:00Z".to_string(),
                max_expression_steps: 10_000,
            })
            .unwrap();
        let HostedBaseEvaluation::Included { row } = evaluated else {
            panic!("expected included row")
        };
        assert_eq!(row.path, "tasks/high.md");
        assert_eq!(row.values["formula.urgency"], 2);
        assert_eq!(row.values["file.name"], "high");
        assert!(!serde_json::to_string(&row).unwrap().contains("# urgent"));
    }

    #[test]
    fn plan_digest_and_scope_fail_closed() {
        let catalog = catalog();
        let source = "views:\n  - type: table\n    name: Projects\n    filters: 'file.backlinks.length > 0'\n";
        let input = json!({"path": "views/projects.base", "view": "projects"});
        let view = CanonicalRecordInput {
            stable_id: None,
            path: "views/projects.base".to_string(),
            document: source.to_string(),
            file_size: source.len() as u64,
            file_mtime: None,
        };
        let scoped = catalog
            .plan_hosted_obsidian_base(&input, &view, &["task".to_string()])
            .unwrap();
        let HostedBasePlanning::Invalid { result } = scoped else {
            panic!("relationship view must fail closed for scoped capability")
        };
        assert_eq!(result.diagnostics[0].code, "scope_denied");

        let HostedBasePlanning::Planned { mut plan } = catalog
            .plan_hosted_obsidian_base(&input, &view, &[])
            .unwrap()
        else {
            panic!("expected unscoped plan")
        };
        plan.view_id.push_str("-tampered");
        assert_eq!(
            plan.validate_integrity().unwrap_err().code,
            "hosted_base_plan_mismatch"
        );
    }

    #[test]
    fn tasknotes_project_backlink_filter_uses_bounded_relationship_context() {
        let catalog = catalog();
        let source = r##"views:
  - type: tasknotesProjects
    name: Projects
    filters:
      and:
        - 'file.backlinks.filter((value.asFile().properties["status"].isEmpty() == false) && (value.asFile().properties["status"] != "done") && (list(value.asFile().properties["projects"]).map(file(value.replace(/^\[[^\]]+\]\((.*)\)$/, "$1").replace("[[", "").replace("]]", "").split("|")[0].split("#")[0].replace(/%20/g, " ")).asLink()).contains(file.asLink()))).length > 0'
    order: [file.name, file.folder]
"##;
        let HostedBasePlanning::Planned { plan } = catalog
            .plan_hosted_obsidian_base(
                &json!({"path": "views/projects.base", "view": "projects"}),
                &CanonicalRecordInput {
                    stable_id: None,
                    path: "views/projects.base".to_string(),
                    document: source.to_string(),
                    file_size: source.len() as u64,
                    file_mtime: None,
                },
                &[],
            )
            .unwrap()
        else {
            panic!("expected relationship plan")
        };
        assert!(plan.requirements.backlinks);
        let project = project(
            &catalog,
            "Projects/mobile.md",
            "---\ntitle: Mobile roadmap\n---\nProject notes\n",
        );
        let task_prepared = catalog
            .project_record(&CanonicalRecordInput {
                stable_id: Some("task:project".to_string()),
                path: "tasks/project-task.md".to_string(),
                document:
                    "---\nstatus: todo\nprojects: ['[[Projects/mobile]]']\n---\nShip mobile\n"
                        .to_string(),
                file_size: 0,
                file_mtime: None,
            })
            .unwrap();
        let task = finalize_to_target(&catalog, task_prepared, "Projects/mobile.md");

        let incomplete = plan
            .evaluate_record(&HostedBaseRecordContext {
                projection: project.clone(),
                related: vec![task.clone()],
                relationship_neighborhood_complete: false,
                query_context: None,
                operation_clock: "2026-08-16T00:00:00Z".to_string(),
                max_expression_steps: 10_000,
            })
            .unwrap_err();
        assert_eq!(incomplete.code, "hosted_base_relationship_state_incomplete");

        let evaluated = plan
            .evaluate_record(&HostedBaseRecordContext {
                projection: project,
                related: vec![task],
                relationship_neighborhood_complete: true,
                query_context: None,
                operation_clock: "2026-08-16T00:00:00Z".to_string(),
                max_expression_steps: 10_000,
            })
            .unwrap();
        let HostedBaseEvaluation::Included { row } = evaluated else {
            panic!("bounded backlink context should include the project")
        };
        assert_eq!(row.path, "Projects/mobile.md");
        assert_eq!(row.effective_frontmatter["title"], "Mobile roadmap");
    }
}
