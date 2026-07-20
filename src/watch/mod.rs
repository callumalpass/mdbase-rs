//! File system watching (§15).
//!
//! Provides a real, debounced collection change stream and the deterministic
//! simulation adapter used by the legacy conformance runner.

mod real;

pub use real::{CollectionWatcher, WatchError};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;

/// A watch event as defined in §15.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchEvent {
    pub event_type: String,
    pub sequence: u64,
    pub occurred_at: String,
    pub payload: Value,
}

/// Simulate watch mode for a collection root given a set of simulated actions.
///
/// This processes the simulate block from a conformance test and returns
/// the events that would be emitted by a real file watcher.
///
/// The `collection_root` must already have the initial files set up.
/// The simulate actions are applied in order, and events are generated.
pub fn simulate_watch(
    collection_root: &Path,
    simulate: &Value,
    input_simulate: Option<&Value>,
) -> WatchResult {
    let mut events = Vec::new();
    let mut listener_error_event: Option<usize> = None;

    // Merge simulate sources
    let sim = if let Some(input_sim) = input_simulate {
        // Merge input.simulate over top-level simulate
        let mut merged = simulate.as_object().cloned().unwrap_or_default();
        if let Some(input_obj) = input_sim.as_object() {
            for (k, v) in input_obj {
                merged.insert(k.clone(), v.clone());
            }
        }
        Value::Object(merged)
    } else {
        simulate.clone()
    };

    let sim_obj = match sim.as_object() {
        Some(obj) => obj,
        None => {
            return WatchResult {
                events,
                listener_error_event: None,
            }
        }
    };

    // Check for listener_error_on_event
    if let Some(v) = sim_obj.get("listener_error_on_event") {
        listener_error_event = v.as_u64().map(|n| n as usize);
    }

    // Open collection to get current state before changes
    let collection = match crate::Collection::open(collection_root) {
        Ok(c) => c,
        Err(_) => {
            return WatchResult {
                events,
                listener_error_event,
            }
        }
    };

    // Process each simulate action
    for (key, val) in sim_obj {
        match key.as_str() {
            "external_create" => {
                if let Some(ev) = process_external_create(collection_root, val, &collection) {
                    events.extend(ev);
                }
            }
            "external_modify" => {
                if let Some(ev) = process_external_modify(collection_root, val, &collection) {
                    events.extend(ev);
                }
            }
            "external_delete" => {
                if let Some(ev) = process_external_delete(collection_root, val, &collection) {
                    events.extend(ev);
                }
            }
            "external_rename" => {
                if let Some(ev) = process_external_rename(collection_root, val, &collection) {
                    events.extend(ev);
                }
            }
            "type_change" => {
                if let Some(ev) = process_type_change(collection_root, val, &collection) {
                    events.extend(ev);
                }
            }
            "config_change" => {
                if let Some(ev) = process_config_change(collection_root, val) {
                    events.extend(ev);
                }
            }
            "rapid_changes" => {
                if let Some(ev) = process_rapid_changes(collection_root, val, &collection) {
                    events.extend(ev);
                }
            }
            "sequential_changes" => {
                if let Some(ev) = process_sequential_changes(collection_root, val, &collection) {
                    events.extend(ev);
                }
            }
            "listener_error_on_event" => {
                // Already handled above
            }
            _ => {}
        }
    }

    WatchResult {
        events,
        listener_error_event,
    }
}

pub struct WatchResult {
    pub events: Vec<Value>,
    pub listener_error_event: Option<usize>,
}

/// Check if a path has a valid markdown extension for this collection.
fn is_markdown_path(path: &str, collection: &crate::Collection) -> bool {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    if ext == "md" {
        return true;
    }

    // Check custom extensions
    collection.settings.extensions.iter().any(|e| {
        let e_trimmed = e.trim_start_matches('.');
        ext == e_trimmed
    })
}

/// Check if a path is excluded by the collection settings.
fn is_path_excluded(path: &str, collection: &crate::Collection) -> bool {
    collection.is_excluded(path)
}

