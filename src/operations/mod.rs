//! CRUD and batch operations (§12).

pub mod backfill;
pub mod batch;
pub mod create;
pub mod delete;
pub mod migrate;
pub mod read;
pub mod rename;
pub mod update;

use std::path::{Component, Path};

/// Validate that a user-supplied path is relative to the collection root.
pub(crate) fn ensure_safe_relative_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("Path must not be empty");
    }
    if path.contains('\0') {
        return Err("Path contains null bytes");
    }
    let p = Path::new(path);
    if p.is_absolute() {
        return Err("Absolute paths are not allowed");
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("Path contains path traversal");
    }
    Ok(())
}
