//! CRUD and batch operations (§12).

pub mod backfill;
pub mod batch;
pub mod create;
pub mod delete;
pub mod migrate;
pub mod read;
pub mod rename;
pub mod type_file;
pub mod update;

use std::io::Write;
use std::path::{Component, Path};

use crate::errors::{
    op_error, CONCURRENT_MODIFICATION, FILE_NOT_FOUND, INVALID_PATH, PATH_TRAVERSAL,
    PERMISSION_DENIED,
};
use crate::{Collection, SpecProfile};

/// Validate that a user-supplied path is relative to the collection root.
pub(crate) fn ensure_safe_relative_path(
    path: &str,
    spec_profile: SpecProfile,
) -> Result<(), serde_json::Value> {
    if path.is_empty() {
        return Err(op_error(INVALID_PATH, "Path must not be empty"));
    }
    if path.contains('\0') {
        return Err(op_error(INVALID_PATH, "Path contains null bytes"));
    }
    let bytes = path.as_bytes();
    let has_windows_prefix = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if path.starts_with(['/', '\\']) || has_windows_prefix || Path::new(path).is_absolute() {
        return Err(op_error(INVALID_PATH, "Absolute paths are not allowed"));
    }
    if path.replace('\\', "/").split('/').any(|part| part == "..") {
        let code = if spec_profile == SpecProfile::V03 {
            PATH_TRAVERSAL
        } else {
            INVALID_PATH
        };
        return Err(op_error(code, "Path contains path traversal"));
    }
    Ok(())
}

/// Reject paths whose existing components contain symbolic links.
///
/// Collection operations accept paths relative to an authorized root. A
/// lexical traversal check alone is insufficient because a symlink below the
/// root can redirect a read or write elsewhere. The root itself is deliberately
/// not inspected: hosts may authorize a collection through a symlink after
/// resolving that grant themselves.
#[allow(clippy::result_large_err)]
pub(crate) fn ensure_safe_relative_path_diagnostic(
    path: &str,
    spec_profile: SpecProfile,
) -> Result<(), crate::diagnostic::Diagnostic> {
    if path.is_empty() {
        return Err(crate::diagnostic::Diagnostic::error(
            INVALID_PATH,
            "Path must not be empty",
            None,
        ));
    }
    if path.contains('\0') {
        return Err(crate::diagnostic::Diagnostic::error(
            INVALID_PATH,
            "Path contains null bytes",
            None,
        ));
    }
    let bytes = path.as_bytes();
    let windows_prefix = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if path.starts_with(['/', '\\']) || windows_prefix || Path::new(path).is_absolute() {
        return Err(crate::diagnostic::Diagnostic::error(
            INVALID_PATH,
            "Absolute paths are not allowed",
            None,
        ));
    }
    if path.replace('\\', "/").split('/').any(|part| part == "..") {
        let code = if spec_profile == SpecProfile::V03 {
            PATH_TRAVERSAL
        } else {
            INVALID_PATH
        };
        return Err(crate::diagnostic::Diagnostic::error(
            code,
            "Path contains path traversal",
            None,
        ));
    }
    Ok(())
}

pub(crate) fn ensure_no_symlink_components(
    collection_root: &Path,
    relative_path: &str,
    spec_profile: SpecProfile,
) -> Result<(), serde_json::Value> {
    let mut candidate = collection_root.to_path_buf();
    for component in Path::new(relative_path).components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(part) => candidate.push(part),
            _ => {
                return Err(path_boundary_error(
                    spec_profile,
                    "Path must remain inside the collection",
                ))
            }
        }

        match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(path_boundary_error(
                    spec_profile,
                    "Symbolic links are not allowed in collection operation paths",
                ))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => {
                return Err(op_error(
                    PERMISSION_DENIED,
                    "Path could not be inspected safely",
                ))
            }
        }
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
pub(crate) fn ensure_no_symlink_components_diagnostic(
    collection_root: &Path,
    relative_path: &str,
    spec_profile: SpecProfile,
) -> Result<(), crate::diagnostic::Diagnostic> {
    let mut candidate = collection_root.to_path_buf();
    for component in Path::new(relative_path).components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(part) => candidate.push(part),
            _ => {
                return Err(path_boundary_diagnostic(
                    spec_profile,
                    "Path must remain inside the collection",
                    relative_path,
                ))
            }
        }
        match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(path_boundary_diagnostic(
                    spec_profile,
                    "Symbolic links are not allowed in collection operation paths",
                    relative_path,
                ))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => {
                return Err(crate::diagnostic::Diagnostic::error(
                    PERMISSION_DENIED,
                    "Path could not be inspected safely",
                    Some(relative_path.to_string()),
                ))
            }
        }
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
pub(crate) fn ensure_no_symlink_components_held_diagnostic(
    collection: &crate::Collection,
    relative_path: &str,
) -> Result<(), crate::diagnostic::Diagnostic> {
    collection
        .held_root()
        .ensure_no_symlink_components(Path::new(relative_path))
        .map_err(|_| {
            path_boundary_diagnostic(
                collection.spec_profile,
                "Symbolic links are not allowed in collection operation paths",
                relative_path,
            )
        })
}

