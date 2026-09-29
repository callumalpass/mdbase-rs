//! File -> cache indexing.

use rusqlite::{Connection, OptionalExtension};
use std::collections::HashSet;

use crate::cache::CacheError;
use crate::expressions::evaluator::{
    extract_embeds_from_body, extract_links_from_body, extract_links_from_fm_value,
};
use crate::Collection;

/// Parse and index a single file into the cache database.
///
/// `rel_path` is the forward-slash separated path relative to the collection root.
#[allow(dead_code)]
pub(crate) fn reindex_file(
    conn: &Connection,
    collection: &Collection,
    rel_path: &str,
) -> Result<(), CacheError> {
    match crate::record_load::load_record(collection, rel_path) {
        Ok(outcome) => index_record_outcome(conn, collection, rel_path, outcome),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => remove_file(conn, rel_path),
        Err(error) => Err(error.into()),
    }
}

/// Revalidate a classified-invalid maintenance hint through the capability-
/// relative no-follow boundary. A still-invalid record gets a bounded stub and
/// an absent record is removed. A repaired record is left to its ordered public
/// create/modify event, so a private hint cannot expose successor content in an
/// earlier runtime generation. Transient failures roll back the transaction.
pub(crate) fn refresh_invalid_file_no_follow(
    conn: &Connection,
    collection: &Collection,
    rel_path: &str,
) -> Result<Option<MaintenanceExpectation>, CacheError> {
    match crate::record_load::load_record_no_follow(collection, rel_path)? {
        Some(
            ref outcome @ crate::record_load::RecordLoadOutcome::Invalid {
                ref facts,
                ref type_names,
                ref state,
                ..
            },
        ) => {
            let reason = state.reason();
            let expectation = MaintenanceExpectation::Invalid {
                revision: facts.revision.clone(),
                reason,
                size: facts.size,
                mtime_ns: facts.mtime_ns,
                ctime_ns: facts.ctime_ns,
                type_names: type_names.iter().cloned().collect(),
            };
            index_record_outcome(conn, collection, rel_path, outcome.clone())?;
            Ok(Some(expectation))
        }
        Some(crate::record_load::RecordLoadOutcome::Parsed { .. }) => Ok(None),
        None => {
            remove_file(conn, rel_path)?;
            Ok(Some(MaintenanceExpectation::Absent))
        }
    }
}

