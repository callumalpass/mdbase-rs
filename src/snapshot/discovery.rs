use std::path::PathBuf;

#[cfg(all(test, unix))]
use std::path::Path;

use crate::runtime::{OperationContext, ProviderError};
use crate::{Collection, OperationCancellation};

struct ScanState<'a> {
    records_only: bool,
    context: Option<&'a OperationContext>,
    discovered: u64,
}

impl Collection {
    /// Scan all Markdown files in the collection.
    ///
    /// Discovery failures are explicit so callers cannot confuse an
    /// incomplete collection with an empty or smaller one.
    pub(crate) fn scan_collection_files_checked(
        &self,
    ) -> Result<Vec<PathBuf>, crate::snapshot::CollectionScanError> {
        let context = OperationContext::current_or_legacy();
        self.scan_collection_files_checked_cancellable(context.cancellation())
    }

    pub(crate) fn scan_collection_files_checked_cancellable(
        &self,
        cancellation: &OperationCancellation,
    ) -> Result<Vec<PathBuf>, crate::snapshot::CollectionScanError> {
        self.scan_collection_relative_paths_checked_cancellable(cancellation)
            .map(|paths| paths.into_iter().map(|path| self.root.join(path)).collect())
    }

    pub(crate) fn scan_collection_relative_paths_checked_cancellable(
        &self,
        cancellation: &OperationCancellation,
    ) -> Result<Vec<String>, crate::snapshot::CollectionScanError> {
        if let Some(context) = OperationContext::current() {
            return self
                .scan_collection_relative_paths_mode_context(&context, true)
                .map_err(crate::snapshot::CollectionScanError::Provider);
        }
        self.scan_collection_relative_paths_mode(cancellation, true)
    }

    #[allow(dead_code)] // retained by the explicit-token compatibility capture
    pub(crate) fn scan_collection_all_relative_paths_checked_cancellable(
        &self,
        cancellation: &OperationCancellation,
    ) -> Result<Vec<String>, crate::snapshot::CollectionScanError> {
        if let Some(context) = OperationContext::current() {
            return self
                .scan_collection_relative_paths_mode_context(&context, false)
                .map_err(crate::snapshot::CollectionScanError::Provider);
        }
        self.scan_collection_relative_paths_mode(cancellation, false)
    }

    pub(crate) fn scan_collection_all_relative_paths_context(
        &self,
        context: &OperationContext,
    ) -> Result<Vec<String>, ProviderError> {
        self.scan_collection_relative_paths_mode_context(context, false)
    }

    pub(crate) fn scan_collection_relative_paths_context(
        &self,
        context: &OperationContext,
    ) -> Result<Vec<String>, ProviderError> {
        self.scan_collection_relative_paths_mode_context(context, true)
    }

    fn scan_collection_relative_paths_mode(
        &self,
        cancellation: &OperationCancellation,
        records_only: bool,
    ) -> Result<Vec<String>, crate::snapshot::CollectionScanError> {
        #[cfg(test)]
        SNAPSHOT_SCAN_CALLS.with(|calls| calls.set(calls.get() + 1));
        let root = self.root_capability().map_err(|source| {
            crate::snapshot::CollectionScanError::ReadDirectory {
                path: self.root.clone(),
                source,
            }
        })?;
        let mut files = Vec::new();
        let mut state = ScanState {
            records_only,
            context: None,
            discovered: 0,
        };
        self.scan_dir_recursive_checked(&root, "", &mut files, cancellation, 0, &mut state)?;
        cancellation
            .check()
            .map_err(|_| crate::snapshot::CollectionScanError::Cancelled)?;
        files.sort();
        Ok(files)
    }

    fn scan_collection_relative_paths_mode_context(
        &self,
        context: &OperationContext,
        records_only: bool,
    ) -> Result<Vec<String>, ProviderError> {
        #[cfg(test)]
        SNAPSHOT_SCAN_CALLS.with(|calls| calls.set(calls.get() + 1));
        context.check()?;
        let root = self.root_capability().map_err(|error| {
            ProviderError::CollectionOpen(format!("failed to read collection directory: {error}"))
        })?;
        let mut files = Vec::new();
        let mut state = ScanState {
            records_only,
            context: Some(context),
            discovered: 0,
        };
        self.scan_dir_recursive_checked(
            &root,
            "",
            &mut files,
            context.cancellation(),
            0,
            &mut state,
        )
        .map_err(|error| match error {
            crate::snapshot::CollectionScanError::Provider(error) => error,
            crate::snapshot::CollectionScanError::Cancelled => context
                .check()
                .err()
                .unwrap_or(ProviderError::OperationCancelled),
            other => ProviderError::CollectionOpen(other.to_string()),
        })?;
        context.check()?;
        files.sort();
        Ok(files)
    }

