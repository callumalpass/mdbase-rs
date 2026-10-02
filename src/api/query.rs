use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::{
    CollectionPath, Diagnostic, MdbaseError, MdbaseResult, OperationOutcome, ProjectedValue,
    QueryMetadata, Revision,
};

/// Sort direction for query ordering and grouping.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueryDirection {
    /// Ascending order.
    #[default]
    Asc,
    /// Descending order.
    Desc,
}

/// One ordered query field.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryOrder {
    /// Field or projection name.
    pub field: String,
    /// Ordering direction.
    pub direction: QueryDirection,
}

/// Frontmatter representation included in query records.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrontmatterMode {
    /// Include effective frontmatter after defaults and computed fields.
    #[default]
    Effective,
    /// Include only persisted frontmatter.
    Persisted,
    /// Include both persisted and effective frontmatter.
    Both,
}

/// Typed builder for the common canonical query surface.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QueryRequest {
    /// Type names used to restrict candidate records.
    #[serde(default)]
    pub types: Vec<String>,
    /// IANA timezone used for calendar semantics in this invocation.
    #[serde(default)]
    pub timezone: Option<String>,
    /// Record used to bind the query `this` context.
    #[serde(default)]
    pub context: Option<CollectionPath>,
    /// Named CEL expressions evaluated before filtering and selection.
    #[serde(default)]
    pub projections: BTreeMap<String, String>,
    /// CEL predicate used to filter candidates.
    #[serde(default, rename = "where")]
    pub where_expression: Option<String>,
    /// Fields retained in each returned record.
    #[serde(default)]
    pub select: Option<Vec<String>>,
    /// Deterministic record ordering.
    #[serde(default)]
    pub order_by: Vec<QueryOrder>,
    /// Ordered grouping fields.
    #[serde(default)]
    pub group_by: Vec<QueryOrder>,
    /// Maximum returned records.
    #[serde(default)]
    pub limit: Option<u64>,
    /// Number of ordered records skipped before returning results.
    #[serde(default)]
    pub offset: u64,
    /// Whether returned records include their Markdown body.
    #[serde(default)]
    pub include_body: bool,
    /// Frontmatter representation to return.
    #[serde(default)]
    pub frontmatter_mode: FrontmatterMode,
    /// Opt-in narrow response envelope, not an evaluation mode.
    #[serde(default)]
    pub output: Option<super::QueryOutput>,
}

impl QueryRequest {
    pub(crate) fn decode_wire(mut value: Value) -> Result<Self, serde_json::Error> {
        if let Some(object) = value.as_object_mut() {
            if let Some(path) = object
                .get("context")
                .and_then(|value| value.get("this"))
                .and_then(|value| value.get("path"))
                .cloned()
            {
                object.insert("context".to_string(), path);
            }
            if let Some(projections) = object.get_mut("projections").and_then(Value::as_object_mut)
            {
                for projection in projections.values_mut() {
                    if let Some(expression) = projection.get("expr").cloned() {
                        *projection = expression;
                    }
                }
            }
        }
        serde_json::from_value(value)
    }

    /// Start a query with canonical defaults.
    pub fn builder() -> Self {
        Self::default()
    }

    /// Add a type filter.
    pub fn type_name(mut self, type_name: impl Into<String>) -> Self {
        self.types.push(type_name.into());
        self
    }

    /// Override the collection timezone for this query invocation.
    pub fn timezone(mut self, timezone: impl Into<String>) -> Self {
        self.timezone = Some(timezone.into());
        self
    }

    /// Set the CEL filter expression.
    pub fn where_expression(mut self, expression: impl Into<String>) -> Self {
        self.where_expression = Some(expression.into());
        self
    }

    /// Append an ordering field.
    pub fn order_by(mut self, field: impl Into<String>, direction: QueryDirection) -> Self {
        self.order_by.push(QueryOrder {
            field: field.into(),
            direction,
        });
        self
    }

    /// Set the maximum returned record count.
    pub fn limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Set the ordered record offset.
    pub fn offset(mut self, offset: u64) -> Self {
        self.offset = offset;
        self
    }

    /// Encode the canonical portable query object used by providers and local
    /// transports, omitting unset defaults that are invalid on the wire.
    pub fn to_wire(&self) -> Value {
        #[cfg(test)]
        crate::query::canonical::record_typed_request_json_encode();
        let mut value = Map::new();
        if !self.types.is_empty() {
            value.insert("types".to_string(), json!(self.types));
        }
        if let Some(timezone) = &self.timezone {
            value.insert("timezone".to_string(), json!(timezone));
        }
        if let Some(context) = &self.context {
            value.insert("context".to_string(), json!({"this": {"path": context}}));
        }
        if !self.projections.is_empty() {
            value.insert(
                "projections".to_string(),
                Value::Object(
                    self.projections
                        .iter()
                        .map(|(name, expression)| (name.clone(), json!({"expr": expression})))
                        .collect(),
                ),
            );
        }
        if let Some(expression) = &self.where_expression {
            value.insert("where".to_string(), Value::String(expression.clone()));
        }
        if let Some(select) = &self.select {
            value.insert("select".to_string(), json!(select));
        }
        insert_order(&mut value, "order_by", &self.order_by);
        insert_order(&mut value, "group_by", &self.group_by);
        if let Some(limit) = self.limit {
            value.insert("limit".to_string(), json!(limit));
        }
        if self.offset != 0 {
            value.insert("offset".to_string(), json!(self.offset));
        }
        if self.include_body {
            value.insert("include_body".to_string(), Value::Bool(true));
        }
        if let Some(mode) = match self.frontmatter_mode {
            FrontmatterMode::Effective => None,
            FrontmatterMode::Persisted => Some("persisted"),
            FrontmatterMode::Both => Some("both"),
        } {
            value.insert(
                "frontmatter_mode".to_string(),
                Value::String(mode.to_string()),
            );
        }
        if let Some(output) = self.output {
            value.insert("output".into(), json!(output));
        }
        Value::Object(value)
    }
}

