use std::collections::BTreeMap;
use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;

use super::{CollectionPath, CollectionPathError};
use crate::v03;
use crate::{Collection, SpecProfile};

fn empty_json_object() -> Value {
    Value::Object(Map::new())
}

/// Result type used by the typed Rust API.
pub type MdbaseResult<T> = Result<T, MdbaseError>;

/// A successful value plus non-fatal operation diagnostics.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OperationOutcome<T> {
    /// Typed operation value.
    pub value: T,
    /// Non-fatal diagnostics emitted while producing the value.
    pub diagnostics: Vec<Diagnostic>,
}

/// Structured failure from the typed Rust API.
#[derive(Debug, Error)]
pub enum MdbaseError {
    /// A collection-relative path was invalid.
    #[error(transparent)]
    InvalidPath(#[from] CollectionPathError),
    /// An operation was requested for an unsupported collection profile.
    #[error("typed canonical operations require a v0.3 collection")]
    UnsupportedProfile,
    /// A mutation requires the legacy collection to be migrated first.
    #[error("operation '{operation}' requires migrating this v0.2 collection to v0.3")]
    MigrationRequired {
        /// Name of the blocked mutation.
        operation: &'static str,
    },
    /// Migration encountered translations that need explicit approval.
    #[error("v0.2 migration contains lossy translations; inspect diagnostics and opt in")]
    LossyMigration {
        /// Diagnostics describing each lossy translation.
        diagnostics: Vec<Diagnostic>,
    },
    /// A typed or deserialized request failed local validation.
    #[error("invalid typed request: {message}")]
    InvalidRequest {
        /// Human-readable request validation failure.
        message: String,
    },
    /// The collection operation returned one or more fatal diagnostics.
    #[error("mdbase operation failed")]
    Operation {
        /// Canonical diagnostics returned by the failed operation.
        diagnostics: Vec<Diagnostic>,
    },
    /// A canonical wire result could not be decoded into its typed form.
    #[error("could not decode canonical operation result: {message}")]
    InvalidResult {
        /// Human-readable decode failure.
        message: String,
    },
}

impl MdbaseError {
    /// Diagnostics emitted by a failed operation, if applicable.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        match self {
            Self::Operation { diagnostics } => diagnostics,
            Self::LossyMigration { diagnostics } => diagnostics,
            _ => &[],
        }
    }
}

/// Extensible diagnostic identifier.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DiagnosticCode(String);

impl DiagnosticCode {
    /// Construct an extensible diagnostic code.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Return the stable string representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DiagnosticCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Typed diagnostic severity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The operation cannot safely succeed.
    Error,
    /// The operation succeeded with a condition requiring attention.
    Warning,
    /// Informational context about a successful operation.
    Info,
}

/// A canonical diagnostic returned by the typed API.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Diagnostic severity.
    pub severity: Severity,
    /// Stable machine-readable identifier.
    pub code: DiagnosticCode,
    /// Human-readable explanation.
    pub message: String,
    /// Related collection path, when available.
    pub path: Option<String>,
    /// Related frontmatter field, when available.
    pub field: Option<String>,
    /// Related type name, when available.
    pub type_name: Option<String>,
    /// JSON Schema location, when available.
    pub schema_location: Option<String>,
    /// Extensible structured diagnostic details.
    pub details: Option<Value>,
}

impl From<v03::Diagnostic> for Diagnostic {
    fn from(value: v03::Diagnostic) -> Self {
        let severity = match value.severity.as_str() {
            "warning" => Severity::Warning,
            "info" => Severity::Info,
            _ => Severity::Error,
        };
        Self {
            severity,
            code: DiagnosticCode::new(value.code),
            message: value.message,
            path: value.path,
            field: value.field,
            type_name: value.type_name,
            schema_location: value.schema_location,
            details: value.details,
        }
    }
}

/// Opaque content revision used for optimistic concurrency.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Revision(String);

impl Revision {
    /// Validate and construct an opaque revision.
    pub fn parse(value: impl Into<String>) -> MdbaseResult<Self> {
        let value = value.into();
        if value.is_empty() {
            return Err(MdbaseError::InvalidResult {
                message: "revision must not be empty".to_string(),
            });
        }
        Ok(Self(value))
    }

