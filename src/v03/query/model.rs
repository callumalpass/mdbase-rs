use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Query {
    #[serde(default)]
    pub types: Vec<String>,
    pub timezone: Option<String>,
    pub context: Option<QueryContext>,
    #[serde(default)]
    pub projections: BTreeMap<String, Projection>,
    #[serde(rename = "where")]
    pub where_expression: Option<String>,
    pub select: Option<Vec<Selection>>,
    #[serde(default)]
    pub order_by: Vec<OrderBy>,
    #[serde(default)]
    pub group_by: Vec<OrderBy>,
    #[serde(default)]
    pub summary_functions: BTreeMap<String, Projection>,
    #[serde(default)]
    pub summaries: Vec<Summary>,
    pub limit: Option<u64>,
    #[serde(default)]
    pub offset: u64,
    #[serde(default)]
    pub include_body: bool,
    #[serde(default)]
    pub frontmatter_mode: FrontmatterMode,
    #[serde(flatten)]
    pub _extensions: BTreeMap<String, Value>,
}

const QUERY_SCHEMA_ID: &str = "https://mdbase.dev/schemas/v0.3/query.schema.json";
const TYPE_NAME_PATTERN: &str = "^[A-Za-z][A-Za-z0-9_-]{0,127}$";
const FIELD_NAME_PATTERN: &str = "^[A-Za-z_][A-Za-z0-9_:-]*$";

/// Validate every query-schema constraint that the closed typed request can
/// express, without serializing it or invoking JSON Schema.
pub(crate) fn validate_typed(request: &crate::api::QueryRequest) -> Vec<crate::v03::Diagnostic> {
    let mut failures = Vec::new();
    // serde_json objects and the schema validator visit known properties
    // lexically. Preserve that observable diagnostic order on the direct path.
    validate_order(&request.group_by, "group_by", &mut failures);
    validate_order(&request.order_by, "order_by", &mut failures);
    for (name, expression) in &request.projections {
        if expression.is_empty() {
            failures.push(min_length(
                "expr",
                &format!("/projections/{}/expr", pointer_segment(name)),
                "/properties/projections/additionalProperties/properties/expr/minLength",
            ));
        }
    }
    for name in request.projections.keys() {
        if !valid_field_name(name) {
            failures.push(schema_failure(
                "schema_pattern",
                format!(
                    "{} does not match {FIELD_NAME_PATTERN:?}",
                    serde_json::json!(name)
                ),
                "projections",
                "/projections",
                "/properties/projections/propertyNames/pattern",
            ));
        }
    }
    if let Some(select) = &request.select {
        if select.is_empty() {
            failures.push(schema_failure(
                "schema_min_items",
                "[] has less than 1 item".to_string(),
                "select",
                "/select",
                "/properties/select/minItems",
            ));
        }
        for (index, field) in select.iter().enumerate() {
            if field.is_empty() {
                failures.push(schema_failure(
                    "schema_min_length",
                    "\"\" is shorter than 1 character".to_string(),
                    &index.to_string(),
                    &format!("/select/{index}"),
                    "/properties/select/items/oneOf",
                ));
            }
        }
    }
    if request.timezone.as_deref() == Some("") {
        failures.push(min_length(
            "timezone",
            "/timezone",
            "/properties/timezone/minLength",
        ));
    }
    if !request.types.is_empty() {
        for (index, type_name) in request.types.iter().enumerate() {
            if !valid_type_name(type_name) {
                failures.push(schema_failure(
                    "schema_pattern",
                    format!(
                        "{} does not match {TYPE_NAME_PATTERN:?}",
                        serde_json::json!(type_name)
                    ),
                    &index.to_string(),
                    &format!("/types/{index}"),
                    "/properties/types/items/pattern",
                ));
            }
        }
        let unique = request
            .types
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if unique.len() != request.types.len() {
            failures.push(schema_failure(
                "schema_unique_items",
                format!(
                    "{} has non-unique elements",
                    serde_json::json!(request.types)
                ),
                "types",
                "/types",
                "/properties/types/uniqueItems",
            ));
        }
    }
    if request.where_expression.as_deref() == Some("") {
        failures.push(min_length("where", "/where", "/properties/where/minLength"));
    }
    failures
}

fn validate_order(
    order: &[crate::api::QueryOrder],
    property: &str,
    failures: &mut Vec<crate::v03::Diagnostic>,
) {
    for (index, item) in order.iter().enumerate() {
        if item.field.is_empty() {
            failures.push(min_length(
                "field",
                &format!("/{property}/{index}/field"),
                &format!("/properties/{property}/items/properties/field/minLength"),
            ));
        }
    }
}