/// Read a file and return its effective frontmatter (defaults applied, computed excluded).
fn read_effective_frontmatter(
    _collection_root: &Path,
    rel_path: &str,
    collection: &crate::Collection,
) -> Option<(Value, Vec<String>)> {
    let result = collection.read(&json!({"path": rel_path}));
    let frontmatter = result.get("frontmatter").cloned()?;
    let types: Vec<String> = result
        .get("types")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Remove computed fields from frontmatter
    let mut fm = frontmatter;
    if let Value::Object(ref mut obj) = fm {
        // Remove fields that are computed
        for type_name in &types {
            let key = type_name.as_str().to_ascii_lowercase();
            if let Some(type_def) = collection.types.get(&key) {
                for (field_name, field_def) in &type_def.fields {
                    if field_def.computed.is_some() {
                        obj.remove(field_name);
                    }
                }
            }
        }
    }

    Some((fm, types))
}

/// Get the raw frontmatter keys and values from a file (preserving YAML order).
/// Returns ordered list of (key, value) pairs.
fn read_raw_frontmatter_ordered(
    collection_root: &Path,
    rel_path: &str,
) -> Vec<(String, serde_yaml::Value)> {
    let full_path = collection_root.join(rel_path);
    let content = match std::fs::read_to_string(&full_path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let doc = crate::frontmatter::parser::parse_document(&content);
    match &doc.frontmatter {
        Some(serde_yaml::Value::Mapping(m)) => m
            .iter()
            .filter_map(|(k, v)| {
                if let serde_yaml::Value::String(key) = k {
                    Some((key.clone(), v.clone()))
                } else {
                    None
                }
            })
            .collect(),
        _ => vec![],
    }
}

/// Validate a file and return validation issues if any.
fn validate_file(
    collection_root: &Path,
    rel_path: &str,
    _collection: &crate::Collection,
) -> Vec<Value> {
    // Re-open collection to get fresh state after file changes
    let fresh_collection = match crate::Collection::open(collection_root) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let result = fresh_collection.validate_op(&json!({"path": rel_path}));
    if let Some(issues) = result.get("issues").and_then(|v| v.as_array()) {
        let error_issues: Vec<Value> = issues
            .iter()
            .filter(|issue| issue.get("severity").and_then(|s| s.as_str()) == Some("error"))
            .cloned()
            .collect();
        if !error_issues.is_empty() {
            return error_issues;
        }
    }
    vec![]
}

fn now_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn process_external_create(
    collection_root: &Path,
    val: &Value,
    collection: &crate::Collection,
) -> Option<Vec<Value>> {
    let path = val.get("path")?.as_str()?;

    // Check if binary file or non-markdown
    let binary = val.get("binary").and_then(|v| v.as_bool()).unwrap_or(false);
    if binary || !is_markdown_path(path, collection) {
        // Non-markdown file: no events
        // But still create the file so state is consistent
        let full_path = collection_root.join(path);
        if let Some(parent) = full_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&full_path, b"binary content");
        return Some(vec![]);
    }

    // Check if excluded
    if is_path_excluded(path, collection) {
        // Create the file but don't emit events
        let full_path = collection_root.join(path);
        if let Some(parent) = full_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Some(content) = val.get("content").and_then(|v| v.as_str()) {
            let _ = std::fs::write(&full_path, content);
        }
        return Some(vec![]);
    }

    // Write the file
    let content = val.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let full_path = collection_root.join(path);
    if let Some(parent) = full_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&full_path, content);

    // Re-open collection to see new file
    let fresh = match crate::Collection::open(collection_root) {
        Ok(c) => c,
        Err(_) => return None,
    };

    let mut events = Vec::new();

    // Read effective frontmatter
    let (frontmatter, types) =
        read_effective_frontmatter(collection_root, path, &fresh).unwrap_or((json!({}), vec![]));

    events.push(json!({
        "event": "file_created",
        "timestamp": now_timestamp(),
        "path": path,
        "types": types,
        "frontmatter": frontmatter,
    }));

    // Check for validation errors
    let issues = validate_file(collection_root, path, &fresh);
    if !issues.is_empty() {
        events.push(json!({
            "event": "validation_error",
            "timestamp": now_timestamp(),
            "path": path,
            "issues": issues,
        }));
    }

    Some(events)
}