    /// Return the opaque revision string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Request to read one record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReadRequest {
    /// Record path relative to the collection root.
    pub path: CollectionPath,
    /// Include the exact UTF-8 record source.
    #[serde(default)]
    pub include_document: bool,
}

impl ReadRequest {
    /// Parse a record path and construct a read request.
    pub fn new(path: impl AsRef<str>) -> MdbaseResult<Self> {
        Ok(Self {
            path: CollectionPath::new(path)?,
            include_document: false,
        })
    }

    /// Request the exact source document.
    pub fn with_document(mut self) -> Self {
        self.include_document = true;
        self
    }
}

/// Request to create one record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreateRequest {
    /// Explicit target path, or `None` to derive it from type rules.
    #[serde(default)]
    pub path: Option<CollectionPath>,
    /// Explicit type name used for path derivation and type membership.
    #[serde(default, rename = "type")]
    pub type_name: Option<String>,
    /// Persisted frontmatter object.
    #[serde(default = "empty_json_object")]
    pub frontmatter: Value,
    /// Markdown body.
    #[serde(default)]
    pub body: String,
    /// Optional optimistic-concurrency precondition.
    #[serde(default)]
    pub if_revision: Option<Revision>,
    /// Include exact post-write source in the result.
    #[serde(default)]
    pub include_document: bool,
}

impl CreateRequest {
    /// Construct a body-only create request with an explicit path.
    pub fn new(path: CollectionPath) -> Self {
        Self {
            path: Some(path),
            type_name: None,
            frontmatter: empty_json_object(),
            body: String::new(),
            if_revision: None,
            include_document: false,
        }
    }

    /// Construct a body-only create request whose path is derived from its type.
    pub fn derived() -> Self {
        Self {
            path: None,
            type_name: None,
            frontmatter: empty_json_object(),
            body: String::new(),
            if_revision: None,
            include_document: false,
        }
    }

    /// Set persisted frontmatter fields.
    pub fn with_frontmatter(mut self, frontmatter: Value) -> Self {
        self.frontmatter = frontmatter;
        self
    }

    /// Set the explicit type name.
    pub fn with_type(mut self, type_name: impl Into<String>) -> Self {
        self.type_name = Some(type_name.into());
        self
    }

    /// Set the Markdown body.
    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    /// Include exact post-write source in the result.
    pub fn with_document(mut self) -> Self {
        self.include_document = true;
        self
    }

    fn into_wire(self) -> Value {
        let mut input = json!({
            "frontmatter": self.frontmatter,
            "body": self.body,
        });
        set_optional(&mut input, "path", self.path.map(|path| json!(path)));
        set_optional(&mut input, "type", self.type_name.map(Value::String));
        set_optional(
            &mut input,
            "if_revision",
            self.if_revision.map(|revision| json!(revision)),
        );
        if self.include_document {
            input["include_document"] = Value::Bool(true);
        }
        input
    }
}

/// Request to patch one record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UpdateRequest {
    /// Existing record path.
    pub path: CollectionPath,
    /// Frontmatter merge patch.
    pub patch: Value,
    /// Complete replacement Markdown source, mutually exclusive with `patch`
    /// and `body`.
    #[serde(default)]
    pub document: Option<String>,
    /// Complete replacement body, or `None` to preserve the current body.
    #[serde(default)]
    pub body: Option<String>,
    /// Optional optimistic-concurrency precondition.
    #[serde(default)]
    pub if_revision: Option<Revision>,
    /// Include exact post-write source in the result.
    #[serde(default)]
    pub include_document: bool,
}

impl UpdateRequest {
    /// Construct a frontmatter update request.
    pub fn new(path: CollectionPath, patch: Value) -> Self {
        Self {
            path,
            patch,
            document: None,
            body: None,
            if_revision: None,
            include_document: false,
        }
    }