    fn scan_dir_recursive_checked(
        &self,
        directory: &cap_std::fs::Dir,
        prefix: &str,
        files: &mut Vec<String>,
        cancellation: &OperationCancellation,
        depth: u64,
        state: &mut ScanState<'_>,
    ) -> Result<(), crate::snapshot::CollectionScanError> {
        use crate::snapshot::CollectionScanError;
        use cap_fs_ext::DirExt;

        cancellation
            .check()
            .map_err(|_| CollectionScanError::Cancelled)?;
        if let Some(context) = state.context {
            context
                .check_depth(depth)
                .map_err(CollectionScanError::Provider)?;
        }
        let display_directory = if prefix.is_empty() {
            self.root.clone()
        } else {
            self.root.join(prefix)
        };
        let entries = directory
            .entries()
            .map_err(|source| CollectionScanError::ReadDirectory {
                path: display_directory.clone(),
                source,
            })?;
        for entry in entries {
            #[cfg(test)]
            maybe_cancel_scan_for_test(cancellation);
            cancellation
                .check()
                .map_err(|_| CollectionScanError::Cancelled)?;
            let entry = entry.map_err(|source| CollectionScanError::ReadEntry {
                directory: display_directory.clone(),
                source,
            })?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| CollectionScanError::NonUtf8Path {
                    path: display_directory.join(&name),
                })?;
            let relative = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            let display_path = self.root.join(&relative);
            let file_type =
                entry
                    .file_type()
                    .map_err(|source| CollectionScanError::InspectEntry {
                        path: display_path.clone(),
                        source,
                    })?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() && self.settings.include_subfolders {
                if crate::record_path::has_hidden_component(&relative)
                    || self.is_excluded(&relative)
                {
                    continue;
                }
                let child = directory.open_dir_nofollow(name).map_err(|source| {
                    CollectionScanError::InspectEntry {
                        path: display_path.clone(),
                        source,
                    }
                })?;
                #[cfg(test)]
                replace_descendant_for_test(&self.root, &relative);
                match child.symlink_metadata("mdbase.yaml") {
                    Ok(metadata) if metadata.is_file() => continue,
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(CollectionScanError::InspectEntry {
                            path: display_path.join("mdbase.yaml"),
                            source,
                        });
                    }
                }
                let child_depth = depth.checked_add(1).ok_or({
                    CollectionScanError::Provider(ProviderError::CaptureLimitExceeded(
                        crate::runtime::CaptureLimitExceeded {
                            kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
                            limit: u64::MAX,
                            attempted: u64::MAX,
                        },
                    ))
                })?;
                self.scan_dir_recursive_checked(
                    &child,
                    &relative,
                    files,
                    cancellation,
                    child_depth,
                    state,
                )?;
            } else if file_type.is_file()
                && (self.validate_record_path(&relative).is_ok()
                    || (!state.records_only && self.validate_file_path(&relative).is_ok()))
            {
                state.discovered = state.discovered.checked_add(1).ok_or({
                    CollectionScanError::Provider(ProviderError::CaptureLimitExceeded(
                        crate::runtime::CaptureLimitExceeded {
                            kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
                            limit: u64::MAX,
                            attempted: u64::MAX,
                        },
                    ))
                })?;
                if let Some(context) = state.context {
                    context
                        .check_entries(state.discovered)
                        .map_err(CollectionScanError::Provider)?;
                }
                files.try_reserve(1).map_err(|_| {
                    CollectionScanError::Provider(ProviderError::CaptureLimitExceeded(
                        crate::runtime::CaptureLimitExceeded {
                            kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
                            limit: usize::MAX as u64,
                            attempted: u64::MAX,
                        },
                    ))
                })?;
                files.push(relative);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static CANCEL_AFTER_SCAN_ENTRIES: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static SNAPSHOT_SCAN_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_snapshot_scan_calls_for_test() {
    SNAPSHOT_SCAN_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(crate) fn snapshot_scan_calls_for_test() -> usize {
    SNAPSHOT_SCAN_CALLS.with(std::cell::Cell::get)
}

#[cfg(all(test, unix))]
fn descendant_replacements(
) -> &'static std::sync::Mutex<std::collections::BTreeMap<(PathBuf, String), PathBuf>> {
    static REPLACEMENTS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<(PathBuf, String), PathBuf>>,
    > = std::sync::OnceLock::new();
    REPLACEMENTS.get_or_init(Default::default)
}

#[cfg(all(test, unix))]
pub(crate) fn replace_descendant_on_scan_for_test(root: &Path, relative: &str, target: &Path) {
    descendant_replacements()
        .lock()
        .expect("descendant replacement lock")
        .insert(
            (root.to_path_buf(), relative.to_string()),
            target.to_path_buf(),
        );
}

#[cfg(all(test, unix))]
fn replace_descendant_for_test(root: &Path, relative: &str) {
    let target = descendant_replacements()
        .lock()
        .expect("descendant replacement lock")
        .remove(&(root.to_path_buf(), relative.to_string()));
    if let Some(target) = target {
        let path = root.join(relative);
        let displaced = root.join(format!("{relative}-displaced"));
        std::fs::rename(&path, displaced).expect("displace descendant for test");
        std::os::unix::fs::symlink(target, path).expect("replace descendant with symlink");
    }
}

#[cfg(all(test, not(unix)))]
fn replace_descendant_for_test(_root: &Path, _relative: &str) {}

#[cfg(test)]
pub(crate) fn cancel_scan_after_entries_for_test(entries: Option<usize>) {
    CANCEL_AFTER_SCAN_ENTRIES.with(|remaining| remaining.set(entries));
}

#[cfg(test)]
fn maybe_cancel_scan_for_test(cancellation: &OperationCancellation) {
    CANCEL_AFTER_SCAN_ENTRIES.with(|remaining| {
        if let Some(value) = remaining.get() {
            if value <= 1 {
                remaining.set(None);
                cancellation.cancel();
            } else {
                remaining.set(Some(value - 1));
            }
        }
    });
}
