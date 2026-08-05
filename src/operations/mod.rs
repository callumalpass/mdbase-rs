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

pub(crate) fn mutation_record_path(
    collection: &Collection,
    path: &str,
) -> Result<crate::api::CollectionPath, serde_json::Value> {
    collection
        .validate_record_path(path)
        .map_err(|error| op_error(INVALID_PATH, &error.to_string()))
}

pub(crate) fn readable_record_path(
    collection: &Collection,
    path: &str,
) -> Result<crate::api::CollectionPath, serde_json::Value> {
    collection
        .validate_record_path(path)
        .map_err(|_| op_error(FILE_NOT_FOUND, &format!("File not found: {path}")))
}

pub(crate) fn ensure_regular_record_file(
    path: &Path,
    display_path: &str,
) -> Result<(), serde_json::Value> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) | Err(_) => Err(op_error(
            FILE_NOT_FOUND,
            &format!("File not found: {display_path}"),
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
    atomic_write_mode(path, contents, false)
}

/// Atomically create a new file without replacing a concurrent creator.
pub(crate) fn atomic_create(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    atomic_write_mode(path, contents, true)
}

fn atomic_write_mode(path: &Path, contents: &[u8], no_clobber: bool) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination has no parent directory",
        )
    })?;
    std::fs::create_dir_all(parent)?;
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
    use super::ensure_safe_relative_path;
    use crate::SpecProfile;

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
