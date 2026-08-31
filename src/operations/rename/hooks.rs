#[cfg(test)]
fn injected_reference_removals(
) -> &'static std::sync::Mutex<std::collections::BTreeSet<std::path::PathBuf>> {
    static REMOVALS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeSet<std::path::PathBuf>>,
    > = std::sync::OnceLock::new();
    REMOVALS.get_or_init(Default::default)
}

#[cfg(all(test, feature = "legacy-collection-mutation"))]
pub(super) fn inject_reference_removal(path: &std::path::Path) {
    injected_reference_removals()
        .lock()
        .expect("rename reference removal lock")
        .insert(path.to_path_buf());
}

#[cfg(test)]
fn injected_reference_open_failures(
) -> &'static std::sync::Mutex<std::collections::BTreeSet<std::path::PathBuf>> {
    static FAILURES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeSet<std::path::PathBuf>>,
    > = std::sync::OnceLock::new();
    FAILURES.get_or_init(Default::default)
}

#[cfg(all(test, feature = "legacy-collection-mutation"))]
pub(super) fn inject_reference_open_failure(path: &std::path::Path) {
    injected_reference_open_failures()
        .lock()
        .expect("rename reference open failure lock")
        .insert(path.to_path_buf());
}

#[cfg(test)]
pub(super) fn take_injected_reference_open_failure(path: &std::path::Path) -> bool {
    injected_reference_open_failures()
        .lock()
        .expect("rename reference open failure lock")
        .remove(path)
}

#[cfg(test)]
pub(super) fn apply_injected_reference_removal(path: &std::path::Path) {
    if injected_reference_removals()
        .lock()
        .expect("rename reference removal lock")
        .remove(path)
    {
        std::fs::remove_file(path).expect("injected rename reference removal");
    }
}

#[cfg(all(test, unix))]
fn injected_root_replacements(
) -> &'static std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf, std::path::PathBuf>> {
    static REPLACEMENTS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf, std::path::PathBuf>>,
    > = std::sync::OnceLock::new();
    REPLACEMENTS.get_or_init(Default::default)
}

#[cfg(all(test, unix, feature = "legacy-collection-mutation"))]
pub(super) fn inject_root_replacement(root: &std::path::Path, target: &std::path::Path) {
    injected_root_replacements()
        .lock()
        .expect("rename root replacement lock")
        .insert(root.to_path_buf(), target.to_path_buf());
}

#[cfg(all(test, unix))]
pub(super) fn apply_injected_root_replacement(root: &std::path::Path) {
    use std::os::unix::fs::symlink;
    let target = injected_root_replacements()
        .lock()
        .expect("rename root replacement lock")
        .remove(root);
    if let Some(target) = target {
        let held = root.with_extension("rename-held-root");
        std::fs::rename(root, &held).expect("hold rename collection root");
        symlink(target, root).expect("replace rename collection root");
    }
}

#[cfg(all(test, not(unix)))]
pub(super) fn apply_injected_root_replacement(_root: &std::path::Path) {}

#[cfg(all(test, unix))]
fn injected_parent_swaps() -> &'static std::sync::Mutex<
    std::collections::BTreeMap<(std::path::PathBuf, std::path::PathBuf), std::path::PathBuf>,
> {
    static SWAPS: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::BTreeMap<
                (std::path::PathBuf, std::path::PathBuf),
                std::path::PathBuf,
            >,
        >,
    > = std::sync::OnceLock::new();
    SWAPS.get_or_init(Default::default)
}

#[cfg(all(test, unix, feature = "legacy-collection-mutation"))]
pub(super) fn inject_parent_swap(
    root: &std::path::Path,
    relative_parent: &std::path::Path,
    target: &std::path::Path,
) {
    injected_parent_swaps()
        .lock()
        .expect("rename parent swap lock")
        .insert(
            (root.to_path_buf(), relative_parent.to_path_buf()),
            target.to_path_buf(),
        );
}

#[cfg(all(test, unix))]
pub(crate) fn apply_injected_parent_swap(
    root: &std::path::Path,
    relative_parent: &std::path::Path,
) {
    use std::os::unix::fs::symlink;
    let target = injected_parent_swaps()
        .lock()
        .expect("rename parent swap lock")
        .remove(&(root.to_path_buf(), relative_parent.to_path_buf()));
    if let Some(target) = target {
        let parent = root.join(relative_parent);
        let held = parent.with_extension("rename-held-parent");
        std::fs::rename(&parent, held).expect("hold rename destination parent");
        symlink(target, parent).expect("replace rename destination parent");
    }
}

#[cfg(all(test, not(unix)))]
pub(crate) fn apply_injected_parent_swap(
    _root: &std::path::Path,
    _relative_parent: &std::path::Path,
) {
}
