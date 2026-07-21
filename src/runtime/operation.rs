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
}

pub fn invalid_operation_result(code: &str, message: impl Into<String>) -> OperationResult {
    OperationResult {
        valid: false,
        result: json!({}),
        diagnostics: vec![Diagnostic::error(code, message, None)],
    }
}
