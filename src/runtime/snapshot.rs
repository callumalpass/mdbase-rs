use crate::frontmatter::parser::{
    parse_record, yaml_mapping_to_json, FrontmatterState, RecordFormat,
};
use crate::operations::{ensure_safe_relative_path, readable_record_path};
use crate::record_load::RecordLoadOutcome;
use crate::Collection;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

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
    /// Classify path-owned resources without reading record contents. Shared by
    /// snapshot capture and the ordinary-file namespace boundary.
    pub(crate) fn structural_resource_kind(
        &self,
        portable: &str,
    ) -> Option<CollectionSnapshotResourceKind> {
        let path = std::path::Path::new(portable);
        let extension = path.extension().and_then(|value| value.to_str());
        if matches!(portable, "mdbase.lock.yaml" | "mdbase.provisions.yaml") {
            Some(CollectionSnapshotResourceKind::Lock)
        } else if path.starts_with(&self.settings().types_folder)
            && matches!(extension, Some("md" | "yaml" | "yml"))
        {
            Some(CollectionSnapshotResourceKind::Type)
        } else if path.starts_with(&self.settings().contracts_folder) && extension == Some("md") {
            Some(CollectionSnapshotResourceKind::Contract)
        } else if extension == Some("json") && is_schema_resource_path(portable) {
            Some(CollectionSnapshotResourceKind::Schema)
        } else if extension == Some("base")
            && !self.is_valid_extension(portable)
            && !crate::record_path::has_hidden_component(portable)
            && crate::views::is_configured_obsidian_source(self, portable)
        {
            Some(CollectionSnapshotResourceKind::View)
        } else {
            None
        }
    }

    /// Capture canonical resources and records from this loaded collection.
    ///
    /// Long-running hosts should normally call [`super::FilesystemProvider::snapshot`],
    /// which also holds the provider's read gate for the full capture.
    pub fn snapshot(&self) -> Result<CollectionSnapshot, ProviderError> {
        self.snapshot_with_context(&super::OperationContext::internal())
    }

    /// Capture with caller-owned cancellation, deadline, and finite budgets.
    pub fn snapshot_with_context(
        &self,
        context: &super::OperationContext,
    ) -> Result<CollectionSnapshot, ProviderError> {
        Ok(collection_snapshot(self, InvalidRecordPolicy::Strict, context)?.snapshot)
    }

    /// Capture watcher state without changing the public synchronization
    /// contract. Classified invalid records are reported out-of-band so the
    /// watcher can retain prior state; genuine capture failures remain errors.
    pub(crate) fn snapshot_for_watcher(&self) -> Result<WatcherSnapshot, ProviderError> {
        collection_snapshot(
            self,
            InvalidRecordPolicy::Observe,
            &super::OperationContext::internal(),
        )
    }

    #[allow(dead_code)] // watcher command transport does not yet carry contexts
    pub(crate) fn snapshot_for_watcher_with_context(
        &self,
        context: &super::OperationContext,
    ) -> Result<WatcherSnapshot, ProviderError> {
        collection_snapshot(self, InvalidRecordPolicy::Observe, context)
    }

    /// Materialize one record for provider and synchronization boundaries.
    ///
    /// Malformed or non-object frontmatter is preserved byte-for-byte as an
    /// opaque body while structured fields remain empty. Invalid UTF-8 cannot
    /// be represented by the string-valued synchronization contract and is
    /// reported as a bounded error without inventing content or expanding the
    /// wire format. Typed `read` remains strict, but transport layers do not
    /// reimplement parsing policy.
    pub fn snapshot_record(&self, path: &str) -> Result<CollectionSnapshotRecord, ProviderError> {
        self.snapshot_record_with_context(path, &super::OperationContext::internal())
    }

    pub fn snapshot_record_with_context(
        &self,
        path: &str,
        context: &super::OperationContext,
    ) -> Result<CollectionSnapshotRecord, ProviderError> {
        context.check()?;
        ensure_safe_relative_path(path, self.spec_profile)
            .map_err(|error| ProviderError::CollectionOpen(record_read_error(path, &error)))?;
        let record_path = readable_record_path(self, path)
            .map_err(|error| ProviderError::CollectionOpen(record_read_error(path, &error)))?;
        match load_snapshot_record(self, record_path.as_str(), context)? {
            SnapshotRecordLoad::Record(record) => Ok(record),
            SnapshotRecordLoad::InvalidUtf8 => Err(ProviderError::CollectionOpen(format!(
                "collection record '{path}' contains invalid UTF-8"
            ))),
            SnapshotRecordLoad::Absent => Err(ProviderError::CollectionOpen(format!(
                "collection record '{path}' is unavailable"
            ))),
        }
    }
}

