//! Frontmatter serialization (§3.4).

use serde_yaml::Value as YamlValue;

/// Serialize a record document.
///
/// Records with no persisted fields are ordinary body-only Markdown files.
/// Frontmatter delimiters are only emitted when the mapping contains fields.
pub fn serialize_document(frontmatter: &serde_yaml::Mapping, body: &str) -> String {
    if frontmatter.is_empty() {
        return body.to_string();
    }

    let yaml_str =
        serde_yaml::to_string(&YamlValue::Mapping(frontmatter.clone())).unwrap_or_default();
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

#[cfg(test)]
mod tests {
    use super::serialize_document;
    use serde_yaml::{Mapping, Value};

    #[test]
    fn empty_frontmatter_preserves_body_only_markdown_exactly() {
        for body in [
            "",
            "# Note",
            "# Note\n",
            "---\nA horizontal rule without a closing fence",
        ] {
            assert_eq!(serialize_document(&Mapping::new(), body), body);
        }
    }

    #[test]
    fn non_empty_frontmatter_uses_yaml_delimiters() {
        let mut frontmatter = Mapping::new();
        frontmatter.insert(Value::String("title".into()), Value::String("Note".into()));

        assert_eq!(
            serialize_document(&frontmatter, "# Body"),
            "---\ntitle: Note\n---\n# Body\n"
        );
    }
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