    /// Construct a complete source replacement.
    pub fn replace_document(path: CollectionPath, document: impl Into<String>) -> Self {
        Self {
            path,
            patch: json!({}),
            document: Some(document.into()),
            body: None,
            if_revision: None,
            include_document: true,
        }
    }

    /// Include exact post-write source in the result.
    pub fn with_document(mut self) -> Self {
        self.include_document = true;
        self
    }

    fn into_wire(self) -> Value {
        let mut input = json!({ "path": self.path });
        if let Some(document) = self.document {
            input["document"] = Value::String(document);
        } else {
            input["patch"] = self.patch;
            set_optional(&mut input, "body", self.body.map(Value::String));
        }
        set_optional(
            &mut input,
            "if_revision",
            self.if_revision.map(|revision| json!(revision)),
        );
        if self.include_document {
            input["include_document"] = Value::Bool(true);
        }
        input
    }
}

/// Request to delete one record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeleteRequest {
    /// Existing record path.
    pub path: CollectionPath,
    /// Report inbound links before deletion.
    #[serde(default)]
    pub check_backlinks: bool,
    /// Optional optimistic-concurrency precondition.
    #[serde(default)]
    pub if_revision: Option<Revision>,
}

impl DeleteRequest {
    /// Construct a delete request.
    pub fn new(path: CollectionPath) -> Self {
        Self {
            path,
            check_backlinks: false,
            if_revision: None,
        }
    }

    fn into_wire(self) -> Value {
        let mut input = json!({
            "path": self.path,
            "check_backlinks": self.check_backlinks,
        });
        set_optional(
            &mut input,
            "if_revision",
            self.if_revision.map(|revision| json!(revision)),
        );
        input
    }
}

/// Request to rename one record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RenameRequest {
    /// Existing record path.
    pub from: CollectionPath,
    /// New record path.
    pub to: CollectionPath,
    /// Rewrite resolvable references to the record.
    #[serde(default = "default_true")]
    pub update_refs: bool,
    /// Optional optimistic-concurrency precondition.
    #[serde(default)]
    pub if_revision: Option<Revision>,
    /// Include exact post-write source in the result.
    #[serde(default)]
    pub include_document: bool,
}

/// Options for translating a v0.2 collection to canonical v0.3 files.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct V02MigrationRequest {
    /// Verify and report the plan without changing files.
    pub dry_run: bool,
    /// Permit translations marked as unable to preserve future write behavior.
    pub allow_lossy: bool,
}

/// One file that a v0.2 migration will create or replace.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct V02MigrationChange {
    /// Collection-relative artifact path.
    pub path: String,
    /// Revision before migration, or `None` for a new artifact.
    pub before_revision: Option<Revision>,
    /// Revision after migration, or `None` for a removed artifact.
    pub after_revision: Option<Revision>,
}

/// Verified v0.2-to-v0.3 migration plan or applied result.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct V02MigrationResult {
    /// Stable content-derived migration identifier.
    pub id: String,
    /// Whether the verified plan was committed.
    pub applied: bool,
    /// Number of records compared across compatibility and canonical reads.
    pub verified_records: usize,
    /// Recovery and provenance manifest path.
    pub manifest_path: String,
    /// Planned or applied artifact changes.
    pub changes: Vec<V02MigrationChange>,
    /// Translation diagnostics, including any lossy mappings.
    pub diagnostics: Vec<Diagnostic>,
}

impl RenameRequest {
    /// Construct a reference-updating rename request.
    pub fn new(from: CollectionPath, to: CollectionPath) -> Self {
        Self {
            from,
            to,
            update_refs: true,
            if_revision: None,
            include_document: false,
        }
    }

    /// Include exact post-write source in the result.
    pub fn with_document(mut self) -> Self {
        self.include_document = true;
        self
    }

    fn into_wire(self) -> Value {
        let mut input = json!({
            "from": self.from,
            "to": self.to,
            "update_refs": self.update_refs,
        });
        set_optional(
            &mut input,
            "if_revision",
            self.if_revision.map(|revision| json!(revision)),
        );
        if self.include_document {
            input["include_document"] = Value::Bool(true);
        }
        input
    }
}

