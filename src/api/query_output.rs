//! Authority-side output shaping shared by canonical and hosted producers.
use super::{CollectionPath, Revision};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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
