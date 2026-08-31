use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Explicit opaque value produced by a caller-selected query projection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectedValue(Value);

impl ProjectedValue {
    /// Wrap one projection-domain JSON value.
    pub fn new(value: Value) -> Self {
        Self(value)
    }
    /// Consume the wrapper without interpreting the projection.
    pub fn into_inner(self) -> Value {
        self.0
    }
}

impl From<Value> for ProjectedValue {
    fn from(value: Value) -> Self {
        Self(value)
    }
}

impl Deref for ProjectedValue {
    type Target = Value;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ProjectedValue {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Explicit opaque canonical query metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QueryMetadata(Value);

impl QueryMetadata {
    /// Wrap canonical query metadata without interpreting extension fields.
    pub fn new(value: Value) -> Self {
        Self(value)
    }
    /// Consume the metadata wrapper.
    pub fn into_inner(self) -> Value {
        self.0
    }
}

impl PartialEq<Value> for QueryMetadata {
    fn eq(&self, other: &Value) -> bool {
        self.0 == *other
    }
}

impl PartialEq<QueryMetadata> for Value {
    fn eq(&self, other: &QueryMetadata) -> bool {
        *self == other.0
    }
}

impl From<Value> for QueryMetadata {
    fn from(value: Value) -> Self {
        Self(value)
    }
}

impl Deref for QueryMetadata {
    type Target = Value;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for QueryMetadata {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Structured evidence describing one canonical reference observation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReferenceEvidence {
    /// Record containing the reference, when supplied by the operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Reference-domain properties whose schema is selected by reference kind.
    #[serde(flatten)]
    pub properties: BTreeMap<String, ProjectedValue>,
}

pub(crate) fn reference_evidence(values: Vec<Value>) -> Vec<ReferenceEvidence> {
    values
        .into_iter()
        .map(|value| {
            serde_json::from_value(value).expect("canonical reference evidence is an object")
        })
        .collect()
}