fn path_boundary_diagnostic(
    spec_profile: SpecProfile,
    message: &str,
    _path: &str,
) -> crate::diagnostic::Diagnostic {
    crate::diagnostic::Diagnostic::error(
        if spec_profile == SpecProfile::V03 {
            PATH_TRAVERSAL
        } else {
            INVALID_PATH
        },
        message,
        None,
    )
}

fn path_boundary_error(spec_profile: SpecProfile, message: &str) -> serde_json::Value {
    op_error(
        if spec_profile == SpecProfile::V03 {
            PATH_TRAVERSAL
        } else {
            INVALID_PATH
        },
        message,
    )
}

#[allow(clippy::result_large_err)]
pub(crate) fn mutation_record_path_diagnostic(
    collection: &Collection,
    path: &str,
) -> Result<crate::api::CollectionPath, crate::diagnostic::Diagnostic> {
    collection.validate_record_path(path).map_err(|error| {
        path_boundary_diagnostic(collection.spec_profile, &error.to_string(), path)
    })
}

pub(crate) fn mutation_record_path(
    collection: &Collection,
    path: &str,
) -> Result<crate::api::CollectionPath, serde_json::Value> {
    collection
        .validate_record_path(path)
        .map_err(|error| path_boundary_error(collection.spec_profile, &error.to_string()))
}

