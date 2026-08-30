use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

/// Cloneable authority for one acquired collection directory.
///
/// `display_path` is diagnostic data only. All I/O after acquisition is made
/// through `directory`; replacing the name in the ambient namespace therefore
/// cannot redirect an operation to another collection.
#[derive(Clone)]
pub(crate) struct CollectionRoot {
    directory: Arc<Dir>,
    identity: Arc<same_file::Handle>,
    display_path: Arc<PathBuf>,
    cache_storage: Arc<tempfile::TempDir>,
}

impl std::fmt::Debug for CollectionRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollectionRoot")
            .field("display_path", &self.display_path)
            .finish_non_exhaustive()
    }
}

impl CollectionRoot {
    /// The sole ambient collection-authority acquisition boundary.
    pub(crate) fn acquire(path: &Path) -> std::io::Result<Self> {
        let directory = Dir::open_ambient_dir(path, cap_std::ambient_authority())?;
        let identity = Arc::new(same_file::Handle::from_file(
            directory.try_clone()?.into_std_file(),
        )?);
        let cache_storage = cache_storage_for(&identity)?;
        Ok(Self {
            directory: Arc::new(directory),
            identity,
            display_path: Arc::new(path.to_path_buf()),
            // SQLite accepts path names rather than directory capabilities. Keep
            // its derived state in identity-keyed private storage, shared only
            // by live authorities for this exact filesystem object. This avoids
            // ever asking SQLite to reopen through the replaceable display name.
            cache_storage,
        })
    }

    pub(crate) fn display_path(&self) -> &Path {
        &self.display_path
    }

    pub(crate) fn dir(&self) -> std::io::Result<Dir> {
        self.directory.try_clone()
    }

    pub(crate) fn cache_storage_path(&self) -> &Path {
        self.cache_storage.path()
    }

    /// Identity checking is the only post-acquisition ambient lookup. It never
    /// returns a replacement handle and is used solely to fail closed.
    pub(crate) fn display_identity_is_current(&self) -> bool {
        same_file::Handle::from_path(self.display_path())
            .is_ok_and(|current| current == *self.identity)
    }

    pub(crate) fn open_dir(&self, relative: &Path) -> std::io::Result<Dir> {
        let mut dir = self.dir()?;
        for part in normal_components(relative)? {
            dir = dir.open_dir_nofollow(part)?;
        }
        Ok(dir)
    }

    pub(crate) fn create_dir_all(&self, relative: &Path) -> std::io::Result<Dir> {
        let mut dir = self.dir()?;
        for part in normal_components(relative)? {
            match dir.open_dir_nofollow(part) {
                Ok(next) => dir = next,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match dir.create_dir(part) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error),
                    }
                    dir = dir.open_dir_nofollow(part)?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(dir)
    }

