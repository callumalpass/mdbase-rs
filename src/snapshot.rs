//! Fallible, operation-scoped views of collection data.

mod discovery;

#[cfg(all(test, unix))]
pub(crate) use discovery::replace_descendant_on_scan_for_test;
#[cfg(test)]
pub(crate) use discovery::{
    cancel_scan_after_entries_for_test, reset_snapshot_scan_calls_for_test,
    snapshot_scan_calls_for_test,
};

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

use crate::expressions::evaluator::ResolvedFileData;
use crate::query::cache_source::{FileRecord, InvalidRecordStub};
use crate::record_load::RecordLoadOutcome;
use crate::{Collection, OperationCancellation};

/// A consistent set of records loaded for one read operation.
///
/// The SQLite cache may supply the records, but Markdown remains authoritative.
/// Cache failures are handled before this value is constructed. This existing
/// checked query boundary remains separate from authoritative operation capture.
pub(crate) struct CollectionSnapshot {
    pub records: Vec<FileRecord>,
    pub invalid_records: Vec<InvalidRecordStub>,
    pub all_files: Option<Arc<Vec<ResolvedFileData>>>,
    pub backlinks: Option<Arc<HashMap<String, Vec<String>>>>,
}

/// One record loaded at the authoritative filesystem boundary.
#[derive(Debug)]
pub(crate) struct AuthoritativeCollectionSnapshotEntry {
    outcome: RecordLoadOutcome,
}

impl AuthoritativeCollectionSnapshotEntry {
    pub(crate) fn relative_path(&self) -> &str {
        self.outcome.path()
    }

    pub(crate) fn outcome(&self) -> &RecordLoadOutcome {
        &self.outcome
    }

    pub(crate) fn parsed(&self) -> Option<crate::record_load::ParsedRecordView<'_>> {
        self.outcome.parsed()
    }

    pub(crate) fn invalid(&self) -> Option<crate::record_load::InvalidRecordView<'_>> {
        self.outcome.invalid()
    }

    pub(crate) fn raw_frontmatter(&self) -> Option<&serde_json::Value> {
        self.parsed().map(|record| record.raw_frontmatter)
    }

    pub(crate) fn effective_frontmatter(&self) -> Option<&serde_json::Value> {
        self.outcome.effective_frontmatter()
    }

    pub(crate) fn type_names(&self) -> &[String] {
        self.outcome.type_names()
    }

    pub(crate) fn body(&self) -> Option<&str> {
        self.outcome.body()
    }

    pub(crate) fn facts(&self) -> &crate::record_load::RecordFileFacts {
        self.outcome.facts()
    }

    pub(crate) fn had_bom(&self) -> Option<bool> {
        self.outcome.had_bom()
    }

    pub(crate) fn resolved_file_data(&self) -> Option<ResolvedFileData> {
        match &self.outcome {
            RecordLoadOutcome::Parsed {
                path,
                document,
                layout,
                effective_frontmatter,
                ..
            }
            | RecordLoadOutcome::Invalid {
                path,
                state:
                    crate::record_load::InvalidRecordState::Frontmatter {
                        document,
                        layout,
                        effective_frontmatter,
                        ..
                    },
                ..
            } => Some(ResolvedFileData {
                path: path.clone(),
                frontmatter: effective_frontmatter.clone(),
                body: layout.body(document).to_string(),
            }),
            RecordLoadOutcome::Invalid {
                state: crate::record_load::InvalidRecordState::InvalidUtf8,
                ..
            } => None,
        }
    }
}

/// One authoritative, operation-scoped filesystem observation.
pub(crate) struct AuthoritativeCollectionSnapshot {
    // Discovery order is stable and sorted; this side index makes repeated
    // operation planning lookups independent of collection size.
    entries: Vec<AuthoritativeCollectionSnapshotEntry>,
    path_to_index: HashMap<String, usize>,
    known_file_paths: Vec<String>,
}

impl AuthoritativeCollectionSnapshot {
    pub(crate) fn entries(&self) -> &[AuthoritativeCollectionSnapshotEntry] {
        &self.entries
    }