/// Prepare a record's parent below the held collection-root capability.
///
/// Every existing component is reopened without following links. Missing
/// components are created relative to the currently held directory and then
/// reopened no-follow, so a competing symlink, non-directory, or replacement
/// race fails instead of redirecting an ambient path operation.
pub(crate) fn prepare_record_parent_no_follow(
    collection: &Collection,
    path: &crate::api::CollectionPath,
) -> std::io::Result<()> {
    use cap_fs_ext::DirExt;

    let path = path.to_path_buf();
    let components = path.components().collect::<Vec<_>>();
    let Some((_, parents)) = components.split_last() else {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    };
    let mut directory = collection.root_capability()?;
    let mut prepared = std::path::PathBuf::new();
    for component in parents {
        let Component::Normal(name) = component else {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        };
        prepared.push(name);
        match directory.open_dir_nofollow(name) {
            Ok(next) => directory = next,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match directory.create_dir(name) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                #[cfg(test)]
                crate::operations::rename::hooks::apply_injected_parent_swap(
                    collection.root(),
                    &prepared,
                );
                directory = directory.open_dir_nofollow(name)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Open one regular record relative to an already-authorized root without
/// following symbolic links in any path component.
///
/// Capability-relative handles bind every component to the opened root on
/// Unix and Windows, eliminating pathname replacement races before reads.
pub(crate) fn open_regular_record_no_follow(
    collection: &Collection,
    relative_path: &str,
) -> std::io::Result<Option<std::fs::File>> {
    use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
    use cap_std::fs::OpenOptions;

    let components = Path::new(relative_path)
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name),
            _ => Err(std::io::Error::from(std::io::ErrorKind::InvalidInput)),
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let Some((leaf, parents)) = components.split_last() else {
        return Ok(None);
    };

    #[cfg(test)]
    {
        if let Some(error) = injected_record_open_failure(collection.root(), relative_path) {
            return open_result_or_unavailable(Err(error));
        }
        replace_record_with_symlink_for_test(collection.root(), relative_path);
    }

    let mut directory = collection.root_capability()?;
    for component in parents {
        let Some(next) = open_result_or_unavailable(directory.open_dir_nofollow(component))? else {
            return Ok(None);
        };
        directory = next;
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let Some(file) = open_result_or_unavailable(directory.open_with(leaf, &options))? else {
        return Ok(None);
    };
    let file = file.into_std();
    let metadata = file.metadata()?;
    if !metadata.is_file() || record_has_multiple_hard_links(&metadata) {
        return Ok(None);
    }
    Ok(Some(file))
}

#[cfg(unix)]
fn record_has_multiple_hard_links(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() > 1
}

#[cfg(not(unix))]
fn record_has_multiple_hard_links(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn open_result_or_unavailable<T>(result: std::io::Result<T>) -> std::io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if is_unavailable_no_follow_error(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_unavailable_no_follow_error(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;

    if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) {
        return true;
    }

    // Linux reports a non-directory component and O_NOFOLLOW refusal as
    // ENOTDIR and ELOOP. Keep the raw-code fallback because capability-backed
    // open implementations do not always preserve the newer ErrorKind values.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if matches!(error.raw_os_error(), Some(2 | 20 | 40)) {
        return true;
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "openbsd"
    ))]
    if matches!(error.raw_os_error(), Some(2 | 20 | 62)) {
        return true;
    }
    #[cfg(any(target_os = "freebsd", target_os = "dragonfly"))]
    if matches!(error.raw_os_error(), Some(2 | 20 | 31)) {
        return true;
    }
    #[cfg(target_os = "netbsd")]
    if matches!(error.raw_os_error(), Some(2 | 20 | 79)) {
        return true;
    }
    #[cfg(any(target_os = "solaris", target_os = "illumos"))]
    if matches!(error.raw_os_error(), Some(2 | 20 | 90)) {
        return true;
    }
    #[cfg(target_os = "aix")]
    if matches!(error.raw_os_error(), Some(2 | 20 | 85)) {
        return true;
    }

    // cap-std reports Windows nofollow symlinks/reparse points with
    // ERROR_STOPPED_ON_SYMLINK. ERROR_DIRECTORY covers a non-directory parent;
    // sharing violations and other transient failures deliberately remain Err.
    #[cfg(windows)]
    if matches!(error.raw_os_error(), Some(2 | 3 | 267 | 681)) {
        return true;
    }

    false
}

#[cfg(test)]
fn record_open_failures() -> &'static std::sync::Mutex<
    std::collections::BTreeMap<(std::path::PathBuf, String), std::io::ErrorKind>,
> {
    static FAILURES: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::BTreeMap<(std::path::PathBuf, String), std::io::ErrorKind>,
        >,
    > = std::sync::OnceLock::new();
    FAILURES.get_or_init(Default::default)
}

#[cfg(test)]
fn injected_record_open_failure(
    collection_root: &Path,
    relative_path: &str,
) -> Option<std::io::Error> {
    record_open_failures()
        .lock()
        .expect("record open failure lock")
        .get(&(collection_root.to_path_buf(), relative_path.to_string()))
        .copied()
        .map(std::io::Error::from)
}

#[cfg(all(test, unix))]
fn record_symlink_replacements() -> &'static std::sync::Mutex<
    std::collections::BTreeMap<(std::path::PathBuf, String), std::path::PathBuf>,
> {
    static REPLACEMENTS: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::BTreeMap<(std::path::PathBuf, String), std::path::PathBuf>,
        >,
    > = std::sync::OnceLock::new();
    REPLACEMENTS.get_or_init(Default::default)
}

#[cfg(all(test, unix))]
fn replace_record_with_symlink_for_test(collection_root: &Path, relative_path: &str) {
    let target = record_symlink_replacements()
        .lock()
        .expect("record replacement lock")
        .remove(&(collection_root.to_path_buf(), relative_path.to_string()));
    if let Some(target) = target {
        let record = collection_root.join(relative_path);
        std::fs::remove_file(&record).expect("remove record before test replacement");
        std::os::unix::fs::symlink(target, record).expect("install test symlink replacement");
    }
}

#[cfg(all(test, not(unix)))]
fn replace_record_with_symlink_for_test(_collection_root: &Path, _relative_path: &str) {}

#[cfg(all(test, unix))]
pub(crate) fn replace_record_with_symlink_on_open_for_test(
    collection_root: &Path,
    relative_path: &str,
    target: &Path,
) {
    record_symlink_replacements()
        .lock()
        .expect("record replacement lock")
        .insert(
            (collection_root.to_path_buf(), relative_path.to_string()),
            target.to_path_buf(),
        );
}

#[cfg(test)]
pub(crate) fn set_record_open_failure(
    collection_root: &Path,
    relative_path: &str,
    failure: Option<std::io::ErrorKind>,
) {
    let key = (collection_root.to_path_buf(), relative_path.to_string());
    let mut failures = record_open_failures()
        .lock()
        .expect("record open failure lock");
    if let Some(failure) = failure {
        failures.insert(key, failure);
    } else {
        failures.remove(&key);
    }
}

pub(crate) fn readable_record_path(
    collection: &Collection,
    path: &str,
) -> Result<crate::api::CollectionPath, serde_json::Value> {
    collection
        .validate_record_path(path)
        .map_err(|_| op_error(FILE_NOT_FOUND, &format!("File not found: {path}")))
}

#[allow(clippy::result_large_err)]
pub(crate) fn ensure_regular_record_file_diagnostic(
    path: &Path,
    display_path: &str,
) -> Result<(), crate::diagnostic::Diagnostic> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) | Err(_) => Err(crate::diagnostic::Diagnostic::error(
            FILE_NOT_FOUND,
            format!("File not found: {display_path}"),
            None,
        )),
    }
}

