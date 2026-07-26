use std::collections::BTreeMap;
use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;

use super::{CollectionPath, CollectionPathError};
use crate::v03;
use crate::{Collection, SpecProfile};

/// Result type used by the typed Rust API.
pub type MdbaseResult<T> = Result<T, MdbaseError>;

/// A successful value plus non-fatal operation diagnostics.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OperationOutcome<T> {
    pub value: T,
    pub diagnostics: Vec<Diagnostic>,
}

/// Structured failure from the typed Rust API.
#[derive(Debug, Error)]
pub enum MdbaseError {
    #[error(transparent)]
    InvalidPath(#[from] CollectionPathError),
    #[error("typed canonical operations require a v0.3 collection")]
    UnsupportedProfile,
    #[error("operation '{operation}' requires migrating this v0.2 collection to v0.3")]
    MigrationRequired { operation: &'static str },
    #[error("v0.2 migration contains lossy translations; inspect diagnostics and opt in")]
    LossyMigration { diagnostics: Vec<Diagnostic> },
    #[error("mdbase operation failed")]
    Operation { diagnostics: Vec<Diagnostic> },
    #[error("could not decode canonical operation result: {message}")]
    InvalidResult { message: String },
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
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

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
    Error,
    Warning,
    Info,
}

/// A canonical diagnostic returned by the typed API.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: DiagnosticCode,
    pub message: String,
    pub path: Option<String>,
    pub field: Option<String>,
    pub type_name: Option<String>,
    pub schema_location: Option<String>,
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
    pub fn parse(value: impl Into<String>) -> MdbaseResult<Self> {
        let value = value.into();
        if value.is_empty() {
            return Err(MdbaseError::InvalidResult {
                message: "revision must not be empty".to_string(),
            });
        }
        Ok(Self(value))
    }

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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadRequest {
    pub path: CollectionPath,
}

impl ReadRequest {
    pub fn new(path: impl AsRef<str>) -> MdbaseResult<Self> {
        Ok(Self {
            path: CollectionPath::new(path)?,
        })
    }
}

/// Request to create one record.
#[derive(Clone, Debug, PartialEq)]
pub struct CreateRequest {
    pub path: Option<CollectionPath>,
    pub type_name: Option<String>,
    pub frontmatter: Value,
    pub body: String,
    pub if_revision: Option<Revision>,
    pub dry_run: bool,
}

impl CreateRequest {
    pub fn new(path: CollectionPath, frontmatter: Value) -> Self {
        Self {
            path: Some(path),
            type_name: None,
            frontmatter,
            body: String::new(),
            if_revision: None,
            dry_run: false,
        }
    }

    pub fn derived(frontmatter: Value) -> Self {
        Self {
            path: None,
            type_name: None,
            frontmatter,
            body: String::new(),
            if_revision: None,
            dry_run: false,
        }
    }

    pub fn with_type(mut self, type_name: impl Into<String>) -> Self {
        self.type_name = Some(type_name.into());
        self
    }

    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }
}

/// Request to patch one record.
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateRequest {
    pub path: CollectionPath,
    pub patch: Value,
    pub body: Option<String>,
    pub if_revision: Option<Revision>,
    pub dry_run: bool,
}

impl UpdateRequest {
    pub fn new(path: CollectionPath, patch: Value) -> Self {
        Self {
            path,
            patch,
            body: None,
            if_revision: None,
            dry_run: false,
        }
    }
}

/// Request to delete one record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeleteRequest {
    pub path: CollectionPath,
    pub check_backlinks: bool,
    pub if_revision: Option<Revision>,
    pub dry_run: bool,
}

impl DeleteRequest {
    pub fn new(path: CollectionPath) -> Self {
        Self {
            path,
            check_backlinks: false,
            if_revision: None,
            dry_run: false,
        }
    }
}

/// Request to rename one record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenameRequest {
    pub from: CollectionPath,
    pub to: CollectionPath,
    pub update_refs: bool,
    pub if_revision: Option<Revision>,
    pub dry_run: bool,
}

/// Options for translating a v0.2 collection to canonical v0.3 files.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct V02MigrationRequest {
    pub dry_run: bool,
    pub allow_lossy: bool,
}

/// One file that a v0.2 migration will create or replace.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct V02MigrationChange {
    pub path: String,
    pub before_revision: Option<Revision>,
    pub after_revision: Option<Revision>,
}

/// Verified v0.2-to-v0.3 migration plan or applied result.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct V02MigrationResult {
    pub id: String,
    pub applied: bool,
    pub verified_records: usize,
    pub manifest_path: String,
    pub changes: Vec<V02MigrationChange>,
    pub diagnostics: Vec<Diagnostic>,
}