fn process_external_modify(
    collection_root: &Path,
    val: &Value,
    _collection: &crate::Collection,
) -> Option<Vec<Value>> {
    let path = val.get("path")?.as_str()?;

    // Read old frontmatter before modification
    let old_raw_fm = read_raw_frontmatter_ordered(collection_root, path);

    // Write new content
    let content = val.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let full_path = collection_root.join(path);
    let _ = std::fs::write(&full_path, content);

    // Re-open collection
    let fresh = match crate::Collection::open(collection_root) {
        Ok(c) => c,
        Err(_) => return None,
    };

    let (frontmatter, types) =
        read_effective_frontmatter(collection_root, path, &fresh).unwrap_or((json!({}), vec![]));

    // Determine changed_fields by comparing raw frontmatter
    let new_raw_fm = read_raw_frontmatter_ordered(collection_root, path);

    let changed_fields = compute_changed_fields(&old_raw_fm, &new_raw_fm);

    let mut events = Vec::new();
    events.push(json!({
        "event": "file_modified",
        "timestamp": now_timestamp(),
        "path": path,
        "types": types,
        "frontmatter": frontmatter,
        "changed_fields": changed_fields,
    }));

    // Check for validation errors
    let issues = validate_file(collection_root, path, &fresh);
    if !issues.is_empty() {
        events.push(json!({
            "event": "validation_error",
            "timestamp": now_timestamp(),
            "path": path,
            "issues": issues,
        }));
    }

    Some(events)
}

fn process_external_delete(
    collection_root: &Path,
    val: &Value,
    collection: &crate::Collection,
) -> Option<Vec<Value>> {
    let path = val.get("path")?.as_str()?;

    // Check non-markdown
    if !is_markdown_path(path, collection) {
        let full_path = collection_root.join(path);
        let _ = std::fs::remove_file(&full_path);
        return Some(vec![]);
    }

    // Get last known types before deletion
    let last_known_types: Vec<String> =
        if let Some((_, types)) = read_effective_frontmatter(collection_root, path, collection) {
            types
        } else {
            vec![]
        };

    // Delete the file
    let full_path = collection_root.join(path);
    let _ = std::fs::remove_file(&full_path);

    Some(vec![json!({
        "event": "file_deleted",
        "timestamp": now_timestamp(),
        "path": path,
        "last_known_types": last_known_types,
    })])
}

fn process_external_rename(
    collection_root: &Path,
    val: &Value,
    collection: &crate::Collection,
) -> Option<Vec<Value>> {
    let from = val.get("from")?.as_str()?;
    let to = val.get("to")?.as_str()?;
    let detection_fails = val
        .get("detection_fails")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Get types before rename
    let types: Vec<String> =
        if let Some((_, types)) = read_effective_frontmatter(collection_root, from, collection) {
            types
        } else {
            vec![]
        };

    // Perform the rename
    let from_path = collection_root.join(from);
    let to_path = collection_root.join(to);
    if let Some(parent) = to_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::rename(&from_path, &to_path);

    if detection_fails {
        // Emit delete + create
        let fresh = match crate::Collection::open(collection_root) {
            Ok(c) => c,
            Err(_) => return None,
        };
        let (frontmatter, new_types) =
            read_effective_frontmatter(collection_root, to, &fresh).unwrap_or((json!({}), vec![]));

        Some(vec![
            json!({
                "event": "file_deleted",
                "timestamp": now_timestamp(),
                "path": from,
                "last_known_types": types,
            }),
            json!({
                "event": "file_created",
                "timestamp": now_timestamp(),
                "path": to,
                "types": new_types,
                "frontmatter": frontmatter,
            }),
        ])
    } else {
        // Emit file_renamed
        Some(vec![json!({
            "event": "file_renamed",
            "timestamp": now_timestamp(),
            "from": from,
            "to": to,
            "types": types,
        })])
    }
}

fn process_type_change(
    collection_root: &Path,
    val: &Value,
    collection: &crate::Collection,
) -> Option<Vec<Value>> {
    let type_name = val.get("type")?.as_str()?;
    let new_definition = val.get("new_definition")?.as_str()?;

    // Find affected files (files of this type) before applying change
    let all_files = collection.build_all_files_data();
    let mut affected_files: Vec<String> = Vec::new();
    for file_data in &all_files {
        let file_types =
            collection.determine_types_for_path(&file_data.frontmatter, Some(&file_data.path));
        if file_types
            .iter()
            .any(|t| t.to_lowercase() == type_name.to_lowercase())
        {
            affected_files.push(file_data.path.clone());
        }
    }
    affected_files.sort();

    // Apply the type change
    let type_path = collection_root
        .join(&collection.settings.types_folder)
        .join(format!("{}.md", type_name));
    let _ = std::fs::write(&type_path, new_definition);

    Some(vec![json!({
        "event": "type_changed",
        "timestamp": now_timestamp(),
        "type_name": type_name,
        "affected_files": affected_files,
    })])
}