fn insert_order(target: &mut Map<String, Value>, name: &str, order: &[QueryOrder]) {
    if !order.is_empty() {
        target.insert(
            name.to_string(),
            Value::Array(
                order
                    .iter()
                    .map(|item| {
                        json!({
                            "field": item.field,
                            "direction": match item.direction {
                                QueryDirection::Asc => "asc",
                                QueryDirection::Desc => "desc",
                            }
                        })
                    })
                    .collect(),
            ),
        );
    }
}

/// Paginated canonical query result.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct QueryResult {
    /// Requested output envelope; absent for ordinary query rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<super::QueryOutput>,
    /// Returned records.
    #[serde(rename = "results")]
    pub records: Vec<ProjectedValue>,
    /// Total matching records before pagination.
    #[serde(skip_serializing)]
    pub total_count: usize,
    /// Whether another page is available.
    #[serde(skip_serializing)]
    pub has_more: bool,
    /// Canonical query metadata.
    pub meta: QueryMetadata,
}

pub(super) fn typed_query_result(
    evaluation: crate::query::canonical::QueryEvaluation,
) -> MdbaseResult<OperationOutcome<QueryResult>> {
    evaluation
        .map(|execution| OperationOutcome {
            value: QueryResult {
                output: execution.output,
                records: execution.records.into_iter().map(Into::into).collect(),
                total_count: execution.total_count,
                has_more: execution.has_more,
                meta: QueryMetadata::new(execution.meta),
            },
            diagnostics: execution
                .diagnostics
                .into_iter()
                .map(Diagnostic::from)
                .collect(),
        })
        .map_err(|diagnostics| MdbaseError::Operation {
            diagnostics: diagnostics.into_iter().map(Diagnostic::from).collect(),
        })
}

/// Opt-in query output. Absence preserves the ordinary record envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryOutput {
    /// Emit only identity, revision, types and explicitly selected values.
    Metadata,
}

/// A narrow row is not a document or a complete semantic frontmatter record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetadataQueryRecord {
    /// Canonical identity.
    pub path: CollectionPath,
    /// Exact-source token from the same version as the selected values.
    pub revision: Revision,
    /// Matched record types.
    pub types: Vec<String>,
    /// Existing selected-value keys; empty when no select was requested.
    pub values: Map<String, Value>,
}

/// Already evaluated row material. Rendering never evaluates collection semantics.
pub struct QueryRecordMaterial<'a> {
    /// Canonical identity.
    pub path: &'a str,
    /// Exact-source token; never a hash of reconstructed values.
    pub revision: &'a str,
    /// Matched types.
    pub types: &'a [String],
    /// Requested persisted frontmatter, absent otherwise.
    pub frontmatter: Option<&'a Value>,
    /// Requested effective frontmatter, absent otherwise.
    pub effective_frontmatter: Option<&'a Value>,
    /// File facts, including already evaluated structural facts.
    pub file: &'a Value,
    /// Requested body, absent otherwise.
    pub body: Option<&'a str>,
    /// Existing select output, absent when no selection was requested.
    pub values: Option<&'a Map<String, Value>>,
}
impl QueryRecordMaterial<'_> {
    /// The only portable renderer for ordinary and narrow record row shapes.
    pub fn render(self, output: Option<QueryOutput>) -> Value {
        if output == Some(QueryOutput::Metadata) {
            return serde_json::to_value(MetadataQueryRecord {
                path: CollectionPath::new(self.path)
                    .expect("query candidates have canonical identities"),
                revision: Revision::parse(self.revision)
                    .expect("query candidates retain exact-source revisions"),
                types: self.types.to_vec(),
                values: self.values.cloned().unwrap_or_default(),
            })
            .expect("metadata query rows serialize");
        }
        let mut row = Map::from_iter([
            ("path".into(), Value::String(self.path.into())),
            ("revision".into(), Value::String(self.revision.into())),
            (
                "types".into(),
                serde_json::to_value(self.types).expect("types serialize"),
            ),
            ("file".into(), self.file.clone()),
        ]);
        for (key, value) in [
            ("frontmatter", self.frontmatter),
            ("effective_frontmatter", self.effective_frontmatter),
        ] {
            if let Some(value) = value {
                row.insert(key.into(), value.clone());
            }
        }
        if let Some(body) = self.body {
            row.insert("body".into(), Value::String(body.into()));
        }
        if let Some(values) = self.values {
            row.insert("values".into(), Value::Object(values.clone()));
        }
        Value::Object(row)
    }
}

impl crate::runtime::HostedBaseRow {
    /// Portable query row; reducer-only facts never cross the response boundary.
    pub fn to_query_record(&self) -> Value {
        let effective = Value::Object(self.effective_frontmatter.clone());
        QueryRecordMaterial {
            path: &self.path,
            revision: &self.revision,
            types: &self.types,
            frontmatter: None,
            effective_frontmatter: Some(&effective),
            file: &self.file,
            body: None,
            values: Some(&self.values),
        }
        .render(None)
    }
}
