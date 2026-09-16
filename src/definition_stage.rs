//! Definition-only workspaces for path-based canonical parsers.
//!
//! Authority reads stay capability-relative. Only definition documents and their
//! explicit local schema wrappers are copied, never an entire collection's JSON.
use std::collections::BTreeSet;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::collection_root::CollectionRoot;
use crate::frontmatter::parser::{parse_document, yaml_to_json};

pub(crate) fn stage(
    root: &CollectionRoot,
    folder: &Path,
    include: impl Fn(&Path) -> bool,
) -> io::Result<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    let mut dependencies = BTreeSet::new();
    for path in root
        .files_recursive(folder)?
        .into_iter()
        .filter(|p| include(p))
    {
        let bytes = root.read(&path)?;
        dependencies.extend(schema_dependencies(&path, &bytes));
        write(directory.path(), &path, &bytes)?;
    }
    for path in dependencies {
        if directory.path().join(&path).is_file() {
            continue;
        }
        match root.read(&path) {
            Ok(bytes) => write(directory.path(), &path, &bytes)?,
            // Keep missing-reference diagnostics owned by the canonical parser.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(directory)
}

pub(crate) fn schema_dependencies(source: &Path, bytes: &[u8]) -> BTreeSet<PathBuf> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return BTreeSet::new();
    };
    let document = parse_document(text);
    let Some(frontmatter) = document.frontmatter else {
        return BTreeSet::new();
    };
    let value = yaml_to_json(&frontmatter);
    // These are the schema wrappers consumed by type and contract parsers. The
    // schema's internal $ref only supports fragments, so there is no recursive
    // dependency graph or arbitrary-directory discovery to maintain here.
    std::iter::once("schema")
        .chain(crate::v03::CONTRACT_SCHEMA_FIELDS)
        .filter_map(|field| {
            let wrapper = value.get(field)?;
            if wrapper.get("value").is_some() {
                return None;
            }
            local_schema_path(source, wrapper.get("ref")?.as_str()?)
        })
        .collect()
}

fn local_schema_path(source: &Path, reference: &str) -> Option<PathBuf> {
    if crate::v03::is_forbidden_reference(reference) {
        return None;
    }
    let reference = reference.split('#').next()?;
    if reference.is_empty() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in source.parent()?.join(reference).components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

fn write(root: &Path, relative: &Path, bytes: &[u8]) -> io::Result<()> {
    let destination = root.join(relative);
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(destination, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn stages_only_definitions_and_explicit_schema_dependencies() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("_types")).unwrap();
        fs::create_dir(root.path().join("other")).unwrap();
        fs::write(
            root.path().join("_types/task.md"),
            "---\nkind: mdbase.type\nschema:\n  ref: ../other/task.json#/$defs/task\n---\n",
        )
        .unwrap();
        fs::write(
            root.path().join("other/task.json"),
            "{\"$defs\":{\"task\":{\"type\":\"object\"}}}",
        )
        .unwrap();
        fs::write(
            root.path().join("other/unrelated.json"),
            b"unreadable as JSON\xff",
        )
        .unwrap();
        let authority = CollectionRoot::acquire(root.path()).unwrap();
        let staged = stage(&authority, Path::new("_types"), |p| {
            p.extension().is_some_and(|e| e == "md")
        })
        .unwrap();
        assert!(staged.path().join("_types/task.md").is_file());
        assert!(staged.path().join("other/task.json").is_file());
        assert!(!staged.path().join("other/unrelated.json").exists());
    }

    #[test]
    fn discovers_every_canonical_contract_wrapper_without_following_internal_refs() {
        for field in crate::v03::CONTRACT_SCHEMA_FIELDS {
            let definition =
                format!("---\n{field}:\n  ref: ../assets/shared.json#/$defs/value\n---\n");
            assert_eq!(
                schema_dependencies(Path::new("_contracts/value.md"), definition.as_bytes()),
                BTreeSet::from([PathBuf::from("assets/shared.json")])
            );
        }
        assert!(schema_dependencies(
            Path::new("_types/value.md"),
            b"---\nschema:\n  value:\n    $ref: '#/$defs/value'\n---\n"
        )
        .is_empty());
    }

    #[test]
    fn missing_dependencies_are_left_for_canonical_diagnostics() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("_types")).unwrap();
        fs::write(
            root.path().join("_types/value.md"),
            "---\nschema:\n  ref: ../missing.json\n---\n",
        )
        .unwrap();
        let authority = CollectionRoot::acquire(root.path()).unwrap();
        let staged = stage(&authority, Path::new("_types"), |_| true).unwrap();
        assert!(staged.path().join("_types/value.md").exists());
        assert!(!staged.path().join("missing.json").exists());
    }

    #[test]
    fn dependency_discovery_does_not_escape_authority_or_fetch_urls() {
        for reference in [
            "../../outside.json",
            "/outside.json",
            "https://example.test/schema.json",
            "file:///outside.json",
            "#/$defs/task",
        ] {
            assert!(
                local_schema_path(Path::new("_types/task.md"), reference).is_none(),
                "{reference}"
            );
        }
        assert_eq!(
            local_schema_path(
                Path::new("_types/nested/task.md"),
                "../../other/./schema.json#/$defs/task"
            ),
            Some(PathBuf::from("other/schema.json"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn referenced_schema_symlinks_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("_types")).unwrap();
        fs::write(
            root.path().join("_types/task.md"),
            "---\nschema:\n  ref: ../schema.json\n---\n",
        )
        .unwrap();
        fs::write(outside.path().join("schema.json"), "{}").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("schema.json"),
            root.path().join("schema.json"),
        )
        .unwrap();
        let authority = CollectionRoot::acquire(root.path()).unwrap();
        assert!(stage(&authority, Path::new("_types"), |_| true).is_err());
    }
}
