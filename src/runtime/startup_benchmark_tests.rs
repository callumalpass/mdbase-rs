//! Payload-free, local-only synthetic startup comparison; never opens user data.
use super::*;
use serde_json::json;
use std::{
    fs,
    time::{Duration, Instant},
};

#[test]
#[ignore = "synthetic startup observation; run optimized with --ignored --nocapture"]
fn metadata_cursor_startup_benchmark() {
    let count = 10_000;
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
    for i in 0..count {
        fs::write(
            root.path().join(format!("note-{i:05}.md")),
            format!(
                "---\nid: test-{i}\ntitle: Synthetic {i}\n---\n{}",
                "body ".repeat(200)
            ),
        )
        .unwrap();
    }
    let runtime = FilesystemRuntime::open(root.path(), Duration::from_millis(5)).unwrap();
    let context = OperationContext::internal();
    // Establish the cache outside both timed paths, as a resident connector does.
    runtime
        .read(
            &OperationRequest::new(OperationKind::Query, json!({"limit": 0})),
            &context,
        )
        .unwrap();
    let started = Instant::now();
    let first = runtime
        .open_read(
            &OperationRequest::new(OperationKind::Query, json!({"limit": 100})),
            &context,
        )
        .unwrap();
    let first_ms = started.elapsed().as_secs_f64() * 1000.0;
    let retained = runtime.measurements().unwrap().retained_read_snapshot_bytes;
    let cursor = first.next.unwrap();
    runtime.release_read(cursor, &context).unwrap();
    let started = Instant::now();
    let all = runtime
        .read(
            &OperationRequest::new(OperationKind::Query, json!({})),
            &context,
        )
        .unwrap();
    let materialize_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(
        first.outcome.operation.to_v03().result["results"]
            .as_array()
            .unwrap()
            .len(),
        100
    );
    assert_eq!(
        all.operation.to_v03().result["results"]
            .as_array()
            .unwrap()
            .len(),
        count
    );
    assert!(retained < 8192);
    println!(
        "STARTUP_BENCH {}",
        json!({"records":count,"first_page_ms":first_ms,
        "full_materialization_ms":materialize_ms,"retained_cursor_bytes":retained})
    );
}
