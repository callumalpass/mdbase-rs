use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use walkdir::WalkDir;

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
    if !collection.rename_root_path_is_current() {
        return Err(RuntimeBatchError::Diagnostic(Box::new(Diagnostic::error(
            crate::errors::CONCURRENT_MODIFICATION,
            "Collection root was replaced before mutation planning.",
            None,
        ))));
    }
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
    let source = &collection.root;
    let mut baseline = BTreeMap::new();
    for entry in WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| should_descend(collection, entry.path()))
    {
        if let Some(context) = context {
            context.check().map_err(RuntimeBatchError::Provider)?;
        }
        let entry = entry.map_err(|error| {
            RuntimeBatchError::Diagnostic(Box::new(Diagnostic::error(
                "batch_preflight_failed",
                format!("Could not inspect collection for batch preflight: {error}"),
                None,
            )))
        })?;
        let relative = entry.path().strip_prefix(source).map_err(|error| {
            RuntimeBatchError::Diagnostic(Box::new(Diagnostic::error(
                "batch_preflight_failed",
                error.to_string(),
                None,
            )))
        })?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).map_err(|error| {
                RuntimeBatchError::Diagnostic(Box::new(copy_error(relative, error)))
            })?;
        } else if entry.file_type().is_file() && should_copy_file(collection, relative) {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    RuntimeBatchError::Diagnostic(Box::new(copy_error(relative, error)))
                })?;
            }
            let bytes = fs::read(entry.path()).map_err(|error| {
                RuntimeBatchError::Diagnostic(Box::new(copy_error(relative, error)))
            })?;
            fs::write(&target, &bytes).map_err(|error| {
                RuntimeBatchError::Diagnostic(Box::new(copy_error(relative, error)))
            })?;
            baseline.insert(portable_path(relative), bytes);
        }
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
    let source = &collection.root;
    for entry in WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| should_descend(collection, entry.path()))
    {
        if let Some(context) = context {
            context.check().map_err(RuntimeBatchError::Provider)?;
        }
        let entry = entry.map_err(|error| {
            RuntimeBatchError::Diagnostic(Box::new(Diagnostic::error(
                "batch_preflight_failed",
                format!("Could not inspect preflight result: {error}"),
                None,
            )))
        })?;
        let relative = entry.path().strip_prefix(source).map_err(|error| {
            RuntimeBatchError::Diagnostic(Box::new(Diagnostic::error(
                "batch_preflight_failed",
                error.to_string(),
                None,
            )))
        })?;
        if entry.file_type().is_file() && should_copy_file(collection, relative) {
            let bytes = fs::read(entry.path()).map_err(|error| {
                RuntimeBatchError::Diagnostic(Box::new(copy_error(relative, error)))
            })?;
            files.insert(portable_path(relative), bytes);
        }
    }
    Ok(files)
}

fn should_descend(collection: &Collection, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(&collection.root) else {
        return false;
    };
    if relative.as_os_str().is_empty() || is_system_definition_path(collection, relative) {
        return true;
    }
    if !path.is_dir() {
        return true;
    }
    let relative = portable_path(relative);
    if collection.is_excluded(&relative) {
        return false;
    }
    // A directory containing its own config is a nested collection boundary.
    // Avoid copying any of it into the preflight workspace.
    !path.join("mdbase.yaml").is_file()
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
        return crate::views::compatibility_source_paths(collection)
            .iter()
            .any(|path| path == &collection.root.join(relative));
    }
    let relative = portable_path(relative);
    !collection.is_excluded(&relative)
        && (collection.is_valid_extension(&relative)
            || Path::new(&relative)
                .extension()
                .and_then(|value| value.to_str())
                == Some("json"))
}

fn is_system_definition_path(collection: &Collection, relative: &Path) -> bool {
    relative.starts_with(Path::new(&collection.settings.types_folder))
        || relative.starts_with(Path::new(&collection.settings.contracts_folder))
        || relative.starts_with(Path::new(&collection.settings.migrations_folder))
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
