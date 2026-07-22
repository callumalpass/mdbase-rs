use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Query {
    #[serde(default)]
    pub types: Vec<String>,
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
    pub snapshot: Option<String>,
    #[serde(default)]
    pub include_body: bool,
    #[serde(default)]
    pub frontmatter: FrontmatterMode,
    #[serde(flatten)]
    pub _extensions: BTreeMap<String, Value>,
}

pub(super) struct Candidate {
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
    Raw,
    Both,
}
