//! Cache-backed data source for the query engine.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use rusqlite::Connection;

use crate::cache::{indexer, sqlite, staleness};
use crate::expressions::evaluator::ResolvedFileData;
use crate::frontmatter::parser::{parse_document, yaml_mapping_to_json};
use crate::Collection;

/// Common intermediate representation that both the cache path and disk-fallback
/// path produce. The query loop reads from these instead of touching disk.
pub(crate) struct FileRecord {
    pub rel_path: String,
    pub raw_frontmatter: serde_json::Value,
    pub effective_frontmatter: serde_json::Value, // defaults + coercion, NO computed fields
    pub body: String,
    pub type_names: Vec<String>,
    pub file_size: u64,
    pub file_mtime_iso: Option<String>,
    pub file_ctime_iso: Option<String>,
}

type QueryData = (
    Vec<FileRecord>,
    Option<Arc<Vec<ResolvedFileData>>>,
    Option<Arc<HashMap<String, Vec<String>>>>,
);

#[derive(Debug, Clone, Default)]
pub(crate) struct LoadQueryPerf {
    pub total_ms: f64,
    pub try_open_cache_ms: f64,
    pub refresh_cache_ms: f64,
    pub scan_files_ms: f64,
    pub load_records_ms: f64,
    pub build_all_files_ms: f64,
    pub build_backlinks_ms: f64,
    pub backlinks_frontmatter_extract_ms: f64,
    pub backlinks_body_links_extract_ms: f64,
    pub backlinks_body_embeds_extract_ms: f64,
    pub backlinks_resolve_targets_ms: f64,
    pub backlinks_files_processed: usize,
    pub backlinks_targets_scanned: usize,
    pub backlinks_resolve_calls: usize,
    pub file_records: usize,
    pub cache_used: bool,
    pub built_link_graph: bool,
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// Convert nanoseconds-since-epoch to ISO 8601 string.
fn ns_to_iso(ns: i64) -> String {
    use chrono::{TimeZone, Utc};
    let secs = ns / 1_000_000_000;
    let nsec = (ns % 1_000_000_000) as u32;
    match Utc.timestamp_opt(secs, nsec) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        _ => String::new(),
    }
}

impl Collection {
    /// Try to open the cache database. Returns `None` if the DB file doesn't
    /// exist or can't be opened.
    fn try_open_cache(&self) -> Option<Connection> {
        let db_path = self.root.join(&self.settings.cache_folder).join("cache.db");
        if !db_path.exists() {
            return None;
        }
        sqlite::open_cache_db(&self.root, &self.settings.cache_folder).ok()
    }

