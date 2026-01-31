//! Type coercion (§7.16).

/// Coerce a value to the expected field type (§7.16).
pub(crate) fn coerce_value(value: &serde_json::Value, target_type: &str) -> Option<serde_json::Value> {
    match target_type {
        "string" => match value {
            serde_json::Value::String(_) => None, // already correct
            serde_json::Value::Number(n) => Some(serde_json::Value::String(n.to_string())),
            serde_json::Value::Bool(b) => Some(serde_json::Value::String(b.to_string())),
            _ => None,
        },
        "integer" => match value {
            serde_json::Value::Number(_) => None, // already correct
            serde_json::Value::String(s) => {
                // Try parsing as i64 first, then as f64 with integer check
                if let Ok(n) = s.parse::<i64>() {
                    Some(serde_json::json!(n))
                } else if let Ok(f) = s.parse::<f64>() {
                    if f.fract() == 0.0 && f.is_finite() {
                        Some(serde_json::json!(f as i64))
                    } else {
                        None // Keep as string, validation will catch it
                    }
                } else {
                    None
                }
            }
            _ => None,
        },
        "number" => match value {
            serde_json::Value::Number(_) => None, // already correct
            serde_json::Value::String(s) => {
                s.parse::<f64>().ok().and_then(|f| {
                    serde_json::Number::from_f64(f).map(serde_json::Value::Number)
                })
            }
            _ => None,
        },
        "boolean" => match value {
            serde_json::Value::Bool(_) => None, // already correct
            serde_json::Value::String(s) => match s.to_lowercase().as_str() {
                "true" | "yes" | "on" => Some(serde_json::Value::Bool(true)),
                "false" | "no" | "off" => Some(serde_json::Value::Bool(false)),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

// --- impl Collection methods for type coercion ---

use crate::Collection;

impl Collection {
    /// Apply defaults from type definitions to frontmatter (for effective frontmatter).
    pub(crate) fn apply_defaults(
        &self,
        frontmatter: &serde_json::Value,
        type_names: &[String],
    ) -> serde_json::Value {
        let mut result = frontmatter.clone();
        let obj = match result.as_object_mut() {
            Some(o) => o,
            None => return result,
        };

        for type_name in type_names {
            if let Some(type_def) = self.types.get(type_name) {
                for (field_name, field_def) in &type_def.fields {
                    if let Some(default) = &field_def.default {
                        if !obj.contains_key(field_name) {
                            // Field is missing — apply default
                            obj.insert(field_name.clone(), default.clone());
                        } else if obj.get(field_name).map_or(false, |v| v.is_null()) && field_def.generated.is_some() {
                            // Field is null AND was generated (e.g., derived with missing source)
                            // Apply default as effective value
                            obj.insert(field_name.clone(), default.clone());
                        }
                    }
                }
            }
        }

        result
    }

    /// Coerce field values to match their declared types (§7.16).
    pub(crate) fn coerce_types(
        &self,
        frontmatter: &serde_json::Value,
        type_names: &[String],
    ) -> serde_json::Value {
        let mut result = frontmatter.clone();
        let obj = match result.as_object_mut() {
            Some(o) => o,
            None => return result,
        };

        for type_name in type_names {
            if let Some(type_def) = self.types.get(type_name) {
                for (field_name, field_def) in &type_def.fields {
                    if let Some(value) = obj.get(field_name).cloned() {
                        if let Some(coerced) = coerce_value(&value, &field_def.field_type) {
                            obj.insert(field_name.clone(), coerced);
                        }
                    }
                }
            }
        }

        serde_json::Value::Object(obj.clone())
    }
}
