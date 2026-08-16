//! Provider-neutral structural facts extracted from one exact Markdown record.
//!
//! This module intentionally does not resolve links against a collection.  A
//! hosted authority can persist this value and resolve the occurrences against
//! a catalogue snapshot later, while a filesystem consumer can use the same
//! extraction result.  Exact Markdown and body prose never enter the value.
//!
//! Body link targets and resolution syntax are retained, while body labels and
//! complete source spellings are redacted before serialization and digesting.
//! Exact mutation planning reparses the encrypted authority when source text is
//! required. Frontmatter source values remain readable by architecture.

use std::cmp::Ordering;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::expressions::evaluator::{
    extract_embeds_from_body, extract_links_from_body, extract_tags_from_body,
    strip_code_blocks_and_inline_code,
};
use crate::frontmatter::parser::{parse_document, yaml_to_json, FrontmatterState};
use crate::links::parser::normalize_link_path;

/// Version of the provider-neutral structural envelope.
pub const RECORD_STRUCTURE_SCHEMA_VERSION: &str = "mdbase-record-structure-v3";

/// Where an occurrence was found in the exact record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuralSourceKind {
    Frontmatter,
    Body,
}

/// Syntax of an outgoing structural occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum StructuralLinkKind {
    #[serde(rename = "wikilink")]
    Wikilink,
    #[serde(rename = "markdown")]
    MarkdownLink,
    #[serde(rename = "embed")]
    WikilinkEmbed,
    #[serde(rename = "image")]
    MarkdownImage,
    /// A frontmatter value that existing mdbase semantics treat as a path.
    #[serde(rename = "path")]
    Path,
}

/// Resolution is deliberately explicit even though this parser has no
/// catalogue context.  `Unresolved` means extraction succeeded and a later
/// resolver may classify the target as resolved, missing, or ambiguous.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuralResolution {
    #[default]
    Unresolved,
    Resolved,
    Missing,
    Ambiguous,
    UnsafeTraversal,
    External,
    Malformed,
}

/// One preserved outgoing link or embed occurrence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuralOccurrence {
    /// Stable position after canonical sorting; distinguishes repeated edges.
    pub ordinal: usize,
    pub source_kind: StructuralSourceKind,
    pub kind: StructuralLinkKind,
    /// Complete source spelling for frontmatter occurrences; empty for body.
    pub raw: String,
    /// Target spelling before anchor removal and path normalization. For body
    /// occurrences this is reduced to the target itself and never includes a
    /// Markdown destination title or visible label.
    pub raw_target: String,
    /// Lexically normalized collection-relative target, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    /// Frontmatter property path for frontmatter occurrences; absent in body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub relative: bool,
    #[serde(default)]
    pub resolution: StructuralResolution,
}

/// Structural facts for a canonical relative record path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordStructure {
    pub schema_version: String,
    pub path: String,
    pub occurrences: Vec<StructuralOccurrence>,
    /// Body tags use the same rules as query and expression semantics.
    pub body_tags: Vec<String>,
    /// Ordered query-visible body links with aliases and anchors removed by the
    /// canonical v0.3 extractor.
    pub body_links: Vec<String>,
    /// Ordered query-visible embed targets from wikilinks and Markdown images.
    pub body_embeds: Vec<String>,
    /// Digest of this envelope with `structural_digest` set to an empty string.
    pub structural_digest: String,
}

/// Compatibility aliases for consumers that name the envelope/model directly.
pub type RecordStructureModel = RecordStructure;
pub type RecordStructureLink = StructuralOccurrence;

/// Parse exact Markdown into provider-neutral structural facts.
pub fn parse_record_structure(path: &str, document: &str) -> RecordStructure {
    RecordStructureParser::new(path).parse(document)
}

/// Parser carrying the source path needed for relative target normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordStructureParser {
    path: String,
}

impl RecordStructureParser {
    pub fn new(path: impl Into<String>) -> Self {
        Self { path: path.into() }
    }

