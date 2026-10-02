//! Bounded, ordered document reads. Collection semantics reuse point reads.

use super::{CollectionPath, RecordDocument, RecordFile, Revision};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Maximum input occurrences, including duplicates.
pub const READ_MANY_MAX_PATHS: usize = 100;
/// Maximum serialized operation envelope; oversized reads must be split.
pub const READ_MANY_MAX_BYTES: usize = 8 * 1024 * 1024;

/// An explicit document batch (not a mutation batch).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadManyRequest {
    /// Ordered input occurrences.
    pub paths: Vec<CollectionPath>,
    /// Include parsed bodies; defaults true.
    #[serde(default = "default_body")]
    pub include_body: bool,
    /// Include exact source Markdown; defaults false.
    #[serde(default)]
    pub include_document: bool,
}
fn default_body() -> bool {
    true
}

impl ReadManyRequest {
    /// Validate the wire request before any storage work.
    pub fn parse(input: &Value) -> Result<Self, String> {
        if input.get("path").is_some() || input.get("contract").is_some() {
            return Err(
                "Document batches require paths only; contract batches require B5 support.".into(),
            );
        }
        let request: Self =
            serde_json::from_value(input.clone()).map_err(|error| error.to_string())?;
        if request.paths.is_empty() || request.paths.len() > READ_MANY_MAX_PATHS {
            return Err(format!(
                "Document batches require 1..={READ_MANY_MAX_PATHS} paths; split larger reads."
            ));
        }
        Ok(request)
    }
}

/// Revision-bearing document with honest omission of unrequested content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BatchReadDocument {
    /// Canonical record identity.
    pub path: CollectionPath,
    /// Exact-source conditional-write token.
    pub revision: Revision,
    /// Matched record types.
    pub types: Vec<String>,
    /// Persisted fields.
    pub frontmatter: Value,
    /// Defaults, coercion and computed fields from this source version.
    pub effective_frontmatter: Value,
    /// Parsed body, when requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Exact source, when requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<String>,
    /// File facts from the same load.
    pub file: RecordFile,
}
impl BatchReadDocument {
    pub(crate) fn from_record(record: RecordDocument, include_body: bool) -> Self {
        Self {
            path: record.path,
            revision: record.revision,
            types: record.types,
            frontmatter: record.frontmatter,
            effective_frontmatter: record.effective_frontmatter,
            body: include_body.then_some(record.body),
            document: record.document,
            file: record.file,
        }
    }
}

/// Expected per-record semantic failure.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadManyError {
    /// Existing semantic diagnostic code.
    pub code: String,
    /// Safe message containing no absolute collection root.
    pub message: String,
}
/// One result per input occurrence; duplicate identities share physical work.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReadManyItem {
    /// Canonically readable document.
    Found {
        /// Requested canonical identity.
        path: CollectionPath,
        /// Coherent document and exact-source token.
        record: BatchReadDocument,
    },
    /// No record exists at the requested path.
    Missing {
        /// Requested canonical identity.
        path: CollectionPath,
    },
    /// Expected parse or validation failure.
    Error {
        /// Requested canonical identity.
        path: CollectionPath,
        /// Expected per-record failure.
        error: ReadManyError,
    },
}
/// Ordered document batch result; no cross-record atomicity is promised.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadManyResult {
    /// One outcome per input occurrence.
    pub items: Vec<ReadManyItem>,
}