    pub(crate) fn entry(&self, path: &str) -> Option<&AuthoritativeCollectionSnapshotEntry> {
        #[cfg(all(test, feature = "legacy-collection-mutation"))]
        SNAPSHOT_ENTRY_LOOKUPS.with(|lookups| lookups.set(lookups.get() + 1));
        self.path_to_index
            .get(path)
            .and_then(|index| self.entries.get(*index))
    }

    pub(crate) fn resolved_files_data(&self) -> Vec<ResolvedFileData> {
        #[cfg(all(test, feature = "legacy-collection-mutation"))]
        SNAPSHOT_RESOLVED_PROJECTIONS.with(|builds| builds.set(builds.get() + 1));
        self.entries
            .iter()
            .filter_map(AuthoritativeCollectionSnapshotEntry::resolved_file_data)
            .collect()
    }

    pub(crate) fn link_resolution_index(
        &self,
        collection: &Collection,
    ) -> crate::links::resolver::LinkResolutionIndex {
        let resolved_files = self.resolved_files_data();
        self.link_resolution_index_from_resolved(collection, &resolved_files)
    }

    pub(crate) fn link_resolution_index_from_resolved(
        &self,
        collection: &Collection,
        resolved_files: &[ResolvedFileData],
    ) -> crate::links::resolver::LinkResolutionIndex {
        let mut index = collection.build_link_resolution_index(resolved_files);
        index.known_paths.extend(
            self.known_file_paths
                .iter()
                .filter(|path| collection.validate_file_path(path.as_str()).is_ok())
                .cloned(),
        );
        for entry in &self.entries {
            if entry.effective_frontmatter().is_some()
                && crate::api::CollectionPath::new(entry.relative_path()).is_ok()
            {
                index.types_by_path.insert(
                    entry.relative_path().to_string(),
                    entry.type_names().to_vec(),
                );
            }
            if entry.raw_frontmatter().is_none() {
                for keyed_paths in index
                    .id_lower_to_paths
                    .values_mut()
                    .chain(index.title_lower_to_paths.values_mut())
                {
                    keyed_paths.retain(|path| path != entry.relative_path());
                }
            }
        }
        index.id_lower_to_paths.retain(|_, paths| !paths.is_empty());
        index
            .title_lower_to_paths
            .retain(|_, paths| !paths.is_empty());
        index
    }
}

#[cfg(all(test, feature = "legacy-collection-mutation"))]
thread_local! {
    static SNAPSHOT_ENTRY_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SNAPSHOT_RESOLVED_PROJECTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(all(test, feature = "legacy-collection-mutation"))]
pub(crate) fn reset_snapshot_projection_counters_for_test() {
    SNAPSHOT_ENTRY_LOOKUPS.with(|value| value.set(0));
    SNAPSHOT_RESOLVED_PROJECTIONS.with(|value| value.set(0));
}

#[cfg(all(test, feature = "legacy-collection-mutation"))]
pub(crate) fn snapshot_entry_lookups_for_test() -> usize {
    SNAPSHOT_ENTRY_LOOKUPS.with(std::cell::Cell::get)
}

#[cfg(all(test, feature = "legacy-collection-mutation"))]
pub(crate) fn snapshot_resolved_projections_for_test() -> usize {
    SNAPSHOT_RESOLVED_PROJECTIONS.with(std::cell::Cell::get)
}

impl Collection {
    /// Capture using the caller runtime context when present, otherwise the
    /// finite compatibility context owned by a context-free API.
    pub(crate) fn capture_collection_snapshot_current(
        &self,
    ) -> Result<AuthoritativeCollectionSnapshot, SnapshotError> {
        let context = crate::runtime::OperationContext::current_or_legacy();
        self.capture_collection_snapshot_context(&context)
    }

