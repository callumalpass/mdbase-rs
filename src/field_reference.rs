//! Standard field-reference access for mdbase frontmatter values.
//!
//! A field reference is either the compatible mdbase dot-path form or a
//! non-root RFC 6901 JSON Pointer, as defined by the v0.3 specification.

use serde_json::Value;

/// Resolve the first value selected by a field reference.
///
/// Legacy `[]` selectors may select several values; this function returns the
/// first. Use collection operations for semantics that consume every item.
pub fn get_value<'a>(source: &'a Value, reference: &str) -> Option<&'a Value> {
    crate::field_references::get_value(source, reference)
}

/// Assign a value through a field reference.
///
/// Missing object containers are created. Array selectors cannot be assigned,
/// and JSON Pointer traversal through an array requires an existing index.
pub fn set_value(target: &mut Value, reference: &str, value: Value) -> Result<(), String> {
    crate::field_references::set_value_with_schema(target, reference, value, None, false)
}