/// One typed mutation in a batch request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "input", rename_all = "lowercase")]
pub enum BatchOperation {
    /// Create one record.
    Create(CreateRequest),
    /// Update one record.
    Update(UpdateRequest),
    /// Delete one record.
    Delete(DeleteRequest),
    /// Rename one record.
    Rename(RenameRequest),
}

impl BatchOperation {
    fn into_wire(self) -> Value {
        match self {
            Self::Create(request) => json!({"kind": "create", "input": request.into_wire()}),
            Self::Update(request) => json!({"kind": "update", "input": request.into_wire()}),
            Self::Delete(request) => json!({"kind": "delete", "input": request.into_wire()}),
            Self::Rename(request) => json!({"kind": "rename", "input": request.into_wire()}),
        }
    }
}

/// Crash-recoverable collection mutation batch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BatchRequest {
    /// Ordered mutations to execute.
    pub operations: Vec<BatchOperation>,
    /// Continue after individual failures instead of using recoverable
    /// non-partial semantics.
    #[serde(default)]
    pub allow_partial: bool,
    /// Execute the complete plan in a disposable shadow collection.
    #[serde(default)]
    pub dry_run: bool,
}

impl BatchRequest {
    /// Construct a non-empty, non-partial batch.
    pub fn new(operations: Vec<BatchOperation>) -> MdbaseResult<Self> {
        if operations.is_empty() {
            return Err(MdbaseError::InvalidRequest {
                message: "batch operations must not be empty".to_string(),
            });
        }
        Ok(Self {
            operations,
            allow_partial: false,
            dry_run: false,
        })
    }

    fn into_wire(self) -> Value {
        json!({
            "operations": self
                .operations
                .into_iter()
                .map(BatchOperation::into_wire)
                .collect::<Vec<_>>(),
            "allow_partial": self.allow_partial,
            "dry_run": self.dry_run,
        })
    }
}

/// Sort direction for query ordering and grouping.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueryDirection {
    /// Ascending order.
    #[default]
    Asc,
    /// Descending order.
    Desc,
}

/// One ordered query field.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryOrder {
    /// Field or projection name.
    pub field: String,
    /// Ordering direction.
    pub direction: QueryDirection,
}

/// Frontmatter representation included in query records.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrontmatterMode {
    /// Include effective frontmatter after defaults and computed fields.
    #[default]
    Effective,
    /// Include only persisted frontmatter.
    Persisted,
    /// Include both persisted and effective frontmatter.
    Both,
}

/// Typed builder for the common canonical query surface.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QueryRequest {
    /// Type names used to restrict candidate records.
    #[serde(default)]
    pub types: Vec<String>,
    /// Record used to bind the query `this` context.
    #[serde(default)]
    pub context: Option<CollectionPath>,
    /// Named CEL expressions evaluated before filtering and selection.
    #[serde(default)]
    pub projections: BTreeMap<String, String>,
    /// CEL predicate used to filter candidates.
    #[serde(default, rename = "where")]
    pub where_expression: Option<String>,
    /// Fields retained in each returned record.
    #[serde(default)]
    pub select: Option<Vec<String>>,
    /// Deterministic record ordering.
    #[serde(default)]
    pub order_by: Vec<QueryOrder>,
    /// Ordered grouping fields.
    #[serde(default)]
    pub group_by: Vec<QueryOrder>,
    /// Maximum returned records.
    #[serde(default)]
    pub limit: Option<u64>,
    /// Number of ordered records skipped before returning results.
    #[serde(default)]
    pub offset: u64,
    /// Optional pagination snapshot expected from an earlier page.
    #[serde(default)]
    pub snapshot: Option<String>,
    /// Whether returned records include their Markdown body.
    #[serde(default)]
    pub include_body: bool,
    /// Frontmatter representation to return.
    #[serde(default)]
    pub frontmatter_mode: FrontmatterMode,
}

impl QueryRequest {
    /// Start a query with canonical defaults.
    pub fn builder() -> Self {
        Self::default()
    }

