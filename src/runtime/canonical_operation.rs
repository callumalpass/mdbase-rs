use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{
    BatchOperationResult, BatchRenamePartialUpdates, BatchRenameResult, BatchResult,
    DeletePreflightResult, DeleteResult, Diagnostic, ProjectedValue, QueryMetadata, RecordDocument,
    ReferenceEvidence, Severity,
};
use crate::diagnostic::Diagnostic as WireDiagnostic;
use crate::v03::OperationResult;

use super::{OperationKind, ProviderError};

/// Typed canonical query value retained by the runtime and read cursors.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonicalQueryValue {
    pub records: Vec<ProjectedValue>,
    /// Exact total when computed; `None` when the provider explicitly deferred it.
    pub total_count: Option<usize>,
    pub has_more: bool,
    pub meta: QueryMetadata,
    pub embedded_diagnostics: Vec<Diagnostic>,
}

/// Typed result of a canonical delete operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum CanonicalDeleteValue {
    Deleted(DeleteResult),
    Preflight(DeletePreflightResult),
}

/// Exact non-mutating standalone rename value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonicalRenamePreflightValue {
    pub from: crate::api::CollectionPath,
    pub to: crate::api::CollectionPath,
    pub dry_run: bool,
    pub would_rename: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references_affected: Vec<ReferenceEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_updates: Option<BatchRenamePartialUpdates>,
}

/// Typed result of a canonical rename operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum CanonicalRenameValue {
    Renamed(Box<BatchRenameResult>),
    Preflight(CanonicalRenamePreflightValue),
}

/// Explicit compatibility-only result families which do not yet have a
/// canonical typed API. These variants must not be used for record reads,
/// queries, or record mutations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "family", content = "value", rename_all = "snake_case")]
pub enum WireOnlyOperationValue {
    Validation(Value),
    ViewResource(Value),
    TypeResource(Value),
}

/// Explicitly named storage for forward-compatible definition-result fields.
/// Core fields consumed by hosts remain closed and typed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DefinitionResultExtensions(pub std::collections::BTreeMap<String, Value>);

/// Typed assessment/apply result for one managed type pack.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonicalTypePackValue {
    pub applicable: bool,
    pub assessment_digest: String,
    #[serde(flatten)]
    pub extensions: DefinitionResultExtensions,
}

/// Successful applied collection-setup result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonicalCollectionSetupAppliedValue {
    pub assessment: crate::v03::CollectionSetupAssessment,
    pub receipt: crate::v03::CollectionSetupReceipt,
}

/// Rejected collection-setup result that retains its complete assessment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonicalCollectionSetupConflictValue {
    pub assessment: crate::v03::CollectionSetupAssessment,
    pub conflicts: Vec<crate::v03::ConfigurationConflict>,
}

/// Closed typed collection-setup result family.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CanonicalCollectionSetupValue {
    Applied(CanonicalCollectionSetupAppliedValue),
    Conflict(CanonicalCollectionSetupConflictValue),
    Assessment(crate::v03::CollectionSetupAssessment),
}

/// Closed semantic value returned by the filesystem runtime.
///
/// `None` means the operation failed before producing a semantic value. It is
/// deliberately variant-specific rather than a generic JSON result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", content = "value", rename_all = "snake_case")]
pub enum CanonicalOperationValue {
    Read(Option<RecordDocument>),
    Query(Option<CanonicalQueryValue>),
    Create(Option<RecordDocument>),
    Update(Option<RecordDocument>),
    Delete(Option<CanonicalDeleteValue>),
    Rename(Option<CanonicalRenameValue>),
    Batch(Option<BatchResult>),
    TypePack(Option<CanonicalTypePackValue>),
    CollectionSetup(Option<Box<CanonicalCollectionSetupValue>>),
    WireOnly(WireOnlyOperationValue),
    /// Exact non-semantic recovery of an ambiguous version-2 runtime journal.
    LegacyRecoveredV03(OperationResult),
}

/// Closed typed operation outcome shared by execution, durable transactions,
/// recovery, claim resolution, and generation-pinned cursors.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonicalOperationOutcome {
    pub valid: bool,
    pub value: CanonicalOperationValue,
    pub diagnostics: Vec<Diagnostic>,
}

