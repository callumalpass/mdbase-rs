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
use crate::query::cache_source::FileRecord;
use crate::v03::query::context::{candidate_context, file_value};
use crate::v03::query::{model::Query, preflight};
use crate::v03::{cel, validate_query, Diagnostic};

use super::{CanonicalRecordInput, CatalogError, CompiledCatalog, SemanticProjection};

pub const HOSTED_QUERY_PLAN_VERSION: u32 = 1;
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
    pub candidate: CandidatePredicate,
    pub residual: CanonicalResidual,
    pub order: Vec<HostedOrder>,
    pub groups: Vec<HostedGroup>,
    pub aggregates: Vec<HostedAggregate>,
    pub page_size: u64,
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
    pub diagnostics: Vec<Diagnostic>,
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
    pub stable_identity_tiebreak: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedGroup {
    pub field: CandidateField,
    pub direction: HostedOrderDirection,
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
    pub canonical_residual: bool,
    pub bounded_top_k: bool,
    pub bounded_grouping: bool,
    pub collection_context: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedQueryBudgets {
    pub max_page_size: u64,
    pub max_candidate_rows: u64,
    pub max_candidate_bytes: u64,
    pub max_exact_documents: u64,
    pub max_exact_bytes: u64,
    pub max_operator_steps: u64,
    pub max_groups: u64,
    pub max_wall_time_ms: u64,
    pub max_snapshot_time_ms: u64,
    pub max_memory_bytes: u64,
}

impl Default for HostedQueryBudgets {
    fn default() -> Self {
        Self {
            max_page_size: MAX_PAGE_SIZE,
            max_candidate_rows: 10_000,
            max_candidate_bytes: 16 * 1024 * 1024,
            max_exact_documents: 2_000,
            max_exact_bytes: 64 * 1024 * 1024,
            max_operator_steps: 2_000_000,
            max_groups: 10_000,
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
    context: Option<Value>,
    #[serde(default)]
    projections: BTreeMap<String, Value>,
    select: Option<Vec<Value>>,
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
        let schema_errors = validate_query(input)
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
        let canonical_query: Query = serde_json::from_value(input.clone()).map_err(|error| {
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
            match lower_expression(&expression, &mut requirements) {
                Some(lowered) => {
                    predicates.push(lowered.predicate);
                    fully_projected = lowered.complete;
                    if !lowered.complete {
                        requirements.canonical_residual = true;
                        requirements.exact_document = true;
                    }
                }
                None => {
                    fully_projected = false;
                    requirements.canonical_residual = true;
                    requirements.exact_document = true;
                }
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
                requirements.bounded_top_k = true;
                requirements.canonical_residual = true;
                requirements.exact_document = true;
                continue;
            };
            accumulate_field_requirement(&field, &mut requirements);
            order.push(HostedOrder {
                field,
                direction: direction(&item.direction),
                stable_identity_tiebreak: true,
            });
        }
        if order.len() != query.order_by.len() && query.limit.is_none() {
            return Err(query_error(
                "unbounded_exact_order",
                "Ordering that is not projection-safe requires an explicit bounded limit.",
            ));
        }

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
                field,
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
                requirements.canonical_residual = true;
                requirements.exact_document = true;
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
        requirements.collection_context =
            query.context.is_some() || canonical_preflight.requires_link_graph();
        requirements.structural_body_facts |= canonical_preflight.requires_file_body_metadata();
        requirements.canonical_residual |= query.context.is_some()
            || !query.projections.is_empty()
            || query.select.is_some()
            || !fully_projected;
        requirements.exact_document |= requirements.canonical_residual;

        let page_size = query.limit.unwrap_or(DEFAULT_PAGE_SIZE).min(MAX_PAGE_SIZE);
        let canonical = serde_jcs::to_vec(input).map_err(|error| {
            query_error(
                "invalid_query",
                format!("Query could not be canonicalized: {error}"),
            )
        })?;
        Ok(HostedQueryPlan {
            version: HOSTED_QUERY_PLAN_VERSION,
            semantic_engine_version: env!("CARGO_PKG_VERSION").to_string(),
            catalog_revision: self.resource_revision().to_string(),
            canonical_query_digest: format!("sha256:{:x}", Sha256::digest(canonical)),
            candidate,
            residual: CanonicalResidual {
                query: input.clone(),
                filter_fully_projected: fully_projected,
            },
            order,
            groups,
            aggregates,
            page_size,
            offset: query.offset,
            requirements,
            budgets: HostedQueryBudgets::default(),
        })
    }

    /// Canonically evaluate one retained exact record against a compiled plan.
    ///
    /// This is the point-residual seam used after provider candidate selection.
    /// It performs no enumeration or storage access. Plans needing a `this`
    /// record, backlinks, or cross-record traversal are rejected so a host must
    /// use the separate bounded collection-context seam rather than silently
    /// changing semantics.
    pub fn evaluate_hosted_residual(
        &self,
        plan: &HostedQueryPlan,
        record: &CanonicalRecordInput,
    ) -> Result<HostedResidualEvaluation, CatalogError> {
        if plan.version != HOSTED_QUERY_PLAN_VERSION
            || plan.catalog_revision != self.resource_revision()
            || plan.semantic_engine_version != env!("CARGO_PKG_VERSION")
        {
            return Err(query_error(
                "hosted_query_plan_mismatch",
                "Hosted query plan is not bound to the current semantic catalog.",
            ));
        }
        if plan.requirements.collection_context {
            return Err(query_error(
                "hosted_collection_context_required",
                "This query requires a bounded collection-context residual.",
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
        if compiled.query.context.is_some() || compiled.requires_link_graph() {
            return Err(query_error(
                "hosted_collection_context_required",
                "This query requires a bounded collection-context residual.",
            ));
        }

        let classified = self.classify_record(record)?;
        let types = classified.types;
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
                diagnostics: Vec::new(),
            });
        }
        let read = self.read_record(&serde_json::json!({"path": record.path}), record);
        let effective = read
            .result
            .get("effective_frontmatter")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        let file_record = FileRecord {
            rel_path: record.path.clone(),
            raw_frontmatter: Value::Object(classified.frontmatter),
            effective_frontmatter: effective.clone(),
            body: classified.body,
            type_names: types.clone(),
            file_size: record.document.len() as u64,
            file_mtime_iso: record.file_mtime.clone(),
            file_ctime_iso: None,
        };
        let collection = self.collection();
        let clock = cel::operation_clock(collection.settings.timezone.as_deref())
            .map_err(|error| query_error(&error.code, error.message))?;
        let type_definitions = Arc::new(collection.types.clone());
        let mut projections = serde_json::Map::new();
        let mut diagnostics = read.diagnostics;
        for (name, expression) in &compiled.projections {
            let context = candidate_context(
                collection,
                &file_record,
                &types,
                &effective,
                &projections,
                None,
                None,
                None,
                type_definitions.clone(),
            );
            match cel::evaluate_compiled(expression, &context, &clock) {
                Ok(value) => {
                    projections.insert(name.clone(), value);
                }
                Err(error) => {
                    diagnostics.push(Diagnostic::error(
                        error.code,
                        error.message,
                        Some(record.path.clone()),
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
            None,
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
                    diagnostics.push(Diagnostic::error(
                        error.code,
                        error.message,
                        Some(record.path.clone()),
                    ));
                    false
                }
            },
        };
        // Force construction here so body-derived file fields use the exact
        // record semantics when the expression evaluator requested them.
        let _ = file_value(
            &file_record,
            &effective,
            compiled.requires_file_body_metadata(),
        );
        Ok(HostedResidualEvaluation {
            matched,
            diagnostics,
        })
    }
}

impl HostedQueryPlan {
    /// Evaluate only whether SQL may discard a projected row. Stale, absent,
    /// incomplete, or semantically uncertain rows always reach canonical work.
    pub fn candidate_verdict(
        &self,
        projection: Option<&SemanticProjection>,
        availability: ProjectionAvailability,
    ) -> CandidateVerdict {
        if availability != ProjectionAvailability::Current {
            return CandidateVerdict::CanonicalRequired;
        }
        let Some(projection) = projection else {
            return CandidateVerdict::CanonicalRequired;
        };
        if !projection.facts.semantic_complete {
            return CandidateVerdict::CanonicalRequired;
        }
        match evaluate_predicate(&self.candidate, projection) {
            Truth::True if self.residual.filter_fully_projected => CandidateVerdict::Retain,
            Truth::True | Truth::Unknown => CandidateVerdict::CanonicalRequired,
            Truth::False => CandidateVerdict::Reject,
        }
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
        [single] if single == "path" => Some(CandidateField::Path),
        [root, rest @ ..] if root == "record" || root == "note" => {
            Some(CandidateField::EffectiveFrontmatter(rest.to_vec()))
        }
        [root, rest @ ..] if root == "raw" => {
            Some(CandidateField::PersistedFrontmatter(rest.to_vec()))
        }
        [root, field] if root == "file" && field == "tags" => Some(CandidateField::BodyTags),
        [root, field]
            if root == "file"
                && ["path", "name", "basename", "extension", "size", "mtime"]
                    .contains(&field.as_str()) =>
        {
            Some(CandidateField::File(field.clone()))
        }
        [single] if !is_reserved_root(single) => {
            Some(CandidateField::EffectiveFrontmatter(vec![single.clone()]))
        }
        _ => None,
    }
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
        CandidatePredicate::Compare { comparison } => {
            let Some(value) = projection_value(projection, &comparison.field) else {
                return Truth::Unknown;
            };
            compare(value, comparison.operator, &comparison.value)
        }
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
    use crate::runtime::{
        CatalogInput, PreparedSemanticProjection, ResolvedRecordStructure, ResolvedTypeResource,
    };
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
                schema: json!({"type": "object"}),
            }],
            contracts: Vec::new(),
        })
        .unwrap()
    }

    fn projection(status: &str, complete: bool) -> SemanticProjection {
        let prepared: PreparedSemanticProjection = catalog()
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
        SemanticProjection {
            facts,
            structure: ResolvedRecordStructure {
                schema_version: prepared.structure.schema_version,
                path: prepared.structure.path,
                structural_digest: prepared.structure.structural_digest,
                body_tags: prepared.structure.body_tags,
                occurrences: Vec::new(),
            },
        }
    }

    #[test]
    fn lowers_safe_filter_and_rejects_only_current_complete_false_rows() {
        let plan = catalog()
            .compile_hosted_query(&json!({
                "types": ["task"],
                "where": "record.status == 'open'",
                "order_by": [{"field": "file.path"}],
                "limit": 50
            }))
            .unwrap();
        assert!(plan.residual.filter_fully_projected);
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
    fn partial_conjunction_keeps_only_necessary_safe_condition() {
        let plan = catalog()
            .compile_hosted_query(&json!({
                "where": "record.status == 'open' && file.body.contains('needle')",
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
        assert!(
            catalog
                .evaluate_hosted_residual(&plan, &matching)
                .unwrap()
                .matched
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
    }

    #[test]
    fn exact_order_requires_a_bound() {
        let error = catalog()
            .compile_hosted_query(&json!({"order_by": [{"field": "file.body"}]}))
            .unwrap_err();
        assert_eq!(error.code, "unbounded_exact_order");
    }

    #[test]
    fn plan_is_closed_versioned_and_deterministic() {
        let query = json!({"types": ["task"], "limit": 5});
        let first = catalog().compile_hosted_query(&query).unwrap();
        let second = catalog().compile_hosted_query(&query).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.version, HOSTED_QUERY_PLAN_VERSION);
        assert!(first.canonical_query_digest.starts_with("sha256:"));
        assert_eq!(serde_json::to_value(first).unwrap()["version"], 1);
    }
}
