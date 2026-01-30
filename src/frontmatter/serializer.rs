//! Frontmatter serialization (§3.4).

use serde_yaml::Value as YamlValue;

/// Serialize a YAML mapping to a frontmatter string (with --- delimiters).
pub fn serialize_document(frontmatter: &serde_yaml::Mapping, body: &str) -> String {
    let yaml_str = serde_yaml::to_string(&YamlValue::Mapping(frontmatter.clone()))
        .unwrap_or_default();
    let mut result = String::from("---\n");
    result.push_str(&yaml_str);
    if !yaml_str.ends_with('\n') {
        result.push('\n');
    }
    result.push_str("---\n");
    if !body.is_empty() {
        result.push_str(body);
        if !body.ends_with('\n') {
            result.push('\n');
        }
    }
    result
}

/// Merge updated fields into an existing YAML mapping.
/// If write_nulls is "omit", null values cause the field to be removed.
pub fn merge_fields(
    existing: &serde_yaml::Mapping,
    updates: &serde_json::Value,
    write_nulls: &str,
) -> serde_yaml::Mapping {
    let mut result = existing.clone();

    if let serde_json::Value::Object(fields) = updates {
        for (key, value) in fields {
            let yaml_key = YamlValue::String(key.clone());
            if value.is_null() && write_nulls == "omit" {
                result.remove(&yaml_key);
            } else {
                result.insert(yaml_key, super::parser::json_to_yaml(value));
            }
        }
    }

    result
}
