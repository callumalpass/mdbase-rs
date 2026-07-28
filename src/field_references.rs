use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Segment {
    key: String,
    each: bool,
}

fn field_path_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^[A-Za-z_][A-Za-z0-9_:-]*(\[\])?(\.[A-Za-z_][A-Za-z0-9_:-]*(\[\])?)*$")
            .expect("field-path pattern is valid")
    })
}

fn json_pointer_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^(?:/(?:[^~/]|~[01])*)+$").expect("JSON Pointer pattern is valid")
    })
}

pub(crate) fn is_valid(reference: &str) -> bool {
    field_path_pattern().is_match(reference) || json_pointer_pattern().is_match(reference)
}

fn parse(reference: &str) -> Option<Vec<Segment>> {
    if !is_valid(reference) {
        return None;
    }
    if let Some(pointer) = reference.strip_prefix('/') {
        return Some(
            pointer
                .split('/')
                .map(|token| Segment {
                    key: token.replace("~1", "/").replace("~0", "~"),
                    each: false,
                })
                .collect(),
        );
    }
    Some(
        reference
            .split('.')
            .map(|token| {
                token
                    .strip_suffix("[]")
                    .map(|key| Segment {
                        key: key.to_string(),
                        each: true,
                    })
                    .unwrap_or_else(|| Segment {
                        key: token.to_string(),
                        each: false,
                    })
            })
            .collect(),
    )
}

pub(crate) fn targets_top_level(reference: &str, field_name: &str) -> bool {
    parse(reference).is_some_and(|segments| {
        segments.len() == 1 && segments[0].key == field_name && !segments[0].each
    })
}

pub(crate) fn get_value<'a>(source: &'a Value, reference: &str) -> Option<&'a Value> {
    get_values(source, reference).into_iter().next()
}

pub(crate) fn get_value_from_object<'a>(
    source: &'a Map<String, Value>,
    reference: &str,
) -> Option<&'a Value> {
    let segments = parse(reference)?;
    let (first, remaining) = segments.split_first()?;
    let first_value = source.get(&first.key)?;
    let mut current = if first.each {
        first_value.as_array()?.iter().collect::<Vec<_>>()
    } else {
        vec![first_value]
    };
    let pointer = reference.starts_with('/');
    for segment in remaining {
        let mut next = Vec::new();
        for value in current {
            let selected = match value {
                Value::Object(object) => object.get(&segment.key),
                Value::Array(array) if pointer => {
                    array_index(&segment.key).and_then(|index| array.get(index))
                }
                _ => None,
            };
            let Some(selected) = selected else {
                continue;
            };
            if segment.each {
                if let Some(array) = selected.as_array() {
                    next.extend(array);
                }
            } else {
                next.push(selected);
            }
        }
        current = next;
    }
    current.into_iter().next()
}

pub(crate) fn get_values<'a>(source: &'a Value, reference: &str) -> Vec<&'a Value> {
    let Some(segments) = parse(reference) else {
        return Vec::new();
    };
    let pointer = reference.starts_with('/');
    let mut current = vec![source];

    for segment in segments {
        let mut next = Vec::new();
        for value in current {
            let selected = match value {
                Value::Object(object) => object.get(&segment.key),
                Value::Array(array) if pointer => {
                    array_index(&segment.key).and_then(|index| array.get(index))
                }
                _ => None,
            };
            let Some(selected) = selected else {
                continue;
            };
            if segment.each {
                if let Some(array) = selected.as_array() {
                    next.extend(array);
                }
            } else {
                next.push(selected);
            }
        }
        current = next;
        if current.is_empty() {
            break;
        }
    }
    current
}

pub(crate) fn set_object_value(
    target: &mut Map<String, Value>,
    reference: &str,
    value: Value,
) -> Result<(), String> {
    let segments =
        parse(reference).ok_or_else(|| format!("Invalid field reference '{reference}'"))?;
    let Some((first, remaining)) = segments.split_first() else {
        return Err(format!("Invalid field reference '{reference}'"));
    };
    if segments.iter().any(|segment| segment.each) {
        return Err(format!(
            "Cannot assign through array selector '{reference}'"
        ));
    }
    if remaining.is_empty() {
        target.insert(first.key.clone(), value);
        return Ok(());
    }
    if !target.contains_key(&first.key) {
        target.insert(first.key.clone(), Value::Object(Map::new()));
    }
    let child = target
        .get_mut(&first.key)
        .expect("field inserted immediately above");
    if !child.is_object() && !child.is_array() {
        return Err(format!(
            "Cannot assign through non-container field '{}' in '{reference}'",
            first.key
        ));
    }
    set_segments(child, remaining, reference, value, None, false)
}

pub(crate) fn set_value_with_schema(
    target: &mut Value,
    reference: &str,
    value: Value,
    schema: Option<&Value>,
    allow_array_append: bool,
) -> Result<(), String> {
    let segments =
        parse(reference).ok_or_else(|| format!("Invalid field reference '{reference}'"))?;
    if segments.iter().any(|segment| segment.each) {
        return Err(format!(
            "Cannot assign through array selector '{reference}'"
        ));
    }
    set_segments(
        target,
        &segments,
        reference,
        value,
        schema,
        allow_array_append,
    )
}