    /// Add a type filter.
    pub fn type_name(mut self, type_name: impl Into<String>) -> Self {
        self.types.push(type_name.into());
        self
    }

    /// Set the CEL filter expression.
    pub fn where_expression(mut self, expression: impl Into<String>) -> Self {
        self.where_expression = Some(expression.into());
        self
    }

    /// Append an ordering field.
    pub fn order_by(mut self, field: impl Into<String>, direction: QueryDirection) -> Self {
        self.order_by.push(QueryOrder {
            field: field.into(),
            direction,
        });
        self
    }

    /// Set the maximum returned record count.
    pub fn limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Set the ordered record offset.
    pub fn offset(mut self, offset: u64) -> Self {
        self.offset = offset;
        self
    }

    pub(crate) fn to_wire(&self) -> Value {
        let mut value = Map::new();
        if !self.types.is_empty() {
            value.insert("types".to_string(), json!(self.types));
        }
        if let Some(context) = &self.context {
            value.insert("context".to_string(), json!({"this": {"path": context}}));
        }
        if !self.projections.is_empty() {
            value.insert(
                "projections".to_string(),
                Value::Object(
                    self.projections
                        .iter()
                        .map(|(name, expression)| (name.clone(), json!({"expr": expression})))
                        .collect(),
                ),
            );
        }
        if let Some(expression) = &self.where_expression {
            value.insert("where".to_string(), Value::String(expression.clone()));
        }
        if let Some(select) = &self.select {
            value.insert("select".to_string(), json!(select));
        }
        insert_order(&mut value, "order_by", &self.order_by);
        insert_order(&mut value, "group_by", &self.group_by);
        if let Some(limit) = self.limit {
            value.insert("limit".to_string(), json!(limit));
        }
        if self.offset != 0 {
            value.insert("offset".to_string(), json!(self.offset));
        }
        if let Some(snapshot) = &self.snapshot {
            value.insert("snapshot".to_string(), Value::String(snapshot.clone()));
        }
        if self.include_body {
            value.insert("include_body".to_string(), Value::Bool(true));
        }
        let frontmatter_mode = match self.frontmatter_mode {
            FrontmatterMode::Effective => None,
            FrontmatterMode::Persisted => Some("persisted"),
            FrontmatterMode::Both => Some("both"),
        };
        if let Some(frontmatter_mode) = frontmatter_mode {
            value.insert(
                "frontmatter_mode".to_string(),
                Value::String(frontmatter_mode.to_string()),
            );
        }
        Value::Object(value)
    }
}

fn insert_order(target: &mut Map<String, Value>, name: &str, order: &[QueryOrder]) {
    if !order.is_empty() {
        target.insert(
            name.to_string(),
            Value::Array(
                order
                    .iter()
                    .map(|item| {
                        json!({
                            "field": item.field,
                            "direction": match item.direction {
                                QueryDirection::Asc => "asc",
                                QueryDirection::Desc => "desc",
                            }
                        })
                    })
                    .collect(),
            ),
        );
    }
}

/// Filesystem metadata attached to a record read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordFile {
    /// Filename including extension.
    pub name: String,
    /// Collection-relative parent folder.
    pub folder: String,
    /// File size in bytes.
    pub size: u64,
    /// Last modification time as an ISO 8601 string.
    pub mtime: String,
}

/// Complete authoritative record document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordDocument {
    /// Canonical collection-relative path.
    pub path: CollectionPath,
    /// Opaque content revision.
    pub revision: Revision,
    /// Effective type memberships.
    #[serde(default)]
    pub types: Vec<String>,
    /// Parsed frontmatter persisted in the Markdown file.
    pub frontmatter: Value,
    /// Frontmatter after read defaults and computed values.
    pub effective_frontmatter: Value,
    /// Markdown body.
    pub body: String,
    /// Exact UTF-8 record source when requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<String>,
    /// Filesystem metadata.
    pub file: RecordFile,
}

/// Result of a delete request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeleteResult {
    /// Target record path.
    pub path: CollectionPath,
    /// Whether the authoritative file was deleted.
    pub deleted: bool,
    /// Inbound references that would become broken.
    #[serde(default)]
    pub broken_links: Vec<Value>,
}