impl RenameRequest {
    pub fn new(from: CollectionPath, to: CollectionPath) -> Self {
        Self {
            from,
            to,
            update_refs: true,
            if_revision: None,
            dry_run: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum QueryDirection {
    #[default]
    Asc,
    Desc,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryOrder {
    pub field: String,
    pub direction: QueryDirection,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FrontmatterMode {
    #[default]
    Effective,
    Raw,
    Both,
}

/// Typed builder for the common canonical query surface.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryRequest {
    pub types: Vec<String>,
    pub context: Option<CollectionPath>,
    pub projections: BTreeMap<String, String>,
    pub where_expression: Option<String>,
    pub select: Option<Vec<String>>,
    pub order_by: Vec<QueryOrder>,
    pub group_by: Vec<QueryOrder>,
    pub limit: Option<u64>,
    pub offset: u64,
    pub snapshot: Option<String>,
    pub include_body: bool,
    pub frontmatter: FrontmatterMode,
}

impl QueryRequest {
    pub fn builder() -> Self {
        Self::default()
    }

    pub fn type_name(mut self, type_name: impl Into<String>) -> Self {
        self.types.push(type_name.into());
        self
    }

    pub fn where_expression(mut self, expression: impl Into<String>) -> Self {
        self.where_expression = Some(expression.into());
        self
    }

    pub fn order_by(mut self, field: impl Into<String>, direction: QueryDirection) -> Self {
        self.order_by.push(QueryOrder {
            field: field.into(),
            direction,
        });
        self
    }

    pub fn limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }

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
        let frontmatter = match self.frontmatter {
            FrontmatterMode::Effective => None,
            FrontmatterMode::Raw => Some("raw"),
            FrontmatterMode::Both => Some("both"),
        };
        if let Some(frontmatter) = frontmatter {
            value.insert(
                "frontmatter".to_string(),
                Value::String(frontmatter.to_string()),
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordFile {
    pub name: String,
    pub folder: String,
    pub size: u64,
    pub mtime: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadResult {
    pub path: CollectionPath,
    pub revision: Revision,
    #[serde(default)]
    pub types: Vec<String>,
    pub frontmatter: Value,
    pub raw_frontmatter: Value,
    pub body: String,
    pub file: RecordFile,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MutationResult {
    pub path: CollectionPath,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<Revision>,
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default)]
    pub frontmatter: Value,
    #[serde(default)]
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeleteResult {
    pub path: CollectionPath,
    pub deleted: bool,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub broken_links: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenameResult {
    pub from: CollectionPath,
    pub to: CollectionPath,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<Revision>,
    #[serde(default)]
    pub references_updated: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct QueryResult {
    #[serde(rename = "results")]
    pub records: Vec<Value>,
    #[serde(skip_serializing)]
    pub total_count: usize,
    #[serde(skip_serializing)]
    pub has_more: bool,
    #[serde(skip_serializing)]
    pub snapshot: Option<String>,
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

    pub fn read(&self, request: ReadRequest) -> MdbaseResult<OperationOutcome<ReadResult>> {
        if self.collection.spec_profile == SpecProfile::V02 {
            return crate::compat::v02::read(self.collection, request);
        }
        self.execute(self.operations()?.read(&json!({"path": request.path})))
    }

    pub fn create(&self, request: CreateRequest) -> MdbaseResult<OperationOutcome<MutationResult>> {
        self.require_canonical("create")?;
        let mut input = json!({
            "frontmatter": request.frontmatter,
            "body": request.body,
            "dry_run": request.dry_run,
        });
        set_optional(&mut input, "path", request.path.map(|path| json!(path)));
        set_optional(&mut input, "type", request.type_name.map(Value::String));
        set_optional(
            &mut input,
            "if_revision",
            request.if_revision.map(|revision| json!(revision)),
        );
        self.execute(self.operations()?.create(&input))
    }

    pub fn update(&self, request: UpdateRequest) -> MdbaseResult<OperationOutcome<MutationResult>> {
        self.require_canonical("update")?;
        let mut input = json!({
            "path": request.path,
            "patch": request.patch,
            "dry_run": request.dry_run,
        });
        set_optional(&mut input, "body", request.body.map(Value::String));
        set_optional(
            &mut input,
            "if_revision",
            request.if_revision.map(|revision| json!(revision)),
        );
        self.execute(self.operations()?.update(&input))
    }

    pub fn delete(&self, request: DeleteRequest) -> MdbaseResult<OperationOutcome<DeleteResult>> {
        self.require_canonical("delete")?;
        let mut input = json!({
            "path": request.path,
            "check_backlinks": request.check_backlinks,
            "dry_run": request.dry_run,
        });
        set_optional(
            &mut input,
            "if_revision",
            request.if_revision.map(|revision| json!(revision)),
        );
        self.execute(self.operations()?.delete(&input))
    }

    pub fn rename(&self, request: RenameRequest) -> MdbaseResult<OperationOutcome<RenameResult>> {
        self.require_canonical("rename")?;
        let mut input = json!({
            "from": request.from,
            "to": request.to,
            "update_refs": request.update_refs,
            "dry_run": request.dry_run,
        });
        set_optional(
            &mut input,
            "if_revision",
            request.if_revision.map(|revision| json!(revision)),
        );
        self.execute(self.operations()?.rename(&input))
    }

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
