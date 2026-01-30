//! Resolve extends chains (§5.4).

use std::collections::HashMap;
use super::schema::TypeDef;

/// Resolve inheritance for all types, merging parent fields into children.
/// Child fields completely replace parent fields of the same name.
pub fn resolve_inheritance(types: &mut HashMap<String, TypeDef>) -> Result<(), String> {
    let names: Vec<String> = types.keys().cloned().collect();
    for name in &names {
        resolve_single(name, types, &mut Vec::new())?;
    }
    Ok(())
}

fn resolve_single(
    name: &str,
    types: &mut HashMap<String, TypeDef>,
    chain: &mut Vec<String>,
) -> Result<(), String> {
    if chain.contains(&name.to_string()) {
        return Err(format!("Circular inheritance: {}", chain.join(" -> ")));
    }

    let extends = match types.get(name) {
        Some(t) => t.extends.clone(),
        None => return Err(format!("Unknown type: {}", name)),
    };

    if let Some(parent_name) = extends {
        if !types.contains_key(&parent_name) {
            return Err(format!(
                "Type '{}' extends unknown type '{}'",
                name, parent_name
            ));
        }

        // Resolve parent first
        chain.push(name.to_string());
        resolve_single(&parent_name, types, chain)?;
        chain.pop();

        // Merge parent fields into child (child overrides)
        let parent = types.get(&parent_name).unwrap();
        let parent_fields = parent.fields.clone();
        let parent_strict = parent.strict.clone();

        let child = types.get_mut(name).unwrap();

        for (field_name, field_def) in parent_fields {
            child.fields.entry(field_name).or_insert(field_def);
        }

        // Inherit strict mode if not set
        if child.strict.is_none() {
            child.strict = parent_strict;
        }
    }

    Ok(())
}