    pub fn parse(&self, document: &str) -> RecordStructure {
        let parsed = parse_document(document);
        let mut occurrences = Vec::new();

        if let FrontmatterState::Mapping(mapping) = parsed.frontmatter_state() {
            collect_frontmatter_occurrences(
                &yaml_to_json(&serde_yaml::Value::Mapping(mapping.clone())),
                &mut occurrences,
                &self.path,
                None,
            );
        }

        // Use the established code/fence semantics before structural scanning.
        // The scanner itself still records malformed constructs outside code.
        let body = strip_code_blocks_and_inline_code(&parsed.body);
        let mut body_occurrences = scan_occurrences(&body, StructuralSourceKind::Body, &self.path);
        body_occurrences.iter_mut().for_each(redact_body_occurrence);
        occurrences.extend(body_occurrences);

        occurrences.sort_by(compare_occurrences);
        for (ordinal, occurrence) in occurrences.iter_mut().enumerate() {
            occurrence.ordinal = ordinal;
        }
        let body_tags = extract_tags_from_body(&parsed.body);
        let body_links = extract_links_from_body(&parsed.body);
        let body_embeds = extract_embeds_from_body(&parsed.body);

        let mut structure = RecordStructure {
            schema_version: RECORD_STRUCTURE_SCHEMA_VERSION.to_string(),
            path: self.path.clone(),
            occurrences,
            body_tags,
            body_links,
            body_embeds,
            structural_digest: String::new(),
        };
        structure.structural_digest = digest_structure(&structure);
        structure
    }
}

fn collect_frontmatter_occurrences(
    value: &Value,
    output: &mut Vec<StructuralOccurrence>,
    path: &str,
    field: Option<String>,
) {
    match value {
        Value::String(value) => {
            let mut found = scan_occurrences(value, StructuralSourceKind::Frontmatter, path);
            for occurrence in &mut found {
                occurrence.field = field.clone();
            }
            if found.is_empty()
                && !value.trim().is_empty()
                && !is_external_target(value.trim())
                && (value.contains('/') || value.contains('.'))
            {
                let mut occurrence = parse_target_occurrence(
                    StructuralSourceKind::Frontmatter,
                    StructuralLinkKind::Path,
                    value.trim().to_string(),
                    value.trim().to_string(),
                    path,
                );
                occurrence.field = field;
                output.push(occurrence);
            } else {
                output.extend(found);
            }
        }
        Value::Array(values) => values
            .iter()
            .for_each(|value| collect_frontmatter_occurrences(value, output, path, field.clone())),
        Value::Object(values) => values.iter().for_each(|(key, value)| {
            let nested_field = match &field {
                Some(parent) => format!("{parent}.{key}"),
                None => key.clone(),
            };
            collect_frontmatter_occurrences(value, output, path, Some(nested_field));
        }),
        _ => {}
    }
}

