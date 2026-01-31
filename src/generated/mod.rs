//! Generated field values: ULID, UUID, timestamps (§7.15).

/// Apply a transform to a source value for derived fields.
pub(crate) fn apply_transform(source: &serde_json::Value, transform: &str) -> serde_json::Value {
    let s = match source {
        serde_json::Value::String(s) => s.clone(),
        _ => source.to_string(),
    };

    let result = match transform {
        "slugify" => slugify(&s),
        "lowercase" => s.to_lowercase(),
        "uppercase" => s.to_uppercase(),
        _ => s,
    };

    serde_json::Value::String(result)
}

/// Slugify a string: lowercase, replace non-alphanumeric with hyphens, collapse multiples.
pub(crate) fn slugify(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Derive a file path from a filename pattern and frontmatter.
pub(crate) fn derive_path(pattern: &str, frontmatter: &serde_json::Value) -> Option<String> {
    let mut result = pattern.to_string();
    let obj = frontmatter.as_object()?;

    // Replace {field} placeholders
    let re = regex::Regex::new(r"\{(\w+)\}").ok()?;
    for cap in re.captures_iter(pattern) {
        let field = &cap[1];
        let value = match obj.get(field) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(v) => v.to_string().trim_matches('"').to_string(),
            None => return None,
        };
        result = result.replace(&format!("{{{}}}", field), &value);
    }

    Some(result)
}

// --- impl Collection methods for generated fields ---

use crate::types::schema::GeneratedStrategy;
use crate::Collection;

impl Collection {
    /// Generate values for fields with generated strategies.
    pub(crate) fn apply_generated(
        &self,
        frontmatter: &mut serde_json::Map<String, serde_json::Value>,
        type_names: &[String],
        is_create: bool,
    ) {
        for type_name in type_names {
            if let Some(type_def) = self.types.get(type_name) {
                for (field_name, field_def) in &type_def.fields {
                    if let Some(strategy) = &field_def.generated {
                        let should_generate = match strategy {
                            GeneratedStrategy::NowOnWrite => true,
                            _ => is_create && !frontmatter.contains_key(field_name),
                        };

                        if should_generate {
                            let value = match strategy {
                                GeneratedStrategy::Ulid => {
                                    serde_json::Value::String(ulid::Ulid::new().to_string())
                                }
                                GeneratedStrategy::Uuid => {
                                    serde_json::Value::String(uuid::Uuid::new_v4().to_string())
                                }
                                GeneratedStrategy::Now | GeneratedStrategy::NowOnWrite => {
                                    serde_json::Value::String(
                                        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                                    )
                                }
                                GeneratedStrategy::Derived { from, transform } => {
                                    if let Some(source) = frontmatter.get(from) {
                                        if source.is_null() {
                                            serde_json::Value::Null
                                        } else {
                                            apply_transform(source, transform)
                                        }
                                    } else {
                                        serde_json::Value::Null
                                    }
                                }
                            };
                            frontmatter.insert(field_name.clone(), value);
                        }
                    }
                }
            }
        }
    }
}