/// Apply a private removal only when the capability-relative loader confirms
/// that the record is genuinely absent. Recreated records are handled by a
/// later observation or their ordered public event.
pub(crate) fn remove_invalid_file_no_follow_if_absent(
    conn: &Connection,
    collection: &Collection,
    rel_path: &str,
) -> Result<Option<MaintenanceExpectation>, CacheError> {
    if crate::record_load::load_record_no_follow(collection, rel_path)?.is_none() {
        remove_file(conn, rel_path)?;
        return Ok(Some(MaintenanceExpectation::Absent));
    }
    Ok(None)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MaintenanceExpectation {
    Absent,
    Invalid {
        revision: String,
        reason: crate::record_load::InvalidRecordReason,
        size: u64,
        mtime_ns: i64,
        ctime_ns: Option<i64>,
        type_names: std::collections::BTreeSet<String>,
    },
}

pub(crate) fn refresh_maintenance_expectation(
    collection: &Collection,
    rel_path: &str,
) -> Result<Option<MaintenanceExpectation>, CacheError> {
    match crate::record_load::load_record_no_follow(collection, rel_path)? {
        Some(crate::record_load::RecordLoadOutcome::Invalid {
            facts,
            type_names,
            state,
            ..
        }) => {
            let reason = state.reason();
            Ok(Some(MaintenanceExpectation::Invalid {
                revision: facts.revision,
                reason,
                size: facts.size,
                mtime_ns: facts.mtime_ns,
                ctime_ns: facts.ctime_ns,
                type_names: type_names.into_iter().collect(),
            }))
        }
        Some(crate::record_load::RecordLoadOutcome::Parsed { .. }) | None => Ok(None),
    }
}

pub(crate) fn maintenance_cache_expectation_is_exact(
    conn: &Connection,
    rel_path: &str,
    expected: &MaintenanceExpectation,
) -> Result<bool, CacheError> {
    let row = conn
        .query_row(
            "SELECT mtime_ns, ctime_ns, size, frontmatter_json, body, effective_json, parse_error, source_revision, failure_reason FROM files WHERE path = ?1",
            [rel_path],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            },
        )
        .optional()?;
    let row_exact = match expected {
        MaintenanceExpectation::Absent => row.is_none(),
        MaintenanceExpectation::Invalid {
            revision,
            reason,
            size,
            mtime_ns,
            ctime_ns,
            ..
        } => {
            row == Some((
                *mtime_ns,
                *ctime_ns,
                *size as i64,
                "{}".to_string(),
                String::new(),
                None,
                1,
                revision.clone(),
                Some(reason.as_str().to_string()),
            ))
        }
    };
    if !row_exact {
        return Ok(false);
    }

    let expected_types = match expected {
        MaintenanceExpectation::Absent => std::collections::BTreeSet::new(),
        MaintenanceExpectation::Invalid { type_names, .. } => type_names.clone(),
    };
    let mut statement = conn.prepare("SELECT type_name FROM file_types WHERE path = ?1")?;
    let actual_types = statement
        .query_map([rel_path], |row| row.get::<_, String>(0))?
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
    if actual_types != expected_types {
        return Ok(false);
    }

    for (table, column) in [
        ("links", "source_path"),
        ("unique_values", "path"),
        ("identity_values", "path"),
    ] {
        let count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1"),
            [rel_path],
            |row| row.get(0),
        )?;
        if count != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn maintenance_expectation_still_current(
    collection: &Collection,
    rel_path: &str,
    expected: &MaintenanceExpectation,
) -> Result<bool, CacheError> {
    let current = crate::record_load::load_record_no_follow(collection, rel_path)?;
    Ok(match (expected, current) {
        (MaintenanceExpectation::Absent, None) => true,
        (
            MaintenanceExpectation::Invalid {
                revision,
                reason,
                size,
                mtime_ns,
                ctime_ns,
                type_names,
            },
            Some(crate::record_load::RecordLoadOutcome::Invalid {
                facts,
                type_names: current_types,
                state,
                ..
            }),
        ) => {
            facts.revision == *revision
                && state.reason() == *reason
                && facts.size == *size
                && facts.mtime_ns == *mtime_ns
                && facts.ctime_ns == *ctime_ns
                && current_types
                    .iter()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>()
                    == *type_names
        }
        _ => false,
    })
}

/// Execute one statement through the connection's prepared-statement cache.
/// Indexing runs the same few statements for every record.
fn execute_cached<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> rusqlite::Result<usize> {
    conn.prepare_cached(sql)?.execute(params)
}

fn index_record_outcome(
    conn: &Connection,
    collection: &Collection,
    rel_path: &str,
    outcome: crate::record_load::RecordLoadOutcome,
) -> Result<(), CacheError> {
    let facts = outcome.facts().clone();
    // Keep UTF-8 availability explicit at this boundary; invalid source is not
    // indexed as a synthetic Markdown body.
    let _utf8_document = outcome.document();
    remove_file(conn, rel_path)?;

    match outcome {
        crate::record_load::RecordLoadOutcome::Invalid {
            path,
            type_names,
            state,
            ..
        } => {
            let reason = state.reason();
            execute_cached(conn,
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
                execute_cached(
                    conn,
                    "INSERT OR IGNORE INTO file_types (path, type_name) VALUES (?1, ?2)",
                    rusqlite::params![rel_path, type_name],
                )?;
            }
        }
        crate::record_load::RecordLoadOutcome::Parsed {
            path,
            raw_frontmatter,
            effective_frontmatter,
            document,
            layout,
            type_names,
            ..
        } => {
            let body = layout.body(&document);
            let fm_str = serde_json::to_string(&raw_frontmatter)?;
            let eff_str = serde_json::to_string(&effective_frontmatter)?;
            execute_cached(conn,
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
                execute_cached(
                    conn,
                    "INSERT OR IGNORE INTO file_types (path, type_name) VALUES (?1, ?2)",
                    rusqlite::params![rel_path, type_name],
                )?;
            }
            insert_links(
                conn,
                rel_path,
                &facts.revision,
                &effective_frontmatter,
                body,
            )?;
            insert_unique_values(
                conn,
                collection,
                rel_path,
                &effective_frontmatter,
                &type_names,
            )?;
            for (kind, key) in crate::links::resolver::record_resolution_keys(
                rel_path,
                &effective_frontmatter,
                &collection.resolution_keys(),
            )
            .unwrap_or_default()
            {
                conn.prepare_cached(
                    "INSERT OR IGNORE INTO resolution_keys (kind, key, path) VALUES (?1, ?2, ?3)",
                )?
                .execute(rusqlite::params![kind.as_str(), key, rel_path])?;
            }
            if let Some(value) = effective_frontmatter
                .get(&collection.settings.id_field)
                .and_then(canonical_unique_value)
            {
                execute_cached(
                    conn,
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
        execute_cached(conn,
            "INSERT INTO links (source_path, target_path, source_revision, resolved, location, field, raw_target) \
             VALUES (?1, ?2, ?3, 0, ?4, NULL, ?5)",
            rusqlite::params![rel_path, raw, source_revision, "body", raw],
        )?;
    }

    // Body embeds
    let body_embeds = extract_embeds_from_body(body);
    for raw in &body_embeds {
        execute_cached(conn,
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
                execute_cached(conn,
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
                        execute_cached(conn,
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
    execute_cached(
        conn,
        "DELETE FROM links WHERE source_path = ?1",
        rusqlite::params![rel_path],
    )?;
    execute_cached(
        conn,
        "DELETE FROM file_types WHERE path = ?1",
        rusqlite::params![rel_path],
    )?;
    execute_cached(
        conn,
        "DELETE FROM unique_values WHERE path = ?1",
        rusqlite::params![rel_path],
    )?;
    execute_cached(
        conn,
        "DELETE FROM identity_values WHERE path = ?1",
        rusqlite::params![rel_path],
    )?;
    execute_cached(
        conn,
        "DELETE FROM resolution_keys WHERE path = ?1",
        rusqlite::params![rel_path],
    )?;
    execute_cached(
        conn,
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
    let mut rows = Vec::new();
    match sources {
        Some(sources) => {
            let mut links = conn.prepare_cached(
                "SELECT rowid, source_path, field, raw_target FROM links WHERE source_path = ?1",
            )?;
            for source in sources {
                let mapped = links.query_map([source], link_row)?;
                for row in mapped {
                    rows.push(row?);
                }
            }
        }
        None => {
            let mut links =
                conn.prepare("SELECT rowid, source_path, field, raw_target FROM links")?;
            let mapped = links.query_map([], link_row)?;
            for row in mapped {
                rows.push(row?);
            }
        }
    }
    if rows.is_empty() {
        return Ok(());
    }

    let resolution_index = match sources {
        Some(_) => load_candidate_resolution_index(conn, &rows)?,
        None => load_resolution_index(conn)?,
    };
    let mut update =
        conn.prepare_cached("UPDATE links SET target_path = ?1, resolved = ?2 WHERE rowid = ?3")?;
    for (rowid, source, field, raw) in rows {
        let target_types = field
            .as_deref()
            .filter(|field| !field.is_empty())
            .and_then(|field| {
                resolution_index
                    .types_by_path
                    .get(&source)
                    .map(|types| collection.field_target_types(types, field))
            })
            .unwrap_or_default();
        let resolved = collection
            .resolve_link_target(&raw, &source, &target_types, &resolution_index)
            .map_err(|error| {
                CacheError::Resolution(format!("{}: {}", error.code, error.message))
            })?;
        update.execute(rusqlite::params![
            resolved.as_deref().unwrap_or(&raw),
            i64::from(resolved.is_some()),
            rowid
        ])?;
    }
    Ok(())
}

type LinkRow = (i64, String, Option<String>, String);

fn link_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LinkRow> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

/// The part of the resolution index that resolving `rows` reads: each source's
/// indexed types, and every indexed record a target could name, from the
/// `resolution_keys` table and exact paths. Resolution against this index
/// equals resolution against the full index, because a lookup never reads
/// keys or paths other than its own.
fn load_candidate_resolution_index(
    conn: &Connection,
    rows: &[LinkRow],
) -> Result<crate::links::resolver::LinkResolutionIndex, CacheError> {
    use crate::links::resolver::{LinkResolutionIndex, ResolutionKeyKind, ResolutionLookup};

    let mut index = LinkResolutionIndex::default();
    let mut simple = HashSet::new();
    let mut paths = HashSet::new();
    for (_, source, _, raw) in rows {
        paths.insert(source.clone());
        match ResolutionLookup::of(raw, source) {
            Some(ResolutionLookup::Simple(key)) => {
                simple.insert(key);
            }
            Some(ResolutionLookup::Path(candidates)) => paths.extend(candidates),
            None => {}
        }
    }
    let mut by_key =
        conn.prepare_cached("SELECT kind, path FROM resolution_keys WHERE key = ?1")?;
    for key in &simple {
        let mapped = by_key.query_map([key], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in mapped {
            let (kind, path) = row?;
            let kind = ResolutionKeyKind::parse(&kind).ok_or_else(|| {
                CacheError::Resolution(format!("unknown resolution key kind '{kind}'"))
            })?;
            index.insert_key(kind, key.clone(), &path);
            index.known_paths.insert(path);
        }
    }
    // A path is a candidate when it is a valid record: it then has a basename key.
    let mut is_record = conn.prepare_cached(
        "SELECT 1 FROM resolution_keys WHERE path = ?1 AND kind = 'basename' LIMIT 1",
    )?;
    let mut known = Vec::new();
    for path in &paths {
        if is_record.exists([path])? {
            known.push(path.clone());
        }
    }
    index.known_paths.extend(known);
    let mut types =
        conn.prepare_cached("SELECT type_name FROM file_types WHERE path = ?1 ORDER BY type_name")?;
    let lookups = index
        .known_paths
        .iter()
        .chain(rows.iter().map(|(_, source, _, _)| source))
        .cloned()
        .collect::<HashSet<_>>();
    for path in lookups {
        let names = types
            .query_map([&path], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if !names.is_empty() {
            index.types_by_path.insert(path, names);
        }
    }
    Ok(index)
}

/// The complete link resolution index from the `resolution_keys` and
/// `file_types` tables, without parsing frontmatter or re-matching types.
fn load_resolution_index(
    conn: &Connection,
) -> Result<crate::links::resolver::LinkResolutionIndex, CacheError> {
    use crate::links::resolver::{LinkResolutionIndex, ResolutionKeyKind};

    let mut index = LinkResolutionIndex::default();
    let mut keys = conn.prepare("SELECT kind, key, path FROM resolution_keys")?;
    let mapped = keys.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in mapped {
        let (kind, key, path) = row?;
        let kind = ResolutionKeyKind::parse(&kind).ok_or_else(|| {
            CacheError::Resolution(format!("unknown resolution key kind '{kind}'"))
        })?;
        if kind == ResolutionKeyKind::Basename {
            index.known_paths.insert(path.clone());
        }
        index.insert_key(kind, key, &path);
    }
    drop(keys);
    let mut types =
        conn.prepare("SELECT path, type_name FROM file_types ORDER BY path, type_name")?;
    let mapped = types.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in mapped {
        let (path, type_name) = row?;
        if index.known_paths.contains(&path) {
            index.types_by_path.entry(path).or_default().push(type_name);
        }
    }
    Ok(index)
}

/// Backlinks and each stored link's resolved target, from one read of the `links` table.
/// A frontmatter link's target wins over a body link with the same text, as when the graph is
/// built from records, because frontmatter links resolve with their declared target types.
pub(crate) fn load_link_graph(
    conn: &Connection,
) -> Result<
    (
        std::collections::HashMap<String, Vec<String>>,
        crate::links::linked_files::StoredLinkTargets,
    ),
    CacheError,
> {
    let mut statement = conn.prepare(
        "SELECT target_path, source_path, raw_target, location FROM links WHERE resolved = 1 \
         ORDER BY target_path, source_path",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut backlinks = std::collections::HashMap::<String, Vec<String>>::new();
    let mut stored = crate::links::linked_files::StoredLinkTargets::new();
    for row in rows {
        let (target, source, raw, location) = row?;
        let targets = stored.entry(source.clone()).or_default();
        if location == "frontmatter" || !targets.contains_key(&raw) {
            targets.insert(raw, target.clone());
        }
        let sources = backlinks.entry(target).or_default();
        if sources.last() != Some(&source) {
            sources.push(source);
        }
    }
    Ok((backlinks, stored))
}

/// Full rebuild: delete everything and reindex all files.
#[allow(dead_code)]
pub(crate) fn reindex_all(
    conn: &mut Connection,
    collection: &Collection,
) -> Result<(), CacheError> {
    let files = collection.scan_collection_relative_paths_checked()?;
    let transaction = conn.transaction()?;
    transaction.execute_batch(
        "DELETE FROM links; DELETE FROM file_types; DELETE FROM unique_values; DELETE FROM identity_values; DELETE FROM resolution_keys; DELETE FROM files; DELETE FROM meta;",
    )?;

    for rel_path in &files {
        reindex_file(&transaction, collection, rel_path)?;
    }

    resolve_all_links(&transaction, collection)?;

    transaction.execute(
        "INSERT INTO meta (key, value) VALUES ('query_snapshot', ?1)",
        [uuid::Uuid::new_v4().simple().to_string()],
    )?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT_TYPE: &str = "---\nkind: mdbase.type\nname: project\nversion: 1\nmatch:\n  path_glob: \"projects/*.md\"\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n";
    const TASK_TYPE: &str = "---\nkind: mdbase.type\nname: task\nversion: 1\nmatch:\n  path_glob: \"tasks/*.md\"\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    properties:\n      project: { type: string }\ncollection:\n  links:\n    project:\n      target_type: project\n---\n";

    fn write(root: &std::path::Path, path: &str, contents: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn links(conn: &Connection) -> Vec<(String, String, String, i64)> {
        let mut statement = conn
            .prepare(
                "SELECT source_path, raw_target, target_path, resolved FROM links \
                 ORDER BY source_path, raw_target, location",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn source_resolution_matches_full_resolution() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write(
            root,
            "mdbase.yaml",
            "spec_version: 0.3.0\nsettings:\n  id_field: id\n",
        );
        write(root, "_types/project.md", PROJECT_TYPE);
        write(root, "_types/task.md", TASK_TYPE);
        write(root, "projects/alpha.md", "---\nid: p-alpha\n---\n");
        write(root, "projects/beta.md", "---\nid: shared\n---\n");
        write(root, "notes/beta.md", "---\nid: shared\n---\n");
        write(root, "notes/Gamma.md", "---\ntitle: Gamma\n---\n");
        write(
            root,
            "tasks/one.md",
            "---\nproject: \"[[p-alpha]]\"\n---\n[[beta]] [[../projects/alpha]] [[notes/beta]] \
             [[missing]] [[shared]] [[GAMMA]] [[/notes/gamma.md]] [x](../notes/Gamma.md)\n",
        );
        write(
            root,
            "tasks/two.md",
            "---\nproject: \"[[shared]]\"\n---\n[[one]]\n",
        );
        let collection = Collection::open(root).unwrap();
        crate::cache::runtime::rebuild(
            &collection,
            &crate::runtime::CollectionGeneration::initial(),
        )
        .unwrap();
        let mut conn = crate::cache::sqlite::open_cache_db(
            collection.held_root().cache_storage_path(),
            &collection.settings.cache_folder,
        )
        .unwrap();
        let full = links(&conn);
        assert!(full
            .iter()
            .any(|(source, raw, target, resolved)| source == "tasks/two.md"
                && raw.contains("shared")
                && target == "projects/beta.md"
                && *resolved == 1));
        assert!(full.iter().any(|(_, _, _, resolved)| *resolved == 0));

        let transaction = conn.transaction().unwrap();
        transaction
            .execute(
                "UPDATE links SET target_path = raw_target, resolved = 0",
                [],
            )
            .unwrap();
        let sources = ["tasks/one.md", "tasks/two.md"]
            .into_iter()
            .map(str::to_string)
            .collect();
        resolve_links_for_sources(&transaction, &collection, &sources).unwrap();
        assert_eq!(links(&transaction), full);
    }
}