fn scan_occurrences(
    text: &str,
    source_kind: StructuralSourceKind,
    path: &str,
) -> Vec<StructuralOccurrence> {
    let chars: Vec<char> = text.chars().collect();
    let mut output = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '\\' {
            index = index.saturating_add(2);
            continue;
        }

        if chars.get(index..index + 3) == Some(&['!', '[', '[']) {
            let start = index;
            index += 3;
            if let Some(end) = find_closing(&chars, index, ']', ']') {
                let inner: String = chars[index..end].iter().collect();
                output.push(parse_target_occurrence(
                    source_kind,
                    StructuralLinkKind::WikilinkEmbed,
                    chars[start..end + 2].iter().collect(),
                    inner,
                    path,
                ));
                index = end + 2;
            } else {
                output.push(malformed_occurrence(
                    source_kind,
                    StructuralLinkKind::WikilinkEmbed,
                    chars[start..].iter().collect(),
                ));
                break;
            }
            continue;
        }

        if chars.get(index..index + 2) == Some(&['[', '[']) {
            let start = index;
            index += 2;
            if let Some(end) = find_closing(&chars, index, ']', ']') {
                let inner: String = chars[index..end].iter().collect();
                output.push(parse_target_occurrence(
                    source_kind,
                    StructuralLinkKind::Wikilink,
                    chars[start..end + 2].iter().collect(),
                    inner,
                    path,
                ));
                index = end + 2;
            } else {
                output.push(malformed_occurrence(
                    source_kind,
                    StructuralLinkKind::Wikilink,
                    chars[start..].iter().collect(),
                ));
                break;
            }
            continue;
        }

        if chars[index] == '[' && (index == 0 || chars[index - 1] != '!') {
            let start = index;
            if let Some(bracket_end) = find_balanced(&chars, index + 1, '[', ']') {
                if chars.get(bracket_end + 1) == Some(&'(') {
                    if let Some(paren_end) = find_balanced(&chars, bracket_end + 2, '(', ')') {
                        let target: String = chars[bracket_end + 2..paren_end].iter().collect();
                        output.push(parse_target_occurrence_with_alias(
                            source_kind,
                            StructuralLinkKind::MarkdownLink,
                            chars[start..paren_end + 1].iter().collect(),
                            target,
                            path,
                            Some(chars[start + 1..bracket_end].iter().collect()),
                        ));
                        index = paren_end + 1;
                        continue;
                    }
                    output.push(malformed_occurrence(
                        source_kind,
                        StructuralLinkKind::MarkdownLink,
                        chars[start..].iter().collect(),
                    ));
                    break;
                }
            }
        }

        if chars[index] == '!' && chars.get(index + 1) == Some(&'[') {
            let start = index;
            if let Some(bracket_end) = find_balanced(&chars, index + 2, '[', ']') {
                if chars.get(bracket_end + 1) == Some(&'(') {
                    if let Some(paren_end) = find_balanced(&chars, bracket_end + 2, '(', ')') {
                        let target: String = chars[bracket_end + 2..paren_end].iter().collect();
                        output.push(parse_target_occurrence_with_alias(
                            source_kind,
                            StructuralLinkKind::MarkdownImage,
                            chars[start..paren_end + 1].iter().collect(),
                            target,
                            path,
                            Some(chars[start + 2..bracket_end].iter().collect()),
                        ));
                        index = paren_end + 1;
                        continue;
                    }
                    output.push(malformed_occurrence(
                        source_kind,
                        StructuralLinkKind::MarkdownImage,
                        chars[start..].iter().collect(),
                    ));
                    break;
                }
            }
        }

        index += 1;
    }
    output
}

fn parse_target_occurrence(
    source_kind: StructuralSourceKind,
    kind: StructuralLinkKind,
    raw: String,
    inner: String,
    path: &str,
) -> StructuralOccurrence {
    parse_target_occurrence_with_alias(source_kind, kind, raw, inner, path, None)
}

fn parse_target_occurrence_with_alias(
    source_kind: StructuralSourceKind,
    kind: StructuralLinkKind,
    raw: String,
    inner: String,
    path: &str,
    explicit_alias: Option<String>,
) -> StructuralOccurrence {
    let (mut target_part, alias) = if matches!(
        kind,
        StructuralLinkKind::Wikilink | StructuralLinkKind::WikilinkEmbed
    ) {
        match inner.find('|') {
            Some(index) => (
                inner[..index].trim().to_string(),
                Some(inner[index + 1..].to_string()),
            ),
            None => (inner.trim().to_string(), explicit_alias),
        }
    } else {
        (inner.trim().to_string(), explicit_alias)
    };
    if matches!(
        kind,
        StructuralLinkKind::MarkdownLink | StructuralLinkKind::MarkdownImage
    ) {
        let Some(destination) = markdown_destination(&target_part) else {
            return malformed_occurrence(source_kind, kind, raw);
        };
        target_part = destination;
    }
    let anchor = target_part
        .find('#')
        .map(|index| target_part[index + 1..].to_string());
    make_occurrence(source_kind, kind, raw, target_part, alias, anchor, path)
}

fn markdown_destination(value: &str) -> Option<String> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('<') {
        return rest
            .split_once('>')
            .map(|(destination, _)| destination.to_string());
    }
    Some(
        value
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string(),
    )
}