impl CanonicalOperationValue {
    pub(crate) fn kind(&self) -> Option<OperationKind> {
        match self {
            Self::Read(_) => Some(OperationKind::Read),
            Self::Query(_) => Some(OperationKind::Query),
            Self::Create(_) => Some(OperationKind::Create),
            Self::Update(_) => Some(OperationKind::Update),
            Self::Delete(_) => Some(OperationKind::Delete),
            Self::Rename(_) => Some(OperationKind::Rename),
            Self::Batch(_) => Some(OperationKind::Batch),
            Self::TypePack(_) => Some(OperationKind::AssessTypePack),
            Self::CollectionSetup(_) => Some(OperationKind::AssessCollectionSetup),
            Self::WireOnly(WireOnlyOperationValue::Validation(_)) => Some(OperationKind::Validate),
            Self::WireOnly(WireOnlyOperationValue::ViewResource(_)) => {
                Some(OperationKind::ExecuteView)
            }
            Self::WireOnly(WireOnlyOperationValue::TypeResource(_)) => {
                Some(OperationKind::ReadType)
            }
            Self::LegacyRecoveredV03(_) => None,
        }
    }
}

impl CanonicalOperationOutcome {
    pub(crate) fn read(outcome: crate::api::OperationOutcome<RecordDocument>) -> Self {
        Self {
            valid: true,
            value: CanonicalOperationValue::Read(Some(outcome.value)),
            diagnostics: outcome.diagnostics,
        }
    }

    pub(crate) fn query(outcome: crate::api::OperationOutcome<crate::api::QueryResult>) -> Self {
        let embedded_diagnostics = outcome.diagnostics.clone();
        Self {
            valid: true,
            value: CanonicalOperationValue::Query(Some(CanonicalQueryValue {
                records: outcome.value.records,
                total_count: Some(outcome.value.total_count),
                has_more: outcome.value.has_more,
                meta: outcome.value.meta,
                embedded_diagnostics,
            })),
            diagnostics: outcome.diagnostics,
        }
    }

    pub(crate) fn legacy_recovered(result: OperationResult) -> Self {
        Self {
            valid: result.valid,
            diagnostics: result.diagnostics.iter().cloned().map(Into::into).collect(),
            value: CanonicalOperationValue::LegacyRecoveredV03(result),
        }
    }

    pub(crate) fn record_mutation(
        operation: OperationKind,
        valid: bool,
        result: BatchOperationResult,
        diagnostics: Vec<Diagnostic>,
    ) -> Self {
        let value = match (operation, result) {
            (OperationKind::Create, BatchOperationResult::Record(record)) => {
                CanonicalOperationValue::Create(Some(record))
            }
            (OperationKind::Update, BatchOperationResult::Record(record)) => {
                CanonicalOperationValue::Update(Some(record))
            }
            (OperationKind::Delete, BatchOperationResult::Delete(result)) => {
                CanonicalOperationValue::Delete(Some(CanonicalDeleteValue::Deleted(result)))
            }
            (OperationKind::Delete, BatchOperationResult::DeletePreflight(result)) => {
                CanonicalOperationValue::Delete(Some(CanonicalDeleteValue::Preflight(
                    DeletePreflightResult {
                        path: result.path,
                        would_delete: result.would_delete,
                        broken_links: result.broken_links,
                    },
                )))
            }
            (OperationKind::Rename, BatchOperationResult::Rename(result)) => {
                CanonicalOperationValue::Rename(Some(CanonicalRenameValue::Renamed(Box::new(
                    result,
                ))))
            }
            (OperationKind::Rename, BatchOperationResult::RenamePreflight(result)) => {
                CanonicalOperationValue::Rename(Some(CanonicalRenameValue::Preflight(
                    CanonicalRenamePreflightValue {
                        from: result.from,
                        to: result.to,
                        dry_run: result.dry_run,
                        would_rename: result.would_rename,
                        references_affected: result.references_affected,
                        partial_updates: result.partial_updates,
                    },
                )))
            }
            _ => return Self::invalid(operation, diagnostics),
        };
        Self {
            valid,
            value,
            diagnostics,
        }
    }

