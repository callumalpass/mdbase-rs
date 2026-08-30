//! Cache-backed data source for the query engine.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use rusqlite::{params_from_iter, Connection, TransactionBehavior};

use crate::cache::{indexer, sqlite, staleness, CacheError};
use crate::expressions::evaluator::ResolvedFileData;
use crate::record_load::RecordLoadOutcome;
use crate::snapshot::{CollectionSnapshot, SnapshotError};
use crate::{Collection, OperationCancellation};

pub(crate) struct MetadataPage {
    pub records: Vec<LocalRecord>,
    pub total: usize,
    pub performance: LoadQueryPerf,
}

/// Parsed record data used by the ordinary query engine.
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

/// Bounded representation of a malformed local record. It deliberately has no
/// frontmatter or body and cannot enter the ordinary candidate pipeline.
pub(crate) struct InvalidRecordStub {
    pub rel_path: String,
    pub type_names: Vec<String>,
    pub file_size: u64,
    pub file_mtime_iso: Option<String>,
    pub file_ctime_iso: Option<String>,
    pub source_revision: String,
    pub reason: String,
}

pub(crate) enum LocalRecord {
    Parsed(FileRecord),
    Invalid(InvalidRecordStub),
}

impl LocalRecord {
    pub(crate) fn path(&self) -> &str {
        match self {
            Self::Parsed(record) => &record.rel_path,
            Self::Invalid(stub) => &stub.rel_path,
        }
    }