fn make_occurrence(
    source_kind: StructuralSourceKind,
    kind: StructuralLinkKind,
    raw: String,
    raw_target: String,
    alias: Option<String>,
    anchor: Option<String>,
    path: &str,
) -> StructuralOccurrence {
    let relative = raw_target.starts_with("./") || raw_target.starts_with("../");
    let target = raw_target
        .split('#')
        .next()
        .unwrap_or(raw_target.as_str())
        .trim()
        .to_string();
    let mut occurrence = StructuralOccurrence {
        source_kind,
        kind,
        ordinal: 0,
        raw,
        raw_target,
        normalized_target: None,
        alias,
        anchor,
        field: None,
        relative,
        resolution: StructuralResolution::Unresolved,
    };
    if target.is_empty() {
        occurrence.resolution = StructuralResolution::Malformed;
    } else if is_external_target(&target) {
        occurrence.resolution = StructuralResolution::External;
    } else {
        let source_dir = Path::new(path)
            .parent()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        let normalized = normalize_link_path(&target, source_dir);
        if normalized == ".." || normalized.starts_with("../") {
            occurrence.resolution = StructuralResolution::UnsafeTraversal;
        }
        occurrence.normalized_target = Some(normalized);
    }
    occurrence
}

fn malformed_occurrence(
    source_kind: StructuralSourceKind,
    kind: StructuralLinkKind,
    raw: String,
) -> StructuralOccurrence {
    StructuralOccurrence {
        source_kind,
        kind,
        ordinal: 0,
        raw: raw.clone(),
        raw_target: raw,
        normalized_target: None,
        alias: None,
        anchor: None,
        field: None,
        relative: false,
        resolution: StructuralResolution::Malformed,
    }
}

fn redact_body_occurrence(occurrence: &mut StructuralOccurrence) {
    debug_assert_eq!(occurrence.source_kind, StructuralSourceKind::Body);
    occurrence.raw.clear();
    occurrence.alias = None;
    occurrence.raw_target = match occurrence.resolution {
        StructuralResolution::Malformed => String::new(),
        StructuralResolution::External => safe_external_target(&occurrence.raw_target),
        _ => occurrence.normalized_target.clone().unwrap_or_default(),
    };
}

