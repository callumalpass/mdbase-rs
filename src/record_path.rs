//! Canonical policy for paths that represent ordinary collection records.

use thiserror::Error;

use crate::api::{CollectionPath, CollectionPathError};
use crate::Collection;

pub(crate) fn has_hidden_component(path: &str) -> bool {
    path.split(['/', '\\'])
        .any(|component| component.starts_with('.'))
}

/// Reason a logical path cannot be used as an ordinary collection record.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum RecordPathError {
    /// The logical path itself is invalid or platform-dependent.
    #[error(transparent)]
    InvalidPath(#[from] CollectionPathError),
    /// Hidden filesystem namespaces are never ordinary record locations.
    #[error("record paths must not use hidden filesystem components")]
    HiddenComponent,
    /// The path is outside the configured ordinary-record namespace.
    #[error("path is reserved or excluded from ordinary collection records")]
    Reserved,
    /// The filename does not use a configured record extension.
    #[error("path does not use a configured record extension")]
    UnsupportedExtension,
}

impl Collection {
    /// Normalize and validate one ordinary record path.
    ///
    /// This is the shared namespace boundary for local operations, hosted
    /// authorities, and filesystem mirrors. Structural resources such as
    /// configuration, types, contracts, schemas, and views use their own
    /// typed operations and must not pass through this policy.
    pub fn validate_record_path(
        &self,
        path: impl AsRef<str>,
    ) -> Result<CollectionPath, RecordPathError> {
        let path = CollectionPath::new(path)?;
        if has_hidden_component(path.as_str()) {
            return Err(RecordPathError::HiddenComponent);
        }
        if self.is_excluded(path.as_str()) {
            return Err(RecordPathError::Reserved);
        }
        if !self.is_valid_extension(path.as_str()) {
            return Err(RecordPathError::UnsupportedExtension);
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::RecordPathError;
    use crate::Collection;

    fn collection() -> (tempfile::TempDir, Collection) {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  record_extensions: [md, mdx]\n",
        )
        .unwrap();
        let collection = Collection::open(directory.path()).unwrap();
        (directory, collection)
    }

    #[test]
    fn record_policy_separates_records_from_control_and_hidden_namespaces() {
        let (_directory, collection) = collection();
        assert_eq!(
            collection
                .validate_record_path(r"notes\example.mdx")
                .unwrap()
                .as_str(),
            "notes/example.mdx"
        );
        for path in [
            "payload.bat",
            "mdbase.yaml",
            "_types/task.md",
            "_contracts/task.md",
            ".git/hooks/post-checkout.md",
            ".obsidian/plugins/example/main.md",
            "notes/.private/example.md",
        ] {
            assert!(
                collection.validate_record_path(path).is_err(),
                "{path} must not be an ordinary record path"
            );
        }
        assert_eq!(
            collection.validate_record_path("payload.bat").unwrap_err(),
            RecordPathError::UnsupportedExtension
        );
    }
}
