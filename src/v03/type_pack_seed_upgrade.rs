//! Seed-type upgrades: digest-pinned baselines, the ordered choice of one by
//! the seed's recorded origin, and a conservative, data-free three-way merge.
use super::ManifestResource;
use super::{frontmatter_bounds, replace_yaml_node, revision};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// A starter the publisher previously shipped for one seed type. It
/// serializes as the `upgrade_baseline` an assessment reports.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(super) struct Base {
    digest: String,
    #[serde(skip_serializing)]
    document: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<u64>,
}

/// `upgrade_from` is one baseline or a list; one baseline is a list of one.
pub(super) fn baselines<'de, D: serde::Deserializer<'de>>(input: D) -> Result<Vec<Base>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Declared {
        One(Base),
        Many(Vec<Base>),
    }
    Ok(match Declared::deserialize(input)? {
        Declared::One(base) => vec![base],
        Declared::Many(bases) => bases,
    })
}

/// Rejects baselines that could not be the desired type's own earlier starters.
pub(super) fn verify(resource: &ManifestResource, desired: &str) -> Result<(), String> {
    if resource.upgrade_from.is_empty() {
        return Ok(());
    }
    if resource.kind != "type" || resource.mode != "seed" {
        return Err("upgrade_from is valid only on seed type resources.".into());
    }
    let desired = parse(desired)?;
    let mut digests = BTreeSet::new();
    for base in &resource.upgrade_from {
        let document = parse(&base.document)?;
        let problem = if revision(base.document.as_bytes()) != base.digest {
            "does not match its document"
        } else if !digests.insert(&base.digest) {
            "is listed more than once"
        } else if base.digest == resource.digest {
            "is the desired document"
        } else if ["kind", "name"]
            .iter()
            .any(|key| document.get(key) != desired.get(key))
        {
            "is not the same type kind and name"
        } else if base
            .version
            .is_some_and(|version| document.get("version") != Some(&json!(version)))
        {
            "declares a different version"
        } else {
            continue;
        };
        return Err(format!("Upgrade baseline {} {problem}.", base.digest));
    }
    Ok(())
}

pub(super) type Planned<'a> = (&'static str, Option<String>, Option<(Vec<u8>, &'a Base)>);

/// An existing seed target's action, reason, and, for an `update`, the bytes
/// and the baseline used. Only byte equality or the recorded origin selects a
/// baseline: merging against any other would apply the differences between it
/// and the true origin as if they were the user's edits.
pub(super) fn plan<'a>(
    resource: &'a ManifestResource,
    live: &[u8],
    desired: &str,
    origin: Option<&str>,
) -> Planned<'a> {
    let bases = &resource.upgrade_from;
    if live == desired.as_bytes() {
        return ("preserve", None, None);
    }
    if let Some(base) = bases.iter().find(|base| base.document.as_bytes() == live) {
        return ("update", None, Some((desired.as_bytes().to_vec(), base)));
    }
    if origin == Some(resource.digest.as_str()) {
        return ("preserve", None, None);
    }
    let Some(base) = bases
        .iter()
        .find(|base| Some(base.digest.as_str()) == origin)
    else {
        let reason = "no upgrade baseline applies to this type's origin, so it is left as it is.";
        return ("preserve", Some(reason.into()), None);
    };
    let merged = std::str::from_utf8(live)
        .map_err(|error| error.to_string())
        .and_then(|live| merge(&base.document, live, desired));
    match merged {
        Ok(document) => ("update", None, Some((document.into_bytes(), base))),
        Err(reason) => ("conflict", Some(reason), None),
    }
}

fn parse(document: &str) -> Result<Value, String> {
    let (start, end) = frontmatter_bounds(document).map_err(|d| d.message.clone())?;
    serde_yaml::from_str(&document[start..end]).map_err(|e| e.to_string())
}

fn merge(base_document: &str, current: &str, desired_document: &str) -> Result<String, String> {
    let base = parse(base_document)?;
    let current_value = parse(current)?;
    let desired = parse(desired_document)?;
    for key in ["kind", "name"] {
        if current_value.get(key) != desired.get(key) {
            return Err(format!("Seed upgrade requires the same type {key}."));
        }
    }
    let merged = merge_value(Some(&base), Some(&current_value), Some(&desired), "")?
        .ok_or("Seed upgrade cannot delete a type.")?;
    let old = current_value.as_object().ok_or("Type must be an object.")?;
    let new = merged.as_object().ok_or("Type must be an object.")?;
    if old.keys().any(|key| !new.contains_key(key)) {
        return Err("Removing a top-level type setting requires manual review.".into());
    }
    // Rewrite only changed top-level nodes. Keep the user's Markdown body and
    // all unrelated YAML (including comments and formatting) byte-for-byte.
    let mut document = current.to_string();
    for (key, value) in new {
        if old.get(key) != Some(value) {
            let (start, end) = frontmatter_bounds(&document).map_err(|d| d.message.clone())?;
            document = replace_yaml_node(&document, start, end, key, value)
                .map_err(|d| d.message.clone())?;
        }
    }
    Ok(document)
}