fn safe_external_target(target: &str) -> String {
    // Markdown destination titles follow whitespace after the destination. The
    // URL itself is a structural relationship target; its optional title is prose.
    target
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

fn is_external_target(target: &str) -> bool {
    let lower = target.trim().to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("ftp://")
        || lower.starts_with("data:")
        || lower.starts_with("//")
}

fn find_closing(chars: &[char], start: usize, first: char, second: char) -> Option<usize> {
    (start..chars.len().saturating_sub(1))
        .find(|&index| chars[index] == first && chars[index + 1] == second)
}

fn find_balanced(chars: &[char], start: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 1;
    for (index, character) in chars.iter().copied().enumerate().skip(start) {
        match character {
            value if value == open => depth += 1,
            value if value == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn compare_occurrences(left: &StructuralOccurrence, right: &StructuralOccurrence) -> Ordering {
    left.source_kind
        .cmp(&right.source_kind)
        .then_with(|| left.kind.cmp(&right.kind))
        .then_with(|| left.raw.cmp(&right.raw))
        .then_with(|| left.raw_target.cmp(&right.raw_target))
        .then_with(|| left.normalized_target.cmp(&right.normalized_target))
        .then_with(|| left.alias.cmp(&right.alias))
        .then_with(|| left.anchor.cmp(&right.anchor))
        .then_with(|| left.field.cmp(&right.field))
        .then_with(|| left.relative.cmp(&right.relative))
        .then_with(|| left.resolution.cmp(&right.resolution))
}

pub(super) fn digest_structure(structure: &RecordStructure) -> String {
    let mut canonical = structure.clone();
    canonical.structural_digest.clear();
    let bytes = serde_jcs::to_vec(&canonical).expect("record structure is serializable");
    format!("sha256:{:x}", Sha256::digest(bytes))
}

impl RecordStructure {
    /// Return the deterministic digest of the structural envelope.
    pub fn structural_digest(&self) -> &str {
        &self.structural_digest
    }

    /// Verify that deserialized structural facts still bind their digest.
    pub fn structural_digest_is_valid(&self) -> bool {
        digest_structure(self) == self.structural_digest
    }

    /// Stable canonical JSON used for persistence and digest comparisons.
    pub fn canonical_json(&self) -> serde_json::Result<Vec<u8>> {
        serde_jcs::to_vec(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(body: &str) -> RecordStructure {
        parse_record_structure("notes/source.md", &format!("---\n---\n{body}"))
    }

    #[test]
    fn preserves_target_syntax_while_redacting_body_labels() {
        let structure = body("[[../target#Heading|Shown]] ![[./asset.png|Asset]] [read](../read.md#intro) ![photo](./photo.jpg)");
        assert_eq!(structure.occurrences.len(), 4);
        assert!(structure
            .occurrences
            .iter()
            .any(|item| item.anchor.as_deref() == Some("Heading")));
        assert!(structure
            .occurrences
            .iter()
            .all(|item| item.alias.is_none() && item.raw.is_empty()));
        assert!(structure
            .occurrences
            .iter()
            .any(|item| item.kind == StructuralLinkKind::WikilinkEmbed && item.relative));
        assert!(structure
            .occurrences
            .iter()
            .any(|item| item.kind == StructuralLinkKind::MarkdownImage));
        assert!(structure
            .occurrences
            .iter()
            .any(|item| item.normalized_target.as_deref() == Some("target")));
    }

    #[test]
    fn markdown_destination_pipe_is_not_a_wikilink_alias_separator() {
        let structure = body("[shown](foo|bar.md) ![photo](assets/a|b.png)");
        assert_eq!(structure.occurrences.len(), 2);
        assert_eq!(structure.occurrences[0].raw_target, "foo|bar.md");
        assert_eq!(structure.occurrences[0].alias, None);
        assert_eq!(structure.occurrences[1].raw_target, "assets/a|b.png");
        assert_eq!(structure.occurrences[1].alias, None);
    }

    #[test]
    fn body_labels_destination_titles_and_malformed_source_are_never_serialized() {
        let structure = body(
            "[[target|wikilink-secret]] [markdown-secret](local.md \"title-secret\") [[malformed-secret [angle](<local.md \"angle-title-secret\")",
        );
        let serialized = serde_json::to_string(&structure).unwrap();
        for secret in [
            "wikilink-secret",
            "markdown-secret",
            "title-secret",
            "malformed-secret",
            "angle-title-secret",
        ] {
            assert!(
                !serialized.contains(secret),
                "leaked {secret}: {serialized}"
            );
        }
        assert!(serialized.contains("target"));
        assert!(serialized.contains("local.md"));
        assert!(structure
            .occurrences
            .iter()
            .any(|occurrence| occurrence.resolution == StructuralResolution::Malformed));
    }

    #[test]
    fn excludes_code_and_fences_but_keeps_tags() {
        let structure = body("#keep `[[inline]] #drop`\n```\n[[fenced]] #drop\n```\n#keep");
        assert_eq!(structure.body_tags, vec!["keep"]);
        assert!(structure.occurrences.is_empty());
    }

    #[test]
    fn external_and_malformed_are_frozen_without_resolution() {
        let structure = body("[web](https://example.com) [[broken");
        assert!(structure
            .occurrences
            .iter()
            .any(|item| item.resolution == StructuralResolution::External));
        assert!(structure
            .occurrences
            .iter()
            .any(|item| item.resolution == StructuralResolution::Malformed));
    }

    #[test]
    fn ordering_and_digest_are_deterministic_and_body_prose_is_absent() {
        let first = body("ordinary prose alpha [[z]] [[a]]");
        let second = body("ordinary prose beta [[z]] [[a]]");
        assert_eq!(first.structural_digest, second.structural_digest);
        let serialized = serde_json::to_string(&first).unwrap();
        assert!(!serialized.contains("ordinary prose"));
        assert_eq!(first.occurrences[0].raw_target, "a");
    }

    #[test]
    fn frontmatter_links_are_structural_without_serializing_field_values() {
        let structure = parse_record_structure(
            "notes/source.md",
            "---\nrelated: '[[other#part|Other]]'\n---\nprose",
        );
        let link = structure
            .occurrences
            .iter()
            .find(|item| item.source_kind == StructuralSourceKind::Frontmatter)
            .unwrap();
        assert_eq!(link.field.as_deref(), Some("related"));
        assert_eq!(link.anchor.as_deref(), Some("part"));
        assert_eq!(link.alias.as_deref(), Some("Other"));
        assert!(!serde_json::to_string(&structure).unwrap().contains("prose"));
    }
}
