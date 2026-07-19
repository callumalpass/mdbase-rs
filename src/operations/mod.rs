//! CRUD and batch operations (§12).

pub mod backfill;
pub mod batch;
pub mod create;
pub mod delete;
pub mod migrate;
pub mod read;
pub mod rename;
pub mod update;

use std::path::Path;

use crate::errors::{op_error, INVALID_PATH, PATH_TRAVERSAL};
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