fn merge_value(
    base: Option<&Value>,
    current: Option<&Value>,
    desired: Option<&Value>,
    path: &str,
) -> Result<Option<Value>, String> {
    if current == base || current == desired {
        return Ok(desired.cloned());
    }
    if desired == base {
        return Ok(current.cloned());
    }
    if path == "/implements" {
        fn index(value: Option<&Value>) -> Result<Map<String, Value>, String> {
            let entries = value
                .and_then(Value::as_array)
                .ok_or("Invalid implements list.")?;
            let mut indexed = Map::new();
            for entry in entries {
                let id = entry
                    .get("contract")
                    .and_then(Value::as_str)
                    .ok_or("Invalid contract identity.")?;
                if indexed.insert(id.into(), entry.clone()).is_some() {
                    return Err(format!(
                        "Multiple versions of {id} require explicit mapping review."
                    ));
                }
            }
            Ok(indexed)
        }
        let b = Value::Object(index(base)?);
        let c = Value::Object(index(current)?);
        let d = Value::Object(index(desired)?);
        let merged = merge_value(Some(&b), Some(&c), Some(&d), "/implementations")?
            .ok_or("Missing implementations.")?;
        return Ok(Some(Value::Array(
            merged.as_object().unwrap().values().cloned().collect(),
        )));
    }
    if let (Some(Value::Object(b)), Some(Value::Object(c)), Some(Value::Object(d))) =
        (base, current, desired)
    {
        let keys = b
            .keys()
            .chain(c.keys())
            .chain(d.keys())
            .collect::<BTreeSet<_>>();
        let mut result = BTreeMap::new();
        for key in keys {
            let escaped = key.replace('~', "~0").replace('/', "~1");
            if let Some(value) = merge_value(
                b.get(key),
                c.get(key),
                d.get(key),
                &format!("{path}/{escaped}"),
            )? {
                result.insert(key.clone(), value);
            }
        }
        return Ok(Some(serde_json::to_value(result).unwrap()));
    }
    Err(format!(
        "Seed upgrade conflicts with customized setting {path}; review it explicitly."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(value: &Value) -> String {
        format!(
            "---\n{}---\nUser body.\n",
            serde_yaml::to_string(value).unwrap()
        )
    }
    fn base() -> Value {
        json!({"kind":"mdbase.type", "name":"task", "version":1,
            "schema":{"value":{"properties":{"status":{"type":"string"}}}},
            "implements":[{"contract":"example.task", "version":"1.0.0", "fields":{"status":"status"}}]})
    }
    fn desired() -> Value {
        let mut value = base();
        value["version"] = json!(2);
        value["implements"][0]["version"] = json!("2.0.0");
        value["implements"][0]["fields"]["assignees"] = json!("assignees");
        value["schema"]["value"]["properties"]["assignees"] =
            json!({"type":"array", "items":{"type":"string"}});
        value
    }
    #[test]
    fn preserves_custom_mapping_settings_and_body_and_is_idempotent() {
        let mut current = base();
        current["implements"][0]["fields"]["status"] = json!("state");
        current["schema"]["value"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("status");
        current["schema"]["value"]["properties"]["state"] = json!({"enum":["todo","done"]});
        current["x-owner"] = json!("custom");
        let merged = merge(&doc(&base()), &doc(&current), &doc(&desired())).unwrap();
        assert!(merged.ends_with("---\nUser body.\n"));
        let (start, end) = frontmatter_bounds(&merged).unwrap();
        let value: Value = serde_yaml::from_str(&merged[start..end]).unwrap();
        assert_eq!(value["implements"][0]["fields"]["status"], "state");
        assert_eq!(value["implements"][0]["version"], "2.0.0");
        assert_eq!(value["implements"][0]["fields"]["assignees"], "assignees");
        assert!(value["schema"]["value"]["properties"]
            .get("status")
            .is_none());
        assert_eq!(value["x-owner"], "custom");
        assert_eq!(
            merge(&doc(&base()), &merged, &doc(&desired())).unwrap(),
            merged
        );
    }
    #[test]
    fn conflicting_field_addition_is_not_overwritten() {
        let mut current = base();
        current["schema"]["value"]["properties"]["assignees"] = json!({"type":"number"});
        assert!(merge(&doc(&base()), &doc(&current), &doc(&desired()))
            .unwrap_err()
            .contains("/assignees"));
    }
    #[test]
    fn missing_and_null_are_distinct_and_deletions_survive() {
        assert_eq!(
            merge_value(Some(&json!(1)), None, Some(&json!(1)), "/x").unwrap(),
            None
        );
        assert!(merge_value(None, Some(&Value::Null), Some(&json!(1)), "/x").is_err());
    }
}