    pub(crate) fn wire_only(operation: OperationKind, result: OperationResult) -> Self {
        if matches!(
            operation,
            OperationKind::AssessTypePack
                | OperationKind::ApplyTypePack
                | OperationKind::AssessCollectionSetup
                | OperationKind::ApplyCollectionSetup
        ) {
            return Self::definition(operation, result)
                .expect("definition APIs produce their closed typed result shape");
        }
        let OperationResult {
            valid,
            result,
            diagnostics,
        } = result;
        let value = match operation {
            OperationKind::Validate => WireOnlyOperationValue::Validation(result),
            OperationKind::ListViews
            | OperationKind::ExecuteView
            | OperationKind::ReadViewSource
            | OperationKind::CreateViewSource
            | OperationKind::UpdateViewSource
            | OperationKind::DeleteViewSource => WireOnlyOperationValue::ViewResource(result),
            OperationKind::ListTypes
            | OperationKind::ReadType
            | OperationKind::CreateType
            | OperationKind::UpdateType => WireOnlyOperationValue::TypeResource(result),
            _ => unreachable!("typed operations cannot use the wire-only constructor"),
        };
        Self {
            valid,
            value: CanonicalOperationValue::WireOnly(value),
            diagnostics: diagnostics.into_iter().map(Into::into).collect(),
        }
    }

    pub(crate) fn definition(
        operation: OperationKind,
        result: OperationResult,
    ) -> Result<Self, ProviderError> {
        let OperationResult {
            valid,
            result,
            diagnostics,
        } = result;
        let empty = result.as_object().is_some_and(serde_json::Map::is_empty);
        let value = match operation {
            OperationKind::AssessTypePack | OperationKind::ApplyTypePack => {
                CanonicalOperationValue::TypePack(
                    (!empty).then(|| decode_value(result)).transpose()?,
                )
            }
            OperationKind::AssessCollectionSetup | OperationKind::ApplyCollectionSetup => {
                CanonicalOperationValue::CollectionSetup(
                    (!empty)
                        .then(|| decode_value(result).map(Box::new))
                        .transpose()?,
                )
            }
            _ => unreachable!("definition constructor requires a definition operation"),
        };
        Ok(Self {
            valid,
            value,
            diagnostics: diagnostics.into_iter().map(Into::into).collect(),
        })
    }

    pub(crate) fn record(&self) -> Option<&RecordDocument> {
        match &self.value {
            CanonicalOperationValue::Read(Some(record))
            | CanonicalOperationValue::Create(Some(record))
            | CanonicalOperationValue::Update(Some(record)) => Some(record),
            _ => None,
        }
    }

    pub(crate) fn batch(execution: crate::mutation::BatchExecution) -> Self {
        let valid = execution.result.failed == 0
            && execution
                .diagnostics
                .iter()
                .all(|item| item.severity != Severity::Error);
        Self {
            valid,
            value: CanonicalOperationValue::Batch(Some(execution.result)),
            diagnostics: execution.diagnostics,
        }
    }

    pub(crate) fn invalid(operation: OperationKind, diagnostics: Vec<Diagnostic>) -> Self {
        let value = match operation {
            OperationKind::Read => CanonicalOperationValue::Read(None),
            OperationKind::Query => CanonicalOperationValue::Query(None),
            OperationKind::Create => CanonicalOperationValue::Create(None),
            OperationKind::Update => CanonicalOperationValue::Update(None),
            OperationKind::Delete => CanonicalOperationValue::Delete(None),
            OperationKind::Rename => CanonicalOperationValue::Rename(None),
            OperationKind::Batch => CanonicalOperationValue::Batch(None),
            _ => unreachable!("typed invalid outcomes are migrated operations"),
        };
        Self {
            valid: false,
            value,
            diagnostics,
        }
    }

    pub(crate) fn failure(operation: OperationKind, error: crate::api::MdbaseError) -> Self {
        let mut diagnostics = error.diagnostics().to_vec();
        if diagnostics.is_empty() {
            diagnostics.push(crate::api::Diagnostic {
                severity: Severity::Error,
                code: crate::api::DiagnosticCode::new("invalid_request"),
                message: error.to_string(),
                path: None,
                field: None,
                type_name: None,
                schema_location: None,
                details: None,
            });
        }
        let value = match operation {
            OperationKind::Read => CanonicalOperationValue::Read(None),
            OperationKind::Query => CanonicalOperationValue::Query(None),
            OperationKind::Create => CanonicalOperationValue::Create(None),
            OperationKind::Update => CanonicalOperationValue::Update(None),
            OperationKind::Delete => CanonicalOperationValue::Delete(None),
            OperationKind::Rename => CanonicalOperationValue::Rename(None),
            OperationKind::Batch => CanonicalOperationValue::Batch(None),
            _ => unreachable!("typed failures are only constructed for migrated operations"),
        };
        Self {
            valid: false,
            value,
            diagnostics,
        }
    }