/// Move a regular collection file without ever replacing an existing target.
///
/// Creating the hard link is the atomic no-clobber point. Since both paths are
/// inside one collection they are on the same filesystem. If unlinking the old
/// name fails, the new link is rolled back and the source is left intact.
pub(crate) fn atomic_rename_noclobber(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::hard_link(from, to)?;
    if let Err(error) = std::fs::remove_file(from) {
        let _ = std::fs::remove_file(to);
        return Err(error);
    }
    if let Some(parent) = to.parent() {
        sync_directory(parent)?;
    }
    if from.parent() != to.parent() {
        if let Some(parent) = from.parent() {
            sync_directory(parent)?;
        }
    }
    Ok(())
}

/// Verify an opaque revision token against the current raw file contents.
///
/// Call this immediately before a mutation. Callers that perform work between
/// this check and the write must retain their existing mtime/file-identity
/// guard as a second check against changes during the operation.
pub(crate) fn ensure_revision(
    path: &Path,
    display_path: &str,
    expected: Option<&str>,
) -> Result<(), serde_json::Value> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let bytes = std::fs::read(path).map_err(|_| {
        op_error(
            CONCURRENT_MODIFICATION,
            &format!("File '{display_path}' no longer matches the requested revision"),
        )
    })?;
    let actual = crate::v03::revision(&bytes);
    if actual != expected {
        return Err(op_error(
            CONCURRENT_MODIFICATION,
            &format!("File '{display_path}' was modified externally"),
        ));
    }
    Ok(())
}

/// Atomically replace one collection file with fully written contents.
///
/// The temporary file lives beside the destination so persistence remains on
/// the same filesystem. Existing permissions are retained on replacement.
pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    atomic_write_mode(path, contents, false, true)
}

/// Atomically create a new file without replacing a concurrent creator.
pub(crate) fn atomic_create(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    atomic_write_mode(path, contents, true, true)
}

/// Atomically replace a file whose parent was already prepared and fenced.
pub(crate) fn atomic_write_in_prepared_parent(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    atomic_write_mode(path, contents, false, false)
}

