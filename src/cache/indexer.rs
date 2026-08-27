//! File -> cache indexing.

use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;

use crate::cache::CacheError;
use crate::expressions::evaluator::{
    extract_embeds_from_body, extract_links_from_body, extract_links_from_fm_value,
};
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
    let outcome = crate::record_load::load_record(collection, abs_path, rel_path)?;
    let facts = outcome.facts().clone();
    // Keep UTF-8 availability explicit at this boundary; invalid source is not
    // indexed as a synthetic Markdown body.
    let _utf8_document = outcome.document();
    remove_file(conn, rel_path)?;

    match outcome {
        crate::record_load::RecordLoadOutcome::Invalid {
            path,
            type_names,
            reason,
            ..
        } => {
            conn.execute(
                "INSERT INTO files (path, mtime_ns, ctime_ns, size, frontmatter_json, body, effective_json, parse_error, source_revision, failure_reason) \
                 VALUES (?1, ?2, ?3, ?4, '{}', '', NULL, 1, ?5, ?6)",
                rusqlite::params![
                    path,
                    facts.mtime_ns,
                    facts.ctime_ns,
                    facts.size as i64,
                    facts.revision,
                    reason.as_str()
                ],
            )?;
            for type_name in type_names {
                conn.execute(
                    "INSERT OR IGNORE INTO file_types (path, type_name) VALUES (?1, ?2)",
                    rusqlite::params![rel_path, type_name],
                )?;
            }
        }
        crate::record_load::RecordLoadOutcome::Parsed {
            path,
            raw_frontmatter,
            effective_frontmatter,
            body,
            type_names,
            ..
        } => {
            let fm_str = serde_json::to_string(&raw_frontmatter)?;
            let eff_str = serde_json::to_string(&effective_frontmatter)?;
            conn.execute(
                "INSERT INTO files (path, mtime_ns, ctime_ns, size, frontmatter_json, body, effective_json, parse_error, source_revision, failure_reason) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, NULL)",
                rusqlite::params![
                    path,
                    facts.mtime_ns,
                    facts.ctime_ns,
                    facts.size as i64,
                    fm_str,
                    body,
                    eff_str,
                    facts.revision
                ],
            )?;
            for type_name in &type_names {
                conn.execute(
                    "INSERT OR IGNORE INTO file_types (path, type_name) VALUES (?1, ?2)",
                    rusqlite::params![rel_path, type_name],
                )?;
            }
            insert_links(
                conn,
                rel_path,
                &facts.revision,
                &effective_frontmatter,
                &body,
            )?;
            insert_unique_values(
                conn,
                collection,
                rel_path,
                &effective_frontmatter,
                &type_names,
            )?;
            if let Some(value) = effective_frontmatter
                .get(&collection.settings.id_field)
                .and_then(canonical_unique_value)
            {
                conn.execute(
                    "INSERT OR REPLACE INTO identity_values (value, path) VALUES (?1, ?2)",
                    rusqlite::params![value, rel_path],
                )?;
            }
        }
    }
    Ok(())
}

pub(crate) fn canonical_unique_value(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(value) if value.is_empty() => None,
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        other => serde_json::to_string(other)
            .ok()
            .filter(|value| !value.is_empty()),
    }
}

