//! File -> cache indexing.

use rusqlite::Connection;
use std::path::Path;

use crate::cache::CacheError;
use crate::expressions::evaluator::{
    extract_embeds_from_body, extract_links_from_body, extract_links_from_fm_value,
};
use crate::frontmatter::parser::{is_parse_error, parse_document, yaml_mapping_to_json};
use crate::Collection;

/// Parse and index a single file into the cache database.
///
/// `abs_path` is the absolute path on disk; `rel_path` is the forward-slash
/// separated path relative to the collection root.
#[allow(dead_code)]
pub(crate) fn reindex_file(
    conn: &Connection,
    collection: &Collection,
    abs_path: &Path,
    rel_path: &str,
) -> Result<(), CacheError> {
    // 1. Read file contents
    let content = std::fs::read_to_string(abs_path)?;

    // 2. Get filesystem metadata
    let metadata = std::fs::metadata(abs_path)?;
    use std::time::UNIX_EPOCH;
    let mtime_ns = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos() as i64)
        .unwrap_or(0);
    let ctime_ns = metadata
        .created()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i64);
    let size = metadata.len() as i64;

    // 3. Parse document
    let doc = parse_document(&content);

    // 4. Convert frontmatter to JSON, detect parse errors
    let (frontmatter_json, body, parse_error) = match &doc.frontmatter {
        Some(yaml_val) if is_parse_error(yaml_val) => {
            // Parse error: store empty object as frontmatter, flag the error
            (serde_json::json!({}), doc.body.clone(), 1i64)
        }
        Some(serde_yaml::Value::Mapping(m)) => {
            let fm = yaml_mapping_to_json(m);
            (fm, doc.body.clone(), 0i64)
        }
        Some(_) => {
            // Non-mapping frontmatter (scalar, list, etc.) -- treat as empty
            (serde_json::json!({}), doc.body.clone(), 0i64)
        }
        None => {
            // No frontmatter delimiters
            (serde_json::json!({}), doc.body.clone(), 0i64)
        }
    };

    // 5. Determine types
    let type_names = collection.determine_types_for_path(&frontmatter_json, Some(rel_path));

    // 6. Compute effective frontmatter (defaults + coercion)
    let effective = collection.apply_defaults(&frontmatter_json, &type_names);
    let effective = collection.coerce_types(&effective, &type_names);

    let fm_str = serde_json::to_string(&frontmatter_json)?;
    let eff_str = serde_json::to_string(&effective)?;

    // 7. Delete old rows for this path (cascade would handle child tables if we
    //    had FK ON DELETE CASCADE, but our schema doesn't have it on all tables,
    //    so delete explicitly).
    remove_file(conn, rel_path)?;

    // 8. Insert into `files`
    conn.execute(
        "INSERT INTO files (path, mtime_ns, ctime_ns, size, frontmatter_json, body, effective_json, parse_error) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![rel_path, mtime_ns, ctime_ns, size, fm_str, body, eff_str, parse_error],
    )?;

    // 9. Insert into `file_types`
    for type_name in &type_names {
        conn.execute(
            "INSERT OR IGNORE INTO file_types (path, type_name) VALUES (?1, ?2)",
            rusqlite::params![rel_path, type_name],
        )?;
    }

    // 10. Extract and insert links
    insert_links(conn, rel_path, &effective, &body)?;

    // 11. Insert unique values
    insert_unique_values(conn, collection, rel_path, &effective, &type_names)?;
    Ok(())
}

/// Extract links from body and frontmatter and insert into the `links` table.
fn insert_links(
    conn: &Connection,
    rel_path: &str,
    frontmatter: &serde_json::Value,
    body: &str,
) -> Result<(), CacheError> {
    // Body links
    let body_links = extract_links_from_body(body);
    for raw in &body_links {
        conn.execute(
            "INSERT INTO links (source_path, target_path, location, field, raw_target) \
             VALUES (?1, ?2, ?3, NULL, ?4)",
            rusqlite::params![rel_path, raw, "body", raw],
        )?;
    }

    // Body embeds
    let body_embeds = extract_embeds_from_body(body);
    for raw in &body_embeds {
        conn.execute(
            "INSERT INTO links (source_path, target_path, location, field, raw_target) \
             VALUES (?1, ?2, ?3, NULL, ?4)",
            rusqlite::params![rel_path, raw, "body", raw],
        )?;
    }

    // Frontmatter links (iterate over each field)
    if let Some(obj) = frontmatter.as_object() {
        for (field_name, val) in obj {
            let mut targets = Vec::new();
            extract_links_from_fm_value(val, &mut targets);
            for raw in &targets {
                conn.execute(
                    "INSERT INTO links (source_path, target_path, location, field, raw_target) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![rel_path, raw, "frontmatter", field_name, raw],
                )?;
            }
        }
    }
    Ok(())
}

/// Insert unique field values into the `unique_values` table.
fn insert_unique_values(
    conn: &Connection,
    collection: &Collection,
    rel_path: &str,
    effective: &serde_json::Value,
    type_names: &[String],
) -> Result<(), CacheError> {
    for type_name in type_names {
        if let Some(type_def) = collection.types.get(type_name) {
            for (field_name, field_def) in &type_def.fields {
                if field_def.unique {
                    if let Some(val) = effective.get(field_name) {
                        let val_str = match val {
                            serde_json::Value::String(s) => s.clone(),
                            serde_json::Value::Number(n) => n.to_string(),
                            serde_json::Value::Bool(b) => b.to_string(),
                            serde_json::Value::Null => continue,
                            _ => serde_json::to_string(val).unwrap_or_default(),
                        };
                        if !val_str.is_empty() {
                            conn.execute(
                                "INSERT OR REPLACE INTO unique_values (type_name, field_name, value, path) \
                                 VALUES (?1, ?2, ?3, ?4)",
                                rusqlite::params![type_name, field_name, val_str, rel_path],
                            )?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Remove a file (by relative path) from all cache tables.
#[allow(dead_code)]
pub(crate) fn remove_file(conn: &Connection, rel_path: &str) -> Result<(), CacheError> {
    conn.execute(
        "DELETE FROM links WHERE source_path = ?1",
        rusqlite::params![rel_path],
    )?;
    conn.execute(
        "DELETE FROM file_types WHERE path = ?1",
        rusqlite::params![rel_path],
    )?;
    conn.execute(
        "DELETE FROM unique_values WHERE path = ?1",
        rusqlite::params![rel_path],
    )?;
    conn.execute(
        "DELETE FROM files WHERE path = ?1",
        rusqlite::params![rel_path],
    )?;
    Ok(())
}

/// Full rebuild: delete everything and reindex all files.
#[allow(dead_code)]
pub(crate) fn reindex_all(
    conn: &mut Connection,
    collection: &Collection,
) -> Result<(), CacheError> {
    let files = collection.scan_collection_files();
    let transaction = conn.transaction()?;
    transaction.execute_batch(
        "DELETE FROM links; DELETE FROM file_types; DELETE FROM unique_values; DELETE FROM files; DELETE FROM meta;",
    )?;

    for abs_path in &files {
        let rel_path = abs_path
            .strip_prefix(&collection.root)
            .map_err(|_| CacheError::OutsideRoot(abs_path.display().to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        reindex_file(&transaction, collection, abs_path, &rel_path)?;
    }

    transaction.execute(
        "INSERT INTO meta (key, value) VALUES ('query_snapshot', ?1)",
        [uuid::Uuid::new_v4().simple().to_string()],
    )?;
    transaction.commit()?;
    Ok(())
}
