//! Frontmatter serialization (§3.4).

use serde_yaml::Value as YamlValue;
use thiserror::Error;

/// An error returned when YAML frontmatter cannot be emitted.
#[derive(Debug, Error)]
#[error("failed to serialize YAML frontmatter: {source}")]
pub struct FrontmatterSerializationError {
    #[source]
    source: serde_yaml::Error,
}

impl From<serde_yaml::Error> for FrontmatterSerializationError {
    fn from(source: serde_yaml::Error) -> Self {
        Self { source }
    }
}

/// Serialize a record document.
///
/// Records with no persisted fields are ordinary body-only Markdown files.
/// Frontmatter delimiters are only emitted when the mapping contains fields.
/// Returns [`FrontmatterSerializationError`] when `serde_yaml` cannot emit an
/// authored YAML value.
pub fn serialize_document(
    frontmatter: &serde_yaml::Mapping,
    body: &str,
) -> Result<String, FrontmatterSerializationError> {
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
/// Returns [`FrontmatterSerializationError`] rather than panicking or
/// substituting empty frontmatter when the YAML emitter rejects a value.
pub fn serialize_document_with_bom(
    had_bom: bool,
    frontmatter: &serde_yaml::Mapping,
    body: &str,
) -> Result<String, FrontmatterSerializationError> {
    let doc = serialize_inner(frontmatter, body)?;
    if had_bom {
        let mut out = String::with_capacity('\u{FEFF}'.len_utf8() + doc.len());
        out.push('\u{FEFF}');
        out.push_str(&doc);
        Ok(out)
    } else {
        Ok(doc)
    }
}

fn serialize_inner(
    frontmatter: &serde_yaml::Mapping,
    body: &str,
) -> Result<String, FrontmatterSerializationError> {
    if frontmatter.is_empty() {
        return Ok(body.to_string());
    }

    let yaml_str = serde_yaml::to_string(&YamlValue::Mapping(frontmatter.clone()))?;
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
    Ok(result)
}

/// Reconcile JSON object fields into an authored YAML mapping.
///
/// Non-string keys and unchanged tagged values are retained so a rewrite never
/// silently normalizes away authored YAML that the JSON projection cannot
/// represent. Changed string fields use the canonical JSON-to-YAML conversion.
pub(crate) fn reconcile_json_object(
    authored: &serde_yaml::Mapping,
    fields: &serde_json::Map<String, serde_json::Value>,
) -> serde_yaml::Mapping {
    let mut result = authored.clone();
    result.retain(|key, _| match key {
        YamlValue::String(key) => fields.contains_key(key),
        _ => true,
    });
    for (key, value) in fields {
        let yaml_key = YamlValue::String(key.clone());
        let unchanged = result
            .get(&yaml_key)
            .is_some_and(|authored| super::parser::yaml_to_json(authored) == *value);
        if !unchanged {
            result.insert(yaml_key, super::parser::json_to_yaml(value));
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
            assert_eq!(serialize_document(&Mapping::new(), body).unwrap(), body);
        }
    }

    #[test]
    fn non_empty_frontmatter_uses_yaml_delimiters() {
        let mut frontmatter = Mapping::new();
        frontmatter.insert(Value::String("title".into()), Value::String("Note".into()));

        assert_eq!(
            serialize_document(&frontmatter, "# Body").unwrap(),
            "---\ntitle: Note\n---\n# Body\n"
        );
    }

    pub(crate) const UNEMITTABLE_TAGGED_COMPLEX_MAPPING: &str =
        "? !key\n  nested: key\n: !value\n  nested: value\n";

    #[test]
    fn tagged_complex_mapping_key_and_value_returns_emitter_error() {
        let Value::Mapping(frontmatter) =
            serde_yaml::from_str::<Value>(UNEMITTABLE_TAGGED_COMPLEX_MAPPING).unwrap()
        else {
            panic!("fixture must be a mapping");
        };

        assert!(serialize_document(&frontmatter, "Body\n").is_err());
        assert!(serialize_document_with_bom(true, &frontmatter, "Body\n").is_err());
    }

    #[test]
    fn bom_policy_preserves_leading_bom_on_serialization() {
        // Documents that originally carried a UTF-8 BOM keep it on write;
        // BOM-less documents never gain one.
        let mut frontmatter = Mapping::new();
        frontmatter.insert(Value::String("title".into()), Value::String("Note".into()));

        assert_eq!(
            serialize_document_with_bom(true, &frontmatter, "# Body\n").unwrap(),
            "\u{feff}---\ntitle: Note\n---\n# Body\n"
        );
        assert_eq!(
            serialize_document_with_bom(false, &frontmatter, "# Body\n").unwrap(),
            "---\ntitle: Note\n---\n# Body\n"
        );
        // Body-only records restore the marker without inventing frontmatter.
        assert_eq!(
            serialize_document_with_bom(true, &Mapping::new(), "# Body\n").unwrap(),
            "\u{feff}# Body\n"
        );
    }
}
