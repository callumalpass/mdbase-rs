//! Revision-safe type definition resource operations.

use std::path::Path;

use serde_json::{json, Value};

use crate::diagnostic::Diagnostic;
use crate::operations::ensure_safe_relative_path;
use crate::v03::{self, OperationResult};
use crate::Collection;

impl Collection {
    pub fn read_type_file(&self, input: &Value) -> OperationResult {
        let path = match self.resolve_type_path(input) {
            Ok(path) => path,
            Err(diagnostic) => return failed(*diagnostic),
        };
        self.type_file_result(&path)
    }

    pub fn create_type_file(&self, input: &Value) -> OperationResult {
        let document = match required_document(input) {
            Ok(document) => document,
            Err(diagnostic) => return failed(*diagnostic),
        };
        let candidate =
            match self.parse_type_candidate(document, input.get("path").and_then(Value::as_str)) {
                Ok(candidate) => candidate,
                Err(diagnostics) => return failed_many(diagnostics),
            };
        if self
            .types
            .contains_key(&candidate.name.to_ascii_lowercase())
        {
            return failed(Diagnostic::error(
                "type_conflict",
                format!("Type '{}' already exists.", candidate.name),
                candidate_path(input).map(str::to_string),
            ));
        }
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}/{}.md", self.settings.types_folder, candidate.name));
        if let Err(diagnostic) = self.validate_type_path(&path) {
            return failed(*diagnostic);
        }
        if self.held_root().exists_file(&path) {
            return failed(Diagnostic::error(
                "path_conflict",
                format!("A type definition already exists at '{path}'."),
                Some(path),
            ));
        }
        if let Err(error) = self
            .held_root()
            .atomic_create(Path::new(&path), document.as_bytes())
        {
            return failed(if error.kind() == std::io::ErrorKind::AlreadyExists {
                Diagnostic::error(
                    "path_conflict",
                    format!("A type definition already exists at '{path}'."),
                    Some(path.clone()),
                )
            } else {
                io_diagnostic(&path, error)
            });
        }
        match self.reopen_held(true) {
            Ok(reloaded) => reloaded.type_file_result(&path),
            Err(error) => {
                let _ = self.held_root().remove_file(Path::new(&path));
                failed(open_diagnostic(error, &path))
            }
        }
    }

    pub fn update_type_file(&self, input: &Value) -> OperationResult {
        let path = match self.resolve_type_path(input) {
            Ok(path) => path,
            Err(diagnostic) => return failed(*diagnostic),
        };
        let document = match required_document(input) {
            Ok(document) => document,
            Err(diagnostic) => return failed(*diagnostic),
        };
        if let Err(diagnostics) = self.parse_type_candidate(document, Some(&path)) {
            return failed_many(diagnostics);
        }
        let previous = match self.held_root().read(&path) {
            Ok(previous) => previous,
            Err(error) => return failed(io_diagnostic(&path, error)),
        };
        if let Err(error) = ensure_revision_bytes(
            &previous,
            &path,
            input.get("if_revision").and_then(Value::as_str),
        ) {
            return legacy_failure(error, Some(&path));
        }
        if let Err(error) = self
            .held_root()
            .atomic_write(Path::new(&path), document.as_bytes())
        {
            return failed(io_diagnostic(&path, error));
        }
        match self.reopen_held(true) {
            Ok(reloaded) => reloaded.type_file_result(&path),
            Err(error) => {
                let rollback = self.held_root().atomic_write(Path::new(&path), &previous);
                let mut diagnostic = open_diagnostic(error, &path);
                if let Err(rollback_error) = rollback {
                    diagnostic.message = format!(
                        "{} The previous type definition could not be restored: {rollback_error}",
                        diagnostic.message
                    );
                }
                failed(diagnostic)
            }
        }
    }

    fn resolve_type_path(&self, input: &Value) -> Result<String, Box<Diagnostic>> {
        if let Some(name) = input.get("name").and_then(Value::as_str) {
            return self
                .types
                .get(&name.to_ascii_lowercase())
                .and_then(|definition| definition.source_path.clone())
                .ok_or_else(|| {
                    Box::new(Diagnostic::error(
                        "unknown_type",
                        format!("Type '{name}' does not exist."),
                        None,
                    ))
                });
        }
        let path = input.get("path").and_then(Value::as_str).ok_or_else(|| {
            Box::new(Diagnostic::error(
                "invalid_request",
                "Type operations require name or path.",
                None,
            ))
        })?;
        self.validate_type_path(path)?;
        let known = self
            .types
            .values()
            .any(|definition| definition.source_path.as_deref() == Some(path));
        if !known {
            return Err(Box::new(Diagnostic::error(
                "unknown_type",
                format!("No type definition is registered at '{path}'."),
                Some(path.to_string()),
            )));
        }
        Ok(path.to_string())
    }

    fn validate_type_path(&self, path: &str) -> Result<(), Box<Diagnostic>> {
        ensure_safe_relative_path(path, self.spec_profile)
            .map_err(|error| Box::new(open_diagnostic(error, path)))?;
        self.held_root()
            .ensure_no_symlink_components(Path::new(path))
            .map_err(|error| Box::new(io_diagnostic(path, error)))?;
        let prefix = format!("{}/", self.settings.types_folder.trim_end_matches('/'));
        if !path.starts_with(&prefix)
            || Path::new(path).extension().and_then(|value| value.to_str()) != Some("md")
        {
            return Err(Box::new(Diagnostic::error(
                "invalid_type_path",
                format!(
                    "Type definitions must be Markdown files inside '{}'.",
                    self.settings.types_folder
                ),
                Some(path.to_string()),
            )));
        }
        Ok(())
    }

    fn parse_type_candidate(
        &self,
        document: &str,
        requested_path: Option<&str>,
    ) -> Result<v03::TypeFile, Vec<Diagnostic>> {
        let fallback = format!("{}/candidate.md", self.settings.types_folder);
        let path = requested_path.unwrap_or(&fallback);
        if let Err(diagnostic) = self.validate_type_path(path) {
            return Err(vec![*diagnostic]);
        }
        // `parse_type_file` may resolve a local schema.ref. Materialize a private snapshot
        // from the held authority so its pathname-based resolver cannot adopt a replacement root.
        let staging = tempfile::tempdir().map_err(|error| vec![io_diagnostic(path, error)])?;
        for relative in self
            .held_root()
            .files_recursive(Path::new(""))
            .map_err(|error| vec![io_diagnostic(path, error)])?
        {
            let bytes = self
                .held_root()
                .read(&relative)
                .map_err(|error| vec![io_diagnostic(path, error)])?;
            let destination = staging.path().join(&relative);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| vec![io_diagnostic(path, error)])?;
            }
            std::fs::write(destination, bytes).map_err(|error| vec![io_diagnostic(path, error)])?;
        }
        let candidate = staging.path().join(path);
        v03::parse_type_file(document, &candidate, staging.path(), path)
    }

    fn type_file_result(&self, path: &str) -> OperationResult {
        let bytes = match self.held_root().read(path) {
            Ok(bytes) => bytes,
            Err(error) => return failed(io_diagnostic(path, error)),
        };
        let document = match String::from_utf8(bytes.clone()) {
            Ok(document) => document,
            Err(_) => {
                return failed(Diagnostic::error(
                    "invalid_type_definition",
                    "Type definition is not valid UTF-8.",
                    Some(path.to_string()),
                ))
            }
        };
        let name = self
            .types
            .values()
            .find(|definition| definition.source_path.as_deref() == Some(path))
            .map(|definition| definition.name.clone())
            .unwrap_or_else(|| {
                Path::new(path)
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or("type")
                    .to_string()
            });
        OperationResult {
            valid: true,
            result: json!({
                "name": name,
                "path": path,
                "revision": v03::revision(&bytes),
                "document": document,
            }),
            diagnostics: Vec::new(),
        }
    }
}

