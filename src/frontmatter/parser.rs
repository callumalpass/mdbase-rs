//! YAML frontmatter parsing (§3).

use serde_yaml::Value as YamlValue;

/// Parsed frontmatter result.
#[derive(Debug, Clone)]
pub struct ParsedDocument {
    /// The frontmatter as a YAML mapping, or None if no frontmatter delimiters found.
    pub frontmatter: Option<YamlValue>,
    /// The body content (everything after the closing ---).
    pub body: String,
    /// Whether the document had frontmatter delimiters.
    pub has_frontmatter: bool,
    /// Whether the original content began with a UTF-8 BOM (U+FEFF).
    ///
    /// BOM write policy: a leading BOM is transparent to parsing but is
    /// preserved on serialization — callers that rewrite a document read with
    /// this parser should re-prepend the BOM (see
    /// `serializer::serialize_document_with_bom`) so external tools that
    /// emitted it keep seeing byte-stable files.
    pub had_bom: bool,
}

/// Structural frontmatter state shared by collection consumers.
///
/// Parsing is deliberately separate from policy: queries may omit opaque
/// records, validation may diagnose them, and snapshots may preserve them.
/// Consumers should match this enum instead of interpreting the YAML error
/// sentinel themselves.
#[derive(Debug, Clone, Copy)]
pub enum FrontmatterState<'a> {
    /// The document has no complete leading frontmatter block.
    Absent,
    /// The document has canonical object frontmatter.
    Mapping(&'a serde_yaml::Mapping),
    /// The frontmatter block is not valid YAML.
    InvalidYaml,
    /// The frontmatter block explicitly contains YAML null.
    Null,
    /// The frontmatter block is valid YAML but is not an object.
    NonMapping(&'a YamlValue),
}

impl ParsedDocument {
    /// Classify frontmatter once so callers only choose policy.
    pub fn frontmatter_state(&self) -> FrontmatterState<'_> {
        match self.frontmatter.as_ref() {
            None => FrontmatterState::Absent,
            Some(value) if is_parse_error(value) => FrontmatterState::InvalidYaml,
            Some(YamlValue::Mapping(mapping)) => FrontmatterState::Mapping(mapping),
            Some(YamlValue::Null) => FrontmatterState::Null,
            Some(value) => FrontmatterState::NonMapping(value),
        }
    }
}

/// Parse a markdown document into frontmatter and body.
///
/// Returns the raw YAML value for frontmatter (which may be a mapping, list, scalar, or null)
/// and the body string. Callers must check that frontmatter is a mapping.
pub fn parse_document(content: &str) -> ParsedDocument {
    // A single leading UTF-8 BOM (U+FEFF) is an encoding marker emitted by
    // some editors/platforms, not document content. Strip exactly one before
    // the delimiter check so BOM'd frontmatter documents parse normally, and
    // remember it so writers can restore the byte prefix.
    let had_bom = content.starts_with('\u{FEFF}');
    let content = if had_bom {
        &content['\u{FEFF}'.len_utf8()..]
    } else {
        content
    };

    // §3.1: Opening --- must be the very first line
    if !content.starts_with("---") {
        return ParsedDocument {
            frontmatter: None,
            body: content.to_string(),
            has_frontmatter: false,
            had_bom,
        };
    }

    // Check that the first line is exactly "---" (possibly with trailing whitespace/newline)
    let first_line_end = content.find('\n').unwrap_or(content.len());
    let first_line = content[..first_line_end].trim_end();
    if first_line != "---" {
        return ParsedDocument {
            frontmatter: None,
            body: content.to_string(),
            has_frontmatter: false,
            had_bom,
        };
    }

    // Find the closing ---
    let after_open = first_line_end + 1;
    if after_open >= content.len() {
        // File is just "---\n" with nothing after
        return ParsedDocument {
            frontmatter: None,
            body: content.to_string(),
            has_frontmatter: false,
            had_bom,
        };
    }

    let rest = &content[after_open..];
    // Search for a line that is exactly "---"
    let mut pos = 0;
    let mut found_close = None;
    for line_with_ending in rest.split_inclusive('\n') {
        let line = line_with_ending
            .strip_suffix('\n')
            .unwrap_or(line_with_ending);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim_end() == "---" {
            found_close = Some(pos);
            break;
        }
        pos += line_with_ending.len();
    }

    let close_pos = match found_close {
        Some(p) => p,
        None => {
            // No closing delimiter found - treat entire content as body (no frontmatter)
            return ParsedDocument {
                frontmatter: None,
                body: content.to_string(),
                has_frontmatter: false,
                had_bom,
            };
        }
    };

    let yaml_str = &rest[..close_pos];
    let body_start = after_open + close_pos + 3; // skip "---"
                                                 // Skip the newline after closing ---
    let body = if body_start < content.len() {
        let b = &content[body_start..];
        if let Some(stripped) = b.strip_prefix('\n') {
            stripped.to_string()
        } else if let Some(stripped) = b.strip_prefix("\r\n") {
            stripped.to_string()
        } else {
            b.to_string()
        }
    } else {
        String::new()
    };

    // Parse the YAML content
    let yaml_value: YamlValue = match serde_yaml::from_str(yaml_str) {
        Ok(v) => v,
        Err(_) => {
            // Return the raw yaml string as an error indicator - caller handles this
            return ParsedDocument {
                frontmatter: Some(YamlValue::Tagged(Box::new(
                    serde_yaml::value::TaggedValue {
                        tag: serde_yaml::value::Tag::new("!parse_error"),
                        value: YamlValue::String(yaml_str.to_string()),
                    },
                ))),
                body,
                has_frontmatter: true,
                had_bom,
            };
        }
    };
    // Obsidian permits an explicit but empty frontmatter block. It has the
    // same persisted fields as a body-only record, not scalar frontmatter.
    let yaml_value = if yaml_str.trim().is_empty() {
        YamlValue::Mapping(serde_yaml::Mapping::new())
    } else {
        yaml_value
    };

    ParsedDocument {
        frontmatter: Some(yaml_value),
        body,
        has_frontmatter: true,
        had_bom,
    }
}

