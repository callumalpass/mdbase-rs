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
    let re = regex::Regex::new(r"\{(\w+)\}").ok()?;
    for cap in re.captures_iter(pattern) {
        let field = &cap[1];
        let value = match obj.get(field) {
            Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
            Some(serde_json::Value::Null) | None => return None,
            Some(serde_json::Value::String(_)) => return None,
            Some(v) => v.to_string().trim_matches('"').to_string(),
        };
        result = result.replace(&format!("{{{field}}}"), &value);
    }
    Some(result)
}

use crate::types::schema::GeneratedStrategy;
use crate::Collection;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GenerationError {
    SequenceOverflow { type_name: String, field: String },
    DependencyCycle { fields: Vec<String> },
}

impl GenerationError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::SequenceOverflow { .. } => "generated_sequence_overflow",
            Self::DependencyCycle { .. } => "generated_dependency_cycle",
        }
    }
}

impl std::fmt::Display for GenerationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SequenceOverflow { type_name, field } => write!(
                formatter,
                "Generated sequence for '{type_name}.{field}' exceeds the i64 range"
            ),
            Self::DependencyCycle { fields } => write!(
                formatter,
                "Generated fields contain a dependency cycle: {}",
                fields.join(", ")
            ),
        }
    }
}

#[derive(Clone)]
pub(crate) struct GeneratedValueContext {
    /// Largest observed or successfully reserved value per normalized type/field.
    sequence_maxima: HashMap<(String, String), i64>,
}

impl GeneratedValueContext {
    pub(crate) fn from_snapshot(
        collection: &Collection,
        snapshot: &crate::snapshot::AuthoritativeCollectionSnapshot,
    ) -> Self {
        let mut maxima = HashMap::<(String, String), i64>::new();
        for entry in snapshot.entries() {
            let Some(frontmatter) = entry
                .effective_frontmatter()
                .and_then(serde_json::Value::as_object)
            else {
                continue;
            };
            for type_name in entry.type_names() {
                let Some(type_def) = collection.types.get(type_name) else {
                    continue;
                };
                for (field_name, field) in &type_def.fields {
                    if !matches!(field.generated, Some(GeneratedStrategy::Sequence(_))) {
                        continue;
                    }
                    if let Some(value) = frontmatter
                        .get(field_name)
                        .and_then(serde_json::Value::as_i64)
                    {
                        maxima
                            .entry((type_name.to_lowercase(), field_name.clone()))
                            .and_modify(|maximum| *maximum = (*maximum).max(value))
                            .or_insert(value);
                    }
                }
            }
        }
        Self {
            sequence_maxima: maxima,
        }
    }

    pub(crate) fn apply_generated(
        &mut self,
        collection: &Collection,
        frontmatter: &mut serde_json::Map<String, serde_json::Value>,
        type_names: &[String],
        is_create: bool,
        file_path: Option<&str>,
    ) -> Result<Vec<String>, GenerationError> {
        self.apply_generated_filtered(
            collection,
            frontmatter,
            type_names,
            is_create,
            file_path,
            None,
        )
    }

    pub(crate) fn apply_generated_filtered(
        &mut self,
        collection: &Collection,
        frontmatter: &mut serde_json::Map<String, serde_json::Value>,
        type_names: &[String],
        is_create: bool,
        file_path: Option<&str>,
        fields_filter: Option<&HashSet<String>>,
    ) -> Result<Vec<String>, GenerationError> {
        let mut fields = Vec::<(String, GeneratedStrategy, String)>::new();
        let mut seen = HashSet::new();
        for type_name in type_names {
            if let Some(type_def) = collection.types.get(type_name) {
                for (field_name, field_def) in &type_def.fields {
                    if fields_filter.is_some_and(|filter| !filter.contains(field_name)) {
                        continue;
                    }
                    if let Some(strategy) = &field_def.generated {
                        let should_generate = matches!(strategy, GeneratedStrategy::NowOnWrite)
                            || (is_create && !frontmatter.contains_key(field_name));
                        if should_generate && seen.insert(field_name.clone()) {
                            fields.push((field_name.clone(), strategy.clone(), type_name.clone()));
                        }
                    }
                }
            }
        }

        // A generated field only waits for another generated field. Sources supplied by
        // raw values or defaults are already present in the effective working map.
        let generated_names = fields
            .iter()
            .map(|(name, _, _)| name.clone())
            .collect::<HashSet<_>>();
        let mut remaining = fields;
        let mut ordered = Vec::new();
        while !remaining.is_empty() {
            let ready = remaining
                .iter()
                .position(|(_, strategy, _)| match strategy {
                    GeneratedStrategy::Derived { from, .. } => {
                        !generated_names.contains(from)
                            || ordered.iter().any(
                                |(name, _, _): &(String, GeneratedStrategy, String)| name == from,
                            )
                    }
                    _ => true,
                });
            let Some(index) = ready else {
                let mut cycle = remaining
                    .iter()
                    .map(|(name, _, _)| name.clone())
                    .collect::<Vec<_>>();
                cycle.sort();
                return Err(GenerationError::DependencyCycle { fields: cycle });
            };
            ordered.push(remaining.remove(index));
        }

        let mut working = collection.coerce_types(
            &collection.apply_defaults(&serde_json::Value::Object(frontmatter.clone()), type_names),
            type_names,
        );
        let mut changed = Vec::new();
        for (field_name, strategy, sequence_type) in ordered {
            let working_map = working.as_object().cloned().unwrap_or_default();
            let value = self.generate_value(
                &strategy,
                &field_name,
                type_names,
                &working_map,
                file_path,
                Some(&sequence_type),
            )?;
            frontmatter.insert(field_name.clone(), value);
            changed.push(field_name);
            working = collection.coerce_types(
                &collection
                    .apply_defaults(&serde_json::Value::Object(frontmatter.clone()), type_names),
                type_names,
            );
        }
        Ok(changed)
    }