    /// Decode the compatibility wire request's result using its explicit
    /// operation discriminator. No shape inference is performed.
    pub(crate) fn recover_v03(
        operation: OperationKind,
        result: OperationResult,
    ) -> Result<Self, ProviderError> {
        let OperationResult {
            valid,
            result,
            diagnostics,
        } = result;
        let diagnostics: Vec<Diagnostic> = diagnostics.into_iter().map(Into::into).collect();
        let empty = result.as_object().is_some_and(serde_json::Map::is_empty);
        let value = match operation {
            OperationKind::Read => {
                CanonicalOperationValue::Read((!empty).then(|| decode_value(result)).transpose()?)
            }
            OperationKind::Query => {
                let query = if empty {
                    None
                } else {
                    let records = result
                        .get("results")
                        .and_then(Value::as_array)
                        .cloned()
                        .ok_or_else(|| invalid_shape("query results must contain an array"))?;
                    let meta = result
                        .get("meta")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({}));
                    let total_count = meta
                        .get("total_count")
                        .and_then(Value::as_u64)
                        .and_then(|value| usize::try_from(value).ok())
                        .or_else(|| {
                            meta.get("total_count_outcome")
                                .is_none()
                                .then_some(records.len())
                        });
                    let has_more = meta
                        .get("has_more")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let embedded_diagnostics = result
                        .get("diagnostics")
                        .cloned()
                        .map(decode_value::<Vec<WireDiagnostic>>)
                        .transpose()?
                        .unwrap_or_default()
                        .into_iter()
                        .map(Into::into)
                        .collect();
                    Some(CanonicalQueryValue {
                        records: records.into_iter().map(Into::into).collect(),
                        total_count,
                        has_more,
                        meta: QueryMetadata::new(meta),
                        embedded_diagnostics,
                    })
                };
                CanonicalOperationValue::Query(query)
            }
            OperationKind::Create => {
                CanonicalOperationValue::Create((!empty).then(|| decode_value(result)).transpose()?)
            }
            OperationKind::Update => {
                CanonicalOperationValue::Update((!empty).then(|| decode_value(result)).transpose()?)
            }
            OperationKind::Delete => {
                let value = if empty {
                    None
                } else if result.get("would_delete").is_some() {
                    Some(CanonicalDeleteValue::Preflight(decode_value(result)?))
                } else {
                    Some(CanonicalDeleteValue::Deleted(decode_value(result)?))
                };
                CanonicalOperationValue::Delete(value)
            }
            OperationKind::Rename => {
                let value = if empty {
                    None
                } else if result.get("would_rename").is_some() {
                    Some(CanonicalRenameValue::Preflight(decode_value(result)?))
                } else {
                    Some(CanonicalRenameValue::Renamed(Box::new(decode_value(
                        result,
                    )?)))
                };
                CanonicalOperationValue::Rename(value)
            }
            OperationKind::Batch => {
                CanonicalOperationValue::Batch((!empty).then(|| decode_value(result)).transpose()?)
            }
            OperationKind::Validate => {
                CanonicalOperationValue::WireOnly(WireOnlyOperationValue::Validation(result))
            }
            OperationKind::ListViews
            | OperationKind::ExecuteView
            | OperationKind::ReadViewSource
            | OperationKind::CreateViewSource
            | OperationKind::UpdateViewSource
            | OperationKind::DeleteViewSource => {
                CanonicalOperationValue::WireOnly(WireOnlyOperationValue::ViewResource(result))
            }
            OperationKind::ListTypes
            | OperationKind::ReadType
            | OperationKind::CreateType
            | OperationKind::UpdateType => {
                CanonicalOperationValue::WireOnly(WireOnlyOperationValue::TypeResource(result))
            }
            OperationKind::AssessTypePack | OperationKind::ApplyTypePack => {
                return Self::definition(
                    operation,
                    OperationResult {
                        valid,
                        result,
                        diagnostics: diagnostics.into_iter().map(wire_diagnostic).collect(),
                    },
                );
            }
            OperationKind::AssessCollectionSetup | OperationKind::ApplyCollectionSetup => {
                return Self::definition(
                    operation,
                    OperationResult {
                        valid,
                        result,
                        diagnostics: diagnostics.into_iter().map(wire_diagnostic).collect(),
                    },
                );
            }
        };
        Ok(Self {
            valid,
            value,
            diagnostics,
        })
    }

    /// Internal adapter for operation families whose canonical implementation
    /// still returns the v0.3 envelope. Hosted typed seams call this adapter at
    /// the implementation edge and never inspect `OperationResult.result`.
    /// Validation and setup/resource value families remain explicitly
    /// `WireOnlyOperationValue` until canonical value models exist.
    pub(crate) fn hosted_wire_edge(
        operation: OperationKind,
        result: OperationResult,
    ) -> Result<Self, ProviderError> {
        Self::recover_v03(operation, result)
    }

    /// The single public compatibility adapter from the typed runtime contract
    /// to the exact portable v0.3 operation envelope.
    pub fn to_v03(&self) -> OperationResult {
        let result = match &self.value {
            CanonicalOperationValue::Read(value)
            | CanonicalOperationValue::Create(value)
            | CanonicalOperationValue::Update(value) => encode_optional(value),
            CanonicalOperationValue::Query(value) => {
                value.as_ref().map_or_else(empty_object, |query| {
                    let mut meta = query.meta.clone().into_inner();
                    if !meta.is_object() {
                        meta = serde_json::json!({});
                    }
                    if let Some(total_count) = query.total_count {
                        meta["total_count"] = Value::from(total_count as u64);
                    } else if let Some(meta) = meta.as_object_mut() {
                        meta.remove("total_count");
                    }
                    meta["has_more"] = Value::Bool(query.has_more);
                    let diagnostics = query
                        .embedded_diagnostics
                        .iter()
                        .cloned()
                        .map(wire_diagnostic)
                        .collect::<Vec<_>>();
                    serde_json::json!({"results": query.records, "meta": meta, "diagnostics": diagnostics})
                })
            }
            CanonicalOperationValue::Delete(value) => {
                value
                    .as_ref()
                    .map_or_else(empty_object, |value| match value {
                        CanonicalDeleteValue::Deleted(value) => encode(value),
                        CanonicalDeleteValue::Preflight(value) => encode(value),
                    })
            }
            CanonicalOperationValue::Rename(value) => {
                value
                    .as_ref()
                    .map_or_else(empty_object, |value| match value {
                        CanonicalRenameValue::Renamed(value) => encode(value),
                        CanonicalRenameValue::Preflight(value) => encode(value),
                    })
            }
            CanonicalOperationValue::Batch(value) => encode_optional(value),
            CanonicalOperationValue::TypePack(value) => encode_optional(value),
            CanonicalOperationValue::CollectionSetup(value) => encode_optional(value),
            CanonicalOperationValue::WireOnly(value) => match value {
                WireOnlyOperationValue::Validation(value)
                | WireOnlyOperationValue::ViewResource(value)
                | WireOnlyOperationValue::TypeResource(value) => value.clone(),
            },
            CanonicalOperationValue::LegacyRecoveredV03(result) => return result.clone(),
        };
        OperationResult {
            valid: self.valid,
            result,
            diagnostics: self
                .diagnostics
                .iter()
                .cloned()
                .map(wire_diagnostic)
                .collect(),
        }
    }

    pub(crate) fn attach_committed_file_facts(
        &mut self,
        facts: &std::collections::BTreeMap<String, crate::transactions::CommittedFileFacts>,
    ) {
        fn attach(
            record: &mut RecordDocument,
            facts: &std::collections::BTreeMap<String, crate::transactions::CommittedFileFacts>,
        ) {
            if let Some(value) = facts.get(record.path.as_str()) {
                value.attach_record_file(&mut record.file);
            }
        }
        match &mut self.value {
            CanonicalOperationValue::Create(Some(record))
            | CanonicalOperationValue::Update(Some(record)) => attach(record, facts),
            CanonicalOperationValue::Rename(Some(CanonicalRenameValue::Renamed(value))) => {
                attach(&mut value.result.document, facts)
            }
            CanonicalOperationValue::Batch(Some(batch)) => {
                for item in &mut batch.operations {
                    match &mut item.result {
                        crate::api::BatchOperationResult::Record(record) => attach(record, facts),
                        crate::api::BatchOperationResult::Rename(rename) => {
                            attach(&mut rename.result.document, facts)
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

fn decode_value<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ProviderError> {
    serde_json::from_value(value).map_err(|error| ProviderError::Transaction {
        code: "typed_outcome_decode_failed",
        message: error.to_string(),
    })
}

fn invalid_shape(message: &str) -> ProviderError {
    ProviderError::Transaction {
        code: "typed_outcome_decode_failed",
        message: message.to_string(),
    }
}
fn empty_object() -> Value {
    serde_json::json!({})
}
fn encode<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("canonical operation values serialize")
}
fn encode_optional<T: Serialize>(value: &Option<T>) -> Value {
    value.as_ref().map_or_else(empty_object, encode)
}

fn wire_diagnostic(value: Diagnostic) -> WireDiagnostic {
    WireDiagnostic {
        severity: match value.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
        .to_string(),
        code: value.code.as_str().to_string(),
        message: value.message,
        path: value.path,
        field: value.field,
        type_name: value.type_name,
        schema_location: value.schema_location,
        details: value.details,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(path: &str) -> Value {
        json!({
            "path": path, "revision": "sha256:revision", "types": ["task"],
            "frontmatter": {"title": "Typed"},
            "effective_frontmatter": {"title": "Typed"}, "body": "body\n",
            "document": "---\ntitle: Typed\n---\nbody\n",
            "file": {"name": path, "folder": "", "size": 30, "mtime": "2026-01-01T00:00:00Z"}
        })
    }

    fn roundtrip(kind: OperationKind, value: Value) -> CanonicalOperationOutcome {
        let wire = OperationResult {
            valid: true,
            result: value,
            diagnostics: ["error", "warning", "info"]
                .into_iter()
                .map(|severity| WireDiagnostic {
                    severity: severity.to_string(),
                    code: format!("fixture_{severity}"),
                    message: "fixture".to_string(),
                    path: Some("a.md".to_string()),
                    field: Some("title".to_string()),
                    type_name: Some("task".to_string()),
                    schema_location: Some("#/title".to_string()),
                    details: Some(json!({"exact": true, "severity": severity})),
                })
                .collect(),
        };
        let typed = CanonicalOperationOutcome::recover_v03(kind, wire.clone()).unwrap();
        assert_eq!(typed.to_v03(), wire);
        typed
    }

    #[test]
    fn migrated_runtime_variants_are_typed_and_wire_exact() {
        assert!(matches!(
            roundtrip(OperationKind::Read, record("a.md")).value,
            CanonicalOperationValue::Read(Some(_))
        ));
        assert!(matches!(
            roundtrip(
                OperationKind::Query,
                json!({"results": [record("a.md")], "meta": {"total_count": 1, "has_more": false}, "diagnostics": []})
            )
            .value,
            CanonicalOperationValue::Query(Some(_))
        ));
        assert!(matches!(
            roundtrip(OperationKind::Create, record("a.md")).value,
            CanonicalOperationValue::Create(Some(_))
        ));
        assert!(matches!(
            roundtrip(OperationKind::Update, record("a.md")).value,
            CanonicalOperationValue::Update(Some(_))
        ));
        assert!(matches!(
            roundtrip(
                OperationKind::Delete,
                json!({"path": "a.md", "deleted": true})
            )
            .value,
            CanonicalOperationValue::Delete(Some(CanonicalDeleteValue::Deleted(_)))
        ));
        assert!(matches!(
            roundtrip(
                OperationKind::Delete,
                json!({"path": "a.md", "would_delete": true, "broken_links": []})
            )
            .value,
            CanonicalOperationValue::Delete(Some(CanonicalDeleteValue::Preflight(_)))
        ));
        let mut rename = record("b.md");
        rename["from"] = json!("a.md");
        rename["to"] = json!("b.md");
        assert!(matches!(
            roundtrip(OperationKind::Rename, rename).value,
            CanonicalOperationValue::Rename(Some(CanonicalRenameValue::Renamed(_)))
        ));
        assert!(matches!(
            roundtrip(
                OperationKind::Rename,
                json!({"from": "a.md", "to": "b.md", "dry_run": true, "would_rename": true})
            )
            .value,
            CanonicalOperationValue::Rename(Some(CanonicalRenameValue::Preflight(_)))
        ));
        assert!(matches!(roundtrip(OperationKind::Batch, json!({"operations": [], "succeeded": 0, "failed": 0, "preflight": false, "dry_run": false})).value, CanonicalOperationValue::Batch(Some(_))));
    }

    #[test]
    fn query_wire_omits_deferred_total_but_retains_outcome() {
        let deferred = CanonicalOperationOutcome {
            valid: true,
            value: CanonicalOperationValue::Query(Some(CanonicalQueryValue {
                records: Vec::new(),
                total_count: None,
                has_more: true,
                meta: QueryMetadata::new(json!({
                    "total_count": 999,
                    "total_count_outcome": {"status": "deferred"}
                })),
                embedded_diagnostics: Vec::new(),
            })),
            diagnostics: Vec::new(),
        };
        assert_eq!(
            deferred.to_v03().result,
            json!({
                "results": [],
                "meta": {
                    "has_more": true,
                    "total_count_outcome": {"status": "deferred"}
                },
                "diagnostics": []
            })
        );

        let exact = CanonicalOperationOutcome {
            value: CanonicalOperationValue::Query(Some(CanonicalQueryValue {
                records: Vec::new(),
                total_count: Some(7),
                has_more: false,
                meta: QueryMetadata::new(json!({})),
                embedded_diagnostics: Vec::new(),
            })),
            ..deferred
        };
        assert_eq!(exact.to_v03().result["meta"]["total_count"], 7);
    }

    #[test]
    #[allow(deprecated)]
    fn deprecated_ephemeral_result_fields_compile_and_match_the_adapter() {
        let operation = roundtrip(OperationKind::Read, record("a.md"));
        let outcome = crate::runtime::ExecutionOutcome::new(
            operation.clone(),
            crate::runtime::CollectionGeneration::initial(),
            crate::runtime::ChangeSet::None,
            None,
            None,
        );
        let _current_connect_compile_fixture: &OperationResult = &outcome.result;
        assert_eq!(outcome.result, operation.to_v03());

        let rejected_operation =
            CanonicalOperationOutcome::invalid(OperationKind::Read, operation.diagnostics.clone());
        let rejection = crate::runtime::CommitRejection::new(rejected_operation.clone());
        let _current_rejection_compile_fixture: &OperationResult = &rejection.result;
        assert_eq!(rejection.result, rejected_operation.to_v03());
    }

    #[test]
    fn wire_only_families_are_explicit_and_exact() {
        for kind in [
            OperationKind::Validate,
            OperationKind::ListViews,
            OperationKind::ReadType,
        ] {
            let typed = roundtrip(kind, json!({"family": kind.as_str()}));
            assert!(matches!(typed.value, CanonicalOperationValue::WireOnly(_)));
        }
    }

    #[test]
    fn definition_families_are_closed_typed_values_and_wire_exact() {
        let pack = roundtrip(
            OperationKind::AssessTypePack,
            json!({"applicable": true, "assessment_digest": "sha256:pack", "status": "current"}),
        );
        assert!(matches!(
            pack.value,
            CanonicalOperationValue::TypePack(Some(CanonicalTypePackValue {
                applicable: true,
                ..
            }))
        ));

        let setup = roundtrip(
            OperationKind::AssessCollectionSetup,
            json!({
                "status": "current", "applicable": true,
                "application_id": "dev.mdbase.test", "declaration_digest": "sha256:d",
                "provision_digest": "sha256:p", "collection_revision": "sha256:c",
                "final_collection_revision": "sha256:c", "configuration": [],
                "type_packs": [], "final_resource_revisions": {},
                "baseline_diagnostic_count": 0, "final_diagnostic_count": 0,
                "resolved_diagnostic_count": 0, "introduced_diagnostic_count": 0,
                "baseline_diagnostic_digest": "sha256:b", "assessment_digest": "sha256:a"
            }),
        );
        assert!(matches!(
            setup.value,
            CanonicalOperationValue::CollectionSetup(Some(value))
                if matches!(*value, CanonicalCollectionSetupValue::Assessment(_))
        ));
    }
}
