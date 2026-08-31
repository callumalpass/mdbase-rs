use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use crate::diagnostic::Diagnostic;
use crate::runtime::{OperationContext, ProviderError};
use crate::Collection;

pub(crate) struct ShadowCollection {
    pub(crate) directory: tempfile::TempDir,
    pub(crate) collection: Collection,
    pub(crate) baseline: crate::transactions::FileBaseline,
}

pub(crate) fn shadow_collection(
    collection: &Collection,
) -> Result<ShadowCollection, Box<Diagnostic>> {
    #[cfg(test)]
    crate::mutation::probe_full_shadow();
    shadow_collection_inner(collection, None).map_err(|error| match error {
        RuntimeBatchError::Diagnostic(diagnostic) => diagnostic,
        RuntimeBatchError::Provider(error) => {
            Box::new(Diagnostic::error(error.code(), error.to_string(), None))
        }
    })
}

pub(crate) fn shadow_collection_context(
    collection: &Collection,
    context: &OperationContext,
) -> Result<ShadowCollection, ProviderError> {
    #[cfg(test)]
    crate::mutation::probe_full_shadow();
    shadow_collection_inner(collection, Some(context)).map_err(|error| match error {
        RuntimeBatchError::Diagnostic(diagnostic) => {
            ProviderError::CollectionOpen(diagnostic.message.clone())
        }
        RuntimeBatchError::Provider(error) => error,
    })
}

enum RuntimeBatchError {
    Diagnostic(Box<Diagnostic>),
    Provider(ProviderError),
}

fn shadow_collection_inner(
    collection: &Collection,
    context: Option<&OperationContext>,
) -> Result<ShadowCollection, RuntimeBatchError> {
    if let Some(context) = context {
        context.check().map_err(RuntimeBatchError::Provider)?;
    }
    let directory = tempfile::tempdir().map_err(|error| {
        RuntimeBatchError::Diagnostic(Box::new(Diagnostic::error(
            "batch_preflight_failed",
            format!("Could not create batch preflight workspace: {error}"),
            None,
        )))
    })?;
    let baseline = copy_collection(collection, directory.path(), context)?;
    let shadow = Collection::open(directory.path()).map_err(|error| {
        RuntimeBatchError::Diagnostic(Box::new(Diagnostic::error(
            "batch_preflight_failed",
            format!("Could not open batch preflight collection: {error:?}"),
            None,
        )))
    })?;
    Ok(ShadowCollection {
        directory,
        collection: shadow,
        baseline,
    })
}

fn copy_collection(
    collection: &Collection,
    destination: &Path,
    context: Option<&OperationContext>,
) -> Result<crate::transactions::FileBaseline, RuntimeBatchError> {
    let mut baseline = BTreeMap::new();
    let mut captured_entries = 0_u64;
    let files = collection
        .held_root()
        .files_recursive(Path::new(""))
        .map_err(|error| {
            RuntimeBatchError::Diagnostic(Box::new(copy_error(Path::new(""), error)))
        })?;
    for relative in files {
        if let Some(context) = context {
            context.check().map_err(RuntimeBatchError::Provider)?;
        }
        if !should_copy_file(collection, &relative)
            || below_nested_collection(collection, &relative)
        {
            continue;
        }
        if let Some(context) = context {
            captured_entries = captured_entries.checked_add(1).ok_or({
                RuntimeBatchError::Provider(ProviderError::CaptureLimitExceeded(
                    crate::runtime::CaptureLimitExceeded {
                        kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
                        limit: u64::MAX,
                        attempted: u64::MAX,
                    },
                ))
            })?;
            charge_capture_path(context, &relative, captured_entries)?;
        }
        let target = destination.join(&relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                RuntimeBatchError::Diagnostic(Box::new(copy_error(&relative, error)))
            })?;
        }
        let bytes = read_capture_file(collection, &relative, context)?;
        fs::write(&target, &bytes).map_err(|error| {
            RuntimeBatchError::Diagnostic(Box::new(copy_error(&relative, error)))
        })?;
        baseline.insert(portable_path(&relative), bytes);
    }
    Ok(baseline)
}

pub(crate) fn collect_collection_files(
    collection: &Collection,
) -> Result<crate::transactions::FileBaseline, Box<Diagnostic>> {
    collect_collection_files_inner(collection, None).map_err(|error| match error {
        RuntimeBatchError::Diagnostic(diagnostic) => diagnostic,
        RuntimeBatchError::Provider(error) => {
            Box::new(Diagnostic::error(error.code(), error.to_string(), None))
        }
    })
}

pub(crate) fn collect_collection_files_context(
    collection: &Collection,
    context: &OperationContext,
) -> Result<crate::transactions::FileBaseline, ProviderError> {
    collect_collection_files_inner(collection, Some(context)).map_err(|error| match error {
        RuntimeBatchError::Diagnostic(diagnostic) => {
            ProviderError::CollectionOpen(diagnostic.message.clone())
        }
        RuntimeBatchError::Provider(error) => error,
    })
}

