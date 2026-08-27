use crate::frontmatter::parser::{parse_document, yaml_mapping_to_json, FrontmatterState};
use crate::operations::{
    ensure_safe_relative_path, open_regular_record_no_follow, readable_record_path,
};
use crate::Collection;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
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
    /// Digest of configuration, lock, contract, schema, type, and saved-view resources.
    pub resource_revision: String,
    pub spec_version: String,
    pub resources: Vec<CollectionSnapshotResource>,
    pub records: Vec<CollectionSnapshotRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectionSnapshotResourceKind {
    Configuration,
    Lock,
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
    /// Present when the record is preserved as opaque Markdown because its
    /// frontmatter cannot be represented as a structured object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontmatter_error: Option<String>,
}

impl Collection {
    /// Capture canonical resources and records from this loaded collection.
    ///
    /// Long-running hosts should normally call [`super::FilesystemProvider::snapshot`],
    /// which also holds the provider's read gate for the full capture.
    pub fn snapshot(&self) -> Result<CollectionSnapshot, ProviderError> {
        collection_snapshot(self)
    }

    /// Materialize one record for provider and synchronization boundaries.
    ///
    /// Malformed or non-object frontmatter is preserved byte-for-byte as an
    /// opaque body while structured fields remain empty. Typed `read` remains
    /// strict, but transport layers no longer need to reimplement this policy.
    pub fn snapshot_record(&self, path: &str) -> Result<CollectionSnapshotRecord, ProviderError> {
        ensure_safe_relative_path(path, self.spec_profile)
            .map_err(|error| ProviderError::CollectionOpen(record_read_error(path, &error)))?;
        let record_path = readable_record_path(self, path)
            .map_err(|error| ProviderError::CollectionOpen(record_read_error(path, &error)))?;
        let mut file = open_regular_record_no_follow(&self.root, record_path.as_str())
            .map_err(|error| {
                ProviderError::CollectionOpen(format!(
                    "failed to open collection record '{path}': {error}"
                ))
            })?
            .ok_or_else(|| {
                ProviderError::CollectionOpen(format!("collection record '{path}' is unavailable"))
            })?;
        let mut document = String::new();
        file.read_to_string(&mut document).map_err(|error| {
            ProviderError::CollectionOpen(format!(
                "failed to read collection record '{path}': {error}"
            ))
        })?;
        Ok(materialize_snapshot_record(
            self,
            record_path.as_str(),
            document,
        ))
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
    let lock_path = root.join("mdbase.lock.yaml");
    if lock_path.exists() {
        resources.push(read_resource(
            lock_path,
            "mdbase.lock.yaml".to_string(),
            CollectionSnapshotResourceKind::Lock,
        )?);
    }
    let provision_lock_path = root.join("mdbase.provisions.yaml");
    if provision_lock_path.exists() {
        resources.push(read_resource(
            provision_lock_path,
            "mdbase.provisions.yaml".to_string(),
            CollectionSnapshotResourceKind::Lock,
        )?);
    }
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
        if !is_schema_resource_path(&path) {
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
    let mut paths = collection
        .scan_collection_files_checked()
        .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
    paths.sort();
    let mut records = Vec::with_capacity(paths.len());
    for absolute in paths {
        let path = absolute
            .strip_prefix(root)
            .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        let record = collection.snapshot_record(&path)?;
        if path.ends_with(".md") && is_canonical_view(&record) {
            resources.push(CollectionSnapshotResource {
                path: record.path,
                kind: CollectionSnapshotResourceKind::View,
                revision: record.revision,
                document: record.document,
            });
        } else {
            records.push(record);
        }
    }
    resources[1..].sort_by(|left, right| left.path.cmp(&right.path));

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

fn is_canonical_view(record: &CollectionSnapshotRecord) -> bool {
    record.frontmatter_error.is_none()
        && !crate::v03::validate_view(
            &Value::Object(record.frontmatter.clone()),
            record.path.as_str(),
        )
        .iter()
        .any(|diagnostic| diagnostic.severity == "error")
}

pub(crate) fn materialize_snapshot_record(
    collection: &Collection,
    path: &str,
    document: String,
) -> CollectionSnapshotRecord {
    let parsed = parse_document(&document);
    let (frontmatter, body, frontmatter_error) = match parsed.frontmatter_state() {
        FrontmatterState::Absent => (Map::new(), parsed.body, None),
        FrontmatterState::Mapping(mapping) => (
            yaml_mapping_to_json(mapping)
                .as_object()
                .cloned()
                .unwrap_or_default(),
            parsed.body,
            None,
        ),
        FrontmatterState::InvalidYaml => (
            Map::new(),
            document.clone(),
            Some("Failed to parse YAML frontmatter".to_string()),
        ),
        FrontmatterState::Null | FrontmatterState::NonMapping(_) => (
            Map::new(),
            document.clone(),
            Some("Frontmatter must be a YAML mapping".to_string()),
        ),
    };
    let types =
        collection.determine_types_for_path(&Value::Object(frontmatter.clone()), Some(path));
    CollectionSnapshotRecord {
        path: path.to_string(),
        revision: crate::v03::revision(document.as_bytes()),
        frontmatter,
        body,
        types,
        document,
        frontmatter_error,
    }
}

pub(crate) fn is_schema_resource_path(path: &str) -> bool {
    let mut components = path.split('/');
    components
        .clone()
        .all(|component| !component.starts_with('.'))
        && components.any(|component| matches!(component, "schemas" | "_schemas"))
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

fn record_read_error(path: &str, value: &Value) -> String {
    let detail = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("diagnostics")
                .and_then(Value::as_array)
                .and_then(|diagnostics| diagnostics.first())
                .and_then(|diagnostic| diagnostic.get("message"))
                .and_then(Value::as_str)
        })
        .unwrap_or("record read returned no revision");
    format!("failed to read collection record '{path}': {detail}")
}