    /// Budgeted authoritative capture used by runtime/canonical paths.
    pub(crate) fn capture_collection_snapshot_context(
        &self,
        context: &crate::runtime::OperationContext,
    ) -> Result<AuthoritativeCollectionSnapshot, SnapshotError> {
        context.check()?;
        let paths = self.scan_collection_all_relative_paths_context(context)?;
        context.check()?;
        let mut entries = Vec::new();
        entries.try_reserve(paths.len()).map_err(|_| {
            crate::runtime::ProviderError::from(crate::runtime::CaptureLimitExceeded {
                kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
                limit: usize::MAX as u64,
                attempted: paths.len() as u64,
            })
        })?;
        for relative_path in &paths {
            context.check()?;
            if self.validate_record_path(relative_path).is_err() {
                continue;
            }
            let display_path = self.root.join(relative_path);
            let outcome =
                crate::record_load::load_record_no_follow_context(self, relative_path, context)?
                    .ok_or_else(|| SnapshotError::Unavailable {
                        collection_path: relative_path.clone(),
                        filesystem_path: display_path,
                    })?;
            entries.push(AuthoritativeCollectionSnapshotEntry { outcome });
            context.check()?;
        }
        context.check()?;
        let path_to_index = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.relative_path().to_string(), index))
            .collect();
        Ok(AuthoritativeCollectionSnapshot {
            entries,
            path_to_index,
            known_file_paths: paths,
        })
    }

    /// Discover, no-follow open, read, and classify every record exactly once.
    /// This is a compatibility seam; runtime callers use the context variant.
    #[allow(dead_code)] // retained for explicit-token compatibility and cancellation tests
    pub(crate) fn capture_collection_snapshot(
        &self,
        cancellation: &OperationCancellation,
    ) -> Result<AuthoritativeCollectionSnapshot, SnapshotError> {
        if let Some(context) = crate::runtime::OperationContext::current() {
            return self.capture_collection_snapshot_context(&context);
        }
        cancellation.check().map_err(|_| SnapshotError::Cancelled)?;
        let paths = self.scan_collection_all_relative_paths_checked_cancellable(cancellation)?;
        let mut entries = Vec::new();
        for relative_path in &paths {
            if self.validate_record_path(relative_path).is_err() {
                continue;
            }
            cancellation.check().map_err(|_| SnapshotError::Cancelled)?;
            let display_path = self.root.join(relative_path);
            let outcome = match crate::record_load::load_record_no_follow_cancellable(
                self,
                relative_path,
                cancellation,
            ) {
                Ok(Some(outcome)) => outcome,
                Ok(None) => {
                    return Err(SnapshotError::Unavailable {
                        collection_path: relative_path.clone(),
                        filesystem_path: display_path,
                    });
                }
                Err(_) if cancellation.stop_reason().is_some() => {
                    return Err(SnapshotError::Cancelled);
                }
                Err(source) => {
                    return Err(SnapshotError::ReadFile {
                        collection_path: relative_path.clone(),
                        filesystem_path: display_path,
                        source,
                    });
                }
            };
            entries.push(AuthoritativeCollectionSnapshotEntry { outcome });
        }
        cancellation.check().map_err(|_| SnapshotError::Cancelled)?;
        let path_to_index = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.relative_path().to_string(), index))
            .collect();
        Ok(AuthoritativeCollectionSnapshot {
            entries,
            path_to_index,
            known_file_paths: paths,
        })
    }
}