/// Check if a parsed YAML value represents a parse error (our sentinel).
pub fn is_parse_error(value: &YamlValue) -> bool {
    matches!(value, YamlValue::Tagged(t) if t.tag == serde_yaml::value::Tag::new("!parse_error"))
}

/// Convert a YAML mapping to a JSON object.
pub fn yaml_mapping_to_json(mapping: &serde_yaml::Mapping) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    for (k, v) in mapping {
        if let YamlValue::String(key) = k {
            obj.insert(key.clone(), yaml_to_json(v));
        }
    }
    serde_json::Value::Object(obj)
}

/// Convert a serde_yaml::Value to serde_json::Value.
pub fn yaml_to_json(yaml: &YamlValue) -> serde_json::Value {
    match yaml {
        YamlValue::Null => serde_json::Value::Null,
        YamlValue::Bool(b) => serde_json::Value::Bool(*b),
        YamlValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_json::Value::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                serde_json::Value::Number(u.into())
            } else if let Some(f) = n.as_f64() {
                if f.is_infinite() {
                    // Preserve infinity as string for validation
                    if f.is_sign_positive() {
                        serde_json::Value::String(".inf".to_string())
                    } else {
                        serde_json::Value::String("-.inf".to_string())
                    }
                } else if f.is_nan() {
                    serde_json::Value::String(".nan".to_string())
                } else {
                    serde_json::Number::from_f64(f)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null)
                }
            } else {
                serde_json::Value::Null
            }
        }
        YamlValue::String(s) => serde_json::Value::String(normalize_yaml_datetime(s)),
        YamlValue::Sequence(seq) => {
            serde_json::Value::Array(seq.iter().map(yaml_to_json).collect())
        }
        YamlValue::Mapping(map) => yaml_mapping_to_json(map),
        YamlValue::Tagged(tagged) => yaml_to_json(&tagged.value),
    }
}

/// Normalize YAML datetime strings to ISO 8601 format.
/// YAML timestamps like "2024-03-15 10:30:00" should become "2024-03-15T10:30:00".
fn normalize_yaml_datetime(s: &str) -> String {
    // Match pattern: YYYY-MM-DD HH:MM:SS (with optional timezone)
    // The space between date and time should be replaced with 'T'
    if s.len() >= 19 {
        let bytes = s.as_bytes();
        // Check for YYYY-MM-DD HH:MM:SS pattern
        if bytes.len() >= 19
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes[10] == b' '
            && bytes[13] == b':'
            && bytes[16] == b':'
            && bytes[0..4].iter().all(|b| b.is_ascii_digit())
            && bytes[5..7].iter().all(|b| b.is_ascii_digit())
            && bytes[8..10].iter().all(|b| b.is_ascii_digit())
            && bytes[11..13].iter().all(|b| b.is_ascii_digit())
            && bytes[14..16].iter().all(|b| b.is_ascii_digit())
            && bytes[17..19].iter().all(|b| b.is_ascii_digit())
        {
            let mut normalized = String::with_capacity(s.len());
            normalized.push_str(&s[..10]);
            normalized.push('T');
            normalized.push_str(&s[11..]);
            return normalized;
        }
    }
    s.to_string()
}

