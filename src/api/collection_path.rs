use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// A normalized, platform-independent path below a collection root.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CollectionPath(String);

impl CollectionPath {
    /// Parse and normalize a logical collection path.
    pub fn new(path: impl AsRef<str>) -> Result<Self, CollectionPathError> {
        path.as_ref().parse()
    }

    /// Return the canonical forward-slash representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Convert the logical components to the current platform's path type.
    pub fn to_path_buf(&self) -> PathBuf {
        self.0.split('/').collect()
    }

    /// Resolve this path lexically below `root`.
    ///
    /// Filesystem operations must additionally reject existing symbolic-link
    /// components at access time.
    pub fn under(&self, root: &Path) -> PathBuf {
        root.join(self.to_path_buf())
    }
}

impl fmt::Display for CollectionPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for CollectionPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for CollectionPath {
    type Err = CollectionPathError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.is_empty() {
            return Err(CollectionPathError::Empty);
        }
        if input.contains('\0') {
            return Err(CollectionPathError::NullByte);
        }
        let bytes = input.as_bytes();
        let windows_prefix = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        if input.starts_with(['/', '\\']) || windows_prefix || Path::new(input).is_absolute() {
            return Err(CollectionPathError::Absolute);
        }

        let normalized = input.replace('\\', "/");
        let mut components = Vec::new();
        for component in normalized.split('/') {
            match component {
                "" => return Err(CollectionPathError::EmptyComponent),
                "." => return Err(CollectionPathError::CurrentDirectory),
                ".." => return Err(CollectionPathError::Traversal),
                value => components.push(value),
            }
        }
        Ok(Self(components.join("/")))
    }
}

impl TryFrom<&Path> for CollectionPath {
    type Error = CollectionPathError;

    fn try_from(path: &Path) -> Result<Self, Self::Error> {
        let value = path.to_str().ok_or(CollectionPathError::NonUnicode)?;
        value.parse()
    }
}

impl Serialize for CollectionPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CollectionPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

/// Reason a logical collection path could not be constructed.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CollectionPathError {
    #[error("collection path must not be empty")]
    Empty,
    #[error("collection path must be relative")]
    Absolute,
    #[error("collection path must not contain '..' traversal")]
    Traversal,
    #[error("collection path must not contain '.' components")]
    CurrentDirectory,
    #[error("collection path must not contain empty components")]
    EmptyComponent,
    #[error("collection path must not contain a NUL byte")]
    NullByte,
    #[error("collection path must be valid Unicode")]
    NonUnicode,
}

#[cfg(test)]
mod tests {
    use super::{CollectionPath, CollectionPathError};

    #[test]
    fn normalizes_platform_separators() {
        let path = CollectionPath::new(r"tasks\ready\note.md").unwrap();
        assert_eq!(path.as_str(), "tasks/ready/note.md");
        assert_eq!(
            serde_json::to_string(&path).unwrap(),
            r#""tasks/ready/note.md""#
        );
    }

    #[test]
    fn rejects_ambiguous_or_escaping_paths() {
        for (path, expected) in [
            ("", CollectionPathError::Empty),
            ("/tmp/a.md", CollectionPathError::Absolute),
            (r"C:\tmp\a.md", CollectionPathError::Absolute),
            ("a/../b.md", CollectionPathError::Traversal),
            ("a/./b.md", CollectionPathError::CurrentDirectory),
            ("a//b.md", CollectionPathError::EmptyComponent),
            ("a/", CollectionPathError::EmptyComponent),
        ] {
            assert_eq!(CollectionPath::new(path).unwrap_err(), expected);
        }
    }
}