    pub(crate) fn types(&self) -> &[String] {
        match self {
            Self::Parsed(record) => &record.type_names,
            Self::Invalid(stub) => &stub.type_names,
        }
    }
}

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
    pub cache_fallback: bool,
    pub built_link_graph: bool,
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn replace_cache_snapshot(conn: &Connection) -> Result<String, CacheError> {
    let snapshot = uuid::Uuid::new_v4().simple().to_string();
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('query_snapshot', ?1)",
        [&snapshot],
    )?;
    Ok(snapshot)
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
    fn try_open_cache(&self) -> Result<Option<Connection>, CacheError> {
        let db_path = self
            .held_root()
            .cache_storage_path()
            .join(&self.settings.cache_folder)
            .join("cache.db");
        if !db_path.exists() {
            return Ok(None);
        }
        Ok(Some(sqlite::open_cache_db(
            self.held_root().cache_storage_path(),
            &self.settings.cache_folder,
        )?))
    }

    /// Incremental freshness check: find stale/new/deleted files, re-index only
    /// what changed. Returns the full list of disk files (used for fallback and
    /// to know which paths exist).
    fn refresh_cache(
        &self,
        conn: &mut Connection,
        cancellation: &OperationCancellation,
    ) -> Result<Vec<PathBuf>, CacheError> {
        let disk_files = self.scan_collection_files_checked()?;

        // Cache refresh errors deliberately fall back to disk. Treating
        // cancellation as an ordinary cache error would therefore continue
        // the expensive operation, so callers check the token immediately
        // after every refresh attempt.
        if cancellation.is_cancelled() {
            return Err(CacheError::Cancelled);
        }

        let changes = staleness::find_changes(conn, &self.root, &disk_files)?;

        if changes.stale.is_empty() && changes.deleted.is_empty() {
            return Ok(disk_files);
        }

        // A provider can execute independent queries concurrently. Serialize
        // only the uncommon cache-write section, then recompute against the
        // winning transaction so two refreshers cannot apply stale deltas.
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changes = staleness::find_changes(&transaction, &self.root, &disk_files)?;

        // Remove deleted files from cache
        for rel_path in &changes.deleted {
            if cancellation.is_cancelled() {
                return Err(CacheError::Cancelled);
            }
            indexer::remove_file(&transaction, rel_path)?;
        }

        // Re-index stale/new files
        for abs_path in &changes.stale {
            if cancellation.is_cancelled() {
                return Err(CacheError::Cancelled);
            }
            let rel_path = abs_path
                .strip_prefix(&self.root)
                .map_err(|_| CacheError::OutsideRoot(abs_path.display().to_string()))?
                .to_string_lossy()
                .replace('\\', "/");
            indexer::reindex_file(&transaction, self, &rel_path)?;
        }

        if !changes.stale.is_empty() || !changes.deleted.is_empty() {
            replace_cache_snapshot(&transaction)?;
        }

        transaction.commit()?;
        Ok(disk_files)
    }

    /// Bulk-load FileRecords from the cache database.
    fn load_file_records_from_cache(
        &self,
        conn: &Connection,
        cancellation: &OperationCancellation,
        include_bodies: bool,
    ) -> Result<Vec<LocalRecord>, CacheError> {
        let body = if include_bodies { "f.body" } else { "''" };
        let mut stmt = conn.prepare(&format!(
            "SELECT f.path, f.frontmatter_json, f.effective_json, {body}, f.size, f.mtime_ns, f.ctime_ns, f.source_revision, f.failure_reason \
             FROM files f"
        ))?;

        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })?;

        // Pre-load all file_types into a map: path -> Vec<type_name>
        let types_map = self.load_file_types_map(conn)?;

        let mut records = Vec::new();
        for row in rows {
            if cancellation.is_cancelled() {
                return Err(CacheError::Cancelled);
            }
            let (
                path,
                fm_json_str,
                eff_json_str,
                body,
                size,
                mtime_ns,
                ctime_ns,
                source_revision,
                invalid_reason,
            ) = row?;

            let file_mtime_iso = (mtime_ns != 0).then(|| ns_to_iso(mtime_ns));
            let file_ctime_iso = ctime_ns.map(ns_to_iso);
            let mut type_names = types_map.get(&path).cloned().unwrap_or_default();
            if let Some(reason) = invalid_reason {
                records.push(LocalRecord::Invalid(InvalidRecordStub {
                    rel_path: path,
                    type_names,
                    file_size: size as u64,
                    file_mtime_iso,
                    file_ctime_iso,
                    source_revision,
                    reason,
                }));
            } else {
                let raw_frontmatter: serde_json::Value = serde_json::from_str(&fm_json_str)?;
                let effective_frontmatter: serde_json::Value = match eff_json_str {
                    Some(value) => serde_json::from_str(&value)?,
                    None => raw_frontmatter.clone(),
                };
                if type_names.is_empty() {
                    // Older cache DBs can have files rows without file_types rows.
                    type_names = self.determine_types_for_path(&raw_frontmatter, Some(&path));
                }
                records.push(LocalRecord::Parsed(FileRecord {
                    rel_path: path,
                    raw_frontmatter,
                    effective_frontmatter,
                    body,
                    type_names,
                    file_size: size as u64,
                    file_mtime_iso,
                    file_ctime_iso,
                }));
            }
        }

        Ok(records)
    }

    /// Refresh the cache, then let SQLite order and paginate metadata-only
    /// queries before record payloads are decoded. Returns `None` when the
    /// requested order cannot be represented exactly or no cache exists.
    pub(crate) fn load_query_metadata_page_profiled_cancellable(
        &self,
        types: &[String],
        order_by: &[(&str, bool)],
        offset: u64,
        limit: Option<u64>,
        cancellation: &OperationCancellation,
    ) -> Option<MetadataPage> {
        self.load_query_metadata_page_inner(types, order_by, offset, limit, cancellation, true)
    }

    pub(crate) fn load_runtime_query_metadata_page_profiled_cancellable(
        &self,
        types: &[String],
        order_by: &[(&str, bool)],
        offset: u64,
        limit: Option<u64>,
        cancellation: &OperationCancellation,
    ) -> Option<MetadataPage> {
        self.load_query_metadata_page_inner(types, order_by, offset, limit, cancellation, false)
    }

    fn load_query_metadata_page_inner(
        &self,
        types: &[String],
        order_by: &[(&str, bool)],
        offset: u64,
        limit: Option<u64>,
        cancellation: &OperationCancellation,
        refresh_from_filesystem: bool,
    ) -> Option<MetadataPage> {
        let mut clauses = Vec::new();
        for (field, descending) in order_by {
            let direction = if *descending { "DESC" } else { "ASC" };
            let clause = match *field {
                "file.path" => format!("f.path {direction}"),
                "file.size" => format!("f.size {direction}"),
                "file.mtime" => format!(
                    "(f.mtime_ns = 0) {}, (f.mtime_ns / 1000000000) {direction}",
                    if *descending { "DESC" } else { "ASC" }
                ),
                "file.ctime" => format!(
                    "(f.ctime_ns IS NULL) {}, (f.ctime_ns / 1000000000) {direction}",
                    if *descending { "DESC" } else { "ASC" }
                ),
                _ => return None,
            };
            clauses.push(clause);
        }
        clauses.push("f.path ASC".to_string());

        let total_started = Instant::now();
        let mut perf = LoadQueryPerf::default();
        let open_started = Instant::now();
        let mut conn = sqlite::open_cache_db(
            self.held_root().cache_storage_path(),
            &self.settings.cache_folder,
        )
        .ok()?;
        perf.try_open_cache_ms = elapsed_ms(open_started);
        perf.cache_used = true;

        if refresh_from_filesystem {
            let refresh_started = Instant::now();
            self.refresh_cache(&mut conn, cancellation).ok()?;
            if cancellation.is_cancelled() {
                return None;
            }
            perf.refresh_cache_ms = elapsed_ms(refresh_started);
        }

        let load_started = Instant::now();
        let normalized_types = types
            .iter()
            .map(|value| value.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let type_filter = if normalized_types.is_empty() {
            String::new()
        } else {
            let placeholders = vec!["?"; normalized_types.len()].join(", ");
            format!(
                " AND EXISTS (SELECT 1 FROM file_types ft \
                 WHERE ft.path = f.path AND lower(ft.type_name) IN ({placeholders}))"
            )
        };
        let count_sql = format!("SELECT COUNT(*) FROM files f WHERE 1 = 1{type_filter}");
        let total = match conn.query_row(
            &count_sql,
            params_from_iter(normalized_types.iter()),
            |row| row.get::<_, usize>(0),
        ) {
            Ok(total) => total,
            Err(_) => return None,
        };
        let sql = format!(
            "SELECT f.path, f.frontmatter_json, f.body, f.size, f.mtime_ns, f.ctime_ns, f.source_revision, f.failure_reason \
             FROM files f WHERE 1 = 1{} ORDER BY {} LIMIT ? OFFSET ?",
            type_filter,
            clauses.join(", ")
        );
        let mut statement = match conn.prepare(&sql) {
            Ok(statement) => statement,
            Err(_) => return None,
        };
        let limit = limit
            .map(|value| value.min(i64::MAX as u64) as i64)
            .unwrap_or(-1);
        let offset = offset.min(i64::MAX as u64) as i64;
        let mut parameters = normalized_types
            .into_iter()
            .map(rusqlite::types::Value::Text)
            .collect::<Vec<_>>();
        parameters.push(rusqlite::types::Value::Integer(limit));
        parameters.push(rusqlite::types::Value::Integer(offset));
        let rows = match statement.query_map(params_from_iter(parameters), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        }) {
            Ok(rows) => rows,
            Err(_) => return None,
        };
        let mut records = Vec::new();
        for row in rows {
            if cancellation.is_cancelled() {
                return None;
            }
            let (
                path,
                frontmatter,
                body,
                size,
                mtime_ns,
                ctime_ns,
                source_revision,
                invalid_reason,
            ) = match row {
                Ok(row) => row,
                Err(_) => return None,
            };
            let type_names = self.load_types_for_path(&conn, &path).ok()?;
            let common_mtime = (mtime_ns != 0).then(|| ns_to_iso(mtime_ns));
            let common_ctime = ctime_ns.map(ns_to_iso);
            if let Some(reason) = invalid_reason {
                records.push(LocalRecord::Invalid(InvalidRecordStub {
                    rel_path: path,
                    type_names,
                    file_size: size as u64,
                    file_mtime_iso: common_mtime,
                    file_ctime_iso: common_ctime,
                    source_revision,
                    reason,
                }));
            } else {
                let raw_frontmatter: serde_json::Value = serde_json::from_str(&frontmatter).ok()?;
                records.push(LocalRecord::Parsed(FileRecord {
                    rel_path: path,
                    effective_frontmatter: raw_frontmatter.clone(),
                    raw_frontmatter,
                    body,
                    type_names,
                    file_size: size as u64,
                    file_mtime_iso: common_mtime,
                    file_ctime_iso: common_ctime,
                }));
            }
        }
        perf.load_records_ms = elapsed_ms(load_started);
        perf.file_records = records.len();
        perf.total_ms = elapsed_ms(total_started);
        Some(MetadataPage {
            records,
            total,
            performance: perf,
        })
    }

    /// Load the file_types table into a HashMap for bulk access.
    fn load_file_types_map(
        &self,
        conn: &Connection,
    ) -> Result<HashMap<String, Vec<String>>, CacheError> {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        let mut stmt = conn.prepare("SELECT path, type_name FROM file_types")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (path, type_name) = row?;
            map.entry(path).or_default().push(type_name);
        }
        Ok(map)
    }

    fn load_types_for_path(
        &self,
        conn: &Connection,
        path: &str,
    ) -> Result<Vec<String>, CacheError> {
        let mut statement = conn.prepare("SELECT type_name FROM file_types WHERE path = ?1")?;
        let rows = statement.query_map([path], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(CacheError::from)
    }

    /// Load local records by reading every file from disk (fallback path).
    fn load_file_records_from_disk(
        &self,
        files: &[PathBuf],
        cancellation: &OperationCancellation,
        include_bodies: bool,
    ) -> Result<Vec<LocalRecord>, SnapshotError> {
        let mut records = Vec::new();

        for file_path in files {
            if cancellation.is_cancelled() {
                return Err(SnapshotError::Cancelled);
            }
            let rel_path = file_path
                .strip_prefix(&self.root)
                .map_err(|_| SnapshotError::OutsideRoot {
                    path: file_path.clone(),
                })?
                .to_string_lossy()
                .replace('\\', "/");

            let outcome = crate::record_load::load_record(self, &rel_path).map_err(|source| {
                SnapshotError::ReadFile {
                    collection_path: rel_path.clone(),
                    filesystem_path: file_path.clone(),
                    source,
                }
            })?;
            let facts = outcome.facts();
            let file_mtime_iso = (facts.mtime_ns != 0).then(|| ns_to_iso(facts.mtime_ns));
            let file_ctime_iso = facts.ctime_ns.map(ns_to_iso);
            let file_size = facts.size;
            let source_revision = facts.revision.clone();
            let record = match outcome {
                RecordLoadOutcome::Parsed {
                    raw_frontmatter,
                    effective_frontmatter,
                    document,
                    layout,
                    type_names,
                    ..
                } => LocalRecord::Parsed(FileRecord {
                    rel_path,
                    raw_frontmatter,
                    effective_frontmatter,
                    body: if include_bodies {
                        layout.body(&document).to_string()
                    } else {
                        String::new()
                    },
                    type_names,
                    file_size,
                    file_mtime_iso,
                    file_ctime_iso,
                }),
                RecordLoadOutcome::Invalid {
                    type_names, state, ..
                } => LocalRecord::Invalid(InvalidRecordStub {
                    rel_path,
                    type_names,
                    file_size,
                    file_mtime_iso,
                    file_ctime_iso,
                    source_revision,
                    reason: state.reason().as_str().to_string(),
                }),
            };
            records.push(record);
        }

        Ok(records)
    }

    /// Load one operation-scoped snapshot, using the derived cache only when it
    /// can be refreshed and decoded completely.
    pub(crate) fn load_query_data_profiled(
        &self,
        profile: bool,
        include_link_graph: bool,
    ) -> Result<(CollectionSnapshot, Option<LoadQueryPerf>), SnapshotError> {
        let context = crate::runtime::OperationContext::current_or_legacy();
        self.load_query_data_profiled_cancellable(
            profile,
            include_link_graph,
            context.cancellation(),
        )
    }

    pub(crate) fn load_query_data_profiled_cancellable(
        &self,
        profile: bool,
        include_link_graph: bool,
        cancellation: &OperationCancellation,
    ) -> Result<(CollectionSnapshot, Option<LoadQueryPerf>), SnapshotError> {
        self.load_query_data_inner(profile, include_link_graph, true, cancellation, true)
    }

    pub(crate) fn load_runtime_query_data_profiled_cancellable(
        &self,
        profile: bool,
        include_link_graph: bool,
        include_bodies: bool,
        cancellation: &OperationCancellation,
    ) -> Result<(CollectionSnapshot, Option<LoadQueryPerf>), SnapshotError> {
        self.load_query_data_inner(
            profile,
            include_link_graph,
            include_bodies,
            cancellation,
            false,
        )
    }

    pub(crate) fn load_query_data_profiled_cancellable_with_bodies(
        &self,
        profile: bool,
        include_link_graph: bool,
        include_bodies: bool,
        cancellation: &OperationCancellation,
    ) -> Result<(CollectionSnapshot, Option<LoadQueryPerf>), SnapshotError> {
        self.load_query_data_inner(
            profile,
            include_link_graph,
            include_bodies,
            cancellation,
            true,
        )
    }

    fn load_query_data_inner(
        &self,
        profile: bool,
        include_link_graph: bool,
        include_bodies: bool,
        cancellation: &OperationCancellation,
        refresh_from_filesystem: bool,
    ) -> Result<(CollectionSnapshot, Option<LoadQueryPerf>), SnapshotError> {
        cancellation.check().map_err(|_| SnapshotError::Cancelled)?;
        let total_start = Instant::now();
        let mut perf = LoadQueryPerf {
            built_link_graph: include_link_graph,
            ..LoadQueryPerf::default()
        };

        let try_open_start = Instant::now();
        let cache = self.try_open_cache();
        perf.try_open_cache_ms = elapsed_ms(try_open_start);

        let mut cached_records = None;
        let mut coordinated_cache_error = None;
        match cache {
            Ok(Some(mut conn)) => {
                let refresh = if refresh_from_filesystem {
                    let refresh_start = Instant::now();
                    let result = self.refresh_cache(&mut conn, cancellation);
                    perf.refresh_cache_ms = elapsed_ms(refresh_start);
                    result.map(|_| ())
                } else {
                    Ok(())
                };
                cancellation.check().map_err(|_| SnapshotError::Cancelled)?;
                match refresh {
                    Ok(_) => {
                        let load_start = Instant::now();
                        let loaded =
                            self.load_file_records_from_cache(&conn, cancellation, include_bodies);
                        perf.load_records_ms = elapsed_ms(load_start);
                        match loaded {
                            Ok(records) => {
                                perf.cache_used = true;
                                cached_records = Some(records);
                            }
                            Err(CacheError::Cancelled)
                            | Err(CacheError::Scan(
                                crate::snapshot::CollectionScanError::Cancelled,
                            )) => return Err(SnapshotError::Cancelled),
                            Err(CacheError::Scan(
                                crate::snapshot::CollectionScanError::Provider(error),
                            )) => return Err(SnapshotError::Provider(error)),
                            Err(error) => {
                                perf.cache_fallback = true;
                                coordinated_cache_error = Some(error.to_string());
                            }
                        }
                    }
                    Err(CacheError::Cancelled)
                    | Err(CacheError::Scan(crate::snapshot::CollectionScanError::Cancelled)) => {
                        return Err(SnapshotError::Cancelled)
                    }
                    Err(CacheError::Scan(crate::snapshot::CollectionScanError::Provider(
                        error,
                    ))) => return Err(SnapshotError::Provider(error)),
                    Err(error) => {
                        perf.cache_fallback = true;
                        coordinated_cache_error = Some(error.to_string());
                    }
                }
            }
            Ok(None) => coordinated_cache_error = Some("cache database is missing".to_string()),
            Err(error) => {
                perf.cache_fallback = true;
                coordinated_cache_error = Some(error.to_string());
            }
        }

        if !refresh_from_filesystem && cached_records.is_none() {
            return Err(SnapshotError::Cache(
                coordinated_cache_error
                    .unwrap_or_else(|| "cache records could not be loaded".to_string()),
            ));
        }

        let file_records = if let Some(records) = cached_records {
            records
        } else {
            let scan_start = Instant::now();
            let files = self.scan_collection_files_checked()?;
            perf.scan_files_ms = elapsed_ms(scan_start);

            let load_start = Instant::now();
            let records = self.load_file_records_from_disk(&files, cancellation, include_bodies)?;
            perf.load_records_ms = elapsed_ms(load_start);
            records
        };

        perf.file_records = file_records.len();
        cancellation.check().map_err(|_| SnapshotError::Cancelled)?;
        let mut parsed_records = Vec::new();
        let mut invalid_records = Vec::new();
        for record in file_records {
            match record {
                LocalRecord::Parsed(record) => parsed_records.push(record),
                LocalRecord::Invalid(stub) => invalid_records.push(stub),
            }
        }

        let (all_files_arc, backlinks_arc) = if include_link_graph {
            // Build all_files_data and backlinks index from the loaded records
            let all_files_start = Instant::now();
            let all_files_data: Vec<ResolvedFileData> = parsed_records
                .iter()
                .take_while(|_| !cancellation.is_cancelled())
                .map(|record| ResolvedFileData {
                    path: record.rel_path.clone(),
                    frontmatter: record.effective_frontmatter.clone(),
                    body: record.body.clone(),
                })
                .collect();
            cancellation.check().map_err(|_| SnapshotError::Cancelled)?;
            perf.build_all_files_ms = elapsed_ms(all_files_start);

            let backlinks_start = Instant::now();
            let all_files_arc = Arc::new(all_files_data);
            let cached_backlinks = (perf.cache_used && !refresh_from_filesystem).then(|| {
                sqlite::open_cache_db(
                    self.held_root().cache_storage_path(),
                    &self.settings.cache_folder,
                )
                .map_err(CacheError::from)
                .and_then(|connection| indexer::load_backlinks(&connection))
            });
            let (backlinks_index, backlinks_perf) = match cached_backlinks {
                Some(Ok(backlinks)) => (backlinks, None),
                _ => self.build_backlinks_index_profiled(&all_files_arc, profile),
            };
            cancellation.check().map_err(|_| SnapshotError::Cancelled)?;
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

        let snapshot = CollectionSnapshot {
            records: parsed_records,
            invalid_records,
            all_files: all_files_arc,
            backlinks: backlinks_arc,
        };
        if profile {
            Ok((snapshot, Some(perf)))
        } else {
            Ok((snapshot, None))
        }
    }
}