    pub(crate) fn generate_value(
        &mut self,
        strategy: &GeneratedStrategy,
        field_name: &str,
        type_names: &[String],
        frontmatter: &serde_json::Map<String, serde_json::Value>,
        file_path: Option<&str>,
        sequence_type: Option<&str>,
    ) -> Result<serde_json::Value, GenerationError> {
        let value = match strategy {
            GeneratedStrategy::Ulid => serde_json::Value::String(ulid::Ulid::new().to_string()),
            GeneratedStrategy::Uuid => serde_json::Value::String(uuid::Uuid::new_v4().to_string()),
            GeneratedStrategy::Now | GeneratedStrategy::NowOnWrite => serde_json::Value::String(
                chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            ),
            GeneratedStrategy::Sequence(start) => {
                let type_name = sequence_type
                    .or_else(|| type_names.first().map(String::as_str))
                    .unwrap_or_default()
                    .to_lowercase();
                let key = (type_name.clone(), field_name.to_string());
                let candidate = match self.sequence_maxima.get(&key).copied() {
                    Some(maximum) => maximum
                        .checked_add(1)
                        .map(|next| next.max(*start))
                        .ok_or_else(|| GenerationError::SequenceOverflow {
                            type_name: type_name.clone(),
                            field: field_name.to_string(),
                        })?,
                    None => *start,
                };
                // Prove that this reservation has a representable successor before
                // mutating the context. A failed reservation therefore changes nothing.
                candidate
                    .checked_add(1)
                    .ok_or_else(|| GenerationError::SequenceOverflow {
                        type_name: type_name.clone(),
                        field: field_name.to_string(),
                    })?;
                self.sequence_maxima.insert(key, candidate);
                serde_json::Value::Number(serde_json::Number::from(candidate))
            }
            GeneratedStrategy::Random(len) => {
                use rand::Rng;
                let charset = b"abcdefghijklmnopqrstuvwxyz0123456789";
                let mut rng = rand::thread_rng();
                let value = (0..*len)
                    .map(|_| charset[rng.gen_range(0..charset.len())] as char)
                    .collect();
                serde_json::Value::String(value)
            }
            GeneratedStrategy::Derived { from, transform } => {
                let source = if from.starts_with("file.") {
                    file_path
                        .and_then(|path| {
                            let path_value = std::path::Path::new(path);
                            match from.as_str() {
                                "file.name" => path_value.file_name()?.to_str().map(str::to_string),
                                "file.basename" => {
                                    path_value.file_stem()?.to_str().map(str::to_string)
                                }
                                "file.ext" => path_value.extension()?.to_str().map(str::to_string),
                                "file.path" => Some(path.to_string()),
                                "file.folder" => Some(
                                    path_value
                                        .parent()
                                        .and_then(|value| value.to_str())
                                        .unwrap_or("")
                                        .to_string(),
                                ),
                                _ => None,
                            }
                        })
                        .map(serde_json::Value::String)
                } else {
                    frontmatter.get(from).cloned()
                };
                source.map_or(serde_json::Value::Null, |source| {
                    if source.is_null() {
                        serde_json::Value::Null
                    } else {
                        apply_transform(&source, transform)
                    }
                })
            }
        };
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(maximum: Option<i64>) -> GeneratedValueContext {
        let mut sequence_maxima = HashMap::new();
        if let Some(maximum) = maximum {
            sequence_maxima.insert(("item".to_string(), "number".to_string()), maximum);
        }
        GeneratedValueContext { sequence_maxima }
    }

    #[test]
    fn sequence_start_is_a_floor_above_existing_values() {
        let mut generated = context(Some(7));
        let value = generated
            .generate_value(
                &GeneratedStrategy::Sequence(100),
                "number",
                &["item".to_string()],
                &serde_json::Map::new(),
                None,
                Some("item"),
            )
            .unwrap();
        assert_eq!(value, serde_json::json!(100));
    }

    #[test]
    fn sequence_overflow_is_typed_and_does_not_consume_a_reservation() {
        let mut generated = context(None);
        let error = generated
            .generate_value(
                &GeneratedStrategy::Sequence(i64::MAX),
                "number",
                &["item".to_string()],
                &serde_json::Map::new(),
                None,
                Some("item"),
            )
            .unwrap_err();
        assert_eq!(error.code(), "generated_sequence_overflow");

        let value = generated
            .generate_value(
                &GeneratedStrategy::Sequence(4),
                "number",
                &["item".to_string()],
                &serde_json::Map::new(),
                None,
                Some("item"),
            )
            .unwrap();
        assert_eq!(value, serde_json::json!(4));
    }

    #[test]
    fn existing_i64_max_reports_overflow_without_wrapping() {
        let mut generated = context(Some(i64::MAX));
        let error = generated
            .generate_value(
                &GeneratedStrategy::Sequence(1),
                "number",
                &["item".to_string()],
                &serde_json::Map::new(),
                None,
                Some("item"),
            )
            .unwrap_err();
        assert_eq!(error.code(), "generated_sequence_overflow");
    }
}
