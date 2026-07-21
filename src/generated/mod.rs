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
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
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
            Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
            Some(serde_json::Value::Null) | None => return None,
            Some(serde_json::Value::String(_)) => return None, // empty string
            Some(v) => v.to_string().trim_matches('"').to_string(),
        };
        result = result.replace(&format!("{{{}}}", field), &value);
    }

    Some(result)
}

// --- impl Collection methods for generated fields ---

use crate::types::schema::GeneratedStrategy;
use crate::Collection;

impl Collection {
    /// Find the maximum value for a sequence field across all files of a given type.
    pub(crate) fn find_max_sequence_value(&self, type_name: &str, field_name: &str) -> Option<i64> {
        use crate::frontmatter::parser::{parse_document, yaml_mapping_to_json};
        let mut max: Option<i64> = None;
        // Reuse collection discovery so exclusions, nested boundaries, custom
        // extensions, and symlink containment exactly match normal reads.
        for entry in self.scan_collection_files() {
            if let Ok(content) = std::fs::read_to_string(&entry) {
                let doc = parse_document(&content);
                if let Some(serde_yaml::Value::Mapping(ref m)) = doc.frontmatter {
                    let json = yaml_mapping_to_json(m);
                    if let Some(obj) = json.as_object() {
                        let file_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if file_type.eq_ignore_ascii_case(type_name) {
                            if let Some(val) = obj.get(field_name).and_then(|v| v.as_i64()) {
                                max = Some(max.map_or(val, |m: i64| m.max(val)));
                            }
                        }
                    }
                }
            }
        }
        max
    }

    /// Generate values for fields with generated strategies.
    /// Fields are processed in dependency order so that derived fields
    /// depending on other generated fields get the correct source values.
    pub(crate) fn apply_generated(
        &self,
        frontmatter: &mut serde_json::Map<String, serde_json::Value>,
        type_names: &[String],
        is_create: bool,
        file_path: Option<&str>,
    ) {
        // Collect all generated fields across all matching types
        let mut generated_fields: Vec<(String, GeneratedStrategy)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for type_name in type_names {
            if let Some(type_def) = self.types.get(type_name) {
                for (field_name, field_def) in &type_def.fields {
                    if let Some(strategy) = &field_def.generated {
                        if seen.insert(field_name.clone()) {
                            generated_fields.push((field_name.clone(), strategy.clone()));
                        }
                    }
                }
            }
        }

        // Sort in dependency order: non-derived first, then derived fields
        // ordered so that a field derived from another generated field comes after it
        generated_fields.sort_by(|a, b| {
            let a_dep = match &a.1 {
                GeneratedStrategy::Derived { from, .. } => Some(from.clone()),
                _ => None,
            };
            let b_dep = match &b.1 {
                GeneratedStrategy::Derived { from, .. } => Some(from.clone()),
                _ => None,
            };
            match (&a_dep, &b_dep) {
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(a_from), _) if *a_from == b.0 => std::cmp::Ordering::Greater,
                (_, Some(b_from)) if *b_from == a.0 => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Equal,
            }
        });

        for (field_name, strategy) in &generated_fields {
            let should_generate = match strategy {
                GeneratedStrategy::NowOnWrite => true,
                _ => is_create && !frontmatter.contains_key(field_name),
            };

            if should_generate {
                let value =
                    self.generate_value(strategy, field_name, type_names, frontmatter, file_path);
                frontmatter.insert(field_name.clone(), value);
            }
        }
    }

    pub(crate) fn generate_value(
        &self,
        strategy: &GeneratedStrategy,
        field_name: &str,
        type_names: &[String],
        frontmatter: &serde_json::Map<String, serde_json::Value>,
        file_path: Option<&str>,
    ) -> serde_json::Value {
        match strategy {
            GeneratedStrategy::Ulid => serde_json::Value::String(ulid::Ulid::new().to_string()),
            GeneratedStrategy::Uuid => serde_json::Value::String(uuid::Uuid::new_v4().to_string()),
            GeneratedStrategy::Now | GeneratedStrategy::NowOnWrite => serde_json::Value::String(
                chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            ),
            GeneratedStrategy::Sequence(start) => {
                let type_name = type_names.first().map(|s| s.as_str()).unwrap_or("");
                let max_val = self.find_max_sequence_value(type_name, field_name);
                let next = if let Some(max) = max_val {
                    max + 1
                } else {
                    *start
                };
                serde_json::Value::Number(serde_json::Number::from(next))
            }
            GeneratedStrategy::Random(len) => {
                use rand::Rng;
                let charset = b"abcdefghijklmnopqrstuvwxyz0123456789";
                let mut rng = rand::thread_rng();
                let s: String = (0..*len)
                    .map(|_| {
                        let idx = rng.gen_range(0..charset.len());
                        charset[idx] as char
                    })
                    .collect();
                serde_json::Value::String(s)
            }
            GeneratedStrategy::Derived { from, transform } => {
                if from.starts_with("file.") {
                    let value = match file_path {
                        Some(path) => {
                            let p = std::path::Path::new(path);
                            match from.as_str() {
                                "file.name" => p
                                    .file_name()
                                    .and_then(|s| s.to_str())
                                    .map(|s| serde_json::Value::String(s.to_string())),
                                "file.basename" => p
                                    .file_stem()
                                    .and_then(|s| s.to_str())
                                    .map(|s| serde_json::Value::String(s.to_string())),
                                "file.ext" => p
                                    .extension()
                                    .and_then(|s| s.to_str())
                                    .map(|s| serde_json::Value::String(s.to_string())),
                                "file.path" => Some(serde_json::Value::String(path.to_string())),
                                "file.folder" => {
                                    let folder = p.parent().and_then(|s| s.to_str()).unwrap_or("");
                                    Some(serde_json::Value::String(folder.to_string()))
                                }
                                _ => None,
                            }
                        }
                        None => None,
                    };
                    match value {
                        Some(v) => apply_transform(&v, transform),
                        None => serde_json::Value::Null,
                    }
                } else if let Some(source) = frontmatter.get(from) {
                    if source.is_null() {
                        serde_json::Value::Null
                    } else {
                        apply_transform(source, transform)
                    }
                } else {
                    serde_json::Value::Null
                }
            }
        }
    }
}
