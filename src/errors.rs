//! Error codes from Appendix C of the mdbase specification.

use thiserror::Error;

// C.1 Validation Error Codes

// Field Errors
pub const MISSING_REQUIRED: &str = "missing_required";
pub const TYPE_MISMATCH: &str = "type_mismatch";
pub const CONSTRAINT_VIOLATION: &str = "constraint_violation";
pub const INVALID_ENUM: &str = "invalid_enum";
pub const UNKNOWN_FIELD: &str = "unknown_field";
pub const DEPRECATED_FIELD: &str = "deprecated_field";
pub const DUPLICATE_ID: &str = "duplicate_id";
pub const DUPLICATE_VALUE: &str = "duplicate_value";

// List Errors
pub const LIST_TOO_SHORT: &str = "list_too_short";
pub const LIST_TOO_LONG: &str = "list_too_long";
pub const LIST_DUPLICATE: &str = "list_duplicate";
pub const LIST_ITEM_INVALID: &str = "list_item_invalid";

// String Errors
pub const STRING_TOO_SHORT: &str = "string_too_short";
pub const STRING_TOO_LONG: &str = "string_too_long";
pub const PATTERN_MISMATCH: &str = "pattern_mismatch";

// Number Errors
pub const NUMBER_TOO_SMALL: &str = "number_too_small";
pub const NUMBER_TOO_LARGE: &str = "number_too_large";
pub const NOT_INTEGER: &str = "not_integer";

// Link Errors
pub const INVALID_LINK: &str = "invalid_link";
pub const LINK_NOT_FOUND: &str = "link_not_found";
pub const LINK_WRONG_TYPE: &str = "link_wrong_type";
pub const AMBIGUOUS_LINK: &str = "ambiguous_link";

// Date/Time Errors
pub const INVALID_DATE: &str = "invalid_date";
pub const INVALID_DATETIME: &str = "invalid_datetime";
pub const INVALID_TIME: &str = "invalid_time";

// C.2 Type System Errors
pub const UNKNOWN_TYPE: &str = "unknown_type";
pub const CIRCULAR_INHERITANCE: &str = "circular_inheritance";
pub const MISSING_PARENT_TYPE: &str = "missing_parent_type";
pub const TYPE_CONFLICT: &str = "type_conflict";
pub const INVALID_TYPE_DEFINITION: &str = "invalid_type_definition";
pub const CIRCULAR_COMPUTED: &str = "circular_computed";

// C.3 Operation Errors - File Operations
pub const FILE_NOT_FOUND: &str = "file_not_found";
pub const PATH_CONFLICT: &str = "path_conflict";
pub const PATH_REQUIRED: &str = "path_required";
pub const INVALID_PATH: &str = "invalid_path";
pub const INVALID_FRONTMATTER: &str = "invalid_frontmatter";
pub const VALIDATION_FAILED: &str = "validation_failed";
pub const PERMISSION_DENIED: &str = "permission_denied";
pub const CONCURRENT_MODIFICATION: &str = "concurrent_modification";
pub const PATH_TRAVERSAL: &str = "path_traversal";

// Rename Operations
pub const RENAME_REF_UPDATE_FAILED: &str = "rename_ref_update_failed";

// Configuration Errors
pub const INVALID_CONFIG: &str = "invalid_config";
pub const MISSING_CONFIG: &str = "missing_config";
pub const UNSUPPORTED_VERSION: &str = "unsupported_version";

// C.4 Expression Errors
pub const INVALID_EXPRESSION: &str = "invalid_expression";
pub const UNKNOWN_FUNCTION: &str = "unknown_function";
pub const WRONG_ARGUMENT_COUNT: &str = "wrong_argument_count";
pub const TYPE_ERROR: &str = "type_error";
pub const EXPRESSION_DEPTH_EXCEEDED: &str = "expression_depth_exceeded";

// C.5 Formula Errors
pub const CIRCULAR_FORMULA: &str = "circular_formula";
pub const INVALID_FORMULA: &str = "invalid_formula";
pub const FORMULA_EVALUATION_ERROR: &str = "formula_evaluation_error";

/// Error severity levels (C.7)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// A structured mdbase error/issue.
#[derive(Debug, Clone)]
pub struct Issue {
    pub code: String,
    pub message: String,
    pub path: Option<String>,
    pub field: Option<String>,
    pub severity: Severity,
    pub expected: Option<serde_json::Value>,
    pub actual: Option<serde_json::Value>,
    pub type_name: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

/// Validation result for a file.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    pub valid: bool,
    pub issues: Vec<Issue>,
}

/// Top-level operation error.
#[derive(Debug, Error)]
pub enum MdbaseError {
    #[error("{code}: {message}")]
    Operation {
        code: String,
        message: String,
        details: Option<serde_json::Value>,
    },

    #[error("Validation failed")]
    Validation(ValidationResult),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Sql(#[from] rusqlite::Error),

    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
}

/// Create a JSON error response for an operation error.
pub(crate) fn op_error(code: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "error": { "code": code, "message": message }
    })
}

/// Convert an Issue to a JSON value.
pub(crate) fn issue_to_json(issue: &Issue) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "code": issue.code,
        "message": issue.message,
        "severity": match issue.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        },
    });
    if let Some(ref path) = issue.path {
        obj["path"] = serde_json::Value::String(path.clone());
    }
    if let Some(ref field) = issue.field {
        obj["field"] = serde_json::Value::String(field.clone());
    }
    if let Some(ref tn) = issue.type_name {
        obj["type"] = serde_json::Value::String(tn.clone());
    }
    obj
}
