use std::collections::BTreeSet;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::ProviderError;
use crate::v03::{Diagnostic, OperationResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Read,
    Query,
    ListViews,
    ExecuteView,
    Validate,
    Create,
    Update,
    Delete,
    Rename,
}

impl OperationKind {
    pub fn is_mutation(self) -> bool {
        matches!(
            self,
            Self::Create | Self::Update | Self::Delete | Self::Rename
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Query => "query",
            Self::ListViews => "list_views",
            Self::ExecuteView => "execute_view",
            Self::Validate => "validate",
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Rename => "rename",
        }
    }
}

impl FromStr for OperationKind {
    type Err = ProviderError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "read" => Ok(Self::Read),
            "query" => Ok(Self::Query),
            "list_views" => Ok(Self::ListViews),
            "execute_view" => Ok(Self::ExecuteView),
            "validate" => Ok(Self::Validate),
            "create" => Ok(Self::Create),
            "update" => Ok(Self::Update),
            "delete" => Ok(Self::Delete),
            "rename" => Ok(Self::Rename),
            other => Err(ProviderError::UnsupportedOperation(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationRequest {
    pub operation: OperationKind,
    #[serde(default)]
    pub input: Value,
}

impl OperationRequest {
    pub fn new(operation: OperationKind, input: Value) -> Self {
        Self { operation, input }
    }

    /// Return the records that may have changed after a successful mutation.
    ///
    /// Keeping this derivation next to the canonical operation envelope lets
    /// hosts synchronize watchers without rescanning the entire collection or
    /// duplicating mutation result semantics.
    pub fn affected_paths(&self, result: &OperationResult) -> BTreeSet<String> {
        if !self.operation.is_mutation() || !result.valid {
            return BTreeSet::new();
        }

        let mut paths = BTreeSet::new();
        let mut insert = |value: Option<&Value>| {
            if let Some(path) = value.and_then(Value::as_str) {
                paths.insert(path.to_string());
            }
        };
        insert(self.input.get("path"));
        insert(result.result.get("path"));
        insert(self.input.get("from"));
        insert(self.input.get("to"));
        insert(result.result.get("from"));
        insert(result.result.get("to"));

        for pointer in ["/references_updated", "/partial_updates/failed"] {
            if let Some(items) = result.result.pointer(pointer).and_then(Value::as_array) {
                for item in items {
                    insert(item.get("path"));
                }
            }
        }
        if let Some(items) = self
            .input
            .get("simulate_before_ref_update")
            .and_then(Value::as_array)
        {
            for item in items {
                insert(item.get("path"));
            }
        }
        paths
    }
}

pub fn invalid_operation_result(code: &str, message: impl Into<String>) -> OperationResult {
    OperationResult {
        valid: false,
        result: json!({}),
        diagnostics: vec![Diagnostic::error(code, message, None)],
    }
}