#[derive(Debug, Error)]
pub(crate) enum CollectionScanError {
    #[error("collection operation cancelled")]
    Cancelled,
    #[error(transparent)]
    Provider(#[from] crate::runtime::ProviderError),
    #[error("failed to read collection directory '{}': {source}", path.display())]
    ReadDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to read an entry in collection directory '{}': {source}", directory.display())]
    ReadEntry {
        directory: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to inspect collection entry '{}': {source}", path.display())]
    InspectEntry {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("collection path is not valid UTF-8: {}", path.display())]
    NonUtf8Path { path: PathBuf },
}

/// Stable reason for a collection discovery failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CollectionDiscoveryCause {
    #[error("failed to read directory")]
    ReadDirectory(#[source] io::Error),
    #[error("failed to read directory entry")]
    ReadEntry(#[source] io::Error),
    #[error("failed to inspect directory entry")]
    InspectEntry(#[source] io::Error),
    #[error("path is not valid UTF-8")]
    NonUtf8Path,
    #[error("entry is outside the configured collection root")]
    OutsideRoot,
}

/// Failure while constructing a filesystem-backed collection snapshot.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CollectionSnapshotError {
    #[error("collection operation cancelled")]
    Cancelled,
    #[error("collection discovery failed at '{}': {cause}", filesystem_path.display())]
    Discovery {
        /// Unambiguous platform filesystem path involved in discovery.
        filesystem_path: PathBuf,
        #[source]
        cause: CollectionDiscoveryCause,
    },
    #[error("collection record '{}' is unavailable at '{}': no regular no-follow file was opened", collection_path, filesystem_path.display())]
    RecordUnavailable {
        /// Canonical collection-relative record path.
        collection_path: String,
        /// Platform filesystem path used only for diagnostics.
        filesystem_path: PathBuf,
    },
    #[error("failed to read collection record '{}' at '{}': {source}", collection_path, filesystem_path.display())]
    RecordRead {
        /// Canonical collection-relative record path.
        collection_path: String,
        /// Platform filesystem path used only for diagnostics.
        filesystem_path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("coordinated collection cache is unavailable: {reason}")]
    CacheUnavailable {
        /// Stable cache failure reason retained from the snapshot boundary.
        reason: String,
    },
}

impl CollectionSnapshotError {
    /// The platform filesystem path associated with the failure, when any.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Discovery {
                filesystem_path, ..
            }
            | Self::RecordUnavailable {
                filesystem_path, ..
            }
            | Self::RecordRead {
                filesystem_path, ..
            } => Some(filesystem_path),
            Self::Cancelled | Self::CacheUnavailable { .. } => None,
        }
    }

