//! Link parsing (§8.2-8.3).

/// Count leading ../ segments in a path target.
pub(crate) fn count_leading_dotdot(target: &str) -> usize {
    let mut count = 0;
    let mut rest = target;
    while rest.starts_with("../") {
        count += 1;
        rest = &rest[3..];
    }
    if rest == ".." {
        count += 1;
    }
    count
}

/// Normalize a link path by resolving . and .. segments relative to a source directory.
pub(crate) fn normalize_link_path(target: &str, source_dir: &str) -> String {
    // If the target is absolute-ish (starts with /) treat it as relative to root
    if target.starts_with('/') {
        let cleaned = target.trim_start_matches('/');
        return normalize_segments(cleaned);
    }

    // If target contains relative segments (./ or ../), resolve relative to source dir
    if target.starts_with("./")
        || target.starts_with("../")
        || target == "."
        || target == ".."
        || target.contains("/./")
        || target.contains("/../")
    {
        let combined = if source_dir.is_empty() {
            target.to_string()
        } else {
            format!("{}/{}", source_dir, target)
        };
        return normalize_segments(&combined);
    }

    // Plain name - no normalization needed
    target.to_string()
}

/// Normalize path segments by resolving . and ..
pub(crate) fn normalize_segments(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.is_empty() || parts.last() == Some(&"..") {
                    parts.push("..");
                } else {
                    parts.pop();
                }
            }
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

// --- impl Collection methods for link parsing ---

use crate::Collection;

impl Collection {
    /// Parse a link value into its components.
    pub fn parse_link(&self, input: &serde_json::Value) -> serde_json::Value {
        let value = match input.get("value").and_then(|v| v.as_str()) {
            Some(v) => v,
            None => {
                return serde_json::json!({"error": {"code": "invalid_input", "message": "parse_link requires 'value' field"}})
            }
        };

        let raw = value.to_string();

        // Wikilink: [[target]], [[target|alias]], [[target#anchor]], [[target#anchor|alias]]
        if value.starts_with("[[") && value.ends_with("]]") {
            let inner = &value[2..value.len() - 2];
            if inner.contains('[') || inner.contains(']') {
                return malformed_link(&raw, "Malformed wikilink delimiters");
            }
            // Split on | for alias
            let (target_part, alias) = if let Some(pipe_idx) = inner.find('|') {
                (&inner[..pipe_idx], Some(inner[pipe_idx + 1..].to_string()))
            } else {
                (inner, None)
            };
            // Split on # for anchor
            let (target, anchor) = if let Some(hash_idx) = target_part.find('#') {
                (
                    target_part[..hash_idx].to_string(),
                    Some(target_part[hash_idx + 1..].to_string()),
                )
            } else {
                (target_part.to_string(), None)
            };
            if target.trim().is_empty() || target.contains(['\n', '\r']) {
                return malformed_link(&raw, "Wikilink target must be nonempty and single-line");
            }
            let is_relative = target.starts_with("./") || target.starts_with("../");
            return serde_json::json!({
                "link": {
                    "raw": raw,
                    "target": target,
                    "alias": alias,
                    "anchor": anchor,
                    "format": "wikilink",
                    "is_relative": is_relative,
                }
            });
        }

        if value.starts_with("[[") {
            return malformed_link(&raw, "Malformed or unterminated wikilink syntax");
        }

        // Markdown link: [text](path) or [text](path#anchor)
        if value.starts_with('[') && value.contains("](") && value.ends_with(')') {
            let bracket_end = value.find("](").unwrap();
            let text = &value[1..bracket_end];
            let path_str = &value[bracket_end + 2..value.len() - 1];
            if text.contains('[')
                || text.contains(']')
                || path_str.contains('(')
                || path_str.contains(')')
                || value[bracket_end + 2..].matches("](").count() > 0
            {
                return malformed_link(&raw, "Malformed Markdown link delimiters");
            }
            let (path, anchor) = if let Some(hash_idx) = path_str.find('#') {
                (
                    path_str[..hash_idx].to_string(),
                    Some(path_str[hash_idx + 1..].to_string()),
                )
            } else {
                (path_str.to_string(), None)
            };
            if path.trim().is_empty() || path.contains(['\n', '\r']) {
                return malformed_link(
                    &raw,
                    "Markdown link target must be nonempty and single-line",
                );
            }
            let is_relative = path.starts_with("./") || path.starts_with("../");
            let alias = Some(text.to_string());
            return serde_json::json!({
                "link": {
                    "raw": raw,
                    "target": path,
                    "alias": alias,
                    "anchor": anchor,
                    "format": "markdown",
                    "is_relative": is_relative,
                }
            });
        }

        // Only strings that clearly begin Markdown link syntax are rejected.
        // Ordinary prose containing brackets remains a permissive bare value.
        if value.starts_with('[') && value.contains("](") {
            return malformed_link(&raw, "Malformed or unterminated Markdown link syntax");
        }

        // Bare/path
        let is_relative = value.starts_with("./") || value.starts_with("../");
        serde_json::json!({
            "link": {
                "raw": raw,
                "target": value,
                "alias": serde_json::Value::Null,
                "anchor": serde_json::Value::Null,
                "format": "path",
                "is_relative": is_relative,
            }
        })
    }
}

fn malformed_link(raw: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "error": {"code": "malformed_link", "message": message},
        "raw": raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collection() -> (tempfile::TempDir, Collection) {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        let collection = Collection::open(root.path()).unwrap();
        (root, collection)
    }

    #[test]
    fn malformed_link_intent_is_not_reclassified_as_a_bare_path() {
        let (_root, collection) = collection();
        for value in [
            "[[unterminated",
            "[[]]",
            "[[   ]]",
            "[[target\nline]]",
            "[[|]]",
            "[[#]]",
            "[[target]]junk]]",
            "[[[target]]]",
            "[label](unterminated",
            "[text]()",
            "[text](   )",
            "[text](#anchor)",
            "[text](target\nline)",
            "[label](target)junk)",
            "[label](target)(extra)",
        ] {
            let parsed = collection.parse_link(&serde_json::json!({"value": value}));
            assert_eq!(parsed["error"]["code"], "malformed_link", "{parsed}");
            assert!(parsed.get("link").is_none(), "{parsed}");
        }
        for (value, target) in [
            ("[[nested]]", "nested"),
            ("[[nested#anchor|Alias]]", "nested"),
            ("[Alias](nested.md#anchor)", "nested.md"),
        ] {
            let valid = collection.parse_link(&serde_json::json!({"value": value}));
            assert_eq!(valid["link"]["target"], target, "{valid}");
        }
    }

    #[test]
    fn permissive_prose_remains_a_bare_path() {
        let (_root, collection) = collection();
        for value in ["prose [aside", "unmatched ]] prose"] {
            let parsed = collection.parse_link(&serde_json::json!({"value": value}));
            assert_eq!(parsed["link"]["format"], "path");
            assert_eq!(parsed["link"]["target"], value);
        }
    }
}
