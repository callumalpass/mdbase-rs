//! Benchmark-only semantic projection and closed candidate evaluator.
//!
//! This module deliberately has no storage, SQL, encryption, or authority types.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::{CanonicalRecordInput, CatalogError, CompiledCatalog};

pub const PROJECTION_SCHEMA_VERSION: &str = "hosted-benchmark-projection-v1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkProjection {
    pub schema_version: String,
    pub path: String,
    pub types: Vec<String>,
    pub file: BenchmarkFileFacts,
    pub persisted_frontmatter: Map<String, Value>,
    pub effective_frontmatter: Map<String, Value>,
    pub relationships: Vec<ProjectionRelationship>,
    pub diagnostics: Vec<BenchmarkDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkFileFacts {
    pub path: String,
    pub name: String,
    pub basename: String,
    pub extension: String,
    pub size: u64,
    pub mtime: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionRelationship {
    pub kind: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchmarkDiagnostic {
    pub code: String,
    pub severity: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CandidateExpression {
    All {
        all: Vec<CandidateExpression>,
    },
    Any {
        any: Vec<CandidateExpression>,
    },
    Not {
        not: Box<CandidateExpression>,
    },
    TypeIn {
        #[serde(rename = "typeIn")]
        type_in: Vec<String>,
    },
    FieldEq {
        #[serde(rename = "fieldEq")]
        field_eq: (String, Value),
    },
    FieldIn {
        #[serde(rename = "fieldIn")]
        field_in: (String, Vec<Value>),
    },
    FieldContains {
        #[serde(rename = "fieldContains")]
        field_contains: (String, Value),
    },
    FieldContainsText {
        #[serde(rename = "fieldContainsText")]
        field_contains_text: (String, String),
    },
    FieldLt {
        #[serde(rename = "fieldLt")]
        field_lt: (String, Value),
    },
    RelationshipTargetEq {
        #[serde(rename = "relationshipTargetEq")]
        relationship_target_eq: String,
    },
    BodyContains {
        #[serde(rename = "bodyContains")]
        body_contains: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateTruth {
    Possible,
    Impossible,
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryRequirements {
    pub exact_document: bool,
    pub body: bool,
    pub persisted_frontmatter: bool,
    pub effective_frontmatter: bool,
    pub relationships: bool,
    pub file_facts: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompiledCandidate {
    pub expression: CandidateExpression,
    pub requirements: QueryRequirements,
}

impl CompiledCatalog {
    /// Produce the provider-neutral projection used only by the storage-model
    /// benchmark. Exact Markdown remains the canonical input.
    pub fn benchmark_project_record(
        &self,
        record: &CanonicalRecordInput,
    ) -> Result<BenchmarkProjection, CatalogError> {
        let classified = self.classify_record(record)?;
        let read = self.read_record(&serde_json::json!({"path": record.path}), record);
        let (effective_frontmatter, mut diagnostics) = if read.valid {
            (
                read.result
                    .get("effective_frontmatter")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default(),
                read.diagnostics
                    .iter()
                    .map(|diagnostic| BenchmarkDiagnostic {
                        code: diagnostic.code.clone(),
                        severity: diagnostic.severity.clone(),
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            (
                Map::new(),
                read.diagnostics
                    .iter()
                    .map(|diagnostic| BenchmarkDiagnostic {
                        code: diagnostic.code.clone(),
                        severity: diagnostic.severity.clone(),
                    })
                    .collect::<Vec<_>>(),
            )
        };
        if classified.frontmatter_error.is_some()
            && !diagnostics
                .iter()
                .any(|item| item.code == "frontmatter_parse_failed")
        {
            diagnostics.push(BenchmarkDiagnostic {
                code: "frontmatter_parse_failed".to_string(),
                severity: "error".to_string(),
            });
        }
        diagnostics.sort_by(|left, right| {
            left.code
                .cmp(&right.code)
                .then_with(|| left.severity.cmp(&right.severity))
        });
        diagnostics.dedup();

        let (name, basename, extension) = file_name_parts(&record.path);
        let relationships = benchmark_relationships(&classified.frontmatter);
        Ok(BenchmarkProjection {
            schema_version: PROJECTION_SCHEMA_VERSION.to_string(),
            path: record.path.clone(),
            types: classified.types,
            file: BenchmarkFileFacts {
                path: record.path.clone(),
                name,
                basename,
                extension,
                size: record.file_size,
                mtime: record
                    .file_mtime
                    .clone()
                    .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string()),
            },
            persisted_frontmatter: classified.frontmatter,
            effective_frontmatter,
            relationships,
            diagnostics,
        })
    }
}

impl BenchmarkProjection {
    /// Stable JCS digest of the semantic envelope. Authority-owned currentness
    /// bindings are intentionally added by the authority's outer digest.
    pub fn canonical_digest(&self) -> Result<String, serde_json::Error> {
        let bytes = serde_jcs::to_vec(self)?;
        Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
    }
}

impl CandidateExpression {
    pub fn compile(self) -> Result<CompiledCandidate, CatalogError> {
        validate_expression(&self)?;
        let mut requirements = QueryRequirements::default();
        accumulate_requirements(&self, &mut requirements);
        Ok(CompiledCandidate {
            expression: self,
            requirements,
        })
    }

    pub fn evaluate_candidate(&self, projection: &BenchmarkProjection) -> CandidateTruth {
        candidate_truth(self, projection)
    }

    pub fn evaluate_canonical(&self, projection: &BenchmarkProjection, body: &str) -> bool {
        canonical_truth(self, projection, body)
    }
}

impl CompiledCandidate {
    pub fn evaluate_candidate(&self, projection: &BenchmarkProjection) -> CandidateTruth {
        self.expression.evaluate_candidate(projection)
    }

    pub fn evaluate_canonical(&self, projection: &BenchmarkProjection, body: &str) -> bool {
        self.expression.evaluate_canonical(projection, body)
    }
}

fn validate_expression(expression: &CandidateExpression) -> Result<(), CatalogError> {
    match expression {
        CandidateExpression::All { all } => {
            for item in all {
                validate_expression(item)?;
            }
        }
        CandidateExpression::Any { any } => {
            for item in any {
                validate_expression(item)?;
            }
        }
        CandidateExpression::Not { not } => validate_expression(not)?,
        CandidateExpression::TypeIn { type_in } if type_in.is_empty() => {
            return Err(candidate_error("typeIn must contain at least one type"));
        }
        CandidateExpression::FieldEq { field_eq }
        | CandidateExpression::FieldLt { field_lt: field_eq } => validate_field(&field_eq.0)?,
        CandidateExpression::FieldIn { field_in } if field_in.1.is_empty() => {
            return Err(candidate_error("fieldIn must contain at least one value"));
        }
        CandidateExpression::FieldIn { field_in } => validate_field(&field_in.0)?,
        CandidateExpression::FieldContains { field_contains } => validate_field(&field_contains.0)?,
        CandidateExpression::FieldContainsText {
            field_contains_text,
        } => {
            validate_field(&field_contains_text.0)?;
            if field_contains_text.1.is_empty() {
                return Err(candidate_error("fieldContainsText must not be empty"));
            }
        }
        CandidateExpression::RelationshipTargetEq {
            relationship_target_eq,
        } if relationship_target_eq.is_empty() => {
            return Err(candidate_error("relationshipTargetEq must not be empty"));
        }
        CandidateExpression::BodyContains { body_contains } if body_contains.is_empty() => {
            return Err(candidate_error("bodyContains must not be empty"));
        }
        _ => {}
    }
    Ok(())
}

fn validate_field(field: &str) -> Result<(), CatalogError> {
    const ALLOWED: &[&str] = &[
        "path",
        "types",
        "file.basename",
        "file.mtime",
        "persisted_frontmatter.status",
        "persisted_frontmatter.priority",
        "persisted_frontmatter.tags",
        "persisted_frontmatter.contexts",
        "persisted_frontmatter.projects",
        "persisted_frontmatter.due",
        "persisted_frontmatter.created_at",
        "effective_frontmatter.status",
        "effective_frontmatter.archived",
        "effective_frontmatter.priority",
        "effective_frontmatter.tags",
        "effective_frontmatter.contexts",
        "effective_frontmatter.projects",
        "effective_frontmatter.due",
        "effective_frontmatter.created_at",
        "effective_frontmatter.reading.status",
        "effective_frontmatter.title",
    ];
    if ALLOWED.contains(&field) {
        Ok(())
    } else {
        Err(candidate_error(&format!("field is not allowed: {field}")))
    }
}

fn candidate_error(message: &str) -> CatalogError {
    CatalogError {
        code: "invalid_candidate_expression".to_string(),
        message: message.to_string(),
    }
}

fn accumulate_requirements(expression: &CandidateExpression, output: &mut QueryRequirements) {
    match expression {
        CandidateExpression::All { all } | CandidateExpression::Any { any: all } => {
            for item in all {
                accumulate_requirements(item, output);
            }
        }
        CandidateExpression::Not { not } => accumulate_requirements(not, output),
        CandidateExpression::FieldEq { field_eq }
        | CandidateExpression::FieldLt { field_lt: field_eq } => {
            field_requirements(&field_eq.0, output)
        }
        CandidateExpression::FieldIn { field_in } => field_requirements(&field_in.0, output),
        CandidateExpression::FieldContains { field_contains } => {
            field_requirements(&field_contains.0, output)
        }
        CandidateExpression::FieldContainsText {
            field_contains_text,
        } => field_requirements(&field_contains_text.0, output),
        CandidateExpression::RelationshipTargetEq { .. } => output.relationships = true,
        CandidateExpression::BodyContains { .. } => {
            output.body = true;
            output.exact_document = true;
        }
        CandidateExpression::TypeIn { .. } => {}
    }
}

fn field_requirements(field: &str, output: &mut QueryRequirements) {
    if field.starts_with("persisted_frontmatter.") {
        output.persisted_frontmatter = true;
    } else if field.starts_with("effective_frontmatter.") {
        output.effective_frontmatter = true;
    } else if field.starts_with("file.") {
        output.file_facts = true;
    }
}

fn candidate_truth(
    expression: &CandidateExpression,
    projection: &BenchmarkProjection,
) -> CandidateTruth {
    match expression {
        CandidateExpression::All { all } => {
            let values = all.iter().map(|item| candidate_truth(item, projection));
            fold_all(values)
        }
        CandidateExpression::Any { any } => {
            let values = any.iter().map(|item| candidate_truth(item, projection));
            fold_any(values)
        }
        CandidateExpression::Not { not } => match candidate_truth(not, projection) {
            CandidateTruth::Possible => CandidateTruth::Impossible,
            CandidateTruth::Impossible => CandidateTruth::Possible,
            CandidateTruth::Unknown => CandidateTruth::Unknown,
        },
        CandidateExpression::BodyContains { .. } => CandidateTruth::Unknown,
        _ => bool_truth(canonical_truth(expression, projection, "")),
    }
}

fn canonical_truth(
    expression: &CandidateExpression,
    projection: &BenchmarkProjection,
    body: &str,
) -> bool {
    match expression {
        CandidateExpression::All { all } => all
            .iter()
            .all(|item| canonical_truth(item, projection, body)),
        CandidateExpression::Any { any } => any
            .iter()
            .any(|item| canonical_truth(item, projection, body)),
        CandidateExpression::Not { not } => !canonical_truth(not, projection, body),
        CandidateExpression::TypeIn { type_in } => {
            type_in.iter().any(|value| projection.types.contains(value))
        }
        CandidateExpression::FieldEq { field_eq } => {
            field_value_owned(projection, &field_eq.0).is_some_and(|value| value == field_eq.1)
        }
        CandidateExpression::FieldIn { field_in } => field_value_owned(projection, &field_in.0)
            .is_some_and(|value| field_in.1.iter().any(|candidate| candidate == &value)),
        CandidateExpression::FieldContains { field_contains } => {
            field_value_owned(projection, &field_contains.0)
                .and_then(|value| value.as_array().cloned())
                .is_some_and(|values| values.contains(&field_contains.1))
        }
        CandidateExpression::FieldContainsText {
            field_contains_text,
        } => field_value_owned(projection, &field_contains_text.0)
            .and_then(|value| value.as_str().map(str::to_string))
            .is_some_and(|value| {
                value
                    .to_lowercase()
                    .contains(&field_contains_text.1.to_lowercase())
            }),
        CandidateExpression::FieldLt { field_lt } => field_value_owned(projection, &field_lt.0)
            .is_some_and(|value| compare_json(&value, &field_lt.1) == Some(Ordering::Less)),
        CandidateExpression::RelationshipTargetEq {
            relationship_target_eq,
        } => projection
            .relationships
            .iter()
            .any(|item| item.target == *relationship_target_eq),
        CandidateExpression::BodyContains { body_contains } => {
            body.to_lowercase().contains(&body_contains.to_lowercase())
        }
    }
}

fn field_value_owned(projection: &BenchmarkProjection, path: &str) -> Option<Value> {
    if path == "path" {
        return Some(Value::String(projection.path.clone()));
    }
    if path == "types" {
        return serde_json::to_value(&projection.types).ok();
    }
    let (root, remainder) = path.split_once('.')?;
    let mut value = match root {
        "persisted_frontmatter" => Value::Object(projection.persisted_frontmatter.clone()),
        "effective_frontmatter" => Value::Object(projection.effective_frontmatter.clone()),
        "file" => serde_json::to_value(&projection.file).ok()?,
        _ => return None,
    };
    for segment in remainder.split('.') {
        value = value.get(segment)?.clone();
    }
    Some(value)
}

fn compare_json(left: &Value, right: &Value) -> Option<Ordering> {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left.as_f64()?.partial_cmp(&right.as_f64()?),
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        (Value::Bool(left), Value::Bool(right)) => Some(left.cmp(right)),
        _ => None,
    }
}

fn fold_all(values: impl Iterator<Item = CandidateTruth>) -> CandidateTruth {
    let mut unknown = false;
    for value in values {
        match value {
            CandidateTruth::Impossible => return CandidateTruth::Impossible,
            CandidateTruth::Unknown => unknown = true,
            CandidateTruth::Possible => {}
        }
    }
    if unknown {
        CandidateTruth::Unknown
    } else {
        CandidateTruth::Possible
    }
}

fn fold_any(values: impl Iterator<Item = CandidateTruth>) -> CandidateTruth {
    let mut unknown = false;
    for value in values {
        match value {
            CandidateTruth::Possible => return CandidateTruth::Possible,
            CandidateTruth::Unknown => unknown = true,
            CandidateTruth::Impossible => {}
        }
    }
    if unknown {
        CandidateTruth::Unknown
    } else {
        CandidateTruth::Impossible
    }
}

fn bool_truth(value: bool) -> CandidateTruth {
    if value {
        CandidateTruth::Possible
    } else {
        CandidateTruth::Impossible
    }
}

fn file_name_parts(path: &str) -> (String, String, String) {
    let name = path.rsplit('/').next().unwrap_or(path).to_string();
    let (basename, extension) = name
        .rsplit_once('.')
        .map(|(base, extension)| (base.to_string(), extension.to_string()))
        .unwrap_or_else(|| (name.clone(), String::new()));
    (name, basename, extension)
}

fn benchmark_relationships(frontmatter: &Map<String, Value>) -> Vec<ProjectionRelationship> {
    let mut output = Vec::new();
    for (field, kind) in [
        ("source", "source"),
        ("request", "request"),
        ("blockedBy", "blockedBy"),
        ("related", "related"),
    ] {
        let Some(value) = frontmatter.get(field) else {
            continue;
        };
        match value {
            Value::String(target) => output.push(ProjectionRelationship {
                kind: kind.to_string(),
                target: normalize_relationship_target(target),
            }),
            Value::Array(values) => {
                for target in values.iter().filter_map(Value::as_str) {
                    output.push(ProjectionRelationship {
                        kind: kind.to_string(),
                        target: normalize_relationship_target(target),
                    });
                }
            }
            _ => {}
        }
    }
    output.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.target.cmp(&right.target))
    });
    output.dedup();
    output
}

fn normalize_relationship_target(value: &str) -> String {
    if !value.starts_with("[[") || !value.ends_with("]]") {
        return value.to_string();
    }
    let target = value[2..value.len() - 2]
        .split('|')
        .next()
        .unwrap_or_default();
    target.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{CatalogInput, ResolvedTypeResource};
    use serde_json::json;

    fn catalog() -> CompiledCatalog {
        CompiledCatalog::compile(CatalogInput {
            resource_revision: "fixture-v1".to_string(),
            configuration_document: "spec_version: 0.3.0\nsettings:\n  default_validation: warn\n"
                .to_string(),
            types: vec![ResolvedTypeResource {
                path: "_types/task.md".to_string(),
                revision: "type-v1".to_string(),
                definition: json!({
                    "kind": "mdbase.type",
                    "name": "task",
                    "version": 1,
                    "match": {"path_glob": "tasks/*.md"},
                    "schema": {"dialect": "json-schema-2020-12", "value": {
                        "type": "object", "additionalProperties": true
                    }},
                    "collection": {"read_defaults": {"status": "open"}}
                }),
                schema: json!({"type": "object", "additionalProperties": true}),
            }],
            contracts: vec![],
        })
        .unwrap()
    }

    fn projection() -> BenchmarkProjection {
        BenchmarkProjection {
            schema_version: PROJECTION_SCHEMA_VERSION.to_string(),
            path: "tasks/one.md".to_string(),
            types: vec!["task".to_string()],
            file: BenchmarkFileFacts {
                path: "tasks/one.md".to_string(),
                name: "one.md".to_string(),
                basename: "one".to_string(),
                extension: "md".to_string(),
                size: 30,
                mtime: "2026-01-01T00:00:00Z".to_string(),
            },
            persisted_frontmatter: serde_json::from_value(json!({"status": "open"})).unwrap(),
            effective_frontmatter: serde_json::from_value(
                json!({"status": "open", "projects": ["project-7"]}),
            )
            .unwrap(),
            relationships: vec![],
            diagnostics: vec![],
        }
    }

    #[test]
    fn body_predicate_is_unknown_for_projection_and_exact_for_body() {
        let expression: CandidateExpression =
            serde_json::from_value(json!({"bodyContains": "needle"})).unwrap();
        assert_eq!(
            expression.evaluate_candidate(&projection()),
            CandidateTruth::Unknown
        );
        assert!(expression.evaluate_canonical(&projection(), "A NEEDLE here"));
    }

    #[test]
    fn projection_predicates_can_exclude_without_false_negatives() {
        let expression: CandidateExpression = serde_json::from_value(json!({"all": [
            {"typeIn": ["task"]},
            {"fieldEq": ["effective_frontmatter.status", "done"]}
        ]}))
        .unwrap();
        assert_eq!(
            expression.evaluate_candidate(&projection()),
            CandidateTruth::Impossible
        );
        assert!(!expression.evaluate_canonical(&projection(), ""));
    }

    #[test]
    fn digest_is_stable() {
        assert_eq!(
            projection().canonical_digest().unwrap(),
            projection().canonical_digest().unwrap()
        );
    }

    #[test]
    fn catalog_projection_uses_canonical_defaults_and_relationship_ids() {
        let document = "---\nsource: \"[[src_0000042]]\"\n---\nBody\n";
        let projected = catalog()
            .benchmark_project_record(&CanonicalRecordInput {
                stable_id: Some("record-1".to_string()),
                path: "tasks/one.md".to_string(),
                document: document.to_string(),
                file_size: document.len() as u64,
                file_mtime: Some("2026-01-01T00:00:00Z".to_string()),
            })
            .unwrap();
        assert_eq!(projected.types, ["task"]);
        assert_eq!(projected.effective_frontmatter["status"], "open");
        assert_eq!(projected.file.basename, "one");
        assert_eq!(
            projected.relationships,
            [ProjectionRelationship {
                kind: "source".to_string(),
                target: "src_0000042".to_string()
            }]
        );
    }

    #[test]
    fn candidate_evaluation_never_excludes_a_canonical_match() {
        let expressions = [
            json!({"all": [{"typeIn": ["task"]}, {"bodyContains": "needle"}]}),
            json!({"any": [{"fieldEq": ["effective_frontmatter.status", "done"]}, {"bodyContains": "needle"}]}),
            json!({"not": {"bodyContains": "absent"}}),
            json!({"fieldContains": ["effective_frontmatter.projects", "project-7"]}),
        ];
        for value in expressions {
            let expression: CandidateExpression = serde_json::from_value(value).unwrap();
            for body in ["needle", "absent", "other"] {
                let canonical = expression.evaluate_canonical(&projection(), body);
                let candidate = expression.evaluate_candidate(&projection());
                assert!(
                    !canonical || candidate != CandidateTruth::Impossible,
                    "canonical match was excluded: {expression:?} / {body}"
                );
            }
        }
    }

    #[test]
    fn generated_candidate_corpus_has_no_false_negatives() {
        let expressions = [
            json!({"all": [{"typeIn": ["task"]}, {"fieldEq": ["effective_frontmatter.status", "open"]}]}),
            json!({"any": [{"fieldContains": ["effective_frontmatter.projects", "project-7"]}, {"bodyContains": "needle"}]}),
            json!({"fieldLt": ["effective_frontmatter.priority", 50]}),
            json!({"not": {"bodyContains": "forbidden"}}),
        ]
        .map(|value| serde_json::from_value::<CandidateExpression>(value).unwrap());
        let mut state = 0x5eed_u64;
        for case in 0..1_024 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let mut projected = projection();
            projected.types = if state & 1 == 0 {
                vec!["task".to_string()]
            } else {
                vec!["note".to_string()]
            };
            projected.effective_frontmatter.insert(
                "status".to_string(),
                json!(if state & 2 == 0 { "open" } else { "done" }),
            );
            projected
                .effective_frontmatter
                .insert("priority".to_string(), json!(((state >> 8) % 100) as i64));
            projected.effective_frontmatter.insert(
                "projects".to_string(),
                if state & 4 == 0 {
                    json!(["project-7"])
                } else {
                    json!(["project-9"])
                },
            );
            let body = match (state >> 4) & 3 {
                0 => "needle",
                1 => "forbidden",
                _ => "ordinary body",
            };
            for expression in &expressions {
                let canonical = expression.evaluate_canonical(&projected, body);
                let candidate = expression.evaluate_candidate(&projected);
                assert!(
                    !canonical || candidate != CandidateTruth::Impossible,
                    "generated case {case} produced a false negative: {expression:?} / {body}"
                );
            }
        }
    }
}