    /// Whether construction stopped because the caller cancelled the operation.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

#[derive(Debug, Error)]
pub(crate) enum SnapshotError {
    #[error("collection operation cancelled")]
    Cancelled,
    #[error(transparent)]
    Provider(#[from] crate::runtime::ProviderError),
    #[error("coordinated runtime cache is unavailable: {0}")]
    Cache(String),
    #[error(transparent)]
    Scan(CollectionScanError),
    #[error("collection entry is outside the configured root: {}", path.display())]
    OutsideRoot { path: PathBuf },
    #[error("collection path is not valid UTF-8: {}", path.display())]
    NonUtf8Path { path: PathBuf },
    #[error(
        "discovered collection record is no longer an available regular file: {collection_path}"
    )]
    #[allow(dead_code)] // explicit-token compatibility capture can still classify this race
    Unavailable {
        collection_path: String,
        filesystem_path: PathBuf,
    },
    #[error("failed to read collection file '{}' at '{}': {source}", collection_path, filesystem_path.display())]
    ReadFile {
        collection_path: String,
        filesystem_path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl SnapshotError {
    pub(crate) fn is_record_load_failure_for(&self, root: &Path, relative_path: &str) -> bool {
        let _ = root;
        matches!(
            self,
            Self::Unavailable {
                collection_path, ..
            }
                | Self::ReadFile {
                    collection_path, ..
                } if collection_path == relative_path
        )
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        match self {
            Self::Cancelled
            | Self::Provider(_)
            | Self::Cache(_)
            | Self::Scan(CollectionScanError::Cancelled)
            | Self::Scan(CollectionScanError::Provider(_)) => None,
            Self::Scan(CollectionScanError::ReadDirectory { path, .. })
            | Self::Scan(CollectionScanError::InspectEntry { path, .. })
            | Self::Scan(CollectionScanError::NonUtf8Path { path })
            | Self::OutsideRoot { path }
            | Self::NonUtf8Path { path } => Some(path),
            Self::ReadFile {
                filesystem_path, ..
            } => Some(filesystem_path),
            Self::Unavailable {
                filesystem_path, ..
            } => Some(filesystem_path),
            Self::Scan(CollectionScanError::ReadEntry { directory, .. }) => Some(directory),
        }
    }
}

impl From<CollectionScanError> for SnapshotError {
    fn from(error: CollectionScanError) -> Self {
        match error {
            CollectionScanError::Cancelled => Self::Cancelled,
            CollectionScanError::Provider(error) => Self::Provider(error),
            CollectionScanError::NonUtf8Path { path } => Self::NonUtf8Path { path },
            other => Self::Scan(other),
        }
    }
}

impl From<SnapshotError> for CollectionSnapshotError {
    fn from(error: SnapshotError) -> Self {
        match error {
            SnapshotError::Cancelled | SnapshotError::Scan(CollectionScanError::Cancelled) => {
                Self::Cancelled
            }
            SnapshotError::Provider(crate::runtime::ProviderError::OperationCancelled)
            | SnapshotError::Provider(crate::runtime::ProviderError::OperationDeadline) => {
                Self::Cancelled
            }
            SnapshotError::Provider(error) => Self::CacheUnavailable {
                reason: error.to_string(),
            },
            SnapshotError::Scan(CollectionScanError::ReadDirectory { path, source }) => {
                Self::Discovery {
                    filesystem_path: path,
                    cause: CollectionDiscoveryCause::ReadDirectory(source),
                }
            }
            SnapshotError::Scan(CollectionScanError::ReadEntry { directory, source }) => {
                Self::Discovery {
                    filesystem_path: directory,
                    cause: CollectionDiscoveryCause::ReadEntry(source),
                }
            }
            SnapshotError::Scan(CollectionScanError::InspectEntry { path, source }) => {
                Self::Discovery {
                    filesystem_path: path,
                    cause: CollectionDiscoveryCause::InspectEntry(source),
                }
            }
            SnapshotError::Scan(CollectionScanError::NonUtf8Path { path })
            | SnapshotError::NonUtf8Path { path } => Self::Discovery {
                filesystem_path: path,
                cause: CollectionDiscoveryCause::NonUtf8Path,
            },
            SnapshotError::OutsideRoot { path } => Self::Discovery {
                filesystem_path: path,
                cause: CollectionDiscoveryCause::OutsideRoot,
            },
            SnapshotError::Cache(reason) => Self::CacheUnavailable { reason },
            SnapshotError::Scan(CollectionScanError::Provider(error)) => Self::CacheUnavailable {
                reason: error.to_string(),
            },
            SnapshotError::Unavailable {
                collection_path,
                filesystem_path,
            } => Self::RecordUnavailable {
                collection_path,
                filesystem_path,
            },
            SnapshotError::ReadFile {
                collection_path,
                filesystem_path,
                source,
            } => Self::RecordRead {
                collection_path,
                filesystem_path,
                source,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Collection) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\n",
        )
        .unwrap();
        let collection = Collection::open(directory.path()).unwrap();
        (directory, collection)
    }

    #[test]
    fn public_snapshot_errors_retain_typed_causes_paths_and_io_sources() {
        let filesystem_path = PathBuf::from("/collection/record.md");
        let error = CollectionSnapshotError::from(SnapshotError::ReadFile {
            collection_path: "nested/record.md".to_string(),
            filesystem_path: filesystem_path.clone(),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        });
        match error {
            CollectionSnapshotError::RecordRead {
                collection_path,
                filesystem_path: found,
                source,
            } => {
                assert_eq!(collection_path, "nested/record.md");
                assert_eq!(found, filesystem_path);
                assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
            }
            other => panic!("unexpected snapshot error: {other:?}"),
        }

        let cancelled = CollectionSnapshotError::from(SnapshotError::Cancelled);
        assert!(cancelled.is_cancelled());
        assert!(cancelled.path().is_none());
    }

    #[test]
    fn malformed_record_is_local_and_valid_sibling_is_retained() {
        let (directory, collection) = fixture();
        std::fs::write(
            directory.path().join("valid.md"),
            "---\ntitle: Valid\n---\nBody\n",
        )
        .unwrap();
        std::fs::write(directory.path().join("broken.md"), "---\na: [broken\n---\n").unwrap();
        let snapshot = collection
            .capture_collection_snapshot(&OperationCancellation::new())
            .unwrap();
        assert_eq!(snapshot.entries().len(), 2);
        assert_eq!(
            snapshot
                .entries()
                .iter()
                .filter(|entry| matches!(entry.outcome(), RecordLoadOutcome::Invalid { .. }))
                .count(),
            1
        );
        assert!(snapshot.entries().iter().any(|entry| {
            entry.relative_path() == "valid.md" && entry.outcome().body() == Some("Body\n")
        }));
    }

    #[test]
    fn nested_collection_marker_prunes_child_records() {
        let (directory, collection) = fixture();
        std::fs::write(directory.path().join("visible.md"), "visible\n").unwrap();
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        std::fs::write(nested.join("hidden.md"), "hidden\n").unwrap();
        let snapshot = collection
            .capture_collection_snapshot(&OperationCancellation::new())
            .unwrap();
        assert_eq!(snapshot.entries().len(), 1);
        assert_eq!(snapshot.entries()[0].relative_path(), "visible.md");
    }

    #[test]
    fn cancellation_during_scan_and_chunked_read_is_explicit() {
        let (directory, collection) = fixture();
        std::fs::write(directory.path().join("large.md"), vec![b'x'; 150_000]).unwrap();
        crate::cancel_scan_after_entries_for_test(Some(1));
        let cancellation = OperationCancellation::new();
        assert!(matches!(
            collection.capture_collection_snapshot(&cancellation),
            Err(SnapshotError::Cancelled)
        ));

        crate::record_load::cancel_after_read_chunks_for_test(Some(1));
        let cancellation = OperationCancellation::new();
        assert!(matches!(
            collection.capture_collection_snapshot(&cancellation),
            Err(SnapshotError::Cancelled)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn held_root_capability_survives_path_replacement_without_redirecting() {
        use std::os::unix::fs::symlink;

        let (directory, collection) = fixture();
        std::fs::write(directory.path().join("record.md"), "original\n").unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::write(external.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        std::fs::write(external.path().join("record.md"), "external\n").unwrap();
        let original_path = directory.path().to_path_buf();
        let held_path = original_path.with_extension("held-root");
        std::fs::rename(&original_path, &held_path).unwrap();
        symlink(external.path(), &original_path).unwrap();

        let snapshot = collection
            .capture_collection_snapshot(&OperationCancellation::new())
            .unwrap();
        assert_eq!(snapshot.entries()[0].outcome().body(), Some("original\n"));
        std::fs::remove_file(&original_path).unwrap();
        std::fs::rename(held_path, original_path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn opened_descendant_capability_never_enumerates_replacement_symlink() {
        let (directory, collection) = fixture();
        let child = directory.path().join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("original.md"), "original\n").unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::write(external.path().join("external.md"), "external\n").unwrap();
        crate::replace_descendant_on_scan_for_test(directory.path(), "child", external.path());

        let paths = collection
            .scan_collection_relative_paths_checked_cancellable(&OperationCancellation::new())
            .unwrap();
        assert!(paths.iter().any(|path| path == "child/original.md"));
        assert!(!paths.iter().any(|path| path.contains("external")));
    }

    #[cfg(unix)]
    #[test]
    fn replacement_with_symlink_is_rejected_no_follow() {
        let (directory, collection) = fixture();
        let record = directory.path().join("record.md");
        std::fs::write(&record, "inside\n").unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside\n").unwrap();
        crate::operations::replace_record_with_symlink_on_open_for_test(
            directory.path(),
            "record.md",
            outside.path(),
        );
        let error = collection
            .capture_collection_snapshot(&OperationCancellation::new())
            .err()
            .expect("replacement must fail");
        assert!(matches!(error, SnapshotError::Unavailable { .. }));
    }

    // Darwin filesystems reject this byte sequence before mdbase can observe it.
    #[cfg(target_os = "linux")]
    #[test]
    fn invalid_utf8_record_path_is_explicit() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let (directory, collection) = fixture();
        let path = directory
            .path()
            .join(OsString::from_vec(b"invalid-\xff.md".to_vec()));
        std::fs::write(&path, "content\n").unwrap();
        let error = collection
            .capture_collection_snapshot(&OperationCancellation::new())
            .err()
            .unwrap();
        assert!(matches!(error, SnapshotError::NonUtf8Path { path: found } if found == path));
    }
}
