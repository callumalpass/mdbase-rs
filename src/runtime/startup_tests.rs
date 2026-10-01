use super::*;
use serde_json::json;
use std::{fs, time::Duration};

#[test]
fn metadata_cursor_retains_a_snapshot_not_all_projected_records() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
    for i in 0..1000 {
        fs::write(
            root.path().join(format!("note-{i:04}.md")),
            format!("---\ntitle: old-{i}\n---\n{}", "body ".repeat(200)),
        )
        .unwrap();
    }
    let runtime = FilesystemRuntime::open(root.path(), Duration::from_millis(5)).unwrap();
    let context = OperationContext::internal();
    let first = runtime
        .open_read(
            &OperationRequest::new(OperationKind::Query, json!({"limit": 1})),
            &context,
        )
        .unwrap();
    let cursor = first.next.unwrap();
    let measurement = runtime.measurements().unwrap();
    assert!(
        measurement.retained_read_snapshot_bytes < 8192,
        "{measurement:?}"
    );
    // A canonical write changes the live projection, not the held read.
    let update = OperationRequest::new(
        OperationKind::Update,
        json!({"path": "note-0001.md", "patch": {"title": "new"}}),
    );
    let prepared = match runtime
        .prepare(&update, &HostClaimId::generate(), &context)
        .unwrap()
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        other => panic!("expected mutation, got {other:?}"),
    };
    assert!(matches!(
        runtime.commit(&prepared, &context).unwrap(),
        CommitAttempt::Committed(_)
    ));
    let page = runtime
        .read_page_with_limit(&cursor, Some(1000), &context)
        .unwrap();
    assert_eq!(page.outcome.generation, first.outcome.generation);
    let retained = runtime.measurements().unwrap().retained_read_snapshot_bytes;
    assert!(retained > measurement.retained_read_snapshot_bytes);
    let wire = page.outcome.operation.to_v03();
    assert_eq!(wire.result["results"].as_array().unwrap().len(), 999);
    assert_eq!(
        wire.result["results"][0]["effective_frontmatter"]["title"],
        "old-1"
    );
    assert!(page.next.is_none());
    let replay = runtime
        .read_page_with_limit(&cursor, Some(1000), &context)
        .unwrap();
    assert_eq!(page, replay);
    assert_eq!(
        runtime.measurements().unwrap().retained_read_snapshot_bytes,
        retained
    );
    assert!(matches!(
        runtime.read_page_with_limit(&cursor, Some(10), &context),
        Err(ProviderError::InvalidReadCursor)
    ));
    runtime.release_read(cursor, &context).unwrap();
    assert_eq!(
        runtime.measurements().unwrap().retained_read_snapshot_bytes,
        0
    );
}

#[test]
fn abandoned_wal_readers_are_reaped_during_other_runtime_activity() {
    for activity in ["read", "prepare", "commit", "synchronize"] {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        for i in 0..3 {
            fs::write(
                root.path().join(format!("note-{i}.md")),
                "---\ntitle: Synthetic\n---\n",
            )
            .unwrap();
        }
        let runtime = FilesystemRuntime::open(root.path(), Duration::from_millis(5)).unwrap();
        let context = OperationContext::internal();
        let create = OperationRequest::new(
            OperationKind::Create,
            json!({"path": "created.md", "frontmatter": {"title": "new"}}),
        );
        let prepared = if activity == "commit" {
            match runtime
                .prepare(&create, &HostClaimId::generate(), &context)
                .unwrap()
            {
                PreparationOutcome::Prepared(prepared) => Some(prepared),
                other => panic!("expected mutation, got {other:?}"),
            }
        } else {
            None
        };
        let query = OperationRequest::new(OperationKind::Query, json!({"limit": 1}));
        let cursor = runtime.open_read(&query, &context).unwrap().next.unwrap();
        runtime.expire_read_leases_for_test();
        match activity {
            "read" => {
                runtime.read(&query, &context).unwrap();
            }
            "prepare" => {
                runtime
                    .prepare(&create, &HostClaimId::generate(), &context)
                    .unwrap();
            }
            "commit" => {
                assert!(matches!(
                    runtime.commit(&prepared.unwrap(), &context).unwrap(),
                    CommitAttempt::Committed(_)
                ));
            }
            "synchronize" => {
                runtime.synchronize().unwrap();
            }
            _ => unreachable!(),
        }
        // release() itself does not reap: false proves the activity did it.
        assert!(
            !runtime.release_read(cursor, &context).unwrap().released,
            "{activity}"
        );
        assert_eq!(
            runtime.measurements().unwrap().retained_read_snapshot_bytes,
            0
        );
    }
}

#[test]
fn metadata_cursor_keeps_original_offset_and_filters_types() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
    fs::create_dir(root.path().join("_types")).unwrap();
    fs::write(root.path().join("_types/source.md"),
        "---\nkind: mdbase.type\nname: source\nversion: 1\nmatch: {where: {type: source}}\nschema: {dialect: json-schema-2020-12, value: {type: object}}\n---\n").unwrap();
    for i in 0..20 {
        let type_name = if i % 2 == 0 { "source" } else { "other" };
        fs::write(
            root.path().join(format!("note-{i:04}.md")),
            format!("---\ntitle: {i}\ntype: {type_name}\n---\n"),
        )
        .unwrap();
    }
    let runtime = FilesystemRuntime::open(root.path(), Duration::from_millis(5)).unwrap();
    let context = OperationContext::internal();
    let first = runtime
        .open_read(
            &OperationRequest::new(
                OperationKind::Query,
                json!({"types": ["source"], "limit": 2, "offset": 5}),
            ),
            &context,
        )
        .unwrap();
    assert_eq!(
        first.outcome.operation.to_v03().result["results"][0]["path"],
        "note-0010.md"
    );
    let next = runtime
        .read_page_with_limit(&first.next.unwrap(), Some(10), &context)
        .unwrap();
    assert_eq!(
        next.outcome.operation.to_v03().result["results"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        next.outcome.operation.to_v03().result["results"][0]["path"],
        "note-0014.md"
    );
    // Offsets outside SQLite's signed range retain the general query behavior.
    let beyond_sqlite = runtime
        .open_read(
            &OperationRequest::new(
                OperationKind::Query,
                json!({"offset": u64::MAX, "limit": 1}),
            ),
            &context,
        )
        .unwrap();
    assert!(beyond_sqlite.next.is_none());
    assert!(beyond_sqlite.outcome.operation.to_v03().result["results"]
        .as_array()
        .unwrap()
        .is_empty());
}
