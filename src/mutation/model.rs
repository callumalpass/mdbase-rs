use serde_json::Value;

use crate::api::{CollectionPath, CreateRequest, UpdateRequest};
use crate::diagnostic::Diagnostic;
use crate::errors::{Issue, Severity};

use super::ResolvedWriteMembership;

#[derive(Clone, Debug, Default)]
pub(crate) struct PreparationOptions {
    /// Exact create source accepted only by the v0.3 wire adapter.
    pub create_document: Option<String>,
    pub dry_run: bool,
}

pub(crate) struct PreparedCreate {
    pub request: CreateRequest,
    pub membership: Option<ResolvedWriteMembership>,
    pub exact_document: Option<String>,
    pub legacy_path: Option<String>,
    pub legacy_revision: Option<String>,
}

pub(crate) struct PreparedUpdate {
    pub request: UpdateRequest,
    pub membership: Option<ResolvedWriteMembership>,
    pub legacy_path: Option<String>,
    pub legacy_revision: Option<String>,
    pub legacy_last_known_mtime: Option<u64>,
}

/// Exact planned record and semantic projection produced before publication.
#[derive(Clone, Debug)]
pub(crate) struct PlannedRecord {
    pub path: CollectionPath,
    pub types: Vec<String>,
    pub frontmatter: Value,
    pub effective_frontmatter: Value,
    pub body: String,
    pub bytes: Vec<u8>,
    pub diagnostics: Vec<Diagnostic>,
    pub before_revision: Option<String>,
    pub include_document: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MutationFailureKind {
    Operation,
    Validation,
}

#[derive(Clone, Debug)]
pub(crate) struct MutationFailure {
    pub diagnostics: Vec<Diagnostic>,
    pub kind: MutationFailureKind,
}

impl MutationFailure {
    pub(crate) fn operation(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            diagnostics: vec![Diagnostic::error(code, message, None)],
            kind: MutationFailureKind::Operation,
        }
    }

    pub(crate) fn diagnostic(diagnostic: Diagnostic) -> Self {
        Self {
            diagnostics: vec![diagnostic],
            kind: MutationFailureKind::Operation,
        }
    }

    pub(crate) fn diagnostics(diagnostics: Vec<Diagnostic>) -> Self {
        let kind = if diagnostics.len() == 1 {
            MutationFailureKind::Operation
        } else {
            MutationFailureKind::Validation
        };
        Self { diagnostics, kind }
    }

    pub(crate) fn validation(issues: &[Issue]) -> Self {
        Self {
            diagnostics: issues.iter().map(diagnostic_from_issue).collect(),
            kind: MutationFailureKind::Validation,
        }
    }
}

pub(crate) fn diagnostic_from_issue(issue: &Issue) -> Diagnostic {
    Diagnostic {
        severity: match issue.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
        .to_string(),
        code: issue.code.clone(),
        message: issue.message.clone(),
        path: issue.path.clone(),
        field: issue.field.clone(),
        type_name: issue.type_name.clone(),
        schema_location: None,
        details: None,
    }
}

impl PlannedRecord {
    pub(crate) fn after_revision(&self) -> String {
        use sha2::{Digest, Sha256};
        format!("sha256:{:x}", Sha256::digest(&self.bytes))
    }
}
