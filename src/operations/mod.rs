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
use std::path::Path;

use crate::errors::{op_error, CONCURRENT_MODIFICATION, INVALID_PATH, PATH_TRAVERSAL};
use crate::SpecProfile;

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
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ensure_safe_relative_path;
    use crate::SpecProfile;

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
}