    pub(crate) fn ensure_no_symlink_components(&self, relative: &Path) -> std::io::Result<()> {
        let (parent, leaf) = split_parent(relative)?;
        let mut dir = self.dir()?;
        for part in normal_components(parent)? {
            match dir.open_dir_nofollow(part) {
                Ok(next) => dir = next,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        match dir.symlink_metadata(leaf) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "symlink component is not allowed",
            )),
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn open_file(&self, relative: &Path) -> std::io::Result<std::fs::File> {
        let (parent, leaf) = split_parent(relative)?;
        let dir = self.open_dir(parent)?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = dir.open_with(leaf, &options)?.into_std();
        let metadata = file.metadata()?;
        if !metadata.is_file() || has_multiple_hard_links(&metadata) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "collection resource is not an unlinked regular file",
            ));
        }
        Ok(file)
    }

    pub(crate) fn read(&self, relative: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
        let mut file = self.open_file(relative.as_ref())?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    pub(crate) fn read_string(&self, relative: impl AsRef<Path>) -> std::io::Result<String> {
        String::from_utf8(self.read(relative)?)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) fn exists_file(&self, relative: impl AsRef<Path>) -> bool {
        self.open_file(relative.as_ref()).is_ok()
    }

    pub(crate) fn atomic_create(&self, relative: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.atomic_publish(relative, bytes, true)
    }

    pub(crate) fn atomic_write(&self, relative: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.atomic_publish(relative, bytes, false)
    }

    fn atomic_publish(
        &self,
        relative: &Path,
        bytes: &[u8],
        create_only: bool,
    ) -> std::io::Result<()> {
        let (parent, leaf) = split_parent(relative)?;
        let dir = self.create_dir_all(parent)?;
        if create_only && dir.symlink_metadata(leaf).is_ok() {
            return Err(std::io::ErrorKind::AlreadyExists.into());
        }
        if !create_only {
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            match dir.open_with(leaf, &options) {
                Ok(file) => {
                    let metadata = file.metadata()?;
                    if !metadata.is_file() || cap_has_multiple_hard_links(&metadata) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "unsafe publication target",
                        ));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        let temp = format!(".mdbase-publish-{}", uuid::Uuid::new_v4().simple());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let mut file = dir.open_with(&temp, &options)?;
        if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            let _ = dir.remove_file(&temp);
            return Err(error);
        }
        drop(file);
        let result = if create_only {
            // Linking a private temporary file publishes without replacing a
            // concurrent creator; removing the temporary name leaves one link.
            dir.hard_link(&temp, &dir, leaf)
                .and_then(|()| dir.remove_file(&temp))
        } else {
            dir.rename(&temp, &dir, leaf)
        };
        if result.is_err() {
            let _ = dir.remove_file(&temp);
        }
        result?;
        dir.open(".")?.sync_all()
    }

    pub(crate) fn remove_file(&self, relative: &Path) -> std::io::Result<()> {
        let (parent, leaf) = split_parent(relative)?;
        let dir = self.open_dir(parent)?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = dir.open_with(leaf, &options)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || cap_has_multiple_hard_links(&metadata) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe removal target",
            ));
        }
        dir.remove_file(leaf)?;
        dir.open(".")?.sync_all()
    }

    pub(crate) fn open_lock_file(&self, relative: &Path) -> std::io::Result<std::fs::File> {
        let (parent, leaf) = split_parent(relative)?;
        let dir = self.create_dir_all(parent)?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .follow(FollowSymlinks::No);
        let file = dir.open_with(leaf, &options)?.into_std();
        let metadata = file.metadata()?;
        if !metadata.is_file() || has_multiple_hard_links(&metadata) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe lock file",
            ));
        }
        Ok(file)
    }

    pub(crate) fn write_new_synced(&self, relative: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let (parent, leaf) = split_parent(relative)?;
        let dir = self.create_dir_all(parent)?;
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let mut file = dir.open_with(leaf, &options)?;
        file.write_all(bytes)?;
        file.sync_all()
    }

    pub(crate) fn remove_dir_all(&self, relative: &Path) -> std::io::Result<()> {
        let (parent, leaf) = split_parent(relative)?;
        self.open_dir(parent)?.remove_dir_all(leaf)
    }

    pub(crate) fn sync_dir(&self, relative: &Path) -> std::io::Result<()> {
        #[cfg(not(windows))]
        self.open_dir(relative)?.open(".")?.sync_all()?;
        Ok(())
    }

    pub(crate) fn child_directories(&self, relative: &Path) -> std::io::Result<Vec<PathBuf>> {
        let dir = match self.open_dir(relative) {
            Ok(dir) => dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut result = Vec::new();
        for entry in dir.entries()? {
            let entry = entry?;
            let name = entry.file_name();
            if entry.file_type()?.is_dir() {
                dir.open_dir_nofollow(&name)?;
                result.push(relative.join(name));
            }
        }
        result.sort();
        Ok(result)
    }

    pub(crate) fn files_recursive(&self, relative: &Path) -> std::io::Result<Vec<PathBuf>> {
        let start = match self.open_dir(relative) {
            Ok(dir) => dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut result = Vec::new();
        collect_files(&start, relative, &mut result)?;
        result.sort();
        Ok(result)
    }
}

static CACHE_STORAGE: LazyLock<Mutex<HashMap<Arc<same_file::Handle>, Weak<tempfile::TempDir>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cache_storage_for(identity: &Arc<same_file::Handle>) -> std::io::Result<Arc<tempfile::TempDir>> {
    let mut registry = CACHE_STORAGE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.retain(|_, storage| storage.strong_count() != 0);
    if let Some(storage) = registry.get(identity).and_then(Weak::upgrade) {
        return Ok(storage);
    }
    let storage = Arc::new(tempfile::tempdir()?);
    registry.insert(Arc::clone(identity), Arc::downgrade(&storage));
    Ok(storage)
}

fn collect_files(dir: &Dir, prefix: &Path, result: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in dir.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let kind = entry.file_type()?;
        let path = prefix.join(&name);
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            let child = dir.open_dir_nofollow(&name)?;
            collect_files(&child, &path, result)?;
        } else if kind.is_file() {
            result.push(path);
        }
    }
    Ok(())
}

fn normal_components(path: &Path) -> std::io::Result<Vec<&OsStr>> {
    path.components()
        .map(|component| match component {
            Component::Normal(part) => Ok(part),
            Component::CurDir => Err(std::io::ErrorKind::InvalidInput.into()),
            _ => Err(std::io::ErrorKind::InvalidInput.into()),
        })
        .collect()
}

fn split_parent(path: &Path) -> std::io::Result<(&Path, &OsStr)> {
    let leaf = path.file_name().ok_or(std::io::ErrorKind::InvalidInput)?;
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    normal_components(parent)?;
    Ok((parent, leaf))
}

#[cfg(unix)]
fn has_multiple_hard_links(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() > 1
}
#[cfg(not(unix))]
fn has_multiple_hard_links(_metadata: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn cap_has_multiple_hard_links(metadata: &cap_std::fs::Metadata) -> bool {
    use cap_fs_ext::MetadataExt;
    metadata.nlink() > 1
}
#[cfg(not(unix))]
fn cap_has_multiple_hard_links(_metadata: &cap_std::fs::Metadata) -> bool {
    false
}