/// Convert a JSON value to a YAML mapping for writing.
pub fn json_to_yaml_mapping(json: &serde_json::Value) -> serde_yaml::Mapping {
    let mut mapping = serde_yaml::Mapping::new();
    if let serde_json::Value::Object(obj) = json {
        for (k, v) in obj {
            mapping.insert(YamlValue::String(k.clone()), json_to_yaml(v));
        }
    }
    mapping
}

/// Convert a serde_json::Value to serde_yaml::Value.
pub fn json_to_yaml(json: &serde_json::Value) -> YamlValue {
    match json {
        serde_json::Value::Null => YamlValue::Null,
        serde_json::Value::Bool(b) => YamlValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                YamlValue::Number(serde_yaml::Number::from(i))
            } else if let Some(f) = n.as_f64() {
                YamlValue::Number(serde_yaml::Number::from(f))
            } else {
                YamlValue::Null
            }
        }
        serde_json::Value::String(s) => YamlValue::String(s.clone()),
        serde_json::Value::Array(arr) => {
            YamlValue::Sequence(arr.iter().map(json_to_yaml).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut mapping = serde_yaml::Mapping::new();
            for (k, v) in obj {
                mapping.insert(YamlValue::String(k.clone()), json_to_yaml(v));
            }
            YamlValue::Mapping(mapping)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_document;

    #[test]
    fn empty_explicit_frontmatter_is_an_empty_mapping() {
        for source in ["---\n---\nBody", "---\n \n---\nBody"] {
            let parsed = parse_document(source);
            assert!(parsed.has_frontmatter);
            assert!(parsed.frontmatter.is_some_and(|value| value.is_mapping()));
            assert_eq!(parsed.body, "Body");
        }
    }

    #[test]
    fn an_unclosed_opening_fence_is_body_only_markdown() {
        let source = "---\nThis is a horizontal rule followed by ordinary Markdown.";
        let parsed = parse_document(source);
        assert!(!parsed.has_frontmatter);
        assert!(parsed.frontmatter.is_none());
        assert_eq!(parsed.body, source);
    }

    #[test]
    fn parses_crlf_frontmatter_without_shifting_the_closing_delimiter() {
        let parsed = parse_document("---\r\ntitle: Windows\r\ncount: 2\r\n---\r\nBody\r\n");
        let frontmatter = parsed
            .frontmatter
            .expect("CRLF frontmatter should parse")
            .as_mapping()
            .expect("frontmatter should be a mapping")
            .clone();

        assert_eq!(
            frontmatter
                .get(serde_yaml::Value::String("title".to_string()))
                .and_then(serde_yaml::Value::as_str),
            Some("Windows")
        );
        assert_eq!(parsed.body, "Body\r\n");
    }

    #[test]
    fn leading_bom_is_stripped_and_recorded_not_treated_as_content() {
        // BOM write policy: strip exactly one leading U+FEFF at parse time and
        // record it so serialization can restore the byte prefix.
        for source in [
            "\u{feff}---\ntitle: Original\n---\nBody\n",
            "\u{feff}Body only, no frontmatter.\n",
        ] {
            let parsed = parse_document(source);
            assert!(parsed.had_bom, "{source:?} must record had_bom");
            assert!(
                !parsed.body.starts_with('\u{feff}'),
                "BOM must not leak into body"
            );
        }

        let parsed = parse_document("\u{feff}---\ntitle: Original\n---\nBody\n");
        assert!(parsed.has_frontmatter);
        assert_eq!(parsed.body, "Body\n");

        // No BOM in, no BOM recorded.
        let parsed = parse_document("---\ntitle: Original\n---\nBody\n");
        assert!(!parsed.had_bom);
    }

    #[test]
    fn bom_before_a_later_horizontal_rule_stays_body_only() {
        let source = "\u{feff}Intro\n\n---\nnot frontmatter\n";
        let parsed = parse_document(source);
        assert!(!parsed.has_frontmatter);
        assert!(parsed.had_bom);
        assert_eq!(parsed.body, source.strip_prefix('\u{feff}').unwrap());
    }
}
