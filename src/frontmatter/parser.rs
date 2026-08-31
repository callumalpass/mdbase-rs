//! YAML frontmatter parsing (§3).

use std::ops::Range;

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

/// Internal parse result that borrows body text by byte range instead of owning it.
#[derive(Debug, Clone)]
pub(crate) struct ParsedDocumentLayout {
    frontmatter: Option<YamlValue>,
    body_range: Range<usize>,
    has_frontmatter: bool,
    had_bom: bool,
}

/// Structural frontmatter state shared by collection consumers.
#[derive(Debug, Clone, Copy)]
pub enum FrontmatterState<'a> {
    Absent,
    Mapping(&'a serde_yaml::Mapping),
    InvalidYaml,
    Null,
    NonMapping(&'a YamlValue),
}

fn frontmatter_state(frontmatter: Option<&YamlValue>) -> FrontmatterState<'_> {
    match frontmatter {
        None => FrontmatterState::Absent,
        Some(value) if is_parse_error(value) => FrontmatterState::InvalidYaml,
        Some(YamlValue::Mapping(mapping)) => FrontmatterState::Mapping(mapping),
        Some(YamlValue::Null) => FrontmatterState::Null,
        Some(value) => FrontmatterState::NonMapping(value),
    }
}

impl ParsedDocument {
    pub fn frontmatter_state(&self) -> FrontmatterState<'_> {
        frontmatter_state(self.frontmatter.as_ref())
    }
}

impl ParsedDocumentLayout {
    pub(crate) fn frontmatter_state(&self) -> FrontmatterState<'_> {
        frontmatter_state(self.frontmatter.as_ref())
    }

    pub(crate) fn body<'a>(&self, document: &'a str) -> &'a str {
        &document[self.body_range.clone()]
    }

    pub(crate) fn had_bom(&self) -> bool {
        self.had_bom
    }

    pub(crate) fn into_parsed_document(self, document: &str) -> ParsedDocument {
        let body = self.body(document).to_string();
        ParsedDocument {
            frontmatter: self.frontmatter,
            body,
            has_frontmatter: self.has_frontmatter,
        }
    }
}

/// Parse a markdown document into frontmatter and body.
pub fn parse_document(content: &str) -> ParsedDocument {
    parse_document_layout(content).into_parsed_document(content)
}

/// Parse while retaining encoding information needed by internal rewrite paths.
pub(crate) fn parse_document_for_rewrite(content: &str) -> (ParsedDocument, bool) {
    let layout = parse_document_layout(content);
    let had_bom = layout.had_bom();
    (layout.into_parsed_document(content), had_bom)
}

/// Parse YAML/frontmatter once while retaining body identity as a byte range.
pub(crate) fn parse_document_layout(content: &str) -> ParsedDocumentLayout {
    let had_bom = content.starts_with('\u{FEFF}');
    let bom_len = if had_bom { '\u{FEFF}'.len_utf8() } else { 0 };
    let without_bom = &content[bom_len..];

    let absent = || ParsedDocumentLayout {
        frontmatter: None,
        body_range: bom_len..content.len(),
        has_frontmatter: false,
        had_bom,
    };
    if !without_bom.starts_with("---") {
        return absent();
    }

    let first_line_end = without_bom.find('\n').unwrap_or(without_bom.len());
    if without_bom[..first_line_end].trim_end() != "---" {
        return absent();
    }
    let after_open = first_line_end + 1;
    if after_open >= without_bom.len() {
        return absent();
    }

    let rest = &without_bom[after_open..];
    let mut position = 0;
    let mut close_position = None;
    for line_with_ending in rest.split_inclusive('\n') {
        let line = line_with_ending
            .strip_suffix('\n')
            .unwrap_or(line_with_ending);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim_end() == "---" {
            close_position = Some(position);
            break;
        }
        position += line_with_ending.len();
    }
    let Some(close_position) = close_position else {
        return absent();
    };

    let yaml_source = &rest[..close_position];
    let after_close = after_open + close_position + 3;
    let suffix = &without_bom[after_close..];
    let newline_len = if suffix.starts_with("\r\n") {
        2
    } else if suffix.starts_with('\n') {
        1
    } else {
        0
    };
    let body_start = bom_len + after_close + newline_len;
    let parsed_yaml = if yaml_source.trim().is_empty() {
        Ok(YamlValue::Mapping(serde_yaml::Mapping::new()))
    } else {
        serde_yaml::from_str(yaml_source)
    };
    let frontmatter = Some(parsed_yaml.unwrap_or_else(|_| {
        YamlValue::Tagged(Box::new(serde_yaml::value::TaggedValue {
            tag: serde_yaml::value::Tag::new("!parse_error"),
            value: YamlValue::String(yaml_source.to_string()),
        }))
    }));

    ParsedDocumentLayout {
        frontmatter,
        body_range: body_start..content.len(),
        has_frontmatter: true,
        had_bom,
    }
}