    /// Incremental freshness check: find stale/new/deleted files, re-index only
    /// what changed. Returns the full list of disk files (used for fallback and
    /// to know which paths exist).
    fn refresh_cache(&self, conn: &Connection) -> Vec<PathBuf> {
        let disk_files = self.scan_collection_files();

        let stale = staleness::find_stale(conn, &self.root, &disk_files);
        let deleted = staleness::find_deleted(conn, &self.root);

        if stale.is_empty() && deleted.is_empty() {
            return disk_files;
        }

        let _ = conn.execute_batch("BEGIN TRANSACTION;");

        // Remove deleted files from cache
        for rel_path in &deleted {
            indexer::remove_file(conn, rel_path);
        }

        // Re-index stale/new files
        for abs_path in &stale {
            let rel_path = match abs_path.strip_prefix(&self.root) {
                Ok(p) => p.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            indexer::reindex_file(conn, self, abs_path, &rel_path);
        }

        let _ = conn.execute_batch("COMMIT;");

        disk_files
    }

    /// Bulk-load FileRecords from the cache database.
    fn load_file_records_from_cache(&self, conn: &Connection) -> Vec<FileRecord> {
        let mut stmt = match conn.prepare(
            "SELECT f.path, f.frontmatter_json, f.effective_json, f.body, f.size, f.mtime_ns, f.ctime_ns \
             FROM files f WHERE f.parse_error = 0"
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let rows = match stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        // Pre-load all file_types into a map: path -> Vec<type_name>
        let types_map = self.load_file_types_map(conn);

        let mut records = Vec::new();
        for row in rows {
            let (path, fm_json_str, eff_json_str, body, size, mtime_ns, ctime_ns) = match row {
                Ok(r) => r,
                Err(_) => continue,
            };

            let raw_frontmatter: serde_json::Value =
                serde_json::from_str(&fm_json_str).unwrap_or(serde_json::json!({}));

            let effective_frontmatter: serde_json::Value = eff_json_str
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(|| raw_frontmatter.clone());

            let mut type_names = types_map.get(&path).cloned().unwrap_or_default();
            if type_names.is_empty() {
                // Older cache DBs can have files rows without file_types rows.
                // Recompute types from cached frontmatter so --types stays correct.
                type_names = self.determine_types_for_path(&raw_frontmatter, Some(&path));
            }

            let file_mtime_iso = if mtime_ns != 0 {
                Some(ns_to_iso(mtime_ns))
            } else {
                None
            };
            let file_ctime_iso = ctime_ns.map(ns_to_iso);

            records.push(FileRecord {
                rel_path: path,
                raw_frontmatter,
                effective_frontmatter,
                body,
                type_names,
                file_size: size as u64,
                file_mtime_iso,
                file_ctime_iso,
            });
        }

        records
    }

    /// Load the file_types table into a HashMap for bulk access.
    fn load_file_types_map(&self, conn: &Connection) -> HashMap<String, Vec<String>> {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        let mut stmt = match conn.prepare("SELECT path, type_name FROM file_types") {
            Ok(s) => s,
            Err(_) => return map,
        };
        let rows = match stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            Ok(r) => r,
            Err(_) => return map,
        };
        for (path, type_name) in rows.flatten() {
            map.entry(path).or_default().push(type_name);
        }
        map
    }

    /// Load FileRecords by reading every file from disk (fallback path).
    fn load_file_records_from_disk(&self, files: &[PathBuf]) -> Vec<FileRecord> {
        let mut records = Vec::new();

        for file_path in files {
            let rel_path = match file_path.strip_prefix(&self.root) {
                Ok(p) => p.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };

            let content = match std::fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let doc = parse_document(&content);
            let raw_frontmatter = match &doc.frontmatter {
                Some(serde_yaml::Value::Mapping(m)) => yaml_mapping_to_json(m),
                _ => serde_json::json!({}),
            };

            let type_names = self.determine_types_for_path(&raw_frontmatter, Some(&rel_path));
            let effective = self.apply_defaults(&raw_frontmatter, &type_names);
            let effective = self.coerce_types(&effective, &type_names);

            let metadata = std::fs::metadata(file_path).ok();
            let file_size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
            let file_mtime_iso = metadata.as_ref().and_then(|m| m.modified().ok()).map(|t| {
                let dt: chrono::DateTime<chrono::Utc> = t.into();
                dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
            });
            let file_ctime_iso = metadata.as_ref().and_then(|m| m.created().ok()).map(|t| {
                let dt: chrono::DateTime<chrono::Utc> = t.into();
                dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
            });

            records.push(FileRecord {
                rel_path,
                raw_frontmatter,
                effective_frontmatter: effective,
                body: doc.body,
                type_names,
                file_size,
                file_mtime_iso,
                file_ctime_iso,
            });
        }

        records
    }

    /// Load all query data: tries cache first, falls back to disk.
    /// Returns (file_records, all_files_data arc, backlinks_index arc).
    #[allow(dead_code)]
    pub(crate) fn load_query_data(&self) -> QueryData {
        self.load_query_data_profiled(false, true).0
    }

    /// Load query data with optional detailed timing.
    pub(crate) fn load_query_data_profiled(
        &self,
        profile: bool,
        include_link_graph: bool,
    ) -> (QueryData, Option<LoadQueryPerf>) {
        let total_start = Instant::now();
        let mut perf = LoadQueryPerf {
            built_link_graph: include_link_graph,
            ..LoadQueryPerf::default()
        };

        let try_open_start = Instant::now();
        let conn = self.try_open_cache();
        perf.try_open_cache_ms = elapsed_ms(try_open_start);

        let file_records = if let Some(conn) = conn {
            perf.cache_used = true;

            let refresh_start = Instant::now();
            self.refresh_cache(&conn);
            perf.refresh_cache_ms = elapsed_ms(refresh_start);

            let load_start = Instant::now();
            let records = self.load_file_records_from_cache(&conn);
            perf.load_records_ms = elapsed_ms(load_start);
            records
        } else {
            perf.cache_used = false;

            let scan_start = Instant::now();
            let files = self.scan_collection_files();
            perf.scan_files_ms = elapsed_ms(scan_start);

            let load_start = Instant::now();
            let records = self.load_file_records_from_disk(&files);
            perf.load_records_ms = elapsed_ms(load_start);
            records
        };

        perf.file_records = file_records.len();

        let (all_files_arc, backlinks_arc) = if include_link_graph {
            // Build all_files_data and backlinks index from the loaded records
            let all_files_start = Instant::now();
            let all_files_data: Vec<ResolvedFileData> = file_records
                .iter()
                .map(|r| ResolvedFileData {
                    path: r.rel_path.clone(),
                    frontmatter: r.effective_frontmatter.clone(),
                    body: r.body.clone(),
                })
                .collect();
            perf.build_all_files_ms = elapsed_ms(all_files_start);

            let backlinks_start = Instant::now();
            let all_files_arc = Arc::new(all_files_data);
            let (backlinks_index, backlinks_perf) =
                self.build_backlinks_index_profiled(&all_files_arc, profile);
            let backlinks_arc = Arc::new(backlinks_index);
            perf.build_backlinks_ms = elapsed_ms(backlinks_start);
            if let Some(bp) = backlinks_perf {
                perf.backlinks_frontmatter_extract_ms = bp.frontmatter_extract_ms;
                perf.backlinks_body_links_extract_ms = bp.body_links_extract_ms;
                perf.backlinks_body_embeds_extract_ms = bp.body_embeds_extract_ms;
                perf.backlinks_resolve_targets_ms = bp.resolve_targets_ms;
                perf.backlinks_files_processed = bp.files_processed;
                perf.backlinks_targets_scanned = bp.targets_scanned;
                perf.backlinks_resolve_calls = bp.resolve_calls;
            }
            (Some(all_files_arc), Some(backlinks_arc))
        } else {
            (None, None)
        };

        perf.total_ms = elapsed_ms(total_start);

        let data = (file_records, all_files_arc, backlinks_arc);
        if profile {
            (data, Some(perf))
        } else {
            (data, None)
        }
    }
}