fn collect_collection_files_inner(
    collection: &Collection,
    context: Option<&OperationContext>,
) -> Result<crate::transactions::FileBaseline, RuntimeBatchError> {
    let mut files = BTreeMap::new();
    let mut captured_entries = 0_u64;
    let paths = collection
        .held_root()
        .files_recursive(Path::new(""))
        .map_err(|error| {
            RuntimeBatchError::Diagnostic(Box::new(copy_error(Path::new(""), error)))
        })?;
    for relative in paths {
        if let Some(context) = context {
            context.check().map_err(RuntimeBatchError::Provider)?;
        }
        if should_copy_file(collection, &relative)
            && !below_nested_collection(collection, &relative)
        {
            if let Some(context) = context {
                captured_entries = captured_entries.checked_add(1).ok_or({
                    RuntimeBatchError::Provider(ProviderError::CaptureLimitExceeded(
                        crate::runtime::CaptureLimitExceeded {
                            kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
                            limit: u64::MAX,
                            attempted: u64::MAX,
                        },
                    ))
                })?;
                charge_capture_path(context, &relative, captured_entries)?;
            }
            let bytes = read_capture_file(collection, &relative, context)?;
            files.insert(portable_path(&relative), bytes);
        }
    }
    Ok(files)
}

fn charge_capture_path(
    context: &OperationContext,
    path: &Path,
    captured_entries: u64,
) -> Result<(), RuntimeBatchError> {
    context.check().map_err(RuntimeBatchError::Provider)?;
    context
        .check_entries(captured_entries)
        .map_err(RuntimeBatchError::Provider)?;
    let depth = path.components().count().saturating_sub(1) as u64;
    context
        .check_depth(depth)
        .map_err(RuntimeBatchError::Provider)
}

fn read_capture_file(
    collection: &Collection,
    relative: &Path,
    context: Option<&OperationContext>,
) -> Result<Vec<u8>, RuntimeBatchError> {
    let mut file = collection
        .held_root()
        .open_file(relative)
        .map_err(|error| RuntimeBatchError::Diagnostic(Box::new(copy_error(relative, error))))?;
    if context.is_none() {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|error| {
            RuntimeBatchError::Diagnostic(Box::new(copy_error(relative, error)))
        })?;
        return Ok(bytes);
    }
    let context = context.unwrap();
    let size = file
        .metadata()
        .map_err(|error| RuntimeBatchError::Diagnostic(Box::new(copy_error(relative, error))))?
        .len();
    context
        .check_file_bytes(size)
        .map_err(RuntimeBatchError::Provider)?;
    let capacity = usize::try_from(size).map_err(|_| {
        RuntimeBatchError::Provider(ProviderError::CaptureLimitExceeded(
            crate::runtime::CaptureLimitExceeded {
                kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
                limit: usize::MAX as u64,
                attempted: size,
            },
        ))
    })?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|_| {
        RuntimeBatchError::Provider(ProviderError::CaptureLimitExceeded(
            crate::runtime::CaptureLimitExceeded {
                kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
                limit: usize::MAX as u64,
                attempted: size,
            },
        ))
    })?;
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        context.check().map_err(RuntimeBatchError::Provider)?;
        let read = file.read(&mut chunk).map_err(|error| {
            RuntimeBatchError::Diagnostic(Box::new(copy_error(relative, error)))
        })?;
        if read == 0 {
            break;
        }
        let attempted = (bytes.len() as u64).checked_add(read as u64).ok_or({
            RuntimeBatchError::Provider(ProviderError::CaptureLimitExceeded(
                crate::runtime::CaptureLimitExceeded {
                    kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
                    limit: u64::MAX,
                    attempted: u64::MAX,
                },
            ))
        })?;
        context
            .check_file_bytes(attempted)
            .map_err(RuntimeBatchError::Provider)?;
        context
            .charge_read(read as u64)
            .map_err(RuntimeBatchError::Provider)?;
        context
            .charge_retained(read as u64)
            .map_err(RuntimeBatchError::Provider)?;
        bytes.extend_from_slice(&chunk[..read]);
        context.check().map_err(RuntimeBatchError::Provider)?;
    }
    Ok(bytes)
}

fn below_nested_collection(collection: &Collection, path: &Path) -> bool {
    let mut parent = path.parent();
    while let Some(candidate) = parent {
        if candidate.as_os_str().is_empty() {
            break;
        }
        if collection
            .held_root()
            .exists_file(candidate.join("mdbase.yaml"))
        {
            return true;
        }
        parent = candidate.parent();
    }
    false
}

fn should_copy_file(collection: &Collection, relative: &Path) -> bool {
    if relative == Path::new("mdbase.yaml") {
        return true;
    }
    if relative == Path::new("mdbase.lock.yaml") {
        return true;
    }
    if relative == Path::new("mdbase.provisions.yaml") {
        return true;
    }
    let extension = relative.extension().and_then(|value| value.to_str());
    if relative.starts_with(Path::new(&collection.settings.migrations_folder)) {
        return matches!(extension, Some("md" | "json"));
    }
    if relative.starts_with(Path::new(&collection.settings.types_folder))
        || relative.starts_with(Path::new(&collection.settings.contracts_folder))
    {
        return extension == Some("md");
    }
    if extension == Some("base") {
        let relative = portable_path(relative);
        return !collection.is_excluded(&relative)
            && crate::views::is_configured_obsidian_source(collection, &relative);
    }
    let relative = portable_path(relative);
    !collection.is_excluded(&relative)
        && (collection.is_valid_extension(&relative)
            || Path::new(&relative)
                .extension()
                .and_then(|value| value.to_str())
                == Some("json"))
}

fn portable_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn copy_error(path: &Path, error: std::io::Error) -> Diagnostic {
    Diagnostic::error(
        "batch_preflight_failed",
        format!(
            "Could not copy '{}' for batch preflight: {error}",
            path.to_string_lossy()
        ),
        None,
    )
}