fn required_document(input: &Value) -> Result<&str, Box<Diagnostic>> {
    input
        .get("document")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Box::new(Diagnostic::error(
                "invalid_request",
                "document must be a string.",
                None,
            ))
        })
}

fn candidate_path(input: &Value) -> Option<&str> {
    input.get("path").and_then(Value::as_str)
}

fn ensure_revision_bytes(
    bytes: &[u8],
    display_path: &str,
    expected: Option<&str>,
) -> Result<(), Value> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if v03::revision(bytes) != expected {
        return Err(crate::errors::op_error(
            crate::errors::CONCURRENT_MODIFICATION,
            &format!("File '{display_path}' was modified externally"),
        ));
    }
    Ok(())
}

fn failed(diagnostic: Diagnostic) -> OperationResult {
    failed_many(vec![diagnostic])
}

fn failed_many(diagnostics: Vec<Diagnostic>) -> OperationResult {
    OperationResult {
        valid: false,
        result: json!({}),
        diagnostics,
    }
}

fn io_diagnostic(path: &str, error: std::io::Error) -> Diagnostic {
    Diagnostic::error(
        "type_write_failed",
        format!("Type definition could not be read or written: {error}"),
        Some(path.to_string()),
    )
}

fn open_diagnostic(error: Value, path: &str) -> Diagnostic {
    let code = error
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("invalid_type_definition");
    let message = error
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("The type definition is invalid.");
    Diagnostic::error(code, message, Some(path.to_string()))
}

