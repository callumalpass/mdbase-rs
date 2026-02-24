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
}

/// Parse a markdown document into frontmatter and body.
///
/// Returns the raw YAML value for frontmatter (which may be a mapping, list, scalar, or null)
/// and the body string. Callers must check that frontmatter is a mapping.
pub fn parse_document(content: &str) -> ParsedDocument {
    // §3.1: Opening --- must be the very first line
    if !content.starts_with("---") {
        return ParsedDocument {
            frontmatter: None,
            body: content.to_string(),
            has_frontmatter: false,
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
        };
    }

    let rest = &content[after_open..];
    // Search for a line that is exactly "---"
    let mut pos = 0;
    let mut found_close = None;
    for line in rest.lines() {
        if line.trim_end() == "---" {
            found_close = Some(pos);
            break;
        }
        pos += line.len() + 1; // +1 for the newline
    }

    let close_pos = match found_close {
        Some(p) => p,
        None => {
            // No closing delimiter found - treat entire content as body (no frontmatter)
            return ParsedDocument {
                frontmatter: None,
                body: content.to_string(),
                has_frontmatter: false,
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
            };
        }
    };

    ParsedDocument {
        frontmatter: Some(yaml_value),
        body,
        has_frontmatter: true,
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
