//! Canonical policy for paths that represent ordinary collection files.

use thiserror::Error;

use crate::api::{CollectionPath, CollectionPathError};
use crate::record_path::has_hidden_component;
use crate::Collection;

/// Reason a logical path cannot be used as an ordinary collection file.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum FilePathError {
    /// The logical path itself is invalid or platform-dependent.
    #[error(transparent)]
    InvalidPath(#[from] CollectionPathError),
    /// Hidden filesystem namespaces are never ordinary file locations.
    #[error("collection file paths must not use hidden filesystem components")]
    HiddenComponent,
    /// The path is outside the configured ordinary-file namespace.
    #[error("path is reserved or excluded from ordinary collection files")]
    Reserved,
    /// Record paths belong to the record API even when the document is invalid.
    #[error("path uses a configured record extension")]
    RecordPath,
}

impl Collection {
    /// Normalize and validate one ordinary collection-file path.
    ///
    /// This is the logical namespace boundary shared by collection hosts.
    /// Callers remain responsible for checking the live filesystem object at
    /// access time: every component must be non-symlinked and the leaf must be
    /// a regular, single-link file.
    pub fn validate_file_path(
        &self,
        path: impl AsRef<str>,
    ) -> Result<CollectionPath, FilePathError> {
        let path = CollectionPath::new(path)?;
        if has_hidden_component(path.as_str()) {
            return Err(FilePathError::HiddenComponent);
        }
        if self.is_excluded(path.as_str()) {
            return Err(FilePathError::Reserved);
        }
        if self.is_valid_extension(path.as_str()) {
            return Err(FilePathError::RecordPath);
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::FilePathError;
    use crate::Collection;

    fn collection() -> (tempfile::TempDir, Collection) {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  record_extensions: [md, mdx]\n  exclude: [private/**]\n",
        )
        .unwrap();
        let collection = Collection::open(directory.path()).unwrap();
        (directory, collection)
    }

    #[test]
    fn file_policy_separates_files_from_records_and_control_namespaces() {
        let (directory, collection) = collection();
        fs::create_dir(directory.path().join("nested")).unwrap();
        fs::write(
            directory.path().join("nested/mdbase.yaml"),
            "spec_version: 0.3.0\n",
        )
        .unwrap();

        assert_eq!(
            collection
                .validate_file_path(r"attachments\photo.png")
                .unwrap()
                .as_str(),
            "attachments/photo.png"
        );
        for path in [
            "record.md",
            "draft.mdx",
            "mdbase.yaml",
            "_types/task.yaml",
            ".git/config",
            "notes/.private/photo.png",
            "private/secret.png",
            "nested/secret.png",
        ] {
            assert!(
                collection.validate_file_path(path).is_err(),
                "{path} must not be an ordinary collection file"
            );
        }
        assert_eq!(
            collection.validate_file_path("record.md").unwrap_err(),
            FilePathError::RecordPath
        );
        assert_eq!(
            collection
                .validate_file_path("private/secret.png")
                .unwrap_err(),
            FilePathError::Reserved
        );
    }
}