fn atomic_write_mode(
    path: &Path,
    contents: &[u8],
    no_clobber: bool,
    create_parent: bool,
) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination has no parent directory",
        )
    })?;
    if create_parent {
        std::fs::create_dir_all(parent)?;
    }
    let permissions = std::fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    if permissions
        .as_ref()
        .is_some_and(std::fs::Permissions::readonly)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "destination is read-only",
        ));
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(contents)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    if let Some(permissions) = permissions {
        std::fs::set_permissions(temporary.path(), permissions)?;
    }
    if no_clobber {
        temporary
            .persist_noclobber(path)
            .map_err(|error| error.error)?;
    } else {
        temporary.persist(path).map_err(|error| error.error)?;
    }
    sync_directory(parent)?;
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let directory = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    match directory.sync_all() {
        Ok(()) => Ok(()),
        // FlushFileBuffers on a directory handle requires a privilege that is
        // not granted to ordinary Windows processes, even when the filesystem
        // supports directory handles. File contents have already been flushed
        // before the atomic metadata operation; do not turn this platform
        // limitation into a false mutation failure after the mutation landed.
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::ensure_no_symlink_components;
    use super::{
        ensure_safe_relative_path, open_regular_record_no_follow, open_result_or_unavailable,
    };
    use crate::{Collection, SpecProfile};
    use std::io::ErrorKind;

    fn collection(root: &std::path::Path) -> Collection {
        std::fs::write(root.join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        Collection::open(root).unwrap()
    }

    #[test]
    fn durability_primitives_cover_create_replace_cross_directory_rename_and_delete() {
        let root = tempfile::tempdir().expect("tempdir");
        let source_dir = root.path().join("source");
        let target_dir = root.path().join("target");
        std::fs::create_dir_all(&source_dir).expect("source directory");
        std::fs::create_dir_all(&target_dir).expect("target directory");
        let source = source_dir.join("record.md");
        let target = target_dir.join("record.md");

        super::atomic_create(&source, b"one").expect("durable create");
        super::atomic_write(&source, b"two").expect("durable replace");
        super::atomic_rename_noclobber(&source, &target).expect("durable rename");
        assert_eq!(std::fs::read(&target).expect("renamed bytes"), b"two");
        std::fs::remove_file(&target).expect("delete");
        super::sync_directory(&target_dir).expect("durable delete");
        assert!(!target.exists());
    }

    #[test]
    fn rejects_absolute_and_traversal_paths_from_any_platform() {
        for path in [
            "/etc/passwd",
            r"\Windows\system.ini",
            r"C:\Windows\system.ini",
            "C:/Windows/system.ini",
            r"\\server\share\note.md",
            r"notes\..\outside.md",
        ] {
            assert!(
                ensure_safe_relative_path(path, SpecProfile::V03).is_err(),
                "expected unsafe path to be rejected: {path}"
            );
        }

        assert!(ensure_safe_relative_path("notes/inside.md", SpecProfile::V03).is_ok());
        assert!(ensure_safe_relative_path(r"notes\inside.md", SpecProfile::V03).is_ok());
    }

    #[test]
    fn nofollow_open_only_classifies_unavailable_failures_as_absence() {
        for kind in [ErrorKind::NotFound, ErrorKind::NotADirectory] {
            assert!(
                open_result_or_unavailable::<()>(Err(std::io::Error::from(kind)))
                    .expect("unavailable result")
                    .is_none()
            );
        }

        for kind in [
            ErrorKind::Interrupted,
            ErrorKind::WouldBlock,
            ErrorKind::OutOfMemory,
            ErrorKind::Other,
            ErrorKind::PermissionDenied,
        ] {
            let error = open_result_or_unavailable::<()>(Err(std::io::Error::from(kind)))
                .expect_err("transient or unrelated open failure");
            assert_eq!(error.kind(), kind);
        }

        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            for code in [2, 20, 40] {
                assert!(
                    open_result_or_unavailable::<()>(Err(std::io::Error::from_raw_os_error(code,)))
                        .expect("Linux unavailable result")
                        .is_none()
                );
            }
            for code in [4, 5, 11, 24] {
                assert!(
                    open_result_or_unavailable::<()>(Err(std::io::Error::from_raw_os_error(code,)))
                        .is_err()
                );
            }
        }

        #[cfg(windows)]
        {
            for code in [2, 3, 267, 681] {
                assert!(
                    open_result_or_unavailable::<()>(Err(std::io::Error::from_raw_os_error(code,)))
                        .expect("Windows unavailable result")
                        .is_none()
                );
            }
            for code in [4, 32, 33, 995] {
                assert!(
                    open_result_or_unavailable::<()>(Err(std::io::Error::from_raw_os_error(code,)))
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn nofollow_open_preserves_missing_non_directory_and_non_regular_outcomes() {
        let root = tempfile::tempdir().expect("tempdir");
        let collection = collection(root.path());
        std::fs::write(root.path().join("parent-file"), b"not a directory").unwrap();
        std::fs::create_dir(root.path().join("directory.md")).unwrap();

        assert!(open_regular_record_no_follow(&collection, "missing.md")
            .unwrap()
            .is_none());
        assert!(
            open_regular_record_no_follow(&collection, "parent-file/record.md")
                .unwrap()
                .is_none()
        );
        assert!(open_regular_record_no_follow(&collection, "directory.md")
            .unwrap()
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn nofollow_open_rejects_parent_and_leaf_symlinks_as_unavailable() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("tempdir");
        let collection = collection(root.path());
        let outside = tempfile::tempdir().expect("outside tempdir");
        std::fs::write(outside.path().join("record.md"), b"outside").unwrap();
        symlink(outside.path(), root.path().join("linked-parent")).unwrap();
        symlink(
            outside.path().join("record.md"),
            root.path().join("linked-leaf.md"),
        )
        .unwrap();

        assert!(
            open_regular_record_no_follow(&collection, "linked-parent/record.md")
                .unwrap()
                .is_none()
        );
        assert!(open_regular_record_no_follow(&collection, "linked-leaf.md")
            .unwrap()
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_below_the_authorized_root() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        symlink(outside.path(), root.path().join("escape")).expect("create symlink");

        let error = ensure_no_symlink_components(root.path(), "escape/record.md", SpecProfile::V03)
            .expect_err("symlink must be rejected");
        assert_eq!(
            error
                .pointer("/error/code")
                .and_then(serde_json::Value::as_str),
            Some("path_traversal")
        );
    }
}
