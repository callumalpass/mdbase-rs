use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
#[cfg(windows)]
use cap_std::fs::OpenOptionsExt;
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
        allow_atomic_replacement(&mut options);
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

    /// Inspect a leaf relative to the held root without following it.
    pub(crate) fn entry_exists(&self, relative: &Path) -> std::io::Result<bool> {
        let (parent, leaf) = split_parent(relative)?;
        let dir = match self.open_dir(parent) {
            Ok(dir) => dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        match dir.symlink_metadata(leaf) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn metadata(&self, relative: &Path) -> std::io::Result<std::fs::Metadata> {
        self.open_file(relative)?.metadata()
    }

    pub(crate) fn modified_millis(&self, relative: &Path) -> std::io::Result<u64> {
        let modified = self.metadata(relative)?.modified()?;
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            .and_then(|duration| {
                u64::try_from(duration.as_millis())
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            })
    }

    pub(crate) fn modified_nanos(&self, relative: &Path) -> std::io::Result<i64> {
        let modified = self.metadata(relative)?.modified()?;
        Ok(modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
            .unwrap_or(0))
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
            allow_atomic_replacement(&mut options);
            match dir.open_with(leaf, &options) {
                Ok(file) => {
                    let metadata = file.metadata()?;
                    if !metadata.is_file()
                        || cap_has_multiple_hard_links(&metadata)
                        || file.into_std().metadata()?.permissions().readonly()
                    {
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
        #[cfg(windows)]
        options
            .access_mode(
                windows_sys::Win32::Foundation::GENERIC_READ
                    | windows_sys::Win32::Foundation::GENERIC_WRITE
                    | windows_sys::Win32::Storage::FileSystem::DELETE,
            )
            .share_mode(
                windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
                    | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE
                    | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE,
            );
        let mut file = dir.open_with(&temp, &options)?;
        if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            let _ = dir.remove_file(&temp);
            return Err(error);
        }
        let result = if create_only {
            drop(file);
            // Linking a private temporary file publishes without replacing a
            // concurrent creator; removing the temporary name leaves one link.
            dir.hard_link(&temp, &dir, leaf)
                .and_then(|()| dir.remove_file(&temp))
        } else {
            atomic_replace(&file, &dir, &temp, leaf)
        };
        if result.is_err() {
            let _ = dir.remove_file(&temp);
        }
        result?;
        sync_directory(&dir)
    }

    /// Rename a regular file inside the held collection without replacing a target.
    pub(crate) fn rename_noclobber(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        let (from_parent, from_leaf) = split_parent(from)?;
        let (to_parent, to_leaf) = split_parent(to)?;
        let source_dir = self.open_dir(from_parent)?;
        let target_dir = self.create_dir_all(to_parent)?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        allow_atomic_replacement(&mut options);
        let source = source_dir.open_with(from_leaf, &options)?;
        let metadata = source.metadata()?;
        if !metadata.is_file() || cap_has_multiple_hard_links(&metadata) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe rename source",
            ));
        }
        source_dir.hard_link(from_leaf, &target_dir, to_leaf)?;
        if let Err(error) = source_dir.remove_file(from_leaf) {
            let _ = target_dir.remove_file(to_leaf);
            return Err(error);
        }
        sync_directory(&target_dir)?;
        if from_parent != to_parent {
            sync_directory(&source_dir)?;
        }
        Ok(())
    }

    pub(crate) fn remove_file(&self, relative: &Path) -> std::io::Result<()> {
        let (parent, leaf) = split_parent(relative)?;
        let dir = self.open_dir(parent)?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        allow_atomic_replacement(&mut options);
        let file = dir.open_with(leaf, &options)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || cap_has_multiple_hard_links(&metadata) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe removal target",
            ));
        }
        dir.remove_file(leaf)?;
        sync_directory(&dir)
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
        sync_directory(&self.open_dir(relative)?)
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

