use serde_json::{Map, Value};

use crate::api::{
    CollectionPath, CreateRequest, DeleteRequest, RenameRequest, RenameResult, UpdateRequest,
};
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

/// A typed delete whose target and collection-dependent evidence were captured
/// before any file is removed.
pub(crate) struct PreparedDelete {
    pub request: DeleteRequest,
    pub dry_run: bool,
    pub before_revision: String,
    pub before_frontmatter: Map<String, Value>,
    pub before_body: String,
    pub types: Vec<String>,
    pub broken_links: Vec<Value>,
    pub legacy_last_known_mtime: Option<u64>,
}

/// Typed rename plus all collection-wide evidence captured before publication.
pub(crate) struct PreparedRename {
    pub request: RenameRequest,
    pub dry_run: bool,
    pub source_revision: String,
    pub source_types: Vec<String>,
    pub source_id: Option<String>,
    pub source_frontmatter: Value,
    pub source_effective_frontmatter: Value,
    pub source_body: String,
    pub source_bytes: Vec<u8>,
    pub reference_plans: Vec<crate::operations::rename::ReferenceRewritePlan>,
    pub warnings: Vec<Value>,
    pub reference_failures: Vec<Value>,
    pub legacy_ref_mtimes: std::collections::HashMap<String, u64>,
    pub legacy_simulations: Vec<(CollectionPath, String)>,
}

/// Exact rename and reference-write evidence retained across publication.
#[derive(Clone, Debug)]
pub(crate) struct PlannedRename {
    pub from: CollectionPath,
    pub to: CollectionPath,
    pub source_revision: String,
    pub source_types: Vec<String>,
    pub source_id: Option<String>,
    pub destination: PlannedRecord,
    pub reference_plans: Vec<crate::operations::rename::ReferenceRewritePlan>,
    pub references_affected: Vec<Value>,
    pub references_updated: Vec<Value>,
    pub warnings: Vec<Value>,
    pub reference_failures: Vec<Value>,
    pub dry_run: bool,
}

impl PlannedRename {
    pub(crate) fn result(&self, document: crate::api::RecordDocument) -> RenameResult {
        RenameResult {
            document,
            from: self.from.clone(),
            to: self.to.clone(),
            references_updated: crate::api::reference_evidence(self.references_updated.clone()),
        }
    }

    pub(crate) fn preflight_result(&self) -> crate::api::RenamePreflightResult {
        crate::api::RenamePreflightResult {
            from: self.from.clone(),
            to: self.to.clone(),
            would_rename: true,
            references_affected: crate::api::reference_evidence(self.references_affected.clone()),
            warnings: self.warnings.clone().into_iter().map(Into::into).collect(),
        }
    }
}

/// Exact planned deletion evidence retained across durable publication.
#[derive(Clone, Debug)]
pub(crate) struct PlannedDelete {
    pub path: CollectionPath,
    pub before_revision: String,
    pub before_frontmatter: Map<String, Value>,
    pub before_body: String,
    pub types: Vec<String>,
    pub broken_links: Vec<Value>,
    pub deleted: bool,
}

impl PlannedDelete {
    pub(crate) fn result(&self) -> crate::api::DeleteResult {
        crate::api::DeleteResult {
            path: self.path.clone(),
            deleted: self.deleted,
            broken_links: crate::api::reference_evidence(self.broken_links.clone()),
        }
    }

    pub(crate) fn preflight_result(&self) -> crate::api::DeletePreflightResult {
        crate::api::DeletePreflightResult {
            path: self.path.clone(),
            would_delete: true,
            broken_links: crate::api::reference_evidence(self.broken_links.clone()),
        }
    }
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
