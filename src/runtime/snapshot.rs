use crate::Collection;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fs;
use walkdir::WalkDir;

use super::ProviderError;

/// A consistent, provider-neutral view of one collection authority.
///
/// Hosts add deployment-specific identity, sequencing, and retention around
/// this value. Keeping those concerns out of the mdbase operation engine lets
/// filesystem and hosted providers share the same canonical document boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionSnapshot {
    /// Digest of collection resources and records at this capture boundary.
    pub revision: String,
    /// Digest of configuration, contract, schema, type, and saved-view resources.
    pub resource_revision: String,
    pub spec_version: String,
    pub resources: Vec<CollectionSnapshotResource>,
    pub records: Vec<CollectionSnapshotRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectionSnapshotResourceKind {
    Configuration,
    Contract,
    Schema,
    Type,
    View,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionSnapshotResource {
    pub path: String,
    pub kind: CollectionSnapshotResourceKind,
    pub revision: String,
    pub document: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionSnapshotRecord {
    pub path: String,
    pub revision: String,
    pub frontmatter: Map<String, Value>,
    pub body: String,
    pub types: Vec<String>,
    pub document: String,
}

impl Collection {
    /// Capture canonical resources and records from this loaded collection.
    ///
    /// Long-running hosts should normally call [`super::FilesystemProvider::snapshot`],
    /// which also holds the provider's read gate for the full capture.
    pub fn snapshot(&self) -> Result<CollectionSnapshot, ProviderError> {
        collection_snapshot(self)
    }
}

fn collection_snapshot(collection: &Collection) -> Result<CollectionSnapshot, ProviderError> {
    let root = collection.root();
    let configuration = read_resource(
        root.join("mdbase.yaml"),
        "mdbase.yaml".to_string(),
        CollectionSnapshotResourceKind::Configuration,
    )?;
    let report = crate::v03::inspect_collection(root);
    if !report.valid {
        let message = report
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.severity == "error")
            .map(|diagnostic| diagnostic.message.as_str())
            .unwrap_or("collection validation failed");
        return Err(ProviderError::CollectionOpen(message.to_string()));
    }

    let mut resources = vec![configuration];
    for type_file in report.types {
        resources.push(read_resource(
            root.join(&type_file.path),
            type_file.path,
            CollectionSnapshotResourceKind::Type,
        )?);
    }
    let contracts_root = root.join(&collection.settings().contracts_folder);
    if contracts_root.exists() {
        for entry in WalkDir::new(&contracts_root)
            .follow_links(false)
            .sort_by_file_name()
        {
            let entry = entry.map_err(|error| {
                ProviderError::CollectionOpen(format!(
                    "failed to inspect contracts folder: {error}"
                ))
            })?;
            if !entry.file_type().is_file()
                || entry.path().extension().and_then(|value| value.to_str()) != Some("md")
            {
                continue;
            }
            let path = relative_resource_path(root, entry.path())?;
            resources.push(read_resource(
                entry.path().to_path_buf(),
                path,
                CollectionSnapshotResourceKind::Contract,
            )?);
        }
    }
    for entry in WalkDir::new(root).follow_links(false).sort_by_file_name() {
        let entry = entry.map_err(|error| {
            ProviderError::CollectionOpen(format!("failed to inspect schema resources: {error}"))
        })?;
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("json")
        {
            continue;
        }
        let path = relative_resource_path(root, entry.path())?;
        if path == ".mdbase" || path.starts_with(".mdbase/") {
            continue;
        }
        resources.push(read_resource(
            entry.path().to_path_buf(),
            path,
            CollectionSnapshotResourceKind::Schema,
        )?);
    }
    for view_file in crate::views::compatibility_source_paths(collection) {
        let path = view_file
            .strip_prefix(root)
            .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        resources.push(read_resource(
            view_file,
            path,
            CollectionSnapshotResourceKind::View,
        )?);
    }
    resources[1..].sort_by(|left, right| left.path.cmp(&right.path));

    let mut paths = collection.scan_collection_files();
    paths.sort();
    let mut records = Vec::with_capacity(paths.len());
    for absolute in paths {
        let path = absolute
            .strip_prefix(root)
            .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        let read = collection.read(&serde_json::json!({
            "path": path,
            "include_document": true,
        }));
        let revision = read
            .get("revision")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::CollectionOpen(operation_error(&read)))?
            .to_string();
        let frontmatter = read
            .get("frontmatter")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let body = read
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let document = read
            .get("document")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProviderError::CollectionOpen(format!(
                    "collection read omitted canonical document for {path}"
                ))
            })?
            .to_string();
        let types = read
            .get("types")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        records.push(CollectionSnapshotRecord {
            path,
            revision,
            frontmatter,
            body,
            types,
            document,
        });
    }

    let spec_version = serde_yaml::from_str::<serde_yaml::Value>(&resources[0].document)
        .ok()
        .and_then(|value| {
            value
                .get("spec_version")
                .and_then(serde_yaml::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| crate::v03::SPEC_VERSION.to_string());
    let resource_revision = resource_revision(&resources);
    let revision = snapshot_revision(&resources, &records);
    Ok(CollectionSnapshot {
        revision,
        resource_revision,
        spec_version,
        resources,
        records,
    })
}

fn resource_revision(resources: &[CollectionSnapshotResource]) -> String {
    let mut digest = Sha256::new();
    for resource in resources {
        for value in [resource.path.as_str(), resource.revision.as_str()] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
    }
    format!("sha256:{:x}", digest.finalize())
}

fn read_resource(
    absolute: std::path::PathBuf,
    path: String,
    kind: CollectionSnapshotResourceKind,
) -> Result<CollectionSnapshotResource, ProviderError> {
    let bytes = fs::read(&absolute).map_err(|error| {
        ProviderError::CollectionOpen(format!("failed to read {path}: {error}"))
    })?;
    let document = String::from_utf8(bytes.clone()).map_err(|error| {
        ProviderError::CollectionOpen(format!("{path} is not valid UTF-8: {error}"))
    })?;
    Ok(CollectionSnapshotResource {
        path,
        kind,
        revision: crate::v03::revision(&bytes),
        document,
    })
}

fn relative_resource_path(
    root: &std::path::Path,
    absolute: &std::path::Path,
) -> Result<String, ProviderError> {
    absolute
        .strip_prefix(root)
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .map_err(|error| ProviderError::CollectionOpen(error.to_string()))
}

fn snapshot_revision(
    resources: &[CollectionSnapshotResource],
    records: &[CollectionSnapshotRecord],
) -> String {
    let mut digest = Sha256::new();
    for (kind, path, revision) in resources
        .iter()
        .map(|resource| {
            (
                "resource",
                resource.path.as_str(),
                resource.revision.as_str(),
            )
        })
        .chain(
            records
                .iter()
                .map(|record| ("record", record.path.as_str(), record.revision.as_str())),
        )
    {
        for value in [kind, path, revision] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
    }
    format!("sha256:{:x}", digest.finalize())
}

fn operation_error(value: &Value) -> String {
    value
        .get("diagnostics")
        .and_then(Value::as_array)
        .and_then(|diagnostics| diagnostics.first())
        .and_then(|diagnostic| diagnostic.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("collection record could not be read")
        .to_string()
}
