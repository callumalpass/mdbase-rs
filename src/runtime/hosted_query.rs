//! Closed, versioned hosted-query planning owned by mdbase-rs.
//!
//! The plan is provider neutral: it contains no SQL, table names, credentials,
//! ciphertext, or authority identifiers. Hosts may translate only this closed
//! candidate IR into their own query language. The canonical query remains in
//! the residual so a candidate false positive cannot change semantics.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::expressions::ast::{BinOp, Expr, UnaryOp};
use crate::expressions::evaluator::resolve_execution_timezone;
use crate::query::cache_source::FileRecord;
use crate::v03::query::context::{candidate_context, file_value, namespace_value};
use crate::v03::query::diagnostics;
use crate::v03::query::model::{Candidate, Query};
use crate::v03::query::preflight::{self, CompiledSelection};
use crate::v03::query::result::serialize_candidate;
use crate::v03::{cel, validate_query, Diagnostic};

use super::{
    CanonicalRecordInput, CatalogError, CompiledCatalog, SemanticProjection,
    RECORD_STRUCTURE_SCHEMA_VERSION, SEMANTIC_PROJECTION_FORMAT_VERSION,
    SEMANTIC_PROJECTION_SCHEMA_VERSION,
};

pub const HOSTED_QUERY_PLAN_VERSION: u32 = 8;
const MAX_PREDICATE_NODES: usize = 256;
const MAX_ORDER_TERMS: usize = 16;
const MAX_GROUP_TERMS: usize = 8;
const MAX_SUMMARIES: usize = 32;
const MAX_PAGE_SIZE: u64 = 1_000;
const DEFAULT_PAGE_SIZE: u64 = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedQueryPlan {
    pub version: u32,
    pub semantic_engine_version: String,
    pub catalog_revision: String,
    pub canonical_query_digest: String,
    pub plan_digest: String,
    pub candidate: CandidatePredicate,
    pub residual: CanonicalResidual,
    pub order: Vec<HostedOrder>,
    pub groups: Vec<HostedGroup>,
    pub aggregates: Vec<HostedAggregate>,
    pub page_size: u64,
    pub requested_limit: Option<u64>,
    pub offset: u64,
    pub requirements: HostedQueryRequirements,
    pub budgets: HostedQueryBudgets,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalResidual {
    /// The validated canonical v0.3 query. It is data, never provider SQL.
    pub query: Value,
    /// Whether candidate facts alone prove the filter. False means retained
    /// candidates must be evaluated by mdbase-rs before emission.
    pub filter_fully_projected: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedResidualEvaluation {
    pub matched: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<Value>,
    pub diagnostics: Vec<Diagnostic>,
    /// Canonical values aligned with the closed plan. Providers may retain
    /// these only for the lifetime of one bounded page/reducer operation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order_values: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_values: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aggregate_values: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CandidatePredicate {
    All,
    None,
    And { terms: Vec<CandidatePredicate> },
    Or { terms: Vec<CandidatePredicate> },
    Not { term: Box<CandidatePredicate> },
    HasType { type_name: String },
    Compare { comparison: CandidateComparison },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateComparison {
    pub field: CandidateField,
    pub operator: CandidateComparisonOperator,
    pub value: Value,
    /// `ExactJson` proves that provider JSON operations have the same false
    /// result as canonical CEL for this literal/operator pair. Conservative
    /// comparisons may be observed but must never narrow provider candidates.
    pub pruning: CandidateComparisonPruning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateComparisonPruning {
    ExactJson,
    /// The value is a canonical tag without a leading `#`. Providers may
    /// narrow only with Obsidian's exact-or-descendant tag rule.
    NormalizedTagHierarchy,
    /// The literal is YYYY-MM-DD. Providers may narrow only records whose
    /// projected value is also exactly YYYY-MM-DD; every other JSON/string
    /// shape remains a candidate for canonical datetime-aware evaluation.
    IsoDateOnlyString,
    Conservative,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", content = "path", rename_all = "snake_case")]
pub enum CandidateField {
    Path,
    Types,
    File(String),
    PersistedFrontmatter(Vec<String>),
    EffectiveFrontmatter(Vec<String>),
    BodyTags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateComparisonOperator {
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    In,
    Contains,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedOrderDirection {
    Ascending,
    Descending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedOrder {
    pub field: CandidateField,
    pub direction: HostedOrderDirection,
    /// Every provider order ends with stable record identity as an implicit
    /// deterministic tie-break. This flag makes the contract explicit.
    pub canonical_path_tiebreak: bool,
    pub semantics: HostedSortSemantics,
    /// A catalog-backed proof that current projections contain only this
    /// scalar kind (or null) for the ordered field. Providers may use this to
    /// implement canonical keyset ordering, but must fail the proof at runtime
    /// if a malformed projection contains another JSON kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_kind: Option<HostedScalarKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedScalarKind {
    String,
    Number,
    Boolean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedSortSemantics {
    /// Canonical v0.3: null last ascending; scalar natural order; arrays and
    /// objects by length; unlike JSON kinds equal; direction reverses the
    /// result; canonical path is the final ascending tie-break.
    CanonicalV03,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedGroup {
    pub field: CandidateField,
    pub output_name: String,
    pub direction: HostedOrderDirection,
    /// The same catalog-backed scalar proof used by hosted ordering. This is
    /// intentionally absent when schemas do not prove one common kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_kind: Option<HostedScalarKind>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedReductionInput {
    pub group_values: Vec<Value>,
    pub aggregate_values: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedReduction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<Value>>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Streaming grouping/summary state for one bounded hosted operation. It
/// retains one fixed-size aggregate state per group rather than every matched
/// row, so provider memory is independent of collection cardinality.
pub struct HostedReductionAccumulator {
    plan: HostedQueryPlan,
    groups: BTreeMap<String, HostedReductionGroupState>,
}

struct HostedReductionGroupState {
    values: serde_json::Map<String, Value>,
    count: u64,
    summaries: Vec<HostedSummaryState>,
}

enum HostedSummaryState {
    Count(u64),
    Empty {
        total: u64,
        empty: u64,
    },
    Number {
        count: u64,
        sum: f64,
        minimum: f64,
        maximum: f64,
        invalid: bool,
    },
    String {
        selected: Option<String>,
        latest: bool,
        invalid: bool,
    },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedAggregate {
    pub field: CandidateField,
    pub function: String,
    pub output_name: String,
    pub provider_safe: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedQueryRequirements {
    pub exact_document: bool,
    pub body_prose: bool,
    pub structural_body_facts: bool,
    pub relationships: bool,
    pub persisted_frontmatter: bool,
    pub effective_frontmatter: bool,
    pub diagnostics: bool,
    pub diagnostic_type_matchers: bool,
    pub canonical_residual: bool,
    pub bounded_top_k: bool,
    pub bounded_grouping: bool,
    pub collection_context: bool,
    #[serde(default)]
    pub query_context: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedQueryBudgets {
    pub max_page_size: u64,
    pub max_offset: u64,
    pub max_candidate_rows: u64,
    pub max_candidate_bytes: u64,
    pub max_exact_documents: u64,
    pub max_exact_bytes: u64,
    pub max_operator_steps: u64,
    pub max_groups: u64,
    pub max_connection_wait_ms: u64,
    pub max_wall_time_ms: u64,
    pub max_snapshot_time_ms: u64,
    pub max_memory_bytes: u64,
}

impl Default for HostedQueryBudgets {
    fn default() -> Self {
        Self {
            max_page_size: MAX_PAGE_SIZE,
            max_offset: 10_000,
            max_candidate_rows: 10_000,
            max_candidate_bytes: 16 * 1024 * 1024,
            max_exact_documents: 2_000,
            max_exact_bytes: 64 * 1024 * 1024,
            max_operator_steps: 2_000_000,
            max_groups: 10_000,
            max_connection_wait_ms: 2_000,
            max_wall_time_ms: 15_000,
            max_snapshot_time_ms: 30_000,
            max_memory_bytes: 128 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionAvailability {
    Current,
    Stale,
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateVerdict {
    Retain,
    Reject,
    CanonicalRequired,
}

#[derive(Debug, Deserialize)]
struct QueryEnvelope {
    #[serde(default)]
    types: Vec<String>,
    timezone: Option<String>,
    #[serde(rename = "where")]
    where_expression: Option<String>,
    #[serde(default)]
    order_by: Vec<QueryOrder>,
    #[serde(default)]
    group_by: Vec<QueryOrder>,
    #[serde(default)]
    summaries: Vec<QuerySummary>,
    #[serde(default)]
    summary_functions: BTreeMap<String, Value>,
    limit: Option<u64>,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    include_body: bool,
    #[serde(default)]
    projections: BTreeMap<String, Value>,
    select: Option<Vec<Value>>,
    pagination: Option<String>,
}

#[derive(Debug, Deserialize)]
struct QueryOrder {
    field: String,
    #[serde(default)]
    direction: QueryDirection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum QueryDirection {
    #[default]
    Asc,
    Desc,
}

#[derive(Debug, Deserialize)]
struct QuerySummary {
    field: String,
    function: String,
    name: Option<String>,
}

impl CompiledCatalog {
    /// Compile a canonical v0.3 query into a closed provider-neutral plan.
    ///
    /// Lowering failure never means false: the candidate becomes broader and
    /// the original canonical query remains an mdbase-rs residual.
    pub fn compile_hosted_query(&self, input: &Value) -> Result<HostedQueryPlan, CatalogError> {
        let semantic_input = semantic_query_input(input)?;
        let schema_errors = validate_query(&semantic_input)
            .into_iter()
            .filter(|diagnostic| diagnostic.severity == "error")
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>();
        if !schema_errors.is_empty() {
            return Err(query_error("invalid_query", schema_errors.join("; ")));
        }
        let query: QueryEnvelope = serde_json::from_value(input.clone()).map_err(|error| {
            query_error(
                "invalid_query",
                format!("Query could not be decoded: {error}"),
            )
        })?;
        let canonical_query: Query =
            serde_json::from_value(semantic_input.clone()).map_err(|error| {
                query_error(
                    "invalid_query",
                    format!("Query could not be decoded: {error}"),
                )
            })?;
        let canonical_preflight = preflight::compile(canonical_query).map_err(|diagnostics| {
            query_error(
                "invalid_query",
                diagnostics
                    .into_iter()
                    .map(|diagnostic| diagnostic.message)
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        })?;
        resolve_execution_timezone(
            query.timezone.as_deref(),
            self.collection().settings.timezone.as_deref(),
        )
        .map_err(|message| query_error("invalid_timezone", message))?;
        if query.order_by.len() > MAX_ORDER_TERMS
            || query.group_by.len() > MAX_GROUP_TERMS
            || query.summaries.len() > MAX_SUMMARIES
        {
            return Err(query_error(
                "query_operator_limit_exceeded",
                "Query ordering, grouping, or summary count exceeds hosted plan limits.",
            ));
        }
        let mut requirements = HostedQueryRequirements::default();
        let mut predicates = Vec::new();
        if !query.types.is_empty() {
            predicates.push(CandidatePredicate::Or {
                terms: query
                    .types
                    .iter()
                    .map(|type_name| CandidatePredicate::HasType {
                        type_name: type_name.to_lowercase(),
                    })
                    .collect(),
            });
        }
        let mut fully_projected = true;
        if let Some(source) = query.where_expression.as_deref() {
            let expression = cel::compile(source).map_err(|error| {
                query_error(
                    &error.code,
                    format!("Query filter did not compile: {}", error.message),
                )
            })?;
            // Lowering may only prove candidate exclusion. The canonical
            // residual remains authoritative for every retained record.
            let lowered = lower_expression(&expression, &mut requirements);
            fully_projected = false;
            requirements.canonical_residual = true;
            match lowered {
                Some(lowered) => {
                    if !lowered.complete {
                        requirements.exact_document = true;
                    }
                    predicates.push(lowered.predicate);
                }
                None => requirements.exact_document = true,
            }
        }
        let candidate = conjunction(predicates);
        if predicate_nodes(&candidate) > MAX_PREDICATE_NODES {
            return Err(query_error(
                "query_operator_limit_exceeded",
                "Hosted candidate predicate exceeds the closed-plan node limit.",
            ));
        }

        let mut order = Vec::with_capacity(query.order_by.len());
        for item in &query.order_by {
            let Some(field) = lower_query_field(&item.field) else {
                return Err(query_error(
                    "unsupported_hosted_order",
                    format!(
                        "Order field '{}' requires a bounded exact sorter that is not available.",
                        item.field
                    ),
                ));
            };
            accumulate_field_requirement(&field, &mut requirements);
            order.push(HostedOrder {
                value_kind: hosted_scalar_kind(&field, &query.types, self.collection()),
                field,
                direction: direction(&item.direction),
                canonical_path_tiebreak: true,
                semantics: HostedSortSemantics::CanonicalV03,
            });
        }
        requirements.bounded_top_k = order
            .iter()
            .any(|item| !matches!(&item.field, CandidateField::Path));

        let mut groups = Vec::with_capacity(query.group_by.len());
        for item in &query.group_by {
            let Some(field) = lower_query_field(&item.field) else {
                return Err(query_error(
                    "unsupported_hosted_group",
                    format!("Group field '{}' is not projection-safe.", item.field),
                ));
            };
            accumulate_field_requirement(&field, &mut requirements);
            groups.push(HostedGroup {
                value_kind: hosted_scalar_kind(&field, &query.types, self.collection()),
                field,
                output_name: item.field.clone(),
                direction: direction(&item.direction),
            });
        }
        requirements.bounded_grouping = !groups.is_empty() || !query.summaries.is_empty();

        let mut aggregates = Vec::with_capacity(query.summaries.len());
        for item in &query.summaries {
            let Some(field) = lower_query_field(&item.field) else {
                return Err(query_error(
                    "unsupported_hosted_summary",
                    format!("Summary field '{}' is not projection-safe.", item.field),
                ));
            };
            let provider_safe = is_builtin_summary(&item.function)
                && !query.summary_functions.contains_key(&item.function);
            if !provider_safe {
                return Err(query_error(
                    "unsupported_hosted_summary",
                    format!(
                        "Summary function '{}' requires a canonical bounded reducer that is not available.",
                        item.function
                    ),
                ));
            }
            accumulate_field_requirement(&field, &mut requirements);
            aggregates.push(HostedAggregate {
                field,
                function: item.function.clone(),
                output_name: item.name.clone().unwrap_or_else(|| item.function.clone()),
                provider_safe,
            });
        }

        requirements.body_prose = query.include_body;
        requirements.exact_document |= query.include_body;
        requirements.query_context = canonical_preflight.requires_this_context();
        requirements.collection_context =
            requirements.query_context || canonical_preflight.requires_link_graph();
        requirements.relationships = canonical_preflight.requires_link_graph();
        requirements.structural_body_facts |= canonical_preflight.requires_file_body_metadata();
        requirements.canonical_residual |= requirements.query_context
            || !query.projections.is_empty()
            || query.select.is_some()
            || !fully_projected;
        requirements.diagnostic_type_matchers = self.has_diagnostic_type_matchers();
        requirements.exact_document |= requirements.query_context
            || !query.projections.is_empty()
            || query.select.is_some()
            || requirements.structural_body_facts
            || requirements.diagnostic_type_matchers;
        requirements.diagnostics = true;

        if query.limit.is_some_and(|limit| limit > MAX_PAGE_SIZE) {
            return Err(query_error(
                "hosted_result_budget_exceeded",
                format!("Requested limit exceeds the hosted page maximum of {MAX_PAGE_SIZE}."),
            ));
        }
        let default_budgets = HostedQueryBudgets::default();
        if query.offset > default_budgets.max_offset {
            return Err(query_error(
                "hosted_offset_budget_exceeded",
                format!(
                    "Requested offset exceeds the hosted maximum of {}.",
                    default_budgets.max_offset
                ),
            ));
        }
        if query
            .pagination
            .as_deref()
            .is_some_and(|value| value != "cursor")
        {
            return Err(query_error(
                "invalid_query",
                "Hosted query pagination must be 'cursor'.",
            ));
        }
        let page_size = query.limit.unwrap_or(DEFAULT_PAGE_SIZE);
        let canonical = serde_jcs::to_vec(&semantic_input).map_err(|error| {
            query_error(
                "invalid_query",
                format!("Query could not be canonicalized: {error}"),
            )
        })?;
        let mut plan = HostedQueryPlan {
            version: HOSTED_QUERY_PLAN_VERSION,
            semantic_engine_version: env!("CARGO_PKG_VERSION").to_string(),
            catalog_revision: self.resource_revision().to_string(),
            canonical_query_digest: format!("sha256:{:x}", Sha256::digest(canonical)),
            plan_digest: String::new(),
            candidate,
            residual: CanonicalResidual {
                query: semantic_input,
                filter_fully_projected: fully_projected,
            },
            order,
            groups,
            aggregates,
            page_size,
            requested_limit: query.limit,
            offset: query.offset,
            requirements,
            budgets: default_budgets,
        };
        plan.plan_digest = plan.integrity_digest()?;
        Ok(plan)
    }

    /// Canonically evaluate one retained exact record against a compiled plan.
    ///
    /// This is the point-residual seam used after provider candidate selection.
    /// It performs no enumeration or storage access. Plans needing a `this`
    /// record use [`Self::evaluate_hosted_residual_with_context`]; backlinks or
    /// cross-record traversal use a separate bounded relationship seam.
    pub fn evaluate_hosted_residual(
        &self,
        plan: &HostedQueryPlan,
        record: &CanonicalRecordInput,
    ) -> Result<HostedResidualEvaluation, CatalogError> {
        self.evaluate_hosted_residual_with_context(plan, record, None)
    }

    /// Evaluate one retained exact record with an optional exact `this`
    /// context. The context is a point input and is never retained in the plan.
    /// Link-graph traversal remains a separate bounded seam.
    pub fn evaluate_hosted_residual_with_context(
        &self,
        plan: &HostedQueryPlan,
        record: &CanonicalRecordInput,
        context_record: Option<&CanonicalRecordInput>,
    ) -> Result<HostedResidualEvaluation, CatalogError> {
        if plan.version != HOSTED_QUERY_PLAN_VERSION
            || plan.catalog_revision != self.resource_revision()
            || plan.semantic_engine_version != env!("CARGO_PKG_VERSION")
            || plan.integrity_digest()? != plan.plan_digest
        {
            return Err(query_error(
                "hosted_query_plan_mismatch",
                "Hosted query plan is not bound to the current semantic catalog.",
            ));
        }
        let canonical = serde_jcs::to_vec(&plan.residual.query).map_err(|error| {
            query_error(
                "invalid_query",
                format!("Residual query could not be canonicalized: {error}"),
            )
        })?;
        if format!("sha256:{:x}", Sha256::digest(canonical)) != plan.canonical_query_digest {
            return Err(query_error(
                "hosted_query_plan_mismatch",
                "Residual query digest does not match its compiled plan.",
            ));
        }

        let query: Query =
            serde_json::from_value(plan.residual.query.clone()).map_err(|error| {
                query_error(
                    "invalid_query",
                    format!("Query could not be decoded: {error}"),
                )
            })?;
        let compiled = preflight::compile(query).map_err(|diagnostics| {
            query_error(
                "invalid_query",
                diagnostics
                    .into_iter()
                    .map(|diagnostic| diagnostic.message)
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        })?;
        if compiled.requires_link_graph() {
            return Err(query_error(
                "hosted_collection_context_required",
                "This query requires a bounded relationship-graph residual.",
            ));
        }

        let this_context = match compiled
            .requires_this_context()
            .then_some(compiled.query.context.as_ref())
            .flatten()
        {
            None => None,
            Some(context) => {
                let expected_path = &context.this.path;
                let Some(context_record) =
                    context_record.filter(|context_record| &context_record.path == expected_path)
                else {
                    return Err(query_error(
                        "context_not_found",
                        format!("Query context record '{expected_path}' was not supplied."),
                    ));
                };
                Some(self.hosted_query_context(context_record)?)
            }
        };

        let classified = self.classify_record(record)?;
        let raw = Value::Object(classified.frontmatter);
        let collection = self.collection();
        let (types, match_failures) =
            collection.determine_types_for_path_checked(&raw, Some(&record.path));
        let mut diagnostics = match_failures
            .into_iter()
            .map(|(type_name, failure)| {
                diagnostics::evaluation(
                    &record.path,
                    "match.expr",
                    "match",
                    failure,
                    Some(type_name),
                )
            })
            .collect::<Vec<_>>();
        if !compiled.query.types.is_empty()
            && !types.iter().any(|actual| {
                compiled
                    .query
                    .types
                    .iter()
                    .any(|wanted| wanted.eq_ignore_ascii_case(actual))
            })
        {
            return Ok(HostedResidualEvaluation {
                matched: false,
                record: None,
                diagnostics,
                order_values: Vec::new(),
                group_values: Vec::new(),
                aggregate_values: Vec::new(),
            });
        }
        let effective = collection.apply_defaults(&raw, &types);
        let effective = collection.coerce_types(&effective, &types);
        let effective = collection.evaluate_computed_fields(
            effective,
            &types,
            &record.path,
            Some(&classified.body),
        );
        let file_record = FileRecord {
            rel_path: record.path.clone(),
            raw_frontmatter: raw,
            effective_frontmatter: effective.clone(),
            body: classified.body,
            type_names: types.clone(),
            file_size: if record.file_size == 0 {
                record.document.len() as u64
            } else {
                record.file_size
            },
            file_mtime_iso: record.file_mtime.clone(),
            file_ctime_iso: None,
        };
        let timezone = resolve_execution_timezone(
            compiled.query.timezone.as_deref(),
            collection.settings.timezone.as_deref(),
        )
        .map_err(|message| query_error("invalid_timezone", message))?;
        let clock = cel::operation_clock(timezone)
            .map_err(|error| query_error(&error.code, error.message))?;
        let type_definitions = Arc::new(collection.types.clone());
        let mut projections = serde_json::Map::new();
        for (name, expression) in &compiled.projections {
            let context = candidate_context(
                collection,
                &file_record,
                &types,
                &effective,
                &projections,
                this_context.clone(),
                None,
                None,
                type_definitions.clone(),
            );
            match cel::evaluate_compiled(expression, &context, &clock) {
                Ok(value) => {
                    projections.insert(name.clone(), value);
                }
                Err(error) => {
                    diagnostics.push(diagnostics::evaluation(
                        &record.path,
                        &format!("projections.{name}"),
                        "query_projection",
                        error,
                        None,
                    ));
                    projections.insert(name.clone(), Value::Null);
                }
            }
        }
        let context = candidate_context(
            collection,
            &file_record,
            &types,
            &effective,
            &projections,
            this_context,
            None,
            None,
            type_definitions,
        );
        let matched = match compiled.where_expression.as_ref() {
            None => true,
            Some(expression) => match cel::evaluate_compiled(expression, &context, &clock) {
                Ok(Value::Bool(true)) => true,
                Ok(_) => false,
                Err(error) => {
                    diagnostics.push(diagnostics::evaluation(
                        &record.path,
                        "where",
                        "query_filter",
                        error,
                        None,
                    ));
                    false
                }
            },
        };
        let file = file_value(
            &file_record,
            &effective,
            compiled.requires_file_body_metadata(),
        );
        let candidate = if matched {
            let mut values = serde_json::Map::new();
            for selection in &compiled.selections {
                match selection {
                    CompiledSelection::Field { source, name } => {
                        values.insert(
                            name.clone(),
                            namespace_value(source, &effective, &projections, &values, &file),
                        );
                    }
                    CompiledSelection::Expression { expression, name } => {
                        let value = match cel::evaluate_compiled(expression, &context, &clock) {
                            Ok(value) => value,
                            Err(error) => {
                                diagnostics.push(diagnostics::evaluation(
                                    &record.path,
                                    &format!("select.{name}"),
                                    "query_selection",
                                    error,
                                    None,
                                ));
                                Value::Null
                            }
                        };
                        values.insert(name.clone(), value);
                    }
                }
            }
            Some(Candidate {
                path: record.path.clone(),
                types,
                raw: file_record.raw_frontmatter,
                effective,
                body: file_record.body,
                file,
                projections,
                values,
            })
        } else {
            None
        };
        let (order_values, group_values, aggregate_values) = candidate
            .as_ref()
            .map(|candidate| hosted_operator_values(plan, candidate))
            .unwrap_or_default();
        let record_output = candidate
            .as_ref()
            .map(|candidate| serialize_candidate(candidate, &compiled.query));
        Ok(HostedResidualEvaluation {
            matched,
            record: record_output,
            diagnostics,
            order_values,
            group_values,
            aggregate_values,
        })
    }

    fn hosted_query_context(
        &self,
        record: &CanonicalRecordInput,
    ) -> Result<Box<crate::expressions::evaluator::EvalContext>, CatalogError> {
        use crate::expressions::evaluator::{EvalContext, NoteNamespaceSource};

        let read = self.read_record(&serde_json::json!({"path": record.path}), record);
        if !read.valid {
            return Err(query_error(
                "context_invalid",
                format!(
                    "Query context record '{}' is not canonically readable.",
                    record.path
                ),
            ));
        }
        let effective = read
            .result
            .get("effective_frontmatter")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let persisted = read
            .result
            .get("frontmatter")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let types = read
            .result
            .get("types")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect::<Vec<_>>();
        let mut bindings = cel::enrich_record_bindings(
            &effective,
            &persisted,
            cel::known_fields(self.collection(), &types).iter(),
        );
        if let Some(object) = bindings.as_object_mut() {
            object.insert(
                "types".to_string(),
                Value::Array(types.iter().cloned().map(Value::String).collect()),
            );
        }
        Ok(Box::new(EvalContext {
            frontmatter: bindings,
            raw_frontmatter: Some(persisted),
            file_path: Some(record.path.clone()),
            body: read
                .result
                .get("body")
                .and_then(Value::as_str)
                .map(String::from),
            file_size: Some(if record.file_size == 0 {
                record.document.len() as u64
            } else {
                record.file_size
            }),
            file_mtime: record.file_mtime.clone(),
            file_ctime: None,
            this_context: None,
            all_files: None,
            traversal_depth: std::cell::Cell::new(0),
            backlinks_index: None,
            type_names: Some(types),
            types: Some(Arc::new(self.collection().types.clone())),
            note_namespace_source: NoteNamespaceSource::Effective,
            string_concat: false,
        }))
    }

    /// Evaluate a projection-complete residual without decrypting exact body
    /// prose. The plan itself proves that no exact, body, collection-context,
    /// selection, or named-projection capability is required.
    pub fn evaluate_hosted_projection_residual(
        &self,
        plan: &HostedQueryPlan,
        projection: &SemanticProjection,
    ) -> Result<HostedResidualEvaluation, CatalogError> {
        if plan.version != HOSTED_QUERY_PLAN_VERSION
            || plan.catalog_revision != self.resource_revision()
            || plan.semantic_engine_version != env!("CARGO_PKG_VERSION")
            || plan.integrity_digest()? != plan.plan_digest
        {
            return Err(query_error(
                "hosted_query_plan_mismatch",
                "Hosted query plan is not bound to the current semantic catalog.",
            ));
        }
        if plan.requirements.exact_document
            || plan.requirements.body_prose
            || plan.requirements.structural_body_facts
            || plan.requirements.collection_context
            || !projection_is_current_for_plan(plan, projection)
        {
            return Err(query_error(
                "hosted_exact_residual_required",
                "This query or projection requires exact canonical evaluation.",
            ));
        }
        let query: Query =
            serde_json::from_value(plan.residual.query.clone()).map_err(|error| {
                query_error(
                    "invalid_query",
                    format!("Query could not be decoded: {error}"),
                )
            })?;
        let compiled = preflight::compile(query).map_err(|diagnostics| {
            query_error(
                "invalid_query",
                diagnostics
                    .into_iter()
                    .map(|diagnostic| diagnostic.message)
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        })?;
        if !compiled.projections.is_empty()
            || !compiled.selections.is_empty()
            || compiled.query.include_body
            || compiled.requires_this_context()
            || compiled.requires_link_graph()
        {
            return Err(query_error(
                "hosted_exact_residual_required",
                "This query requires an exact or collection-context residual.",
            ));
        }
        if !compiled.query.types.is_empty()
            && !projection.facts.types.iter().any(|actual| {
                compiled
                    .query
                    .types
                    .iter()
                    .any(|wanted| wanted.eq_ignore_ascii_case(actual))
            })
        {
            return Ok(HostedResidualEvaluation {
                matched: false,
                record: None,
                diagnostics: Vec::new(),
                order_values: Vec::new(),
                group_values: Vec::new(),
                aggregate_values: Vec::new(),
            });
        }
        let collection = self.collection();
        let timezone = resolve_execution_timezone(
            compiled.query.timezone.as_deref(),
            collection.settings.timezone.as_deref(),
        )
        .map_err(|message| query_error("invalid_timezone", message))?;
        let clock = cel::operation_clock(timezone)
            .map_err(|error| query_error(&error.code, error.message))?;
        let file_record = FileRecord {
            rel_path: projection.facts.path.clone(),
            raw_frontmatter: Value::Object(projection.facts.persisted_frontmatter.clone()),
            effective_frontmatter: Value::Object(projection.facts.effective_frontmatter.clone()),
            body: String::new(),
            type_names: projection.facts.types.clone(),
            file_size: projection.facts.file.size,
            file_mtime_iso: projection.facts.file.mtime.clone(),
            file_ctime_iso: None,
        };
        let effective = Value::Object(projection.facts.effective_frontmatter.clone());
        let projections = serde_json::Map::new();
        let context = candidate_context(
            collection,
            &file_record,
            &projection.facts.types,
            &effective,
            &projections,
            None,
            None,
            None,
            Arc::new(collection.types.clone()),
        );
        let mut diagnostics = Vec::new();
        let matched = match compiled.where_expression.as_ref() {
            None => true,
            Some(expression) => match cel::evaluate_compiled(expression, &context, &clock) {
                Ok(Value::Bool(true)) => true,
                Ok(_) => false,
                Err(error) => {
                    diagnostics.push(diagnostics::evaluation(
                        &projection.facts.path,
                        "where",
                        "query_filter",
                        error,
                        None,
                    ));
                    false
                }
            },
        };
        let mut file = file_value(&file_record, &effective, false);
        complete_file_value_from_projection(&mut file, &effective, projection);
        let candidate = matched.then(|| Candidate {
            path: projection.facts.path.clone(),
            types: projection.facts.types.clone(),
            raw: file_record.raw_frontmatter,
            effective,
            body: String::new(),
            file,
            projections,
            values: serde_json::Map::new(),
        });
        let (order_values, group_values, aggregate_values) = candidate
            .as_ref()
            .map(|candidate| hosted_operator_values(plan, candidate))
            .unwrap_or_default();
        let record = candidate
            .as_ref()
            .map(|candidate| serialize_candidate(candidate, &compiled.query));
        Ok(HostedResidualEvaluation {
            matched,
            record,
            diagnostics,
            order_values,
            group_values,
            aggregate_values,
        })
    }
}

impl HostedQueryPlan {
    /// Verify that every closed-plan field still matches the mdbase-rs digest
    /// captured at compilation. Hosts must call this before translating a
    /// durably stored plan into provider predicates.
    pub fn validate_integrity(&self) -> Result<(), CatalogError> {
        if self.version != HOSTED_QUERY_PLAN_VERSION || self.integrity_digest()? != self.plan_digest
        {
            return Err(query_error(
                "hosted_query_plan_mismatch",
                "Hosted query plan integrity validation failed.",
            ));
        }
        Ok(())
    }

    fn integrity_digest(&self) -> Result<String, CatalogError> {
        let mut unsigned = self.clone();
        unsigned.plan_digest.clear();
        let bytes = serde_jcs::to_vec(&unsigned).map_err(|error| {
            query_error(
                "invalid_hosted_query_plan",
                format!("Hosted query plan could not be canonicalized: {error}"),
            )
        })?;
        Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
    }

    /// Compare canonical operator values emitted by the point-residual seam.
    /// The path tie-break is always ascending, matching filesystem execution.
    pub fn compare_order_values(
        &self,
        left_values: &[Value],
        left_path: &str,
        right_values: &[Value],
        right_path: &str,
    ) -> std::cmp::Ordering {
        for (index, order) in self.order.iter().enumerate() {
            let comparison = compare_hosted_values(
                left_values.get(index).unwrap_or(&Value::Null),
                right_values.get(index).unwrap_or(&Value::Null),
                order.direction,
            );
            if comparison != std::cmp::Ordering::Equal {
                return comparison;
            }
        }
        left_path.cmp(right_path)
    }

    /// Build canonical query groups and built-in summaries from one bounded
    /// set of already-matched records. No record content is retained here.
    pub fn reduce_matches(
        &self,
        rows: &[HostedReductionInput],
    ) -> Result<HostedReduction, CatalogError> {
        let mut accumulator = self.start_reduction();
        for row in rows {
            accumulator.push(row)?;
        }
        accumulator.finish()
    }

    pub fn start_reduction(&self) -> HostedReductionAccumulator {
        HostedReductionAccumulator {
            plan: self.clone(),
            groups: BTreeMap::new(),
        }
    }

    /// Evaluate only whether SQL may discard a projected row. Stale, absent,
    /// incomplete, or semantically uncertain rows always reach canonical work.
    pub fn candidate_verdict(
        &self,
        projection: Option<&SemanticProjection>,
        availability: ProjectionAvailability,
    ) -> CandidateVerdict {
        if self.integrity_digest().ok().as_deref() != Some(self.plan_digest.as_str()) {
            return CandidateVerdict::CanonicalRequired;
        }
        if availability != ProjectionAvailability::Current {
            return CandidateVerdict::CanonicalRequired;
        }
        let Some(projection) = projection else {
            return CandidateVerdict::CanonicalRequired;
        };
        if !projection_is_current_for_plan(self, projection) {
            return CandidateVerdict::CanonicalRequired;
        }
        match evaluate_predicate(&self.candidate, projection) {
            Truth::True if self.residual.filter_fully_projected => CandidateVerdict::Retain,
            Truth::True | Truth::Unknown => CandidateVerdict::CanonicalRequired,
            Truth::False if self.requirements.diagnostic_type_matchers => {
                CandidateVerdict::CanonicalRequired
            }
            Truth::False => CandidateVerdict::Reject,
        }
    }
}

impl HostedReductionAccumulator {
    pub fn push(&mut self, row: &HostedReductionInput) -> Result<(), CatalogError> {
        if row.group_values.len() != self.plan.groups.len()
            || row.aggregate_values.len() != self.plan.aggregates.len()
        {
            return Err(query_error(
                "hosted_query_plan_mismatch",
                "Hosted reduction values do not align with the compiled plan.",
            ));
        }
        if self.plan.groups.is_empty() && self.plan.aggregates.is_empty() {
            return Ok(());
        }
        let values = self
            .plan
            .groups
            .iter()
            .zip(&row.group_values)
            .map(|(group, value)| (group.output_name.clone(), value.clone()))
            .collect::<serde_json::Map<_, _>>();
        let key = serde_json::to_string(&values).map_err(|error| {
            query_error(
                "invalid_hosted_reduction",
                format!("Hosted group values could not be canonicalized: {error}"),
            )
        })?;
        if !self.groups.contains_key(&key) {
            if !self.plan.groups.is_empty()
                && self.groups.len() as u64 >= self.plan.budgets.max_groups
            {
                return Err(query_error(
                    "hosted_group_budget_exceeded",
                    format!(
                        "Hosted grouping exceeded the maximum of {} groups.",
                        self.plan.budgets.max_groups
                    ),
                ));
            }
            self.groups.insert(
                key.clone(),
                HostedReductionGroupState {
                    values,
                    count: 0,
                    summaries: self
                        .plan
                        .aggregates
                        .iter()
                        .map(|aggregate| HostedSummaryState::new(&aggregate.function))
                        .collect(),
                },
            );
        }
        let group = self
            .groups
            .get_mut(&key)
            .expect("hosted reduction group was inserted above");
        group.count = group.count.saturating_add(1);
        for (summary, value) in group.summaries.iter_mut().zip(&row.aggregate_values) {
            summary.push(value);
        }
        Ok(())
    }

    pub fn finish(self) -> Result<HostedReduction, CatalogError> {
        if self.plan.groups.is_empty() && self.plan.aggregates.is_empty() {
            return Ok(HostedReduction {
                groups: None,
                diagnostics: Vec::new(),
            });
        }
        let mut groups = self.groups.into_values().collect::<Vec<_>>();
        groups.sort_by(|left, right| {
            for group in &self.plan.groups {
                let comparison = compare_hosted_values(
                    left.values.get(&group.output_name).unwrap_or(&Value::Null),
                    right.values.get(&group.output_name).unwrap_or(&Value::Null),
                    group.direction,
                );
                if comparison != std::cmp::Ordering::Equal {
                    return comparison;
                }
            }
            std::cmp::Ordering::Equal
        });
        let mut diagnostics = Vec::new();
        let groups = groups
            .into_iter()
            .map(|group| {
                let mut summaries = serde_json::Map::new();
                for (aggregate, state) in self.plan.aggregates.iter().zip(group.summaries) {
                    match state.finish(&aggregate.function) {
                        Ok(value) => {
                            summaries.insert(aggregate.output_name.clone(), value);
                        }
                        Err(message) => {
                            diagnostics.push(Diagnostic {
                                severity: "warning".to_string(),
                                code: "expression_evaluation_error".to_string(),
                                message,
                                path: None,
                                field: Some(format!("summaries.{}", aggregate.output_name)),
                                type_name: None,
                                schema_location: None,
                                details: Some(serde_json::json!({"context": "query_summary"})),
                            });
                            summaries.insert(aggregate.output_name.clone(), Value::Null);
                        }
                    }
                }
                serde_json::json!({
                    "values": group.values,
                    "count": group.count,
                    "summaries": summaries,
                })
            })
            .collect();
        Ok(HostedReduction {
            groups: Some(groups),
            diagnostics,
        })
    }
}

impl HostedSummaryState {
    fn new(function: &str) -> Self {
        match function {
            "count" => Self::Count(0),
            "empty" | "filled" => Self::Empty { total: 0, empty: 0 },
            "sum" | "average" | "minimum" | "maximum" => Self::Number {
                count: 0,
                sum: 0.0,
                minimum: f64::INFINITY,
                maximum: f64::NEG_INFINITY,
                invalid: false,
            },
            "earliest" => Self::String {
                selected: None,
                latest: false,
                invalid: false,
            },
            "latest" => Self::String {
                selected: None,
                latest: true,
                invalid: false,
            },
            _ => Self::Unknown,
        }
    }

    fn push(&mut self, value: &Value) {
        match self {
            Self::Count(count) => *count = count.saturating_add(1),
            Self::Empty { total, empty } => {
                *total = total.saturating_add(1);
                if hosted_value_is_empty(value) {
                    *empty = empty.saturating_add(1);
                }
            }
            Self::Number {
                count,
                sum,
                minimum,
                maximum,
                invalid,
            } => {
                if value.is_null() {
                    return;
                }
                let Some(number) = value.as_f64() else {
                    *invalid = true;
                    return;
                };
                *count = count.saturating_add(1);
                *sum += number;
                *minimum = minimum.min(number);
                *maximum = maximum.max(number);
            }
            Self::String {
                selected,
                latest,
                invalid,
            } => {
                if value.is_null() {
                    return;
                }
                let Some(value) = value.as_str() else {
                    *invalid = true;
                    return;
                };
                if selected.as_deref().is_none_or(|current| {
                    if *latest {
                        value > current
                    } else {
                        value < current
                    }
                }) {
                    *selected = Some(value.to_string());
                }
            }
            Self::Unknown => {}
        }
    }

    fn finish(self, function: &str) -> Result<Value, String> {
        match self {
            Self::Count(count) => Ok(serde_json::json!(count)),
            Self::Empty { total, empty } => Ok(serde_json::json!(if function == "empty" {
                empty
            } else {
                total.saturating_sub(empty)
            })),
            Self::Number {
                count,
                sum,
                minimum,
                maximum,
                invalid,
            } => {
                if invalid {
                    return Err(format!(
                        "Summary '{function}' received a non-numeric value."
                    ));
                }
                if count == 0 {
                    return Ok(Value::Null);
                }
                let number = match function {
                    "sum" => sum,
                    "average" => sum / count as f64,
                    "minimum" => minimum,
                    "maximum" => maximum,
                    _ => return Err(format!("Unknown hosted summary function '{function}'.")),
                };
                Ok(hosted_number_value(number))
            }
            Self::String {
                selected, invalid, ..
            } => {
                if invalid {
                    return Err(format!("Summary '{function}' received a non-string value."));
                }
                Ok(selected.map_or(Value::Null, Value::String))
            }
            Self::Unknown => Err(format!("Unknown hosted summary function '{function}'.")),
        }
    }
}

fn hosted_operator_values(
    plan: &HostedQueryPlan,
    candidate: &Candidate,
) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
    (
        plan.order
            .iter()
            .map(|order| hosted_candidate_value(candidate, &order.field))
            .collect(),
        plan.groups
            .iter()
            .map(|group| hosted_candidate_value(candidate, &group.field))
            .collect(),
        plan.aggregates
            .iter()
            .map(|aggregate| hosted_candidate_value(candidate, &aggregate.field))
            .collect(),
    )
}

fn hosted_candidate_value(candidate: &Candidate, field: &CandidateField) -> Value {
    match field {
        CandidateField::Path => Value::String(candidate.path.clone()),
        CandidateField::Types => {
            Value::Array(candidate.types.iter().cloned().map(Value::String).collect())
        }
        CandidateField::File(name) => candidate.file.get(name).cloned().unwrap_or(Value::Null),
        CandidateField::PersistedFrontmatter(path) => nested_value(&candidate.raw, path),
        CandidateField::EffectiveFrontmatter(path) => nested_value(&candidate.effective, path),
        CandidateField::BodyTags => candidate.file.get("tags").cloned().unwrap_or(Value::Null),
    }
}

fn nested_value(root: &Value, path: &[String]) -> Value {
    path.iter()
        .try_fold(root, |value, segment| value.get(segment))
        .cloned()
        .unwrap_or(Value::Null)
}

fn compare_hosted_values(
    left: &Value,
    right: &Value,
    direction: HostedOrderDirection,
) -> std::cmp::Ordering {
    let ascending = match (left.is_null(), right.is_null()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        _ => match (left, right) {
            (Value::Number(left), Value::Number(right)) => left
                .as_f64()
                .partial_cmp(&right.as_f64())
                .unwrap_or(std::cmp::Ordering::Equal),
            (Value::String(left), Value::String(right)) => left.cmp(right),
            (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
            (Value::Array(left), Value::Array(right)) => left.len().cmp(&right.len()),
            (Value::Object(left), Value::Object(right)) => left.len().cmp(&right.len()),
            _ => std::cmp::Ordering::Equal,
        },
    };
    match direction {
        HostedOrderDirection::Ascending => ascending,
        HostedOrderDirection::Descending => ascending.reverse(),
    }
}

fn hosted_number_value(value: f64) -> Value {
    if value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
        serde_json::json!(value as i64)
    } else {
        serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

fn hosted_value_is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(value) => value.is_empty(),
        Value::Array(value) => value.is_empty(),
        Value::Object(value) => value.is_empty(),
        Value::Bool(_) | Value::Number(_) => false,
    }
}

#[derive(Clone, Copy)]
enum Truth {
    True,
    False,
    Unknown,
}

fn lower_expression(
    expression: &Expr,
    requirements: &mut HostedQueryRequirements,
) -> Option<LoweredPredicate> {
    match expression {
        Expr::Bool(true) => Some(LoweredPredicate::complete(CandidatePredicate::All)),
        Expr::Bool(false) => Some(LoweredPredicate::complete(CandidatePredicate::None)),
        Expr::BinOp(left, BinOp::And, right) => {
            let left = lower_expression(left, requirements);
            let right = lower_expression(right, requirements);
            match (left, right) {
                (Some(left), Some(right)) => Some(LoweredPredicate {
                    predicate: CandidatePredicate::And {
                        terms: vec![left.predicate, right.predicate],
                    },
                    complete: left.complete && right.complete,
                }),
                // A safe conjunction remains a necessary condition even when
                // its sibling needs exact evaluation.
                (Some(safe), None) | (None, Some(safe)) => Some(LoweredPredicate {
                    predicate: safe.predicate,
                    complete: false,
                }),
                (None, None) => None,
            }
        }
        Expr::BinOp(left, BinOp::Or, right) => {
            let left = lower_expression(left, requirements)?;
            let right = lower_expression(right, requirements)?;
            if !left.complete || !right.complete {
                return None;
            }
            Some(LoweredPredicate::complete(CandidatePredicate::Or {
                terms: vec![left.predicate, right.predicate],
            }))
        }
        Expr::UnaryOp(UnaryOp::Not, inner) => {
            let inner = lower_expression(inner, requirements)?;
            if !inner.complete {
                return None;
            }
            Some(LoweredPredicate::complete(CandidatePredicate::Not {
                term: Box::new(inner.predicate),
            }))
        }
        Expr::BinOp(left, operator, right) => {
            let (field, value, reversed) =
                if let Some((field, value)) = field_and_literal(left, right) {
                    (field, value, false)
                } else {
                    let (field, value) = field_and_literal(right, left)?;
                    (field, value, true)
                };
            let operator = match (operator, reversed) {
                (BinOp::Eq, _) => CandidateComparisonOperator::Equal,
                (BinOp::Neq, _) => CandidateComparisonOperator::NotEqual,
                (BinOp::Lt, false) | (BinOp::Gt, true) => CandidateComparisonOperator::LessThan,
                (BinOp::Lte, false) | (BinOp::Gte, true) => {
                    CandidateComparisonOperator::LessThanOrEqual
                }
                (BinOp::Gt, false) | (BinOp::Lt, true) => CandidateComparisonOperator::GreaterThan,
                (BinOp::Gte, false) | (BinOp::Lte, true) => {
                    CandidateComparisonOperator::GreaterThanOrEqual
                }
                (BinOp::In, false) => CandidateComparisonOperator::In,
                (BinOp::In, true) => CandidateComparisonOperator::Contains,
                _ => return None,
            };
            accumulate_field_requirement(&field, requirements);
            Some(LoweredPredicate::complete(CandidatePredicate::Compare {
                comparison: CandidateComparison {
                    field,
                    operator,
                    pruning: comparison_pruning(operator, &value),
                    value,
                },
            }))
        }
        Expr::Call(function, arguments) if arguments.len() == 1 => {
            let Expr::Dot(receiver, method) = function.as_ref() else {
                return None;
            };
            if method != "contains" {
                return None;
            }
            let field = lower_field(receiver)?;
            let value = literal(&arguments[0])?;
            accumulate_field_requirement(&field, requirements);
            Some(LoweredPredicate::complete(CandidatePredicate::Compare {
                comparison: CandidateComparison {
                    field,
                    operator: CandidateComparisonOperator::Contains,
                    pruning: comparison_pruning(CandidateComparisonOperator::Contains, &value),
                    value,
                },
            }))
        }
        _ => None,
    }
}

struct LoweredPredicate {
    predicate: CandidatePredicate,
    complete: bool,
}

impl LoweredPredicate {
    fn complete(predicate: CandidatePredicate) -> Self {
        Self {
            predicate,
            complete: true,
        }
    }
}

fn field_and_literal(field: &Expr, value: &Expr) -> Option<(CandidateField, Value)> {
    Some((lower_field(field)?, literal(value)?))
}

fn lower_field(expression: &Expr) -> Option<CandidateField> {
    let path = expression_path(expression)?;
    lower_query_field(&path)
}

fn lower_query_field(path: &str) -> Option<CandidateField> {
    let segments = path.split('.').map(str::to_string).collect::<Vec<_>>();
    match segments.as_slice() {
        [single] if single == "types" => Some(CandidateField::Types),
        [root, rest @ ..] if root == "record" || root == "note" => {
            Some(CandidateField::EffectiveFrontmatter(rest.to_vec()))
        }
        [root, rest @ ..] if root == "raw" => {
            Some(CandidateField::PersistedFrontmatter(rest.to_vec()))
        }
        [root, field] if root == "file" && field == "tags" => Some(CandidateField::BodyTags),
        [root, field] if root == "file" && field == "path" => Some(CandidateField::Path),
        [root, field]
            if root == "file"
                && ["name", "basename", "ext", "size", "mtime"].contains(&field.as_str()) =>
        {
            Some(CandidateField::File(field.clone()))
        }
        [single] if !is_reserved_root(single) => {
            Some(CandidateField::EffectiveFrontmatter(vec![single.clone()]))
        }
        _ => None,
    }
}

fn hosted_scalar_kind(
    field: &CandidateField,
    selected_types: &[String],
    collection: &crate::Collection,
) -> Option<HostedScalarKind> {
    match field {
        CandidateField::Path => Some(HostedScalarKind::String),
        CandidateField::File(name) => match name.as_str() {
            "path" | "name" | "basename" | "ext" | "extension" | "mtime" => {
                Some(HostedScalarKind::String)
            }
            "size" => Some(HostedScalarKind::Number),
            _ => None,
        },
        CandidateField::PersistedFrontmatter(path) | CandidateField::EffectiveFrontmatter(path) => {
            if selected_types.is_empty() || path.is_empty() {
                return None;
            }
            let mut proven = None;
            for type_name in selected_types {
                let type_file = collection
                    .types
                    .values()
                    .find(|candidate| candidate.name.eq_ignore_ascii_case(type_name))?;
                let kind = field_scalar_kind(&type_file.fields, path)?;
                if proven.is_some_and(|current| current != kind) {
                    return None;
                }
                proven = Some(kind);
            }
            proven
        }
        CandidateField::Types | CandidateField::BodyTags => None,
    }
}

fn field_scalar_kind(
    fields: &std::collections::HashMap<String, crate::types::schema::FieldDef>,
    path: &[String],
) -> Option<HostedScalarKind> {
    let (first, rest) = path.split_first()?;
    let mut field = fields.get(first)?;
    for segment in rest {
        field = field.fields.as_ref()?.get(segment)?;
    }
    match field.field_type.as_str() {
        "string" | "date" | "datetime" | "time" | "duration" | "link" | "path" | "enum" => {
            Some(HostedScalarKind::String)
        }
        "number" | "integer" => Some(HostedScalarKind::Number),
        "boolean" => Some(HostedScalarKind::Boolean),
        _ => None,
    }
}

fn semantic_query_input(input: &Value) -> Result<Value, CatalogError> {
    let mut object = input
        .as_object()
        .cloned()
        .ok_or_else(|| query_error("invalid_query", "Hosted query input must be an object."))?;
    // Hosted pagination controls page transport, not record semantics. Keeping
    // them out of the canonical digest lets a cursor consumer change its next
    // page size without changing the pinned query meaning.
    for control in [
        "pagination",
        "cursor",
        "release_cursor",
        "snapshot",
        "limit",
        "offset",
    ] {
        object.remove(control);
    }
    Ok(Value::Object(object))
}

fn expression_path(expression: &Expr) -> Option<String> {
    match expression {
        Expr::Ident(name) => Some(name.clone()),
        Expr::Dot(parent, field) => Some(format!("{}.{}", expression_path(parent)?, field)),
        Expr::Index(parent, index) => {
            let Expr::Str(field) = index.as_ref() else {
                return None;
            };
            Some(format!("{}.{}", expression_path(parent)?, field))
        }
        _ => None,
    }
}

fn literal(expression: &Expr) -> Option<Value> {
    match expression {
        Expr::Null => Some(Value::Null),
        Expr::Bool(value) => Some(Value::Bool(*value)),
        Expr::Number(value) => serde_json::Number::from_f64(*value).map(Value::Number),
        Expr::Str(value) => Some(Value::String(value.clone())),
        Expr::Array(values) => values
            .iter()
            .map(literal)
            .collect::<Option<Vec<_>>>()
            .map(Value::Array),
        _ => None,
    }
}

fn evaluate_predicate(predicate: &CandidatePredicate, projection: &SemanticProjection) -> Truth {
    match predicate {
        CandidatePredicate::All => Truth::True,
        CandidatePredicate::None => Truth::False,
        CandidatePredicate::And { terms } => fold_and(
            terms
                .iter()
                .map(|term| evaluate_predicate(term, projection)),
        ),
        CandidatePredicate::Or { terms } => fold_or(
            terms
                .iter()
                .map(|term| evaluate_predicate(term, projection)),
        ),
        CandidatePredicate::Not { term } => match evaluate_predicate(term, projection) {
            Truth::True => Truth::False,
            Truth::False => Truth::True,
            Truth::Unknown => Truth::Unknown,
        },
        CandidatePredicate::HasType { type_name } => bool_truth(
            projection
                .facts
                .types
                .iter()
                .any(|candidate| candidate == type_name),
        ),
        CandidatePredicate::Compare { comparison }
            if comparison.pruning == CandidateComparisonPruning::ExactJson =>
        {
            let Some(value) = projection_value(projection, &comparison.field) else {
                return Truth::Unknown;
            };
            compare(value, comparison.operator, &comparison.value)
        }
        CandidatePredicate::Compare { .. } => Truth::Unknown,
    }
}

fn projection_value<'a>(
    projection: &'a SemanticProjection,
    field: &CandidateField,
) -> Option<&'a Value> {
    match field {
        CandidateField::Path => None,
        CandidateField::Types => None,
        CandidateField::File(name) => match name.as_str() {
            // Scalar file fields are handled through owned values below. Until
            // the IR carries typed scalars, retaining them is conservative.
            "path" | "name" | "basename" | "extension" | "size" | "mtime" => None,
            _ => None,
        },
        CandidateField::PersistedFrontmatter(path) => {
            nested(&projection.facts.persisted_frontmatter, path)
        }
        CandidateField::EffectiveFrontmatter(path) => {
            nested(&projection.facts.effective_frontmatter, path)
        }
        CandidateField::BodyTags => None,
    }
}

fn projection_is_current_for_plan(plan: &HostedQueryPlan, projection: &SemanticProjection) -> bool {
    projection.facts.semantic_complete
        && projection.facts.schema_version == SEMANTIC_PROJECTION_SCHEMA_VERSION
        && projection.facts.format_version == SEMANTIC_PROJECTION_FORMAT_VERSION
        && projection.facts.semantic_engine_version == plan.semantic_engine_version
        && projection.facts.catalog_revision == plan.catalog_revision
        && projection.structure.schema_version == RECORD_STRUCTURE_SCHEMA_VERSION
        && projection.structure.path == projection.facts.path
        && projection.structure.structural_digest_is_valid()
}

fn complete_file_value_from_projection(
    file: &mut Value,
    effective: &Value,
    projection: &SemanticProjection,
) {
    let mut tags = effective
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect::<Vec<_>>();
    for tag in &projection.structure.body_tags {
        if !tags.contains(tag) {
            tags.push(tag.clone());
        }
    }
    if let Some(object) = file.as_object_mut() {
        object.insert("tags".to_string(), serde_json::json!(tags));
        object.insert(
            "links".to_string(),
            serde_json::json!(projection.structure.body_links),
        );
        object.insert(
            "embeds".to_string(),
            serde_json::json!(projection.structure.body_embeds),
        );
    }
}

fn nested<'a>(root: &'a serde_json::Map<String, Value>, path: &[String]) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    let mut value = root.get(first)?;
    for segment in rest {
        value = value.get(segment)?;
    }
    Some(value)
}

fn compare(left: &Value, operator: CandidateComparisonOperator, right: &Value) -> Truth {
    use CandidateComparisonOperator as Op;
    match operator {
        Op::Equal => bool_truth(left == right),
        Op::NotEqual => bool_truth(left != right),
        Op::In => right
            .as_array()
            .map(|values| bool_truth(values.contains(left)))
            .unwrap_or(Truth::Unknown),
        Op::Contains => left
            .as_array()
            .map(|values| bool_truth(values.contains(right)))
            .or_else(|| Some(bool_truth(left.as_str()?.contains(right.as_str()?))))
            .unwrap_or(Truth::Unknown),
        Op::LessThan | Op::LessThanOrEqual | Op::GreaterThan | Op::GreaterThanOrEqual => {
            let ordering = match (left, right) {
                (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
                (Value::Number(left), Value::Number(right)) => left
                    .as_f64()
                    .zip(right.as_f64())
                    .and_then(|(left, right)| left.partial_cmp(&right)),
                _ => None,
            };
            let Some(ordering) = ordering else {
                return Truth::Unknown;
            };
            bool_truth(match operator {
                Op::LessThan => ordering.is_lt(),
                Op::LessThanOrEqual => ordering.is_le(),
                Op::GreaterThan => ordering.is_gt(),
                Op::GreaterThanOrEqual => ordering.is_ge(),
                _ => unreachable!(),
            })
        }
    }
}

fn comparison_pruning(
    operator: CandidateComparisonOperator,
    value: &Value,
) -> CandidateComparisonPruning {
    use CandidateComparisonOperator as Op;
    let exact_scalar = |value: &Value| match value {
        Value::Null | Value::Bool(_) => true,
        Value::String(value) => !ambiguous_canonical_string(value),
        Value::Number(_) | Value::Array(_) | Value::Object(_) => false,
    };
    if value.as_str().is_some_and(is_iso_date_only)
        && matches!(
            operator,
            Op::Equal
                | Op::NotEqual
                | Op::LessThan
                | Op::LessThanOrEqual
                | Op::GreaterThan
                | Op::GreaterThanOrEqual
        )
    {
        return CandidateComparisonPruning::IsoDateOnlyString;
    }
    let exact = match operator {
        Op::Equal | Op::NotEqual => exact_scalar(value),
        Op::LessThan | Op::LessThanOrEqual | Op::GreaterThan | Op::GreaterThanOrEqual => {
            value.is_number()
                || value
                    .as_str()
                    .is_some_and(|value| !ambiguous_canonical_string(value))
        }
        Op::In => value
            .as_array()
            .is_some_and(|values| values.iter().all(exact_scalar)),
        Op::Contains => exact_scalar(value),
    };
    if exact {
        CandidateComparisonPruning::ExactJson
    } else {
        CandidateComparisonPruning::Conservative
    }
}

fn is_iso_date_only(value: &str) -> bool {
    value.len() == 10
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[7] == b'-'
        && chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok()
}

fn ambiguous_canonical_string(value: &str) -> bool {
    value.parse::<f64>().is_ok()
        || chrono::DateTime::parse_from_rfc3339(value).is_ok()
        || chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok()
        || chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S").is_ok()
}

fn fold_and(values: impl Iterator<Item = Truth>) -> Truth {
    let mut unknown = false;
    for value in values {
        match value {
            Truth::False => return Truth::False,
            Truth::Unknown => unknown = true,
            Truth::True => {}
        }
    }
    if unknown {
        Truth::Unknown
    } else {
        Truth::True
    }
}

fn fold_or(values: impl Iterator<Item = Truth>) -> Truth {
    let mut unknown = false;
    for value in values {
        match value {
            Truth::True => return Truth::True,
            Truth::Unknown => unknown = true,
            Truth::False => {}
        }
    }
    if unknown {
        Truth::Unknown
    } else {
        Truth::False
    }
}

fn bool_truth(value: bool) -> Truth {
    if value {
        Truth::True
    } else {
        Truth::False
    }
}

fn conjunction(mut predicates: Vec<CandidatePredicate>) -> CandidatePredicate {
    match predicates.len() {
        0 => CandidatePredicate::All,
        1 => predicates.pop().expect("length checked"),
        _ => CandidatePredicate::And { terms: predicates },
    }
}

fn predicate_nodes(predicate: &CandidatePredicate) -> usize {
    match predicate {
        CandidatePredicate::And { terms } | CandidatePredicate::Or { terms } => {
            1 + terms.iter().map(predicate_nodes).sum::<usize>()
        }
        CandidatePredicate::Not { term } => 1 + predicate_nodes(term),
        _ => 1,
    }
}

fn accumulate_field_requirement(
    field: &CandidateField,
    requirements: &mut HostedQueryRequirements,
) {
    match field {
        CandidateField::PersistedFrontmatter(_) => requirements.persisted_frontmatter = true,
        CandidateField::EffectiveFrontmatter(_) => requirements.effective_frontmatter = true,
        CandidateField::BodyTags => requirements.structural_body_facts = true,
        CandidateField::Path | CandidateField::Types | CandidateField::File(_) => {}
    }
}

fn direction(value: &QueryDirection) -> HostedOrderDirection {
    match value {
        QueryDirection::Asc => HostedOrderDirection::Ascending,
        QueryDirection::Desc => HostedOrderDirection::Descending,
    }
}

fn is_builtin_summary(name: &str) -> bool {
    matches!(
        name,
        "count"
            | "sum"
            | "average"
            | "minimum"
            | "maximum"
            | "earliest"
            | "latest"
            | "empty"
            | "filled"
    )
}

fn is_reserved_root(value: &str) -> bool {
    matches!(
        value,
        "record"
            | "raw"
            | "present"
            | "file"
            | "note"
            | "projection"
            | "this"
            | "values"
            | "old"
            | "operation"
            | "event"
            | "workflow"
            | "trigger"
            | "steps"
            | "vars"
            | "item"
    )
}

fn query_error(code: &str, message: impl Into<String>) -> CatalogError {
    CatalogError {
        code: code.to_string(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{CatalogInput, PreparedSemanticProjection, ResolvedTypeResource};
    use serde_json::json;

    fn catalog() -> CompiledCatalog {
        CompiledCatalog::compile(CatalogInput {
            resource_revision: "catalog-7".to_string(),
            configuration_document: "spec_version: 0.3.0\nsettings:\n  default_validation: warn\n"
                .to_string(),
            types: vec![ResolvedTypeResource {
                path: "_types/task.md".to_string(),
                revision: "type-1".to_string(),
                definition: json!({
                    "kind": "mdbase.type",
                    "name": "task",
                    "version": 1,
                    "match": {"path_glob": "tasks/*.md"},
                    "schema": {"dialect": "json-schema-2020-12", "value": {
                        "type": "object",
                        "properties": {"status": {"type": "string"}}
                    }}
                }),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "status": {"type": "string"},
                        "effort": {"type": "number"}
                    }
                }),
            }],
            contracts: Vec::new(),
        })
        .unwrap()
    }

    fn projection(status: &str, complete: bool) -> SemanticProjection {
        let catalog = catalog();
        let prepared: PreparedSemanticProjection = catalog
            .project_record(&super::super::CanonicalRecordInput {
                stable_id: Some("record-1".to_string()),
                path: "tasks/one.md".to_string(),
                document: format!("---\nstatus: {status}\n---\nText\n"),
                file_size: 0,
                file_mtime: None,
            })
            .unwrap();
        let mut facts = prepared.facts;
        facts.semantic_complete = complete;
        let unresolved = PreparedSemanticProjection {
            facts,
            structure: prepared.structure,
        };
        let plan = catalog
            .plan_record_resolution(&unresolved.structure)
            .unwrap();
        let resolved = catalog
            .resolve_record_structure(&unresolved.structure, &plan, &[])
            .unwrap();
        catalog.finalize_projection(unresolved, resolved).unwrap()
    }

    fn finalized_projection(
        catalog: &CompiledCatalog,
        record: &CanonicalRecordInput,
    ) -> SemanticProjection {
        let prepared = catalog.project_record(record).unwrap();
        let plan = catalog.plan_record_resolution(&prepared.structure).unwrap();
        let resolved = catalog
            .resolve_record_structure(&prepared.structure, &plan, &[])
            .unwrap();
        catalog.finalize_projection(prepared, resolved).unwrap()
    }

    #[test]
    fn exact_json_comparisons_prune_but_still_use_canonical_residuals() {
        let plan = catalog()
            .compile_hosted_query(&json!({
                "types": ["task"],
                "where": "record.status == 'open'",
                "order_by": [{"field": "file.path"}],
                "limit": 50
            }))
            .unwrap();
        assert!(!plan.residual.filter_fully_projected);
        assert!(matches!(
            plan.candidate,
            CandidatePredicate::And { ref terms }
                if terms.iter().any(|term| matches!(
                    term,
                    CandidatePredicate::Compare { comparison }
                        if comparison.pruning == CandidateComparisonPruning::ExactJson
                ))
        ));
        assert_eq!(plan.page_size, 50);
        assert_eq!(
            plan.candidate_verdict(
                Some(&projection("closed", true)),
                ProjectionAvailability::Current
            ),
            CandidateVerdict::Reject
        );
        assert_eq!(
            plan.candidate_verdict(
                Some(&projection("closed", true)),
                ProjectionAvailability::Stale
            ),
            CandidateVerdict::CanonicalRequired
        );
    }

    #[test]
    fn datetime_and_numeric_coercions_are_never_exact_json_pruning_proofs() {
        let date = catalog()
            .compile_hosted_query(&json!({
                "where": "record.due < '2026-06-01'",
                "limit": 10
            }))
            .unwrap();
        assert!(matches!(
            date.candidate,
            CandidatePredicate::Compare { ref comparison }
                if comparison.pruning == CandidateComparisonPruning::IsoDateOnlyString
        ));
        let numeric = catalog()
            .compile_hosted_query(&json!({"where": "record.count == 42", "limit": 10}))
            .unwrap();
        assert!(matches!(
            numeric.candidate,
            CandidatePredicate::Compare { ref comparison }
                if comparison.pruning == CandidateComparisonPruning::Conservative
        ));
        for plan in [date, numeric] {
            assert_eq!(
                plan.candidate_verdict(
                    Some(&projection("open", true)),
                    ProjectionAvailability::Current
                ),
                CandidateVerdict::CanonicalRequired
            );
        }
    }

    #[test]
    fn projection_complete_filter_uses_canonical_residual_without_exact_body() {
        let catalog = catalog();
        let plan = catalog
            .compile_hosted_query(&json!({
                "types": ["task"],
                "where": "record.status == 'open'",
                "limit": 10
            }))
            .unwrap();
        assert!(!plan.requirements.exact_document);

        let matching = catalog
            .evaluate_hosted_projection_residual(&plan, &projection("open", true))
            .unwrap();
        assert!(matching.matched);
        assert_eq!(
            matching.record.unwrap()["effective_frontmatter"]["status"],
            "open"
        );

        let missing = catalog
            .evaluate_hosted_projection_residual(&plan, &projection("closed", true))
            .unwrap();
        assert!(!missing.matched);
        assert!(missing.record.is_none());
    }

    #[test]
    fn projection_result_preserves_canonical_body_metadata_without_body_prose() {
        let catalog = catalog();
        let record = CanonicalRecordInput {
            stable_id: Some("record-1".to_string()),
            path: "tasks/one.md".to_string(),
            document: "---\nstatus: open\ntags: [frontmatter]\n---\n#body [[target#anchor|Alias]] [other](other.md#section) ![[embed#part]] ![image](asset.png)\n"
                .to_string(),
            file_size: 0,
            file_mtime: None,
        };
        let projection = finalized_projection(&catalog, &record);
        let plan = catalog
            .compile_hosted_query(&json!({"types": ["task"], "limit": 10}))
            .unwrap();

        let exact = catalog
            .evaluate_hosted_residual(&plan, &record)
            .unwrap()
            .record
            .unwrap();
        let projected = catalog
            .evaluate_hosted_projection_residual(&plan, &projection)
            .unwrap()
            .record
            .unwrap();

        for field in ["tags", "links", "embeds"] {
            assert_eq!(
                projected.pointer(&format!("/file/{field}")),
                exact.pointer(&format!("/file/{field}")),
                "projection-backed file.{field} diverged from exact evaluation"
            );
        }
        assert_eq!(projected["file"]["tags"], json!(["frontmatter", "body"]));
        assert_eq!(projected["file"]["links"], json!(["target", "other.md"]));
        assert_eq!(
            projected["file"]["embeds"],
            json!(["embed#part", "asset.png"])
        );
        assert!(!projected.to_string().contains("Alias"));
    }

    #[test]
    fn projection_binding_mismatch_requires_exact_fallback() {
        let catalog = catalog();
        let plan = catalog
            .compile_hosted_query(&json!({"types": ["task"], "limit": 10}))
            .unwrap();
        let mut stale_projection = projection("open", true);
        stale_projection.facts.catalog_revision = "stale-catalog".to_string();

        assert_eq!(
            plan.candidate_verdict(Some(&stale_projection), ProjectionAvailability::Current),
            CandidateVerdict::CanonicalRequired
        );
        let error = catalog
            .evaluate_hosted_projection_residual(&plan, &stale_projection)
            .unwrap_err();
        assert_eq!(error.code, "hosted_exact_residual_required");

        let mut projection = projection("open", true);
        projection
            .structure
            .body_links
            .push("forged.md".to_string());
        assert_eq!(
            plan.candidate_verdict(Some(&projection), ProjectionAvailability::Current),
            CandidateVerdict::CanonicalRequired
        );
        let error = catalog
            .evaluate_hosted_projection_residual(&plan, &projection)
            .unwrap_err();
        assert_eq!(error.code, "hosted_exact_residual_required");
    }

    #[test]
    fn diagnostic_matchers_prevent_type_candidate_pruning() {
        let catalog = CompiledCatalog::compile(CatalogInput {
            resource_revision: "catalog-diagnostic".to_string(),
            configuration_document: "spec_version: 0.3.0\nsettings:\n  default_validation: warn\n"
                .to_string(),
            types: vec![ResolvedTypeResource {
                path: "_types/task.md".to_string(),
                revision: "type-1".to_string(),
                definition: json!({
                    "kind": "mdbase.type",
                    "name": "task",
                    "version": 1,
                    "match": {"expr": {"$expr": "missing.value == true"}},
                    "schema": {"dialect": "json-schema-2020-12", "value": {
                        "type": "object"
                    }}
                }),
                schema: json!({"type": "object"}),
            }],
            contracts: Vec::new(),
        })
        .unwrap();
        let record = CanonicalRecordInput {
            stable_id: Some("record-1".to_string()),
            path: "notes/one.md".to_string(),
            document: "---\nstatus: open\n---\nText\n".to_string(),
            file_size: 0,
            file_mtime: None,
        };
        let projection = finalized_projection(&catalog, &record);
        let plan = catalog
            .compile_hosted_query(&json!({"types": ["task"], "limit": 10}))
            .unwrap();

        assert!(plan.requirements.diagnostic_type_matchers);
        assert_eq!(
            plan.candidate_verdict(Some(&projection), ProjectionAvailability::Current),
            CandidateVerdict::CanonicalRequired
        );
    }

    #[test]
    fn body_structural_filters_require_exact_until_query_context_supports_them() {
        let catalog = catalog();
        let plan = catalog
            .compile_hosted_query(&json!({
                "where": "file.tags.contains('urgent')",
                "limit": 10
            }))
            .unwrap();
        assert!(plan.requirements.structural_body_facts);
        assert!(plan.requirements.exact_document);
        let error = catalog
            .evaluate_hosted_projection_residual(&plan, &projection("open", true))
            .unwrap_err();
        assert_eq!(error.code, "hosted_exact_residual_required");
    }

    #[test]
    fn incomplete_projection_is_fail_closed_even_under_negation() {
        let plan = catalog()
            .compile_hosted_query(&json!({
                "where": "!(record.status == 'closed')",
                "limit": 10
            }))
            .unwrap();
        assert_eq!(
            plan.candidate_verdict(
                Some(&projection("closed", false)),
                ProjectionAvailability::Current
            ),
            CandidateVerdict::CanonicalRequired
        );
    }

    #[test]
    fn proven_conjunct_may_prune_before_an_exact_body_residual() {
        let plan = catalog()
            .compile_hosted_query(&json!({
                "where": "record.status == 'open' && file.body.contains('needle')",
                "include_body": true,
                "limit": 10
            }))
            .unwrap();
        assert!(!plan.residual.filter_fully_projected);
        assert!(plan.requirements.canonical_residual);
        assert_eq!(
            plan.candidate_verdict(
                Some(&projection("closed", true)),
                ProjectionAvailability::Current
            ),
            CandidateVerdict::Reject
        );
    }

    #[test]
    fn partial_disjunction_and_negation_never_narrow_candidates() {
        for source in [
            "record.status == 'open' || file.body.contains('needle')",
            "!(record.status == 'open' && file.body.contains('needle'))",
        ] {
            let plan = catalog()
                .compile_hosted_query(&json!({"where": source, "limit": 10}))
                .unwrap();
            assert_eq!(plan.candidate, CandidatePredicate::All);
            assert!(!plan.residual.filter_fully_projected);
            assert_eq!(
                plan.candidate_verdict(
                    Some(&projection("closed", true)),
                    ProjectionAvailability::Current
                ),
                CandidateVerdict::CanonicalRequired
            );
        }
    }

    #[test]
    fn type_filter_is_disjunctive() {
        let plan = catalog()
            .compile_hosted_query(&json!({"types": ["task", "note"], "limit": 5}))
            .unwrap();
        assert!(matches!(
            plan.candidate,
            CandidatePredicate::Or { ref terms } if terms.len() == 2
        ));
    }

    #[test]
    fn exact_residual_evaluates_body_without_collection_enumeration() {
        let catalog = catalog();
        let plan = catalog
            .compile_hosted_query(&json!({
                "where": "record.status == 'open' && file.body.contains('needle')",
                "include_body": true,
                "limit": 10
            }))
            .unwrap();
        let matching = CanonicalRecordInput {
            stable_id: Some("record-1".to_string()),
            path: "tasks/one.md".to_string(),
            document: "---\nstatus: open\n---\nsecret needle\n".to_string(),
            file_size: 0,
            file_mtime: None,
        };
        let missing = CanonicalRecordInput {
            document: "---\nstatus: open\n---\nother prose\n".to_string(),
            ..matching.clone()
        };
        let evaluation = catalog.evaluate_hosted_residual(&plan, &matching).unwrap();
        assert!(evaluation.matched);
        assert_eq!(
            evaluation.record.as_ref().unwrap()["body"],
            "secret needle\n"
        );
        assert!(
            !catalog
                .evaluate_hosted_residual(&plan, &missing)
                .unwrap()
                .matched
        );
    }

    #[test]
    fn residual_plan_binding_is_fail_closed() {
        let catalog = catalog();
        let mut plan = catalog
            .compile_hosted_query(&json!({"where": "record.status == 'open'", "limit": 5}))
            .unwrap();
        plan.residual.query["where"] = Value::String("true".to_string());
        let error = catalog
            .evaluate_hosted_residual(
                &plan,
                &CanonicalRecordInput {
                    stable_id: None,
                    path: "tasks/one.md".to_string(),
                    document: "---\nstatus: open\n---\n".to_string(),
                    file_size: 0,
                    file_mtime: None,
                },
            )
            .unwrap_err();
        assert_eq!(error.code, "hosted_query_plan_mismatch");

        let mut plan = catalog
            .compile_hosted_query(&json!({"types": ["task"], "limit": 5}))
            .unwrap();
        plan.candidate = CandidatePredicate::None;
        let error = catalog
            .evaluate_hosted_residual(
                &plan,
                &CanonicalRecordInput {
                    stable_id: None,
                    path: "tasks/one.md".to_string(),
                    document: "---\nstatus: open\n---\n".to_string(),
                    file_size: 0,
                    file_mtime: None,
                },
            )
            .unwrap_err();
        assert_eq!(error.code, "hosted_query_plan_mismatch");
    }

    #[test]
    fn residual_honors_query_timezone_and_provider_file_size() {
        let catalog = catalog();
        let invalid = catalog
            .compile_hosted_query(&json!({"timezone": "local", "limit": 5}))
            .unwrap_err();
        assert_eq!(invalid.code, "invalid_timezone");

        let plan = catalog
            .compile_hosted_query(&json!({
                "timezone": "Australia/Melbourne",
                "where": "file.size == 999",
                "limit": 5
            }))
            .unwrap();
        let evaluation = catalog
            .evaluate_hosted_residual(
                &plan,
                &CanonicalRecordInput {
                    stable_id: None,
                    path: "tasks/one.md".to_string(),
                    document: "---\nstatus: open\n---\nshort\n".to_string(),
                    file_size: 999,
                    file_mtime: None,
                },
            )
            .unwrap();
        assert!(evaluation.matched);
    }

    #[test]
    fn page_maximum_is_a_typed_error_not_a_clamp() {
        let error = catalog()
            .compile_hosted_query(&json!({"limit": MAX_PAGE_SIZE + 1}))
            .unwrap_err();
        assert_eq!(error.code, "hosted_result_budget_exceeded");

        let error = catalog()
            .compile_hosted_query(&json!({"offset": 10_001, "limit": 1}))
            .unwrap_err();
        assert_eq!(error.code, "hosted_offset_budget_exceeded");
    }

    #[test]
    fn exact_order_is_rejected_until_a_bounded_sorter_exists() {
        let error = catalog()
            .compile_hosted_query(&json!({"order_by": [{"field": "file.body"}]}))
            .unwrap_err();
        assert_eq!(error.code, "unsupported_hosted_order");
    }

    #[test]
    fn plan_is_closed_versioned_and_deterministic() {
        let query = json!({"types": ["task"], "limit": 5});
        let first = catalog().compile_hosted_query(&query).unwrap();
        let second = catalog().compile_hosted_query(&query).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.version, HOSTED_QUERY_PLAN_VERSION);
        assert_eq!(first.version, 8);
        assert!(first.canonical_query_digest.starts_with("sha256:"));
        assert!(first.plan_digest.starts_with("sha256:"));
        assert_eq!(serde_json::to_value(first).unwrap()["version"], 8);
    }

    #[test]
    fn cursor_control_is_host_transport_and_ordering_is_explicit() {
        let plan = catalog()
            .compile_hosted_query(&json!({
                "pagination": "cursor",
                "order_by": [{"field": "file.ext", "direction": "desc"}],
                "limit": 20
            }))
            .unwrap();
        assert_eq!(plan.order[0].field, CandidateField::File("ext".to_string()));
        assert_eq!(plan.order[0].semantics, HostedSortSemantics::CanonicalV03);
        assert!(plan.order[0].canonical_path_tiebreak);
        assert!(plan.requirements.bounded_top_k);

        let grouped = catalog()
            .compile_hosted_query(&json!({
                "types": ["task"],
                "group_by": [{"field": "record.status"}],
                "limit": 20
            }))
            .unwrap();
        assert_eq!(grouped.groups[0].output_name, "record.status");
        assert!(grouped.requirements.bounded_grouping);
        assert_eq!(grouped.groups[0].value_kind, Some(HostedScalarKind::String));

        let ordered = catalog()
            .compile_hosted_query(&json!({
                "types": ["task"],
                "order_by": [
                    {"field": "record.status"},
                    {"field": "file.mtime", "direction": "desc"}
                ]
            }))
            .unwrap();
        assert_eq!(ordered.order[0].value_kind, Some(HostedScalarKind::String));
        assert_eq!(ordered.order[1].value_kind, Some(HostedScalarKind::String));
    }

    #[test]
    fn canonical_operator_values_drive_bounded_order_and_reduction() {
        let catalog = catalog();
        let plan = catalog
            .compile_hosted_query(&json!({
                "order_by": [{"field": "record.effort", "direction": "desc"}],
                "group_by": [{"field": "record.status"}],
                "summaries": [
                    {"field": "record.effort", "function": "sum", "name": "effort"},
                    {"field": "record.status", "function": "count", "name": "records"}
                ]
            }))
            .unwrap();
        let evaluate = |path: &str, status: &str, effort: u64| {
            catalog
                .evaluate_hosted_residual(
                    &plan,
                    &CanonicalRecordInput {
                        stable_id: None,
                        path: path.to_string(),
                        document: format!("---\nstatus: {status}\neffort: {effort}\n---\nBody\n"),
                        file_size: 0,
                        file_mtime: None,
                    },
                )
                .unwrap()
        };
        let low = evaluate("tasks/low.md", "open", 2);
        let high = evaluate("tasks/high.md", "open", 5);
        assert!(plan
            .compare_order_values(
                &high.order_values,
                "tasks/high.md",
                &low.order_values,
                "tasks/low.md"
            )
            .is_lt());
        let reduction = plan
            .reduce_matches(&[
                HostedReductionInput {
                    group_values: low.group_values,
                    aggregate_values: low.aggregate_values,
                },
                HostedReductionInput {
                    group_values: high.group_values,
                    aggregate_values: high.aggregate_values,
                },
            ])
            .unwrap();
        assert_eq!(
            reduction.groups,
            Some(vec![json!({
                "values": {"record.status": "open"},
                "count": 2,
                "summaries": {"effort": 7, "records": 2}
            })])
        );
        assert!(reduction.diagnostics.is_empty());
    }

    #[test]
    fn streaming_reduction_retains_only_bounded_group_state() {
        let plan = catalog()
            .compile_hosted_query(&json!({
                "group_by": [{"field": "record.status"}],
                "summaries": [
                    {"field": "record.effort", "function": "sum", "name": "sum"},
                    {"field": "record.effort", "function": "average", "name": "average"},
                    {"field": "record.effort", "function": "minimum", "name": "minimum"},
                    {"field": "record.effort", "function": "maximum", "name": "maximum"},
                    {"field": "record.effort", "function": "count", "name": "count"}
                ]
            }))
            .unwrap();
        let mut accumulator = plan.start_reduction();
        for index in 0..100_000_u64 {
            accumulator
                .push(&HostedReductionInput {
                    group_values: vec![Value::String(if index % 2 == 0 {
                        "even".to_string()
                    } else {
                        "odd".to_string()
                    })],
                    aggregate_values: vec![serde_json::json!(index); 5],
                })
                .unwrap();
        }
        assert_eq!(accumulator.groups.len(), 2);
        let reduction = accumulator.finish().unwrap();
        let groups = reduction.groups.unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["count"], 50_000);
        assert_eq!(groups[0]["summaries"]["count"], 50_000);
        assert_eq!(groups[0]["summaries"]["minimum"], 0);
        assert_eq!(groups[0]["summaries"]["maximum"], 99_998);
        assert_eq!(groups[1]["summaries"]["minimum"], 1);
        assert_eq!(groups[1]["summaries"]["maximum"], 99_999);
        assert!(reduction.diagnostics.is_empty());
    }

    #[test]
    fn transport_controls_are_absent_from_semantic_plan_and_digest() {
        let catalog = catalog();
        let query = json!({
            "types": ["task"],
            "order_by": [{"field": "file.path"}],
            "limit": 20
        });
        let controlled = json!({
            "types": ["task"],
            "order_by": [{"field": "file.path"}],
            "limit": 250,
            "offset": 50,
            "pagination": "cursor",
            "cursor": "opaque-secret-token",
            "snapshot": "provider-snapshot-id",
            "release_cursor": true
        });

        let base = catalog.compile_hosted_query(&query).unwrap();
        let with_transport = catalog.compile_hosted_query(&controlled).unwrap();
        let residual = with_transport.residual.query.as_object().unwrap();
        assert_eq!(
            with_transport.canonical_query_digest,
            base.canonical_query_digest
        );
        assert_eq!(with_transport.residual.query, base.residual.query);
        assert_ne!(with_transport.plan_digest, base.plan_digest);
        for control in [
            "pagination",
            "cursor",
            "snapshot",
            "release_cursor",
            "limit",
            "offset",
        ] {
            assert!(!residual.contains_key(control));
        }
    }
}