fn process_config_change(collection_root: &Path, val: &Value) -> Option<Vec<Value>> {
    let new_config = val.get("new_config")?.as_str()?;

    // Compute hash of old config
    let old_config_path = collection_root.join("mdbase.yaml");
    let old_content = std::fs::read_to_string(&old_config_path).unwrap_or_default();
    let previous_hash = format!("{:x}", md5_hash(old_content.as_bytes()));

    // Write new config
    let _ = std::fs::write(&old_config_path, new_config);

    // Compute hash of new config
    let new_hash = format!("{:x}", md5_hash(new_config.as_bytes()));

    Some(vec![json!({
        "event": "config_changed",
        "timestamp": now_timestamp(),
        "previous_hash": previous_hash,
        "new_hash": new_hash,
    })])
}

/// Simple hash function for config change detection.
fn md5_hash(data: &[u8]) -> u64 {
    // Use a simple FNV-1a hash for deterministic hashing
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in data {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn process_rapid_changes(
    collection_root: &Path,
    val: &Value,
    _collection: &crate::Collection,
) -> Option<Vec<Value>> {
    // Rapid changes: debounce to single event or no event
    // Two formats:
    // 1. { path, changes: [{content}, ...] } - multiple writes to same file
    // 2. { steps: [{action, path, content}, ...] } - mixed create/delete

    if let Some(steps) = val.get("steps").and_then(|v| v.as_array()) {
        // Format 2: mixed actions
        // Track what happened: if file was created then deleted, emit nothing
        let mut created: HashMap<String, String> = HashMap::new();
        let mut deleted: std::collections::HashSet<String> = std::collections::HashSet::new();

        for step in steps {
            let action = step.get("action").and_then(|v| v.as_str()).unwrap_or("");
            let path = step.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = step.get("content").and_then(|v| v.as_str()).unwrap_or("");

            match action {
                "create" => {
                    let full_path = collection_root.join(path);
                    if let Some(parent) = full_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(&full_path, content);
                    created.insert(path.to_string(), content.to_string());
                    deleted.remove(path);
                }
                "delete" => {
                    let full_path = collection_root.join(path);
                    let _ = std::fs::remove_file(&full_path);
                    if created.contains_key(path) {
                        // Created then deleted within debounce window: cancel out
                        created.remove(path);
                    }
                    deleted.insert(path.to_string());
                }
                "modify" => {
                    let full_path = collection_root.join(path);
                    let _ = std::fs::write(&full_path, content);
                    if !created.contains_key(path) {
                        // Existing file modified
                    }
                }
                _ => {}
            }
        }

        let mut events = Vec::new();

        // Files that were created and NOT deleted: emit file_created with final state
        for path in created.keys() {
            if !deleted.contains(path) {
                let fresh = crate::Collection::open(collection_root).ok()?;
                let (frontmatter, types) =
                    read_effective_frontmatter(collection_root, path, &fresh)
                        .unwrap_or((json!({}), vec![]));
                events.push(json!({
                    "event": "file_created",
                    "timestamp": now_timestamp(),
                    "path": path,
                    "types": types,
                    "frontmatter": frontmatter,
                }));
            }
        }

        Some(events)
    } else if let Some(path) = val.get("path").and_then(|v| v.as_str()) {
        // Format 1: multiple writes to same file (debounced to single event)
        let changes = val.get("changes").and_then(|v| v.as_array())?;

        // Read old raw frontmatter before changes
        let old_raw_fm = read_raw_frontmatter_ordered(collection_root, path);

        // Apply each change (last one wins due to debouncing)
        for change in changes {
            if let Some(content) = change.get("content").and_then(|v| v.as_str()) {
                let full_path = collection_root.join(path);
                let _ = std::fs::write(&full_path, content);
            }
        }

        // Generate single event for the final state
        let fresh = crate::Collection::open(collection_root).ok()?;
        let (frontmatter, types) = read_effective_frontmatter(collection_root, path, &fresh)
            .unwrap_or((json!({}), vec![]));

        // Determine changed fields
        let new_raw_fm = read_raw_frontmatter_ordered(collection_root, path);
        let changed_fields = compute_changed_fields(&old_raw_fm, &new_raw_fm);

        Some(vec![json!({
            "event": "file_modified",
            "timestamp": now_timestamp(),
            "path": path,
            "types": types,
            "frontmatter": frontmatter,
            "changed_fields": changed_fields,
        })])
    } else {
        None
    }
}

fn process_sequential_changes(
    collection_root: &Path,
    val: &Value,
    _collection: &crate::Collection,
) -> Option<Vec<Value>> {
    let steps = val.as_array()?;
    let mut events = Vec::new();

    for step in steps {
        let action = step.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let path = step.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let content = step.get("content").and_then(|v| v.as_str()).unwrap_or("");

        match action {
            "modify" => {
                // Read old frontmatter
                let old_raw_fm = read_raw_frontmatter_ordered(collection_root, path);

                // Apply modification
                let full_path = collection_root.join(path);
                let _ = std::fs::write(&full_path, content);

                // Re-open collection
                let fresh = match crate::Collection::open(collection_root) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                let (frontmatter, types) =
                    read_effective_frontmatter(collection_root, path, &fresh)
                        .unwrap_or((json!({}), vec![]));

                let new_raw_fm = read_raw_frontmatter_ordered(collection_root, path);
                let changed_fields = compute_changed_fields(&old_raw_fm, &new_raw_fm);

                events.push(json!({
                    "event": "file_modified",
                    "timestamp": now_timestamp(),
                    "path": path,
                    "types": types,
                    "frontmatter": frontmatter,
                    "changed_fields": changed_fields,
                }));
            }
            "create" => {
                let full_path = collection_root.join(path);
                if let Some(parent) = full_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&full_path, content);

                let fresh = match crate::Collection::open(collection_root) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                let (frontmatter, types) =
                    read_effective_frontmatter(collection_root, path, &fresh)
                        .unwrap_or((json!({}), vec![]));

                events.push(json!({
                    "event": "file_created",
                    "timestamp": now_timestamp(),
                    "path": path,
                    "types": types,
                    "frontmatter": frontmatter,
                }));
            }
            "delete" => {
                let fresh = match crate::Collection::open(collection_root) {
                    Ok(c) => c,
                    Err(_) => {
                        let full_path = collection_root.join(path);
                        let _ = std::fs::remove_file(&full_path);
                        continue;
                    }
                };
                let last_known_types: Vec<String> =
                    read_effective_frontmatter(collection_root, path, &fresh)
                        .map(|(_, types)| types)
                        .unwrap_or_default();

                let full_path = collection_root.join(path);
                let _ = std::fs::remove_file(&full_path);

                events.push(json!({
                    "event": "file_deleted",
                    "timestamp": now_timestamp(),
                    "path": path,
                    "last_known_types": last_known_types,
                }));
            }
            _ => {}
        }
    }

    Some(events)
}

/// Compute which raw frontmatter fields changed between old and new.
/// Order follows the new frontmatter's YAML field order, then removed fields.
fn compute_changed_fields(
    old: &[(String, serde_yaml::Value)],
    new: &[(String, serde_yaml::Value)],
) -> Vec<String> {
    let mut changed = Vec::new();
    let old_map: std::collections::HashMap<&str, &serde_yaml::Value> =
        old.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let new_map: std::collections::HashMap<&str, &serde_yaml::Value> =
        new.iter().map(|(k, v)| (k.as_str(), v)).collect();

    // Check for added or changed fields (in new frontmatter order)
    for (key, new_val) in new {
        // Skip type/types keys as they're not content fields
        if key == "type" || key == "types" {
            continue;
        }
        match old_map.get(key.as_str()) {
            Some(old_val) if *old_val == new_val => {} // unchanged
            _ => changed.push(key.clone()),
        }
    }

    // Check for removed fields (in old frontmatter order)
    for (key, _) in old {
        if key == "type" || key == "types" {
            continue;
        }
        if !new_map.contains_key(key.as_str()) {
            changed.push(key.clone());
        }
    }

    changed
}