/// Non-mutating preview of a delete request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeletePreflightResult {
    /// Target record path.
    pub path: CollectionPath,
    /// Whether the record would be deleted.
    pub would_delete: bool,
    /// Inbound references that would become broken.
    #[serde(default)]
    pub broken_links: Vec<Value>,
}

/// Result of a rename request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenameResult {
    /// Complete authoritative renamed record.
    #[serde(flatten)]
    pub document: RecordDocument,
    /// Original record path.
    pub from: CollectionPath,
    /// New record path.
    pub to: CollectionPath,
    /// References rewritten as part of the rename.
    #[serde(default)]
    pub references_updated: Vec<Value>,
}

/// Non-mutating preview of a rename request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenamePreflightResult {
    /// Original record path.
    pub from: CollectionPath,
    /// Proposed record path.
    pub to: CollectionPath,
    /// Whether the record would be renamed.
    pub would_rename: bool,
    /// References that would be rewritten.
    #[serde(default)]
    pub references_affected: Vec<Value>,
    /// Preflight warnings.
    #[serde(default)]
    pub warnings: Vec<Value>,
}

/// Result for one operation within a batch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BatchItemResult {
    /// Zero-based position in the request.
    pub index: usize,
    /// Canonical mutation kind.
    pub kind: String,
    /// Whether this mutation succeeded.
    pub valid: bool,
    /// Mutation-specific canonical result.
    pub result: Value,
    /// Diagnostics emitted by this mutation.
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
}

/// Aggregate result from a typed batch request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BatchResult {
    /// Per-operation results in request order.
    pub operations: Vec<BatchItemResult>,
    /// Number of successful operations.
    pub succeeded: usize,
    /// Number of failed operations.
    pub failed: usize,
    /// Whether execution occurred only in the preflight workspace.
    pub preflight: bool,
    /// Whether the request explicitly asked for a dry run.
    pub dry_run: bool,
}

/// Paginated canonical query result.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct QueryResult {
    /// Returned records.
    #[serde(rename = "results")]
    pub records: Vec<Value>,
    /// Total matching records before pagination.
    #[serde(skip_serializing)]
    pub total_count: usize,
    /// Whether another page is available.
    #[serde(skip_serializing)]
    pub has_more: bool,
    /// Opaque snapshot token for consistent pagination.
    #[serde(skip_serializing)]
    pub snapshot: Option<String>,
    /// Canonical query metadata.
    pub meta: Value,
}

/// Borrowed typed operation service for one loaded collection.
pub struct TypedCollection<'a> {
    collection: &'a Collection,
}

impl<'a> TypedCollection<'a> {
    pub(crate) fn new(collection: &'a Collection) -> MdbaseResult<Self> {
        Ok(Self { collection })
    }

    /// Read one record.
    pub fn read(&self, request: ReadRequest) -> MdbaseResult<OperationOutcome<RecordDocument>> {
        if self.collection.spec_profile == SpecProfile::V02 {
            return crate::compat::v02::read(self.collection, request);
        }
        self.execute(self.operations()?.read(&json!({
            "path": request.path,
            "include_document": request.include_document,
        })))
    }

    /// Create one canonical record.
    pub fn create(&self, request: CreateRequest) -> MdbaseResult<OperationOutcome<RecordDocument>> {
        self.require_canonical("create")?;
        let input = request.into_wire();
        self.execute(self.operations()?.create(&input))
    }

    /// Patch one canonical record.
    pub fn update(&self, request: UpdateRequest) -> MdbaseResult<OperationOutcome<RecordDocument>> {
        self.require_canonical("update")?;
        let input = request.into_wire();
        self.execute(self.operations()?.update(&input))
    }

    /// Delete one canonical record.
    pub fn delete(&self, request: DeleteRequest) -> MdbaseResult<OperationOutcome<DeleteResult>> {
        self.require_canonical("delete")?;
        let input = request.into_wire();
        self.execute(self.operations()?.delete(&input))
    }