fn min_length(field: &str, instance_path: &str, schema_path: &str) -> crate::v03::Diagnostic {
    schema_failure(
        "schema_min_length",
        "\"\" is shorter than 1 character".to_string(),
        field,
        instance_path,
        schema_path,
    )
}

fn schema_failure(
    code: &str,
    message: String,
    field: &str,
    instance_path: &str,
    schema_path: &str,
) -> crate::v03::Diagnostic {
    super::diagnostics::invalid_schema(crate::v03::Diagnostic {
        severity: "error".to_string(),
        code: code.to_string(),
        message,
        path: Some("query".to_string()),
        field: Some(field.to_string()),
        type_name: None,
        schema_location: Some(format!("{QUERY_SCHEMA_ID}#{schema_path}")),
        details: Some(serde_json::json!({
            "instance_path": instance_path,
            "schema_path": schema_path,
        })),
    })
}

fn valid_type_name(value: &str) -> bool {
    value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        && value
            .as_bytes()
            .iter()
            .skip(1)
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_field_name(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && value
            .as_bytes()
            .iter()
            .skip(1)
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'-'))
}

fn pointer_segment(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

impl Query {
    /// Construct the internal query model directly from the intentionally
    /// narrower typed API. Wire-only selections, summaries, and extensions
    /// remain available only through the wire adapter.
    pub(crate) fn from_typed(request: &crate::api::QueryRequest) -> Self {
        Self {
            types: request.types.clone(),
            timezone: request.timezone.clone(),
            context: request.context.as_ref().map(|path| QueryContext {
                this: ContextRecord {
                    path: path.as_str().to_string(),
                },
            }),
            projections: request
                .projections
                .iter()
                .map(|(name, expr)| {
                    (
                        name.clone(),
                        Projection {
                            expr: expr.clone(),
                            _description: None,
                            _extensions: BTreeMap::new(),
                        },
                    )
                })
                .collect(),
            where_expression: request.where_expression.clone(),
            select: request
                .select
                .as_ref()
                .map(|fields| fields.iter().cloned().map(Selection::Field).collect()),
            order_by: request.order_by.iter().map(OrderBy::from_typed).collect(),
            group_by: request.group_by.iter().map(OrderBy::from_typed).collect(),
            summary_functions: BTreeMap::new(),
            summaries: Vec::new(),
            limit: request.limit,
            offset: request.offset,
            include_body: request.include_body,
            frontmatter_mode: match request.frontmatter_mode {
                crate::api::FrontmatterMode::Effective => FrontmatterMode::Effective,
                crate::api::FrontmatterMode::Persisted => FrontmatterMode::Persisted,
                crate::api::FrontmatterMode::Both => FrontmatterMode::Both,
            },
            _extensions: BTreeMap::new(),
        }
    }
}

pub(crate) struct Candidate {
    pub path: String,
    pub types: Vec<String>,
    pub raw: Value,
    pub effective: Value,
    pub body: String,
    pub file: Value,
    pub projections: Map<String, Value>,
    pub values: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct QueryContext {
    pub this: ContextRecord,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ContextRecord {
    pub path: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Projection {
    pub expr: String,
    #[serde(default, rename = "description")]
    pub _description: Option<String>,
    #[serde(flatten)]
    pub _extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum Selection {
    Field(String),
    Expression(SelectionExpression),
}

impl Selection {
    pub fn output_name(&self) -> &str {
        match self {
            Self::Field(field) => field.rsplit('.').next().unwrap_or(field),
            Self::Expression(expression) => &expression.name,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct SelectionExpression {
    pub name: String,
    pub expr: String,
    #[serde(default, rename = "label")]
    pub _label: Option<String>,
    #[serde(default, rename = "description")]
    pub _description: Option<String>,
    #[serde(flatten)]
    pub _extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct OrderBy {
    pub field: String,
    #[serde(default)]
    pub direction: Direction,
}

impl OrderBy {
    fn from_typed(order: &crate::api::QueryOrder) -> Self {
        Self {
            field: order.field.clone(),
            direction: match order.direction {
                crate::api::QueryDirection::Asc => Direction::Asc,
                crate::api::QueryDirection::Desc => Direction::Desc,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Direction {
    #[default]
    Asc,
    Desc,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Summary {
    pub field: String,
    pub function: String,
    pub name: Option<String>,
    #[serde(default, rename = "label")]
    pub _label: Option<String>,
}

impl Summary {
    pub fn output_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.function)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FrontmatterMode {
    #[default]
    Effective,
    Persisted,
    Both,
}