fn set_segments(
    current: &mut Value,
    segments: &[Segment],
    reference: &str,
    value: Value,
    schema: Option<&Value>,
    allow_array_append: bool,
) -> Result<(), String> {
    let Some((segment, remaining)) = segments.split_first() else {
        return Err(format!("Invalid field reference '{reference}'"));
    };
    let last = remaining.is_empty();

    match current {
        Value::Object(object) => {
            if last {
                object.insert(segment.key.clone(), value);
                return Ok(());
            }
            let child_schema = schema_property(schema, &segment.key);
            if !object.contains_key(&segment.key) {
                object.insert(
                    segment.key.clone(),
                    if is_array_schema(child_schema) {
                        Value::Array(Vec::new())
                    } else {
                        Value::Object(Map::new())
                    },
                );
            }
            let child = object
                .get_mut(&segment.key)
                .expect("field inserted immediately above");
            if !child.is_object() && !child.is_array() {
                return Err(format!(
                    "Cannot assign through non-container field '{}' in '{reference}'",
                    segment.key
                ));
            }
            set_segments(
                child,
                remaining,
                reference,
                value,
                child_schema,
                allow_array_append,
            )
        }
        Value::Array(array) => {
            let index = array_index(&segment.key).ok_or_else(|| {
                format!(
                    "Cannot use '{}' as an array index in '{reference}'",
                    segment.key
                )
            })?;
            if last {
                if index < array.len() {
                    array[index] = value;
                    return Ok(());
                }
                if allow_array_append && index == array.len() {
                    array.push(value);
                    return Ok(());
                }
                return Err(format!(
                    "Array index {index} does not exist in '{reference}'"
                ));
            }
            let child = array
                .get_mut(index)
                .ok_or_else(|| format!("Array index {index} does not exist in '{reference}'"))?;
            set_segments(
                child,
                remaining,
                reference,
                value,
                schema_items(schema),
                allow_array_append,
            )
        }
        _ => Err(format!(
            "Cannot assign through a non-object value in '{reference}'"
        )),
    }
}

pub(crate) fn schema_declares(schema: &Value, reference: &str) -> bool {
    let Some(segments) = parse(reference) else {
        return false;
    };
    let pointer = reference.starts_with('/');
    let mut current = schema;

    for segment in segments {
        if pointer && is_array_schema(Some(current)) {
            if array_index(&segment.key).is_none() {
                return false;
            }
            let Some(items) = schema_items(Some(current)) else {
                return false;
            };
            current = items;
            continue;
        }
        let Some(child) = schema_property(Some(current), &segment.key) else {
            return false;
        };
        current = child;
        if segment.each {
            let Some(items) = schema_items(Some(current)) else {
                return false;
            };
            current = items;
        }
    }
    true
}

fn array_index(token: &str) -> Option<usize> {
    if token == "0" {
        return Some(0);
    }
    if token.starts_with('0')
        || token.is_empty()
        || !token.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    token.parse().ok()
}

fn schema_property<'a>(schema: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    schema?.get("properties")?.as_object()?.get(key)
}

fn schema_items(schema: Option<&Value>) -> Option<&Value> {
    schema?.get("items")
}

fn is_array_schema(schema: Option<&Value>) -> bool {
    schema
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str)
        == Some("array")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn resolves_json_pointer_keys_and_legacy_array_selectors() {
        let source = json!({
            "@type": "Contact",
            "a/b": "slash",
            "a~b": "tilde",
            "relations": [{"id": 1}, {"id": 2}]
        });
        assert_eq!(get_value(&source, "/@type"), Some(&json!("Contact")));
        assert_eq!(get_value(&source, "/a~1b"), Some(&json!("slash")));
        assert_eq!(get_value(&source, "/a~0b"), Some(&json!("tilde")));
        assert_eq!(get_value(&source, "/relations/1/id"), Some(&json!(2)));
        assert_eq!(
            get_values(&source, "relations[].id"),
            vec![&json!(1), &json!(2)]
        );
    }

    #[test]
    fn writes_exact_keys_and_checks_schema_declarations() {
        let schema = json!({
            "type": "object",
            "properties": {
                "@type": {"const": "Contact"},
                "a/b": {"type": "string"},
                "metadata": {
                    "type": "object",
                    "properties": {"label": {"type": "string"}}
                }
            }
        });
        let mut target = json!({"metadata": {}});
        set_value_with_schema(&mut target, "/@type", json!("Contact"), None, false).unwrap();
        set_value_with_schema(&mut target, "/metadata/a~1b", json!("slash"), None, false).unwrap();
        assert_eq!(target["@type"], "Contact");
        assert_eq!(target["metadata"]["a/b"], "slash");
        assert!(schema_declares(&schema, "/@type"));
        assert!(schema_declares(&schema, "/a~1b"));
        assert!(schema_declares(&schema, "metadata.label"));
        assert!(targets_top_level("/@type", "@type"));
    }
}