fn legacy_failure(error: Value, path: Option<&str>) -> OperationResult {
    let code = error
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("type_write_failed");
    let message = error
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("The type definition could not be changed.");
    failed(Diagnostic::error(code, message, path.map(str::to_string)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn collection() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  types_folder: _types\n",
        )
        .unwrap();
        fs::create_dir(directory.path().join("_types")).unwrap();
        fs::write(
            directory.path().join("_types/note.md"),
            type_document("note", "Note"),
        )
        .unwrap();
        directory
    }

    fn type_document(name: &str, description: &str) -> String {
        format!(
            "---\nkind: mdbase.type\nname: {name}\nversion: 1\ndescription: {description}\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    properties:\n      title: {{ type: string }}\n---\n"
        )
    }

    #[test]
    fn reads_creates_and_updates_type_documents_with_revisions() {
        let directory = collection();
        let collection = Collection::open(directory.path()).unwrap();
        let read = collection.read_type_file(&json!({"name": "note"}));
        assert!(read.valid);
        let revision = read.result["revision"].as_str().unwrap();

        let created = collection.create_type_file(&json!({
            "document": type_document("project", "Project")
        }));
        assert!(created.valid, "{:?}", created.diagnostics);
        assert_eq!(created.result["path"], "_types/project.md");

        let updated = collection.update_type_file(&json!({
            "path": "_types/note.md",
            "if_revision": revision,
            "document": type_document("note", "Updated note")
        }));
        assert!(updated.valid, "{:?}", updated.diagnostics);
        assert_ne!(updated.result["revision"], revision);
        assert!(updated.result["document"]
            .as_str()
            .unwrap()
            .contains("Updated note"));
    }

    #[test]
    fn rejects_stale_or_invalid_updates_without_losing_the_previous_type() {
        let directory = collection();
        let collection = Collection::open(directory.path()).unwrap();
        let stale = collection.update_type_file(&json!({
            "name": "note",
            "if_revision": "sha256:stale",
            "document": type_document("note", "Changed")
        }));
        assert!(!stale.valid);
        assert_eq!(stale.diagnostics[0].code, "concurrent_modification");

        let revision = collection.read_type_file(&json!({"name": "note"})).result["revision"]
            .as_str()
            .unwrap()
            .to_string();
        let invalid = collection.update_type_file(&json!({
            "name": "note",
            "if_revision": revision,
            "document": "---\nkind: mdbase.type\nname: note\n---\n"
        }));
        assert!(!invalid.valid);
        let reloaded = Collection::open(directory.path()).unwrap();
        assert!(reloaded.read_type_file(&json!({"name": "note"})).valid);
    }

    #[cfg(unix)]
    #[test]
    fn replacement_root_never_receives_type_resource_io() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("collection");
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("mdbase.yaml"),
            "spec_version: 0.3.0\nsettings:\n  types_folder: _types\n",
        )
        .unwrap();
        fs::create_dir(root.join("_types")).unwrap();
        fs::write(root.join("_types/note.md"), type_document("note", "Note")).unwrap();
        let collection = Collection::open(&root).unwrap();
        let held = parent.path().join("held");
        fs::rename(&root, &held).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();

        assert!(collection.read_type_file(&json!({"name": "note"})).valid);
        let created = collection.create_type_file(&json!({
            "document": type_document("project", "Project")
        }));
        assert!(created.valid, "{:?}", created.diagnostics);
        assert!(held.join("_types/project.md").is_file());
        assert!(!root.join("_types").exists());
    }

    #[test]
    fn concurrent_type_creates_never_replace_the_winner() {
        let directory = collection();
        let collection = Arc::new(Collection::open(directory.path()).unwrap());
        let barrier = Arc::new(Barrier::new(9));
        let handles = (0..8)
            .map(|index| {
                let collection = collection.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    collection.create_type_file(&json!({
                        "path": "_types/shared.md",
                        "document": type_document("shared", &format!("Writer {index}")),
                    }))
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(results.iter().filter(|result| result.valid).count(), 1);
        assert!(results.iter().filter(|result| !result.valid).all(|result| {
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "path_conflict")
        }));
        let persisted = fs::read_to_string(directory.path().join("_types/shared.md")).unwrap();
        let winning_document = results.iter().find(|result| result.valid).unwrap().result
            ["document"]
            .as_str()
            .unwrap();
        assert_eq!(persisted, winning_document);
    }
}