#[cfg(not(windows))]
fn atomic_replace(
    _source: &cap_std::fs::File,
    parent: &Dir,
    temp: &str,
    destination: &OsStr,
) -> std::io::Result<()> {
    // cap-std implements this as renameat on Unix, with both names resolved
    // relative to the already-open parent directory.
    parent.rename(temp, parent, destination)
}

#[cfg(windows)]
fn atomic_replace(
    source: &cap_std::fs::File,
    parent: &Dir,
    _temp: &str,
    destination: &OsStr,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Wdk::Storage::FileSystem::{
        FileRenameInformationEx, NtSetInformationFile, FILE_RENAME_INFORMATION,
        FILE_RENAME_REPLACE_IF_EXISTS,
    };
    use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    let name = destination.encode_wide().collect::<Vec<_>>();
    let name_bytes = name
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or(std::io::ErrorKind::InvalidInput)?;
    let header = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    let size = header
        .checked_add(name_bytes)
        .ok_or(std::io::ErrorKind::InvalidInput)?
        .max(std::mem::size_of::<FILE_RENAME_INFORMATION>());
    let words = size.div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0_usize; words];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let parent_file = parent.try_clone()?.into_std_file();
    let mut status_block = IO_STATUS_BLOCK::default();
    unsafe {
        // The NT handle-relative API resolves this leaf in the already-open
        // parent directory. The Win32 SetFileInformationByHandle wrapper does
        // not support a non-null RootDirectory and would return ERROR_INVALID_PARAMETER.
        (*info).Anonymous.Flags = FILE_RENAME_REPLACE_IF_EXISTS;
        (*info).RootDirectory = parent_file.as_raw_handle();
        (*info).FileNameLength =
            u32::try_from(name_bytes).map_err(|_| std::io::ErrorKind::InvalidInput)?;
        std::ptr::copy_nonoverlapping(
            name.as_ptr().cast::<u8>(),
            buffer.as_mut_ptr().cast::<u8>().add(header),
            name_bytes,
        );
        let status = NtSetInformationFile(
            source.as_raw_handle(),
            &mut status_block,
            info.cast(),
            u32::try_from(size).map_err(|_| std::io::ErrorKind::InvalidInput)?,
            FileRenameInformationEx,
        );
        if status < 0 {
            // Unsupported handle-relative replacement and every other failure
            // are fail-closed; never emulate this with remove-then-rename.
            return Err(std::io::Error::from_raw_os_error(
                RtlNtStatusToDosError(status) as i32,
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn allow_atomic_replacement(options: &mut OpenOptions) {
    options.share_mode(
        windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
            | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE
            | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE,
    );
}

#[cfg(not(windows))]
fn allow_atomic_replacement(_options: &mut OpenOptions) {}

fn sync_directory(dir: &Dir) -> std::io::Result<()> {
    #[cfg(not(windows))]
    dir.open(".")?.sync_all()?;
    #[cfg(windows)]
    let _ = dir;
    Ok(())
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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::CollectionRoot;
    use std::path::Path;

    #[test]
    fn rename_info_ex_layout_can_hold_a_relative_destination() {
        use windows_sys::Wdk::Storage::FileSystem::{
            FILE_RENAME_INFORMATION, FILE_RENAME_INFORMATION_0,
        };

        assert!(
            std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName)
                >= std::mem::size_of::<FILE_RENAME_INFORMATION_0>()
        );
        assert!(
            std::mem::size_of::<FILE_RENAME_INFORMATION>()
                >= std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName)
                    + std::mem::size_of::<u16>()
        );
    }

    #[test]
    fn capability_relative_atomic_write_replaces_destination() {
        let directory = tempfile::tempdir().unwrap();
        let root = CollectionRoot::acquire(directory.path()).unwrap();
        root.atomic_create(Path::new("record.md"), b"before")
            .unwrap();
        root.atomic_write(Path::new("record.md"), b"after").unwrap();
        assert_eq!(root.read("record.md").unwrap(), b"after");
        assert!(root
            .files_recursive(Path::new(""))
            .unwrap()
            .iter()
            .all(|path| !path.to_string_lossy().contains(".mdbase-publish-")));
    }
}
