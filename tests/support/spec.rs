//! The mdbase-spec checkout that spec-backed tests read: `MDBASE_SPEC_REPO_DIR`
//! when set (as CI and `scripts/ci-local` set it), otherwise the sibling
//! `../mdbase-spec`, which must be at the revision CI pins.

use std::path::{Path, PathBuf};
use std::process::Command;

const CI_WORKFLOW: &str = include_str!("../../.github/workflows/ci.yml");

/// The spec revision CI checks out, read from the workflow so the two cannot drift.
pub fn pinned_revision() -> &'static str {
    CI_WORKFLOW
        .lines()
        .find_map(|line| line.trim().strip_prefix("MDBASE_SPEC_REVISION:"))
        .map(str::trim)
        .expect("ci.yml pins MDBASE_SPEC_REVISION")
}

/// The spec checkout root. A sibling checkout at another revision fails with
/// the fix rather than as an unexplained fixture mismatch.
pub fn spec_root() -> PathBuf {
    if let Some(directory) = std::env::var_os("MDBASE_SPEC_REPO_DIR") {
        return PathBuf::from(directory);
    }
    let sibling = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mdbase-spec");
    if let Some(head) = git_head(&sibling) {
        let pinned = pinned_revision();
        assert!(
            head == pinned,
            "../mdbase-spec is at {head}, but CI pins {pinned}. Run scripts/ci-local, \
             which tests against a pinned worktree, or set MDBASE_SPEC_REPO_DIR to a \
             checkout of {pinned}."
        );
    }
    sibling
}

fn git_head(directory: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
