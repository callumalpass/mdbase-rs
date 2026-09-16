//! Definition-only workspaces for path-based canonical parsers.
//!
//! Authority reads stay capability-relative. Only definition documents and their
//! explicit local schema wrappers are copied, never an entire collection's JSON.
use std::collections::BTreeSet;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::collection_root::CollectionRoot;
use crate::frontmatter::parser::{parse_document, yaml_to_json};

#[derive(Default)]
pub(crate) struct SchemaDependencies {
    pub files: BTreeSet<PathBuf>,
    // Canonicalization still traverses `detour/..`; preserve those directories
    // without copying unrelated files merely to recreate them.
    directories: BTreeSet<PathBuf>,
}

impl SchemaDependencies {
    pub fn extend(&mut self, other: Self) {
        self.files.extend(other.files);
        self.directories.extend(other.directories);
    }

    pub fn stage_directories(&self, root: &CollectionRoot, destination: &Path) -> io::Result<()> {
        for path in &self.directories {
            match root.open_dir(path) {
                Ok(_) => std::fs::create_dir_all(destination.join(path))?,
                // Do not invent missing reference components; let the parser
                // retain its missing-reference diagnostic.
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

pub(crate) fn stage(
    root: &CollectionRoot,
    folder: &Path,
    include: impl Fn(&Path) -> bool,
) -> io::Result<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    let mut dependencies = SchemaDependencies::default();
    for path in root
        .files_recursive(folder)?
        .into_iter()
        .filter(|p| include(p))
    {
        let bytes = root.read(&path)?;
        dependencies.extend(schema_dependencies(&path, &bytes));
        write(directory.path(), &path, &bytes)?;
    }
    dependencies.stage_directories(root, directory.path())?;
    for path in dependencies.files {
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

pub(crate) fn schema_dependencies(source: &Path, bytes: &[u8]) -> SchemaDependencies {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return SchemaDependencies::default();
    };
    let document = parse_document(text);
    let Some(frontmatter) = document.frontmatter else {
        return SchemaDependencies::default();
    };
    let value = yaml_to_json(&frontmatter);
    // These are the schema wrappers consumed by type and contract parsers. The
    // schema's internal $ref only supports fragments, so there is no recursive
    // dependency graph or arbitrary-directory discovery to maintain here.
    let mut dependencies = SchemaDependencies::default();
    for reference in std::iter::once("schema")
        .chain(crate::v03::CONTRACT_SCHEMA_FIELDS)
        .filter_map(|field| {
            let wrapper = value.get(field)?;
            if wrapper.get("value").is_some() {
                return None;
            }
            local_schema_path(source, wrapper.get("ref")?.as_str()?)
        })
    {
        dependencies.extend(reference);
    }
    dependencies
}

fn local_schema_path(source: &Path, reference: &str) -> Option<SchemaDependencies> {
    if crate::v03::is_forbidden_reference(reference) {
        return None;
    }
    let reference = reference.split('#').next()?;
    if reference.is_empty() {
        return None;
    }
    let mut normalized = PathBuf::new();
    let mut dependencies = SchemaDependencies::default();
    for component in source.parent()?.join(reference).components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                dependencies.directories.insert(normalized.clone());
                if !normalized.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    dependencies.files.insert(normalized);
    Some(dependencies)
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
                schema_dependencies(Path::new("_contracts/value.md"), definition.as_bytes()).files,
                BTreeSet::from([PathBuf::from("assets/shared.json")])
            );
        }
        assert!(schema_dependencies(
            Path::new("_types/value.md"),
            b"---\nschema:\n  value:\n    $ref: '#/$defs/value'\n---\n"
        )
        .files
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
            )
            .map(|dependencies| dependencies.files),
            Some(BTreeSet::from([PathBuf::from("other/schema.json")]))
        );
    }

    #[test]
    fn preserves_reference_directory_traversal_without_copying_unrelated_json() {
        for exists in [false, true] {
            let root = tempfile::tempdir().unwrap();
            fs::create_dir(root.path().join("_types")).unwrap();
            fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
            fs::write(root.path().join("schema.json"), "{\"type\":\"object\"}").unwrap();
            fs::write(root.path().join("_types/task.md"), "---\nkind: mdbase.type\nname: task\nschema:\n  dialect: json-schema-2020-12\n  ref: ../detour/../schema.json\n---\n").unwrap();
            if exists {
                fs::create_dir(root.path().join("detour")).unwrap();
                fs::write(root.path().join("detour/unrelated.json"), "{}").unwrap();
            }
            let reference = "../detour/../schema.json";
            let source_resolves = crate::v03::resolve_schema_ref(
                reference,
                &root.path().join("_types/task.md"),
                root.path(),
            )
            .is_ok();
            let authority = CollectionRoot::acquire(root.path()).unwrap();
            let staged = stage(&authority, Path::new("_types"), |_| true).unwrap();
            let staged_resolves = crate::v03::resolve_schema_ref(
                reference,
                &staged.path().join("_types/task.md"),
                staged.path(),
            )
            .is_ok();
            // Path canonicalization differs across platforms when the cancelled
            // component is missing. Scoped staging must preserve the platform's
            // source behavior rather than impose Unix semantics on Windows.
            assert_eq!(staged_resolves, source_resolves);
            assert!(!staged.path().join("detour/unrelated.json").exists());
            if source_resolves {
                let collection = crate::Collection::open(root.path()).unwrap();
                assert!(collection.types.contains_key("task"));
                let shadow = crate::mutation::shadow::shadow_collection(&collection).unwrap();
                assert!(shadow.collection.types.contains_key("task"));
                assert_eq!(shadow.directory.path().join("detour").is_dir(), exists);
                assert!(!shadow
                    .directory
                    .path()
                    .join("detour/unrelated.json")
                    .exists());
            }
        }
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

        fs::remove_file(root.path().join("schema.json")).unwrap();
        fs::write(root.path().join("schema.json"), "{}").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("detour")).unwrap();
        fs::write(
            root.path().join("_types/task.md"),
            "---\nschema:\n  ref: ../detour/../schema.json\n---\n",
        )
        .unwrap();
        let staged = stage(&authority, Path::new("_types"), |_| true).unwrap();
        assert!(!staged.path().join("detour").exists());
        assert!(crate::v03::resolve_schema_ref(
            "../detour/../schema.json",
            &staged.path().join("_types/task.md"),
            staged.path()
        )
        .is_err());
    }
}
