use std::process::Command;

#[test]
fn canonical_consumer_graphs_resolve_without_legacy_mutation() {
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = repository.join("scripts/check-no-legacy-feature.sh");
    let mut command = if cfg!(windows) {
        let mut command = Command::new("bash");
        command.arg(script);
        command
    } else {
        Command::new(script)
    };
    let status = command
        .current_dir(repository)
        .status()
        .expect("feature graph guard must execute");
    assert!(status.success(), "canonical feature graph guard failed");
}