    /// Preview one delete without mutating authoritative state.
    pub fn preflight_delete(
        &self,
        request: DeleteRequest,
    ) -> MdbaseResult<OperationOutcome<DeletePreflightResult>> {
        self.require_canonical("delete")?;
        let mut input = request.into_wire();
        input["dry_run"] = Value::Bool(true);
        self.execute(self.operations()?.delete(&input))
    }

    /// Rename one canonical record and optionally rewrite references.
    pub fn rename(&self, request: RenameRequest) -> MdbaseResult<OperationOutcome<RenameResult>> {
        self.require_canonical("rename")?;
        let input = request.into_wire();
        self.execute(self.operations()?.rename(&input))
    }

    /// Preview one rename without mutating authoritative state.
    pub fn preflight_rename(
        &self,
        request: RenameRequest,
    ) -> MdbaseResult<OperationOutcome<RenamePreflightResult>> {
        self.require_canonical("rename")?;
        let mut input = request.into_wire();
        input["dry_run"] = Value::Bool(true);
        self.execute(self.operations()?.rename(&input))
    }

    /// Execute typed mutations as one recoverable batch.
    pub fn batch(&self, request: BatchRequest) -> MdbaseResult<OperationOutcome<BatchResult>> {
        self.require_canonical("batch")?;
        let input = request.into_wire();
        self.execute(self.operations()?.batch(&input))
    }

    /// Query canonical or read-only-compatible records.
    pub fn query(&self, request: QueryRequest) -> MdbaseResult<OperationOutcome<QueryResult>> {
        if self.collection.spec_profile == SpecProfile::V02 {
            return crate::compat::v02::query(self.collection, request);
        }
        let result = self.operations()?.query(&request.to_wire());
        let diagnostics = result
            .diagnostics
            .into_iter()
            .map(Diagnostic::from)
            .collect::<Vec<_>>();
        if !result.valid {
            return Err(MdbaseError::Operation { diagnostics });
        }
        let records = result
            .result
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| MdbaseError::InvalidResult {
                message: "query result does not contain a results array".to_string(),
            })?;
        let meta = result
            .result
            .get("meta")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let total_count = meta
            .get("total_count")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| MdbaseError::InvalidResult {
                message: "query result does not contain a valid total_count".to_string(),
            })?;
        let has_more = meta
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let snapshot = meta
            .get("snapshot")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(OperationOutcome {
            value: QueryResult {
                records,
                total_count,
                has_more,
                snapshot,
                meta,
            },
            diagnostics,
        })
    }

    /// Plan or atomically apply the explicit v0.2-to-v0.3 migration.
    pub fn migrate_v02(&self, request: V02MigrationRequest) -> MdbaseResult<V02MigrationResult> {
        crate::compat::v02_migration::migrate(self.collection, request)
    }

    fn operations(&self) -> MdbaseResult<v03::Operations<'_>> {
        self.collection
            .v03_operations()
            .map_err(|diagnostic| MdbaseError::Operation {
                diagnostics: vec![(*diagnostic).into()],
            })
    }

    fn require_canonical(&self, operation: &'static str) -> MdbaseResult<()> {
        if self.collection.spec_profile == SpecProfile::V02 {
            return Err(MdbaseError::MigrationRequired { operation });
        }
        Ok(())
    }

    fn execute<T: DeserializeOwned>(
        &self,
        result: v03::OperationResult,
    ) -> MdbaseResult<OperationOutcome<T>> {
        let diagnostics = result
            .diagnostics
            .into_iter()
            .map(Diagnostic::from)
            .collect::<Vec<_>>();
        if !result.valid {
            return Err(MdbaseError::Operation { diagnostics });
        }
        let value =
            serde_json::from_value(result.result).map_err(|error| MdbaseError::InvalidResult {
                message: error.to_string(),
            })?;
        Ok(OperationOutcome { value, diagnostics })
    }
}

fn set_optional(target: &mut Value, key: &str, value: Option<Value>) {
    if let (Some(object), Some(value)) = (target.as_object_mut(), value) {
        object.insert(key.to_string(), value);
    }
}

const fn default_true() -> bool {
    true
}
