#[cfg(not(windows))]
use std::process::Command;

#[cfg(not(windows))]
#[test]
fn canonical_consumer_graphs_resolve_without_legacy_mutation() {
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let status = Command::new(repository.join("scripts/check-no-legacy-feature.sh"))
        .current_dir(repository)
        .status()
        .expect("feature graph guard must execute");
    assert!(status.success(), "canonical feature graph guard failed");
}