pub fn is_parse_error(value: &YamlValue) -> bool {
    matches!(value, YamlValue::Tagged(t) if t.tag == serde_yaml::value::Tag::new("!parse_error"))
}

pub fn yaml_mapping_to_json(mapping: &serde_yaml::Mapping) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    for (k, v) in mapping {
        if let YamlValue::String(key) = k {
            obj.insert(key.clone(), yaml_to_json(v));
        }
    }
    serde_json::Value::Object(obj)
}

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
                    serde_json::Value::String(
                        if f.is_sign_positive() {
                            ".inf"
                        } else {
                            "-.inf"
                        }
                        .to_string(),
                    )
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

fn normalize_yaml_datetime(s: &str) -> String {
    if s.len() >= 19 {
        let bytes = s.as_bytes();
        if bytes[4] == b'-'
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

pub fn json_to_yaml_mapping(json: &serde_json::Value) -> serde_yaml::Mapping {
    let mut mapping = serde_yaml::Mapping::new();
    if let serde_json::Value::Object(obj) = json {
        for (k, v) in obj {
            mapping.insert(YamlValue::String(k.clone()), json_to_yaml(v));
        }
    }
    mapping
}

pub fn json_to_yaml(json: &serde_json::Value) -> YamlValue {
    match json {
        serde_json::Value::Null => YamlValue::Null,
        serde_json::Value::Bool(b) => YamlValue::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(YamlValue::from)
            .or_else(|| n.as_f64().map(YamlValue::from))
            .unwrap_or(YamlValue::Null),
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
    use super::{parse_document, parse_document_layout, FrontmatterState};

    #[test]
    fn layout_borrows_body_and_public_parse_stays_owned() {
        let source = "\u{feff}---\ntitle: Original\n---\nBody\n";
        let layout = parse_document_layout(source);
        assert!(layout.had_bom());
        assert!(layout.has_frontmatter);
        assert!(matches!(
            layout.frontmatter_state(),
            FrontmatterState::Mapping(_)
        ));
        assert_eq!(layout.body(source), "Body\n");
        let body = layout.body(source);
        let body_start = source.find("Body").unwrap();
        assert!(std::ptr::eq(body.as_ptr(), source[body_start..].as_ptr()));
        assert_eq!(parse_document(source).body, "Body\n");
    }

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
        assert_eq!(
            parsed.frontmatter.as_ref().and_then(YamlValueExt::title),
            Some("Windows")
        );
        assert_eq!(parsed.body, "Body\r\n");
    }

    trait YamlValueExt {
        fn title(&self) -> Option<&str>;
    }
    impl YamlValueExt for serde_yaml::Value {
        fn title(&self) -> Option<&str> {
            self.as_mapping()?
                .get(serde_yaml::Value::String("title".to_string()))?
                .as_str()
        }
    }

    #[test]
    fn leading_bom_is_stripped_from_body() {
        for source in [
            "\u{feff}---\ntitle: Original\n---\nBody\n",
            "\u{feff}Body only, no frontmatter.\n",
        ] {
            assert!(!parse_document(source).body.starts_with('\u{feff}'));
        }
    }

    #[test]
    fn bom_before_a_later_horizontal_rule_stays_body_only() {
        let source = "\u{feff}Intro\n\n---\nnot frontmatter\n";
        let parsed = parse_document(source);
        assert!(!parsed.has_frontmatter);
        assert_eq!(parsed.body, source.strip_prefix('\u{feff}').unwrap());
    }
}