pub(crate) struct WatcherSnapshot {
    pub(crate) snapshot: CollectionSnapshot,
    pub(crate) invalid_records: BTreeSet<String>,
}

#[derive(Clone, Copy)]
enum InvalidRecordPolicy {
    Strict,
    Observe,
}

fn collection_snapshot(
    collection: &Collection,
    invalid_policy: InvalidRecordPolicy,
    context: &super::OperationContext,
) -> Result<WatcherSnapshot, ProviderError> {
    context.check()?;
    let authority = collection.held_root();
    let mut resource_entries = 1_u64;
    context.check_resource_entries(resource_entries)?;
    let configuration = read_resource_held(
        authority,
        "mdbase.yaml".to_string(),
        CollectionSnapshotResourceKind::Configuration,
        context,
    )?;
    let mut resources = vec![configuration];
    for path in authority
        .files_recursive(std::path::Path::new(""))
        .map_err(|error| {
            ProviderError::CollectionOpen(format!("failed to inspect resources: {error}"))
        })?
    {
        context.check()?;
        let portable = path.to_string_lossy().replace('\\', "/");
        let kind = collection.structural_resource_kind(&portable);
        if let Some(kind) = kind {
            resource_entries =
                resource_entries
                    .checked_add(1)
                    .ok_or(crate::runtime::CaptureLimitExceeded {
                        kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
                        limit: u64::MAX,
                        attempted: u64::MAX,
                    })?;
            context.check_resource_entries(resource_entries)?;
            resources.push(read_resource_held(authority, portable, kind, context)?);
        }
    }
    context.check()?;
    let paths = collection.scan_collection_relative_paths_context(context)?;
    context.check()?;
    let mut records = Vec::new();
    records
        .try_reserve(paths.len())
        .map_err(|_| crate::runtime::CaptureLimitExceeded {
            kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
            limit: usize::MAX as u64,
            attempted: paths.len() as u64,
        })?;
    let mut invalid_records = BTreeSet::new();
    for path in paths {
        context.check()?;
        let record = match load_snapshot_record(collection, &path, context)? {
            SnapshotRecordLoad::Record(record) => {
                if record.frontmatter_error.is_some()
                    && matches!(invalid_policy, InvalidRecordPolicy::Observe)
                {
                    invalid_records.insert(path.clone());
                }
                record
            }
            SnapshotRecordLoad::InvalidUtf8 => {
                if matches!(invalid_policy, InvalidRecordPolicy::Strict) {
                    return Err(ProviderError::CollectionOpen(format!(
                        "collection record '{path}' contains invalid UTF-8"
                    )));
                }
                invalid_records.insert(path);
                continue;
            }
            // Enumeration races and no-follow/non-regular outcomes are
            // absence for observation. Strict synchronization fails rather
            // than publishing a checkpoint that silently lost an enumerated
            // record.
            SnapshotRecordLoad::Absent => {
                if matches!(invalid_policy, InvalidRecordPolicy::Strict) {
                    return Err(ProviderError::CollectionOpen(format!(
                        "collection record '{path}' became unavailable during snapshot capture"
                    )));
                }
                continue;
            }
        };
        if path.ends_with(".md") && is_canonical_view(collection, &record) {
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
    context.check()?;
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
    Ok(WatcherSnapshot {
        snapshot: CollectionSnapshot {
            revision,
            resource_revision,
            spec_version,
            resources,
            records,
        },
        invalid_records,
    })
}

fn is_canonical_view(collection: &Collection, record: &CollectionSnapshotRecord) -> bool {
    let raw = Value::Object(record.frontmatter.clone());
    let effective = || crate::views::effective_frontmatter(collection, &record.types, &raw);
    record.frontmatter_error.is_none()
        && matches!(
            crate::views::resolve_view_record(collection, &record.path, &record.types, effective),
            crate::views::ViewRecord::View(_)
        )
}

enum SnapshotRecordLoad {
    Record(CollectionSnapshotRecord),
    InvalidUtf8,
    Absent,
}

fn load_snapshot_record(
    collection: &Collection,
    path: &str,
    context: &super::OperationContext,
) -> Result<SnapshotRecordLoad, ProviderError> {
    let Some(outcome) =
        crate::record_load::load_record_no_follow_context(collection, path, context)?
    else {
        return Ok(SnapshotRecordLoad::Absent);
    };
    match outcome {
        RecordLoadOutcome::Parsed {
            path,
            facts,
            document,
            layout,
            raw_frontmatter,
            type_names,
            ..
        } => Ok(SnapshotRecordLoad::Record(CollectionSnapshotRecord {
            path,
            revision: facts.revision,
            frontmatter: raw_frontmatter.as_object().cloned().unwrap_or_default(),
            body: layout.body(&document).to_string(),
            types: type_names,
            document,
            frontmatter_error: None,
        })),
        RecordLoadOutcome::Invalid {
            state: crate::record_load::InvalidRecordState::InvalidUtf8,
            ..
        } => Ok(SnapshotRecordLoad::InvalidUtf8),
        RecordLoadOutcome::Invalid {
            path,
            facts,
            state:
                crate::record_load::InvalidRecordState::Frontmatter {
                    document, reason, ..
                },
            ..
        } => {
            let mut record = materialize_snapshot_record(collection, &path, document);
            record.revision = facts.revision;
            debug_assert_eq!(
                record.frontmatter_error.as_deref(),
                Some(match reason {
                    crate::record_load::InvalidFrontmatterReason::InvalidYaml => {
                        "Failed to parse YAML frontmatter"
                    }
                    crate::record_load::InvalidFrontmatterReason::NonMappingFrontmatter => {
                        "Frontmatter must be a YAML mapping"
                    }
                })
            );
            Ok(SnapshotRecordLoad::Record(record))
        }
    }
}

pub(crate) fn materialize_snapshot_record(
    collection: &Collection,
    path: &str,
    document: String,
) -> CollectionSnapshotRecord {
    let parsed = parse_record(RecordFormat::for_path(path), &document);
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

/// The snapshot a definition mutation produces without re-reading records.
///
/// `baseline`/`desired` are the exact bytes the mutation changes. Every changed
/// path must be a definition resource (configuration, lock, type, contract or
/// schema), so no record's bytes differ from `before`: records are only
/// reinterpreted under the `staged` definitions, which can move them into or
/// out of the saved-view namespace. Configured `.base` view sources and the
/// record extensions depend on `mdbase.yaml` alone, so files newly qualifying
/// as either are discovered (and only those read) when it changes.
pub(crate) fn definition_change_snapshot(
    authority: &Collection,
    before: &CollectionSnapshot,
    staged: &Collection,
    baseline: &crate::transactions::FileBaseline,
    desired: &crate::transactions::FileBaseline,
    context: &super::OperationContext,
) -> Result<CollectionSnapshot, ProviderError> {
    let mut resources = before
        .resources
        .iter()
        .filter(|resource| resource.kind != CollectionSnapshotResourceKind::View)
        .map(|resource| (resource.path.clone(), resource.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let changed = baseline
        .keys()
        .chain(desired.keys())
        .filter(|path| baseline.get(*path) != desired.get(*path))
        .collect::<BTreeSet<_>>();
    for path in &changed {
        let kind = if path.as_str() == "mdbase.yaml" {
            CollectionSnapshotResourceKind::Configuration
        } else {
            match staged.structural_resource_kind(path) {
                Some(kind) if kind != CollectionSnapshotResourceKind::View => kind,
                _ => {
                    return Err(ProviderError::Transaction {
                        code: "definition_mutation_invariant",
                        message: format!(
                            "a definition mutation changed non-definition file {path}"
                        ),
                    })
                }
            }
        };
        match desired.get(*path) {
            Some(bytes) => {
                let document = String::from_utf8(bytes.clone()).map_err(|error| {
                    ProviderError::CollectionOpen(format!("{path} is not valid UTF-8: {error}"))
                })?;
                resources.insert(
                    (*path).clone(),
                    CollectionSnapshotResource {
                        path: (*path).clone(),
                        kind,
                        revision: crate::v03::revision(bytes),
                        document,
                    },
                );
            }
            None => {
                resources.remove(*path);
            }
        }
    }
    let configuration = resources.remove("mdbase.yaml").ok_or_else(|| {
        ProviderError::CollectionOpen("definition mutation removed mdbase.yaml".to_string())
    })?;

    let mut views = Vec::new();
    let mut added_records = Vec::new();
    if changed.iter().any(|path| path.as_str() == "mdbase.yaml") {
        for path in authority.scan_collection_all_relative_paths_context(context)? {
            if staged.validate_record_path_after_traversal(&path).is_ok()
                && authority
                    .validate_record_path_after_traversal(&path)
                    .is_err()
            {
                let file = read_resource_held(
                    authority.held_root(),
                    path.clone(),
                    CollectionSnapshotResourceKind::View,
                    context,
                )?;
                let mut record = materialize_snapshot_record(staged, &path, file.document);
                record.revision = file.revision;
                added_records.push(record);
            }
        }
        for path in authority
            .held_root()
            .files_recursive(std::path::Path::new(""))
            .map_err(|error| {
                ProviderError::CollectionOpen(format!("failed to inspect resources: {error}"))
            })?
        {
            context.check()?;
            let portable = path.to_string_lossy().replace('\\', "/");
            if portable.ends_with(".base")
                && staged.structural_resource_kind(&portable)
                    == Some(CollectionSnapshotResourceKind::View)
            {
                let known = before
                    .resources
                    .iter()
                    .find(|resource| resource.path == portable)
                    .cloned();
                views.push(match known {
                    Some(resource) => resource,
                    None => read_resource_held(
                        authority.held_root(),
                        portable,
                        CollectionSnapshotResourceKind::View,
                        context,
                    )?,
                });
            }
        }
    } else {
        views.extend(
            before
                .resources
                .iter()
                .filter(|resource| {
                    resource.kind == CollectionSnapshotResourceKind::View
                        && !resource.path.ends_with(".md")
                })
                .cloned(),
        );
    }

    let saved_views = before.resources.iter().filter(|resource| {
        resource.kind == CollectionSnapshotResourceKind::View && resource.path.ends_with(".md")
    });
    let mut records = Vec::with_capacity(before.records.len());
    for record in before
        .records
        .iter()
        .cloned()
        .map(|mut record| {
            // As `materialize_snapshot_record` does: malformed frontmatter is
            // captured as an empty mapping.
            record.types = staged.determine_types_for_path(
                &Value::Object(record.frontmatter.clone()),
                Some(&record.path),
            );
            record
        })
        .chain(saved_views.map(|view| {
            let mut record = materialize_snapshot_record(staged, &view.path, view.document.clone());
            record.revision = view.revision.clone();
            record
        }))
        .chain(added_records)
    {
        context.check()?;
        if record.path.ends_with(".md") && is_canonical_view(staged, &record) {
            views.push(CollectionSnapshotResource {
                path: record.path,
                kind: CollectionSnapshotResourceKind::View,
                revision: record.revision,
                document: record.document,
            });
        } else {
            records.push(record);
        }
    }
    records.sort_by(|left, right| left.path.cmp(&right.path));

    let mut resources = std::iter::once(configuration)
        .chain(resources.into_values())
        .chain(views)
        .collect::<Vec<_>>();
    resources[1..].sort_by(|left, right| left.path.cmp(&right.path));
    Ok(CollectionSnapshot {
        revision: snapshot_revision(&resources, &records),
        resource_revision: resource_revision(&resources),
        spec_version: before.spec_version.clone(),
        resources,
        records,
    })
}

fn read_resource_held(
    root: &crate::collection_root::CollectionRoot,
    path: String,
    kind: CollectionSnapshotResourceKind,
    context: &super::OperationContext,
) -> Result<CollectionSnapshotResource, ProviderError> {
    use std::io::Read;
    context.check()?;
    let mut file = root
        .open_file(std::path::Path::new(&path))
        .map_err(|error| {
            ProviderError::CollectionOpen(format!("failed to open {path}: {error}"))
        })?;
    let size = file
        .metadata()
        .map_err(|error| {
            ProviderError::CollectionOpen(format!("failed to inspect {path}: {error}"))
        })?
        .len();
    context.check_file_bytes(size)?;
    let capacity = usize::try_from(size).map_err(|_| crate::runtime::CaptureLimitExceeded {
        kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
        limit: usize::MAX as u64,
        attempted: size,
    })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| crate::runtime::CaptureLimitExceeded {
            kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
            limit: usize::MAX as u64,
            attempted: size,
        })?;
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        context.check()?;
        let read = file.read(&mut chunk).map_err(|error| {
            ProviderError::CollectionOpen(format!("failed to read {path}: {error}"))
        })?;
        if read == 0 {
            break;
        }
        let attempted = (bytes.len() as u64).checked_add(read as u64).ok_or(
            crate::runtime::CaptureLimitExceeded {
                kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
                limit: u64::MAX,
                attempted: u64::MAX,
            },
        )?;
        context.check_file_bytes(attempted)?;
        context.charge_read(read as u64)?;
        context.charge_retained(read as u64)?;
        bytes.extend_from_slice(&chunk[..read]);
        context.check()?;
    }
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
