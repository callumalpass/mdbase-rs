//! Type definition schema types (§5).

use std::collections::HashMap;

/// A field type definition.
#[derive(Debug, Clone)]
pub struct FieldDef {
    pub field_type: String,
    pub required: bool,
    pub default: Option<serde_json::Value>,
    pub generated: Option<GeneratedStrategy>,
    pub description: Option<String>,
    pub deprecated: Option<String>,
    pub unique: bool,
    // Constraints
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub min_length: Option<usize>,
    pub max_length: Option<usize>,
    pub pattern: Option<String>,
    pub values: Option<Vec<String>>,               // enum values
    pub items: Option<Box<FieldDef>>,              // list item type
    pub fields: Option<HashMap<String, FieldDef>>, // object fields
    pub min_items: Option<usize>,
    pub max_items: Option<usize>,
    pub list_unique: bool,             // unique constraint on list items
    pub target: Option<String>,        // link target type
    pub validate_exists: Option<bool>, // link existence validation
    pub computed: Option<String>,      // computed field expression (§5.12)
}

/// Generated field strategy (§7.15).
#[derive(Debug, Clone)]
pub enum GeneratedStrategy {
    Ulid,
    Uuid,
    Now,
    NowOnWrite,
    Sequence(i64), // start value, default 1
    Random(u64),
    Derived { from: String, transform: String },
}

/// A type definition (§5).
#[derive(Debug, Clone)]
pub struct TypeDef {
    pub name: String,
    pub description: Option<String>,
    pub extends: Option<String>,
    pub strict: Option<StrictMode>,
    pub filename_pattern: Option<String>,
    pub path_pattern: Option<String>,
    pub display_name_key: Option<String>,
    pub fields: HashMap<String, FieldDef>,
    pub match_rules: Option<MatchRules>,
}

/// Strictness mode for unknown fields.
#[derive(Debug, Clone, PartialEq)]
pub enum StrictMode {
    Off,   // false - allow unknown fields
    Warn,  // "warn" - warn about unknown fields
    Error, // true - error on unknown fields
}

/// Match rules for auto-association (§6).
#[derive(Debug, Clone)]
pub struct MatchRules {
    pub path_glob: Option<String>,
    pub fields_present: Option<Vec<String>>,
    pub where_clause: Option<serde_json::Value>,
}

impl Default for FieldDef {
    fn default() -> Self {
        Self {
            field_type: "any".to_string(),
            required: false,
            default: None,
            generated: None,
            description: None,
            deprecated: None,
            unique: false,
            min: None,
            max: None,
            min_length: None,
            max_length: None,
            pattern: None,
            values: None,
            items: None,
            fields: None,
            min_items: None,
            max_items: None,
            list_unique: false,
            target: None,
            validate_exists: None,
            computed: None,
        }
    }
}
