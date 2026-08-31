use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::CollectionPath;

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
}

impl QueryRequest {
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
    /// Returned records.
    #[serde(rename = "results")]
    pub records: Vec<Value>,
    /// Total matching records before pagination.
    #[serde(skip_serializing)]
    pub total_count: usize,
    /// Whether another page is available.
    #[serde(skip_serializing)]
    pub has_more: bool,
    /// Canonical query metadata.
    pub meta: Value,
}