/// Extract links from body and frontmatter and insert into the `links` table.
fn insert_links(
    conn: &Connection,
    rel_path: &str,
    source_revision: &str,
    frontmatter: &serde_json::Value,
    body: &str,
) -> Result<(), CacheError> {
    // Body links
    let body_links = extract_links_from_body(body);
    for raw in &body_links {
        conn.execute(
            "INSERT INTO links (source_path, target_path, source_revision, resolved, location, field, raw_target) \
             VALUES (?1, ?2, ?3, 0, ?4, NULL, ?5)",
            rusqlite::params![rel_path, raw, source_revision, "body", raw],
        )?;
    }

    // Body embeds
    let body_embeds = extract_embeds_from_body(body);
    for raw in &body_embeds {
        conn.execute(
            "INSERT INTO links (source_path, target_path, source_revision, resolved, location, field, raw_target) \
             VALUES (?1, ?2, ?3, 0, ?4, NULL, ?5)",
            rusqlite::params![rel_path, raw, source_revision, "body", raw],
        )?;
    }

    // Frontmatter links (iterate over each field)
    if let Some(obj) = frontmatter.as_object() {
        for (field_name, val) in obj {
            let mut targets = Vec::new();
            extract_links_from_fm_value(val, &mut targets);
            for raw in &targets {
                conn.execute(
                    "INSERT INTO links (source_path, target_path, source_revision, resolved, location, field, raw_target) \
                     VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6)",
                    rusqlite::params![rel_path, raw, source_revision, "frontmatter", field_name, raw],
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
            let mut field_references = type_def
                .fields
                .iter()
                .filter(|(_, field)| field.unique)
                .map(|(name, _)| name.clone())
                .collect::<HashSet<_>>();
            field_references.extend(
                type_def
                    .v03_frontmatter
                    .as_ref()
                    .and_then(|value| value.pointer("/collection/unique"))
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|rule| rule.get("field"))
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string),
            );
            for field_reference in field_references {
                if let Some(val) = crate::field_references::get_value(effective, &field_reference) {
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
                            rusqlite::params![type_name, field_reference, val_str, rel_path],
                        )?;
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
        "DELETE FROM identity_values WHERE path = ?1",
        rusqlite::params![rel_path],
    )?;
    conn.execute(
        "DELETE FROM files WHERE path = ?1",
        rusqlite::params![rel_path],
    )?;
    Ok(())
}

pub(crate) fn resolve_all_links(
    conn: &Connection,
    collection: &Collection,
) -> Result<(), CacheError> {
    resolve_links(conn, collection, None)
}

pub(crate) fn resolve_links_for_sources(
    conn: &Connection,
    collection: &Collection,
    sources: &HashSet<String>,
) -> Result<(), CacheError> {
    resolve_links(conn, collection, Some(sources))
}

fn resolve_links(
    conn: &Connection,
    collection: &Collection,
    sources: Option<&HashSet<String>>,
) -> Result<(), CacheError> {
    let mut files = conn.prepare(
        "SELECT path, COALESCE(effective_json, frontmatter_json) FROM files WHERE parse_error = 0",
    )?;
    let rows = files.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut resolution_records = Vec::new();
    for row in rows {
        let (path, frontmatter) = row?;
        resolution_records.push(crate::expressions::evaluator::ResolvedFileData {
            path,
            frontmatter: serde_json::from_str(&frontmatter)?,
            body: String::new(),
        });
    }
    drop(files);
    let resolution_index = collection.build_link_resolution_index(&resolution_records);

    let mut links = conn.prepare("SELECT rowid, source_path, raw_target FROM links")?;
    let rows = links.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut updates = Vec::new();
    for row in rows {
        let (rowid, source, raw) = row?;
        if sources.is_some_and(|sources| !sources.contains(&source)) {
            continue;
        }
        let resolved = collection.resolve_link_target(&raw, &source, &resolution_index);
        updates.push((rowid, raw, resolved));
    }
    drop(links);
    for (rowid, raw, resolved) in updates {
        conn.execute(
            "UPDATE links SET target_path = ?1, resolved = ?2 WHERE rowid = ?3",
            rusqlite::params![
                resolved.as_deref().unwrap_or(&raw),
                i64::from(resolved.is_some()),
                rowid
            ],
        )?;
    }
    Ok(())
}

pub(crate) fn load_backlinks(
    conn: &Connection,
) -> Result<std::collections::HashMap<String, Vec<String>>, CacheError> {
    let mut statement = conn.prepare(
        "SELECT target_path, source_path FROM links WHERE resolved = 1 ORDER BY target_path, source_path",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut backlinks = std::collections::HashMap::<String, Vec<String>>::new();
    for row in rows {
        let (target, source) = row?;
        let sources = backlinks.entry(target).or_default();
        if sources.last() != Some(&source) {
            sources.push(source);
        }
    }
    Ok(backlinks)
}

/// Full rebuild: delete everything and reindex all files.
#[allow(dead_code)]
pub(crate) fn reindex_all(
    conn: &mut Connection,
    collection: &Collection,
) -> Result<(), CacheError> {
    let files = collection.scan_collection_files_checked()?;
    let transaction = conn.transaction()?;
    transaction.execute_batch(
        "DELETE FROM links; DELETE FROM file_types; DELETE FROM unique_values; DELETE FROM identity_values; DELETE FROM files; DELETE FROM meta;",
    )?;

    for abs_path in &files {
        let rel_path = abs_path
            .strip_prefix(&collection.root)
            .map_err(|_| CacheError::OutsideRoot(abs_path.display().to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        reindex_file(&transaction, collection, abs_path, &rel_path)?;
    }

    resolve_all_links(&transaction, collection)?;

    transaction.execute(
        "INSERT INTO meta (key, value) VALUES ('query_snapshot', ?1)",
        [uuid::Uuid::new_v4().simple().to_string()],
    )?;
    transaction.commit()?;
    Ok(())
}
