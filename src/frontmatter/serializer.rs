//! Frontmatter serialization (§3.4).

use serde_yaml::Value as YamlValue;

/// Serialize a record document.
///
/// Records with no persisted fields are ordinary body-only Markdown files.
/// Frontmatter delimiters are only emitted when the mapping contains fields.
pub fn serialize_document(frontmatter: &serde_yaml::Mapping, body: &str) -> String {
    serialize_document_with_bom(false, frontmatter, body)
}

/// Serialize a record document, restoring a leading UTF-8 BOM.
///
/// BOM write policy: parsing strips one leading `U+FEFF` so BOM'd documents
/// classify as frontmatter records; serialization re-prepends the byte when
/// the original content had it. This keeps round-trips byte-stable apart from
/// the intended edit, which minimizes diffs for external tools that rely on
/// the marker (e.g. Windows editors/PowerShell).
///
/// Records with no persisted fields remain body-only files; with `had_bom`
/// the BOM is still restored so body-only documents round-trip unchanged too.
pub fn serialize_document_with_bom(
    had_bom: bool,
    frontmatter: &serde_yaml::Mapping,
    body: &str,
) -> String {
    let doc = serialize_inner(frontmatter, body);
    if had_bom {
        let mut out = String::with_capacity('\u{FEFF}'.len_utf8() + doc.len());
        out.push('\u{FEFF}');
        out.push_str(&doc);
        out
    } else {
        doc
    }
}

fn serialize_inner(frontmatter: &serde_yaml::Mapping, body: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::{serialize_document, serialize_document_with_bom};
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

    #[test]
    fn bom_policy_preserves_leading_bom_on_serialization() {
        // Documents that originally carried a UTF-8 BOM keep it on write;
        // BOM-less documents never gain one.
        let mut frontmatter = Mapping::new();
        frontmatter.insert(Value::String("title".into()), Value::String("Note".into()));

        assert_eq!(
            serialize_document_with_bom(true, &frontmatter, "# Body\n"),
            "\u{feff}---\ntitle: Note\n---\n# Body\n"
        );
        assert_eq!(
            serialize_document_with_bom(false, &frontmatter, "# Body\n"),
            "---\ntitle: Note\n---\n# Body\n"
        );
        // Body-only records restore the marker without inventing frontmatter.
        assert_eq!(
            serialize_document_with_bom(true, &Mapping::new(), "# Body\n"),
            "\u{feff}# Body\n"
        );
    }
}
