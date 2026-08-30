use super::*;
use crate::v03::OperationResult;
use crate::Collection;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::sync::{mpsc, Arc, Barrier, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
struct RecordingObserver {
    performance: Mutex<Vec<OperationPerformance>>,
    errors: Mutex<Vec<OperationError>>,
}

impl RuntimeObserver for RecordingObserver {
    fn on_performance(&self, observation: &OperationPerformance) {
        self.performance.lock().unwrap().push(observation.clone());
    }

    fn on_error(&self, observation: &OperationError) {
        self.errors.lock().unwrap().push(observation.clone());
    }
}

fn collection() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  default_validation: error\n",
    )
    .unwrap();
    directory
}

#[test]
fn authority_capture_sources_reject_fresh_tokens_and_unbounded_reads() {
    let sources = [
        ("snapshot", include_str!("../snapshot.rs")),
        ("discovery", include_str!("../snapshot/discovery.rs")),
        ("record_load", include_str!("../record_load.rs")),
        ("runtime_snapshot", include_str!("snapshot.rs")),
        ("mutation_shadow", include_str!("../mutation/shadow.rs")),
        (
            "mutation_preparation",
            include_str!("../mutation/preparation.rs"),
        ),
        ("mutation_batch", include_str!("../mutation/batch.rs")),
        ("runtime_batch", include_str!("../v03/batch.rs")),
        ("links", include_str!("../links/traversal.rs")),
        ("validation", include_str!("../validation/validator.rs")),
        ("backfill", include_str!("../operations/backfill.rs")),
    ];
    for (name, source) in sources {
        let production = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert!(
            !production.contains("OperationCancellation::new()"),
            "{name}"
        );
        assert!(!production.contains("fs::read("), "{name}");
        assert!(!production.contains("fs::read_to_string("), "{name}");
        assert!(!production.contains("collection.snapshot()"), "{name}");
        assert!(
            !production.contains("shadow.collection.snapshot()"),
            "{name}"
        );
    }
}

#[test]
fn provider_capture_limits_are_exact_and_never_publish_partial_snapshots() {
    let directory = collection();
    let config = fs::read(directory.path().join("mdbase.yaml")).unwrap();
    let record = b"---\ntitle: bounded\n---\nbody\n";
    fs::write(directory.path().join("bounded.md"), record).unwrap();
    let provider = FilesystemProvider::open(directory.path()).unwrap();
    let total = (config.len() + record.len()) as u64;
    let limits = CaptureLimits::builder()
        .max_entries(1)
        .max_resource_entries(1)
        .max_file_bytes(config.len().max(record.len()) as u64)
        .max_aggregate_bytes(total)
        .max_retained_bytes(total)
        .build();
    let context = OperationContext::with_capture_limits(
        &crate::OperationCancellation::new(),
        OperationDeadline::after(Duration::from_secs(1)),
        limits,
    );
    let snapshot = provider.snapshot_with_context(&context).unwrap();
    assert_eq!(snapshot.records.len(), 1);
    assert_eq!(snapshot.resources.len(), 1);

    let too_small = CaptureLimits::builder()
        .max_entries(1)
        .max_resource_entries(1)
        .max_file_bytes(config.len().max(record.len()) as u64)
        .max_aggregate_bytes(total - 1)
        .max_retained_bytes(total)
        .build();
    let context = OperationContext::with_capture_limits(
        &crate::OperationCancellation::new(),
        OperationDeadline::after(Duration::from_secs(1)),
        too_small,
    );
    assert!(matches!(
        provider.snapshot_with_context(&context),
        Err(ProviderError::CaptureLimitExceeded(CaptureLimitExceeded {
            kind: CaptureLimitKind::AggregateBytes,
            ..
        }))
    ));
}

#[test]
fn provider_capture_rejects_oversized_single_record_and_entry_boundary() {
    let directory = collection();
    fs::write(directory.path().join("large.md"), vec![b'x'; 128]).unwrap();
    let provider = FilesystemProvider::open(directory.path()).unwrap();
    let limits = CaptureLimits::builder()
        .max_entries(1)
        .max_file_bytes(127)
        .build();
    let context = OperationContext::with_capture_limits(
        &crate::OperationCancellation::new(),
        OperationDeadline::after(Duration::from_secs(1)),
        limits,
    );
    assert!(matches!(
        provider.snapshot_with_context(&context),
        Err(ProviderError::CaptureLimitExceeded(CaptureLimitExceeded {
            kind: CaptureLimitKind::FileBytes,
            limit: 127,
            attempted: 128
        }))
    ));

    let limits = CaptureLimits::builder().max_entries(0).build();
    let context = OperationContext::with_capture_limits(
        &crate::OperationCancellation::new(),
        OperationDeadline::after(Duration::from_secs(1)),
        limits,
    );
    assert!(matches!(
        provider.snapshot_with_context(&context),
        Err(ProviderError::CaptureLimitExceeded(CaptureLimitExceeded {
            kind: CaptureLimitKind::Entries,
            limit: 0,
            attempted: 1
        }))
    ));
}

#[test]
fn query_capture_limit_is_terminal_before_cache_fallback_and_scans_once() {
    let directory = collection();
    fs::write(
        directory.path().join("bounded.md"),
        "---\ntitle: bounded\n---\n",
    )
    .unwrap();
    let provider = FilesystemProvider::open(directory.path()).unwrap();
    let request = OperationRequest::new(OperationKind::Query, json!({}));

    for cache_state in ["missing", "warmed"] {
        if cache_state == "warmed" {
            provider.execute(&request).unwrap();
        } else {
            let _ = fs::remove_dir_all(directory.path().join(".mdbase/cache"));
        }
        crate::reset_snapshot_scan_calls_for_test();
        let limits = CaptureLimits::builder().max_entries(0).build();
        let context = OperationContext::with_capture_limits(
            &crate::OperationCancellation::new(),
            OperationDeadline::after(Duration::from_secs(2)),
            limits,
        );
        assert!(matches!(
            provider.execute_with_context(&request, &context),
            Err(ProviderError::CaptureLimitExceeded(CaptureLimitExceeded {
                kind: CaptureLimitKind::Entries,
                limit: 0,
                attempted: 1,
            }))
        ));
        assert_eq!(
            crate::snapshot_scan_calls_for_test(),
            1,
            "{cache_state} cache must not trigger a fallback scan"
        );
    }
}

#[test]
fn zero_retained_bytes_rejects_nonempty_regular_and_cache_backed_queries() {
    let directory = collection();
    fs::write(
        directory.path().join("bounded.md"),
        "---\ntitle: bounded\n---\n",
    )
    .unwrap();
    let provider = FilesystemProvider::open(directory.path()).unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let request = OperationRequest::new(OperationKind::Query, json!({}));
    let provider_context = OperationContext::with_capture_limits(
        &crate::OperationCancellation::new(),
        OperationDeadline::after(Duration::from_secs(2)),
        CaptureLimits::builder().max_retained_bytes(0).build(),
    );
    assert!(matches!(
        provider.execute_with_context(&request, &provider_context),
        Err(ProviderError::CaptureLimitExceeded(CaptureLimitExceeded {
            kind: CaptureLimitKind::RetainedBytes,
            limit: 0,
            ..
        }))
    ));

    for paged in [false, true] {
        let context = OperationContext::with_capture_limits(
            &crate::OperationCancellation::new(),
            OperationDeadline::after(Duration::from_secs(2)),
            CaptureLimits::builder().max_retained_bytes(0).build(),
        );
        let result = if paged {
            runtime.open_read(&request, &context).map(|_| ())
        } else {
            runtime.read(&request, &context).map(|_| ())
        };
        assert!(matches!(
            result,
            Err(ProviderError::CaptureLimitExceeded(CaptureLimitExceeded {
                kind: CaptureLimitKind::RetainedBytes,
                limit: 0,
                ..
            }))
        ));
    }
}

fn runtime_query_paths(runtime: &FilesystemRuntime) -> BTreeSet<String> {
    let outcome = runtime
        .read(
            &OperationRequest::new(
                OperationKind::Query,
                json!({"frontmatter_mode": "persisted"}),
            ),
            &OperationContext::legacy(),
        )
        .unwrap();
    let CanonicalOperationValue::Query(Some(query)) = &outcome.operation.value else {
        panic!("expected typed query outcome")
    };
    query
        .records
        .iter()
        .filter_map(|record| record["path"].as_str().map(str::to_string))
        .collect()
}

fn runtime_query_revision(runtime: &FilesystemRuntime, path: &str) -> String {
    let outcome = runtime
        .read(
            &OperationRequest::new(
                OperationKind::Query,
                json!({"frontmatter_mode": "persisted"}),
            ),
            &OperationContext::legacy(),
        )
        .unwrap();
    let CanonicalOperationValue::Query(Some(query)) = &outcome.operation.value else {
        panic!("expected typed query outcome")
    };
    query
        .records
        .iter()
        .find(|record| record["path"] == path)
        .unwrap()["file"]["revision"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn provider_observes_external_changes_between_requests() {
    let directory = collection();
    let provider = FilesystemProvider::open(directory.path()).unwrap();
    fs::write(
        directory.path().join("external.md"),
        "---\ntitle: External\n---\nBody\n",
    )
    .unwrap();

    let read = provider
        .execute(&OperationRequest::new(
            OperationKind::Read,
            json!({"path": "external.md"}),
        ))
        .unwrap();
    assert!(read.valid);
    assert_eq!(read.result["frontmatter"]["title"], "External");
}

#[test]
fn provider_snapshot_is_canonical_stable_and_observes_external_changes() {
    let directory = collection();
    fs::create_dir(directory.path().join("_types")).unwrap();
    fs::write(
        directory.path().join("_types/task.md"),
        "---\nkind: mdbase.type\nname: task\nversion: 1\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("task.md"),
        "---\ntitle: Original\n---\nBody\n",
    )
    .unwrap();
    let provider = FilesystemProvider::open(directory.path()).unwrap();

    let first = provider.snapshot().unwrap();
    let repeated = provider.snapshot().unwrap();
    assert_eq!(first, repeated);
    assert_eq!(first.resources.len(), 2);
    assert_eq!(first.resources[0].path, "mdbase.yaml");
    assert_eq!(first.records.len(), 1);
    assert_eq!(first.records[0].path, "task.md");
    assert_eq!(first.records[0].body, "Body\n");
    assert_eq!(
        first.records[0].document,
        "---\ntitle: Original\n---\nBody\n"
    );

    fs::write(
        directory.path().join("task.md"),
        "---\ntitle: Changed\n---\nBody\n",
    )
    .unwrap();
    let changed = provider.snapshot().unwrap();
    assert_ne!(changed.revision, first.revision);
    assert_eq!(changed.records[0].frontmatter["title"], "Changed");
}

#[test]
fn provider_snapshot_skips_paths_with_hidden_components() {
    let directory = collection();
    fs::create_dir_all(directory.path().join(".clump/commands")).unwrap();
    fs::create_dir_all(directory.path().join("notes/.private")).unwrap();
    fs::write(
        directory.path().join(".clump/commands/tool.md"),
        "---\ntitle: Tool configuration\n---\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("notes/.private/draft.md"),
        "---\ntitle: Private draft\n---\n",
    )
    .unwrap();
    fs::write(
        directory.path().join(".hidden.md"),
        "---\ntitle: Hidden root record\n---\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("visible.md"),
        "---\ntitle: Visible\n---\n",
    )
    .unwrap();

    let snapshot = FilesystemProvider::open(directory.path())
        .unwrap()
        .snapshot()
        .unwrap();

    assert_eq!(snapshot.records.len(), 1);
    assert_eq!(snapshot.records[0].path, "visible.md");
}

#[test]
fn provider_snapshot_preserves_invalid_frontmatter_as_opaque_markdown() {
    let directory = collection();
    fs::write(
        directory.path().join("broken.md"),
        "---\ntitle: [unterminated\n---\n",
    )
    .unwrap();

    let snapshot = FilesystemProvider::open(directory.path())
        .unwrap()
        .snapshot()
        .unwrap();

    assert_eq!(snapshot.records.len(), 1);
    let broken = &snapshot.records[0];
    assert_eq!(broken.path, "broken.md");
    assert!(broken.frontmatter.is_empty());
    assert_eq!(broken.body, "---\ntitle: [unterminated\n---\n");
    assert_eq!(broken.document, broken.body);
    assert_eq!(
        broken.frontmatter_error.as_deref(),
        Some("Failed to parse YAML frontmatter")
    );
}

#[test]
fn provider_snapshot_rejects_unrepresentable_utf8_without_synthetic_wire_content() {
    let directory = collection();
    fs::write(directory.path().join("binary.md"), b"bad\xffutf8\n").unwrap();
    fs::write(
        directory.path().join("healthy.md"),
        "---\ntitle: Healthy\n---\nBody\n",
    )
    .unwrap();

    let provider = FilesystemProvider::open(directory.path()).unwrap();
    let error = provider
        .snapshot()
        .expect_err("strict synchronization snapshot must not omit invalid bytes");
    assert!(error.to_string().contains("binary.md"));
    assert!(error.to_string().contains("invalid UTF-8"));

    let error = provider
        .snapshot_record("binary.md")
        .expect_err("targeted synchronization snapshot must also fail");
    assert!(error.to_string().contains("invalid UTF-8"));
}

#[test]
fn strict_snapshot_rejects_post_enumeration_record_disappearance() {
    let directory = collection();
    fs::write(
        directory.path().join("tracked.md"),
        "---\ntitle: Present\n---\n",
    )
    .unwrap();
    let collection = Collection::open(directory.path()).unwrap();
    crate::operations::set_record_open_failure(
        directory.path(),
        "tracked.md",
        Some(std::io::ErrorKind::NotFound),
    );
    let error = collection
        .snapshot()
        .expect_err("strict snapshot cannot publish an incomplete checkpoint");
    crate::operations::set_record_open_failure(directory.path(), "tracked.md", None);
    assert!(error.to_string().contains("tracked.md"));
    assert!(error.to_string().contains("became unavailable"));
}

#[test]
fn provider_snapshot_preserves_non_mapping_frontmatter_as_opaque_markdown() {
    let directory = collection();
    let document = "---\n- one\n- two\n---\nBody\n";
    fs::write(directory.path().join("list.md"), document).unwrap();

    let provider = FilesystemProvider::open(directory.path()).unwrap();
    let record = provider.snapshot_record("list.md").unwrap();

    assert!(record.frontmatter.is_empty());
    assert_eq!(record.body, document);
    assert_eq!(record.document, document);
    assert_eq!(
        record.frontmatter_error.as_deref(),
        Some("Frontmatter must be a YAML mapping")
    );
}

#[test]
fn canonical_create_preserves_opaque_body_while_typed_read_stays_strict() {
    let directory = collection();
    let collection = Collection::open(directory.path()).unwrap();
    let operations = collection.v03_operations().unwrap();
    let document = "---\ntitle: [unterminated\n---\nOpaque body";

    let created = operations.create(&json!({
        "path": "opaque.md",
        "frontmatter": {},
        "body": document,
    }));
    assert!(created.valid, "{:?}", created.diagnostics);
    assert_eq!(created.result["body"], document);

    let read = operations.read(&json!({"path": "opaque.md"}));
    assert!(!read.valid);
    assert_eq!(read.diagnostics[0].code, "invalid_frontmatter");

    let record = collection.snapshot_record("opaque.md").unwrap();
    assert_eq!(record.body, document);
    assert_eq!(record.document, document);
}

#[test]
fn provider_snapshot_includes_configured_saved_view_sources() {
    let directory = collection();
    fs::write(
        directory.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  default_validation: error\nx-obsidian:\n  bases:\n    include:\n      - views/**/*.base\n",
    )
    .unwrap();
    fs::create_dir(directory.path().join("views")).unwrap();
    fs::write(
        directory.path().join("views/tasks.base"),
        "views:\n  - type: table\n    name: Tasks\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("ignored.base"),
        "views:\n  - type: table\n    name: Ignored\n",
    )
    .unwrap();
    fs::create_dir_all(directory.path().join(".git/hooks")).unwrap();
    fs::write(
        directory.path().join(".git/hooks/post-commit.base"),
        "views:\n  - type: table\n    name: Hidden\n",
    )
    .unwrap();
    let provider = FilesystemProvider::open(directory.path()).unwrap();

    let first = provider.snapshot().unwrap();
    assert_eq!(first.resources.len(), 2);
    assert_eq!(first.resources[1].path, "views/tasks.base");
    assert_eq!(
        first.resources[1].kind,
        CollectionSnapshotResourceKind::View
    );
    assert!(first.resources[1].document.contains("name: Tasks"));
    assert!(first.records.is_empty());

    fs::write(
        directory.path().join("views/tasks.base"),
        "views:\n  - type: table\n    name: Changed\n",
    )
    .unwrap();
    let changed = provider.snapshot().unwrap();
    assert_ne!(changed.resource_revision, first.resource_revision);
    assert_ne!(changed.revision, first.revision);
}

#[test]
fn provider_snapshot_classifies_only_valid_canonical_markdown_views_as_resources() {
    let directory = collection();
    fs::create_dir(directory.path().join("views")).unwrap();
    fs::write(
        directory.path().join("views/tasks.md"),
        "---\ntype: view\nid: tasks\nversion: 1\nname: Tasks\nviews:\n  - id: all\n    name: All tasks\n---\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("views/invalid.md"),
        "---\ntype: view\nid: invalid\n---\nInvalid view\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("note.md"),
        "---\ntitle: Ordinary note\n---\nBody\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("broken.md"),
        "---\ntitle: [unterminated\n---\nOpaque\n",
    )
    .unwrap();

    let snapshot = FilesystemProvider::open(directory.path())
        .unwrap()
        .snapshot()
        .unwrap();

    let view_resources = snapshot
        .resources
        .iter()
        .filter(|resource| resource.kind == CollectionSnapshotResourceKind::View)
        .collect::<Vec<_>>();
    assert_eq!(view_resources.len(), 1);
    assert_eq!(view_resources[0].path, "views/tasks.md");
    assert_eq!(
        snapshot
            .records
            .iter()
            .map(|record| record.path.as_str())
            .collect::<Vec<_>>(),
        ["broken.md", "note.md", "views/invalid.md"]
    );
    assert_eq!(
        snapshot.records[0].frontmatter_error.as_deref(),
        Some("Failed to parse YAML frontmatter")
    );
}

#[test]
fn provider_snapshot_includes_contract_and_schema_resources() {
    let directory = collection();
    fs::create_dir(directory.path().join("_contracts")).unwrap();
    fs::create_dir(directory.path().join("_schemas")).unwrap();
    fs::create_dir_all(directory.path().join(".git/hooks")).unwrap();
    fs::write(
        directory.path().join("_contracts/task.md"),
        "---\nkind: mdbase.contract\ncontract_type: record\nid: example.task\nversion: 1.0.0\nrecord_schema:\n  dialect: json-schema-2020-12\n  ref: ../_schemas/task.json\n---\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("_schemas/task.json"),
        "{\"$schema\":\"https://json-schema.org/draft/2020-12/schema\",\"type\":\"object\"}\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("package.json"),
        "{\"scripts\":{\"postinstall\":\"malware\"}}\n",
    )
    .unwrap();
    fs::write(
        directory.path().join(".git/hooks/payload.json"),
        "{\"type\":\"object\"}\n",
    )
    .unwrap();
    let provider = FilesystemProvider::open(directory.path()).unwrap();

    let snapshot = provider.snapshot().unwrap();
    assert_eq!(snapshot.resources.len(), 3);
    assert_eq!(snapshot.records.len(), 0);
    assert_eq!(snapshot.resources[1].path, "_contracts/task.md");
    assert_eq!(
        snapshot.resources[1].kind,
        CollectionSnapshotResourceKind::Contract
    );
    assert_eq!(snapshot.resources[2].path, "_schemas/task.json");
    assert_eq!(
        snapshot.resources[2].kind,
        CollectionSnapshotResourceKind::Schema
    );
}

#[test]
fn provider_snapshot_includes_the_portable_type_pack_lock() {
    let directory = collection();
    fs::write(
        directory.path().join("mdbase.lock.yaml"),
        "{\n  \"kind\": \"mdbase.type-pack-lock\",\n  \"lock_version\": 1,\n  \"packs\": []\n}\n",
    )
    .unwrap();
    let provider = FilesystemProvider::open(directory.path()).unwrap();

    let snapshot = provider.snapshot().unwrap();
    assert_eq!(snapshot.resources.len(), 2);
    assert_eq!(snapshot.resources[1].path, "mdbase.lock.yaml");
    assert_eq!(
        snapshot.resources[1].kind,
        CollectionSnapshotResourceKind::Lock
    );
}

#[test]
fn provider_snapshot_matches_the_portable_sdk_digest_fixture() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nx-obsidian:\n  bases:\n    include:\n      - views/**/*.base\n",
    )
    .unwrap();
    fs::create_dir(directory.path().join("views")).unwrap();
    fs::write(directory.path().join("views/tasks.base"), "views: []\n").unwrap();
    fs::create_dir(directory.path().join("notes")).unwrap();
    fs::write(
        directory.path().join("notes/one.md"),
        "---\ntitle: One\n---\nBody\n",
    )
    .unwrap();

    let snapshot = FilesystemProvider::open(directory.path())
        .unwrap()
        .snapshot()
        .unwrap();
    assert_eq!(
        snapshot.resource_revision,
        "sha256:09367b66bc7e29a90ee2cafa992f3477dd523d09558f542fb9fe4418312984a8"
    );
    assert_eq!(
        snapshot.revision,
        "sha256:b01ab663203cd44b2a837e0a2fcf73f06bd0cae8787efb3148c3821f255e4806"
    );
}

#[test]
fn runtime_public_batch_rejects_partial_before_item_decode_or_shadow() {
    let directory = collection();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let before = runtime.current_generation().unwrap();
    crate::mutation::reset_mutation_path_probes();
    let request = OperationRequest::new(
        OperationKind::Batch,
        json!({
            "allow_partial": true,
            "operations": [
                {"kind": "create", "input": {"path": "must-not-stage.md"}},
                {"input": "malformed and must not decode"}
            ]
        }),
    );
    let outcome = runtime
        .prepare(
            &request,
            &HostClaimId::generate(),
            &OperationContext::legacy(),
        )
        .unwrap();
    let PreparationOutcome::NoMutation(outcome) = outcome else {
        panic!("partial runtime batch must be rejected without preparation")
    };
    assert!(!outcome.operation.valid);
    assert_eq!(
        outcome.operation.to_v03().diagnostics[0].code,
        "invalid_request"
    );
    assert_eq!(runtime.current_generation().unwrap(), before);
    assert!(!directory.path().join("must-not-stage.md").exists());
    assert_eq!(
        crate::mutation::mutation_path_probes(),
        crate::mutation::MutationPathProbes::default()
    );
}

#[test]
fn runtime_atomic_batch_commits_multiple_renames_reference_updates_and_replays_claim() {
    let directory = collection();
    fs::write(directory.path().join("old-one.md"), "one\n").unwrap();
    fs::write(directory.path().join("old-two.md"), "two\n").unwrap();
    fs::write(
        directory.path().join("refs.md"),
        "See [[old-one]] and [[old-two]].\n",
    )
    .unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let claim = HostClaimId::generate();
    let request = OperationRequest::new(
        OperationKind::Batch,
        json!({
            "operations": [
                {"kind": "rename", "input": {
                    "from": "old-one.md", "to": "new-one.md", "update_refs": true,
                    "include_document": true
                }},
                {"kind": "rename", "input": {
                    "from": "old-two.md", "to": "new-two.md", "update_refs": true,
                    "include_document": true
                }}
            ]
        }),
    );
    crate::mutation::reset_mutation_path_probes();
    let prepared = match runtime
        .prepare(&request, &claim, &OperationContext::legacy())
        .unwrap()
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        other => panic!("expected atomic batch preparation: {other:?}"),
    };
    assert_eq!(crate::mutation::mutation_path_probes().full_shadows, 1);
    assert!(!directory.path().join("new-one.md").exists());
    crate::transactions::inject_post_commit_replacement(
        directory.path(),
        "new-one.md",
        Some(b"external replacement".to_vec()),
    );
    let committed = match runtime
        .commit(&prepared, &OperationContext::legacy())
        .unwrap()
    {
        CommitAttempt::Committed(outcome) => outcome,
        other => panic!("expected committed batch: {other:?}"),
    };
    assert!(
        committed.operation.valid,
        "{:?}",
        committed.operation.to_v03().diagnostics
    );
    assert_eq!(committed.operation.to_v03().result["succeeded"], 2);
    for item in committed.operation.to_v03().result["operations"]
        .as_array()
        .unwrap()
    {
        assert!(item["result"]["file"]["size"].as_u64().unwrap() > 0);
        assert!(!item["result"]["file"]["mtime"].as_str().unwrap().is_empty());
    }
    assert_eq!(
        committed.operation.to_v03().result["operations"][0]["result"]["document"],
        "one\n"
    );
    assert_eq!(
        committed.operation.to_v03().result["operations"][0]["result"]["file"]["size"],
        4
    );
    assert_eq!(
        fs::read(directory.path().join("new-one.md")).unwrap(),
        b"external replacement"
    );
    let ChangeSet::Exact(changes) = &committed.changes else {
        panic!("batch must publish exact changes")
    };
    let page = changes
        .page(
            None,
            std::num::NonZeroUsize::new(10).unwrap(),
            std::num::NonZeroUsize::new(10).unwrap(),
        )
        .unwrap();
    assert_eq!(
        page.items
            .iter()
            .filter(|change| matches!(
                change,
                CanonicalChange::Record(RecordChange {
                    kind: RecordChangeKind::Renamed,
                    ..
                })
            ))
            .count(),
        2
    );
    assert!(page.items.iter().any(|change| matches!(
        change,
        CanonicalChange::Record(RecordChange {
            kind: RecordChangeKind::Updated,
            path,
            ..
        }) if path.as_str() == "refs.md"
    )));
    assert!(fs::read_to_string(directory.path().join("refs.md"))
        .unwrap()
        .contains("[[new-one]]"));
    assert!(matches!(
        runtime
            .resolve_claim(&claim, &OperationContext::legacy())
            .unwrap(),
        Some((commit_id, DurableCommitState::Committed { .. })) if commit_id == *prepared.commit_id()
    ));
    drop(runtime);
    let reopened = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    assert!(matches!(
        reopened
            .resolve_claim(&claim, &OperationContext::legacy())
            .unwrap(),
        Some((commit_id, DurableCommitState::Committed { .. })) if commit_id == *prepared.commit_id()
    ));
}

#[test]
fn runtime_atomic_batch_cancel_cas_claim_and_prepared_reopen_are_durable() {
    let directory = collection();
    fs::write(directory.path().join("cas.md"), "---\ntitle: Before\n---\n").unwrap();

    let cancelled_claim = HostClaimId::generate();
    let cancelled_request = OperationRequest::new(
        OperationKind::Batch,
        json!({"operations": [
            {"kind": "create", "input": {"path": "cancel-one.md"}},
            {"kind": "create", "input": {"path": "cancel-two.md"}}
        ]}),
    );
    let cancelled_commit = {
        let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
        match runtime
            .prepare(
                &cancelled_request,
                &cancelled_claim,
                &OperationContext::legacy(),
            )
            .unwrap()
        {
            PreparationOutcome::Prepared(prepared) => prepared.commit_id().clone(),
            other => panic!("expected prepared batch: {other:?}"),
        }
    };
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let attached = runtime
        .attach_prepared(&cancelled_claim, &OperationContext::legacy())
        .unwrap()
        .expect("prepared batch claim must survive reopen");
    assert_eq!(attached.commit_id(), &cancelled_commit);
    assert_eq!(
        runtime
            .cancel(&attached, &OperationContext::legacy())
            .unwrap(),
        CancelOutcome::CancelledBeforeCommit
    );
    assert!(!directory.path().join("cancel-one.md").exists());
    assert!(!directory.path().join("cancel-two.md").exists());
    assert!(matches!(
        runtime
            .resolve_claim(&cancelled_claim, &OperationContext::legacy())
            .unwrap(),
        Some((commit_id, DurableCommitState::CancelledBeforeCommit))
            if commit_id == cancelled_commit
    ));

    let conflict_claim = HostClaimId::generate();
    let conflict_request = OperationRequest::new(
        OperationKind::Batch,
        json!({"operations": [
            {"kind": "update", "input": {
                "path": "cas.md", "fields": {"title": "Planned"}
            }},
            {"kind": "create", "input": {"path": "cas-sibling.md"}}
        ]}),
    );
    let conflict = match runtime
        .prepare(
            &conflict_request,
            &conflict_claim,
            &OperationContext::legacy(),
        )
        .unwrap()
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        other => panic!("expected prepared CAS batch: {other:?}"),
    };
    assert!(matches!(
        runtime.prepare(
            &OperationRequest::new(
                OperationKind::Batch,
                json!({"operations": [
                    {"kind": "create", "input": {"path": "claim-mismatch.md"}}
                ]}),
            ),
            &conflict_claim,
            &OperationContext::legacy(),
        ),
        Err(ProviderError::ClaimMismatch)
    ));
    fs::write(
        directory.path().join("cas.md"),
        "---\ntitle: External\n---\n",
    )
    .unwrap();
    let rejection = match runtime
        .commit(&conflict, &OperationContext::legacy())
        .unwrap()
    {
        CommitAttempt::RejectedBeforeCommit { rejection } => rejection,
        other => panic!("expected atomic CAS rejection: {other:?}"),
    };
    assert_eq!(
        rejection.operation.to_v03().diagnostics[0].code,
        "concurrent_modification"
    );
    assert!(!directory.path().join("cas-sibling.md").exists());
    assert!(fs::read_to_string(directory.path().join("cas.md"))
        .unwrap()
        .contains("External"));
    drop(runtime);
    let reopened = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    assert!(matches!(
        reopened
            .resolve_claim(&conflict_claim, &OperationContext::legacy())
            .unwrap(),
        Some((commit_id, DurableCommitState::RejectedBeforeCommit { .. }))
            if commit_id == *conflict.commit_id()
    ));
}

#[test]
fn runtime_atomic_batch_recovers_every_durable_settlement_crash_boundary() {
    for point in 1..=4 {
        let directory = collection();
        let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
        let claim = HostClaimId::generate();
        let request = OperationRequest::new(
            OperationKind::Batch,
            json!({"operations": [
                {"kind": "create", "input": {
                    "path": format!("crash-{point}-one.md"), "body": "one"
                }},
                {"kind": "create", "input": {
                    "path": format!("crash-{point}-two.md"), "body": "two"
                }}
            ]}),
        );
        let prepared = match runtime
            .prepare(&request, &claim, &OperationContext::legacy())
            .unwrap()
        {
            PreparationOutcome::Prepared(prepared) => prepared,
            other => panic!("expected prepared batch at crash point {point}: {other:?}"),
        };
        crate::transactions::set_runtime_crash_point(prepared.commit_id(), point);
        assert!(runtime
            .commit(&prepared, &OperationContext::legacy())
            .is_err());
        drop(runtime);

        let reopened = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
        assert!(matches!(
            reopened
                .resolve_claim(&claim, &OperationContext::legacy())
                .unwrap(),
            Some((commit_id, DurableCommitState::Committed { .. }))
                if commit_id == *prepared.commit_id()
        ));
        for suffix in ["one", "two"] {
            assert_eq!(
                fs::read_to_string(directory.path().join(format!("crash-{point}-{suffix}.md")),)
                    .unwrap(),
                suffix
            );
        }
    }
}

#[test]
fn runtime_prepare_is_durable_but_does_not_change_canonical_files() {
    let directory = collection();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let claim = HostClaimId::generate();
    let request = OperationRequest::new(
        OperationKind::Create,
        json!({
            "path": "prepared.md",
            "frontmatter": {"title": "Prepared"},
            "body": "Body\n"
        }),
    );
    let prepared = match runtime
        .prepare(&request, &claim, &OperationContext::legacy())
        .unwrap()
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        other => panic!("expected a prepared mutation, got {other:?}"),
    };
    assert!(!directory.path().join("prepared.md").exists());
    assert!(matches!(
        runtime
            .resolve_claim(&claim, &OperationContext::legacy())
            .unwrap(),
        Some((commit_id, DurableCommitState::Prepared)) if commit_id == *prepared.commit_id()
    ));

    let outcome = match runtime
        .commit(&prepared, &OperationContext::legacy())
        .unwrap()
    {
        CommitAttempt::Committed(outcome) => outcome,
        other => panic!("expected a committed mutation, got {other:?}"),
    };
    assert!(outcome.operation.valid);
    assert_eq!(outcome.generation.sequence(), 1);
    assert_eq!(outcome.commit_id.as_ref(), Some(prepared.commit_id()));
    assert_eq!(
        outcome
            .change_event
            .as_ref()
            .expect("committed mutation has an event")
            .watermark
            .get(),
        1
    );
    assert!(
        matches!(outcome.changes, ChangeSet::Exact(ref batch) if batch.descriptor().count == 1)
    );
    assert!(directory.path().join("prepared.md").exists());
}

#[test]
fn runtime_create_update_replay_committed_facts_after_reopen_and_postcommit_path_changes() {
    let directory = collection();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    crate::mutation::reset_mutation_path_probes();
    let claim = HostClaimId::generate();
    let create = OperationRequest::new(
        OperationKind::Create,
        json!({"path": "runtime-facts.md", "body": "planned", "include_document": true}),
    );
    let prepared = match runtime
        .prepare(&create, &claim, &OperationContext::legacy())
        .unwrap()
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        other => panic!("expected create preparation: {other:?}"),
    };
    assert_eq!(
        crate::mutation::mutation_path_probes(),
        crate::mutation::MutationPathProbes {
            wire_request_decodes: 1,
            runtime_request_decodes: 1,
            sparse_shadows: 1,
            ..Default::default()
        }
    );
    crate::transactions::inject_post_commit_replacement(
        directory.path(),
        "runtime-facts.md",
        Some(b"external replacement".to_vec()),
    );
    let created = match runtime
        .commit(&prepared, &OperationContext::legacy())
        .unwrap()
    {
        CommitAttempt::Committed(outcome) => outcome,
        other => panic!("expected committed create: {other:?}"),
    };
    assert_eq!(created.operation.to_v03().result["document"], "planned");
    assert_eq!(
        created.operation.to_v03().result["revision"],
        crate::v03::revision(b"planned")
    );
    assert_eq!(created.operation.to_v03().result["file"]["size"], 7);
    assert_ne!(created.operation.to_v03().result["file"]["mtime"], "");
    assert_eq!(
        fs::read(directory.path().join("runtime-facts.md")).unwrap(),
        b"external replacement"
    );
    let create_commit_id = prepared.commit_id().clone();
    drop(runtime);
    let reopened = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    assert_eq!(
        reopened
            .resolve_commit(&create_commit_id, &OperationContext::legacy())
            .unwrap(),
        Some(DurableCommitState::Committed {
            outcome: created.clone()
        })
    );
    assert_eq!(
        reopened
            .resolve_claim(&claim, &OperationContext::legacy())
            .unwrap(),
        Some((
            create_commit_id,
            DurableCommitState::Committed { outcome: created }
        ))
    );
    drop(reopened);

    let directory = collection();
    fs::write(directory.path().join("runtime-facts.md"), "before").unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    crate::mutation::reset_mutation_path_probes();
    let claim = HostClaimId::generate();
    let update = OperationRequest::new(
        OperationKind::Update,
        json!({"path": "runtime-facts.md", "document": "after"}),
    );
    let prepared = match runtime
        .prepare(&update, &claim, &OperationContext::legacy())
        .unwrap()
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        other => panic!("expected update preparation: {other:?}"),
    };
    assert_eq!(
        crate::mutation::mutation_path_probes(),
        crate::mutation::MutationPathProbes {
            wire_request_decodes: 1,
            runtime_request_decodes: 1,
            sparse_shadows: 1,
            ..Default::default()
        }
    );
    crate::transactions::inject_post_commit_replacement(directory.path(), "runtime-facts.md", None);
    let updated = match runtime
        .commit(&prepared, &OperationContext::legacy())
        .unwrap()
    {
        CommitAttempt::Committed(outcome) => outcome,
        other => panic!("expected committed update: {other:?}"),
    };
    assert_eq!(updated.operation.to_v03().result["document"], "after");
    assert_eq!(
        updated.operation.to_v03().result["revision"],
        crate::v03::revision(b"after")
    );
    assert_eq!(updated.operation.to_v03().result["file"]["size"], 5);
    assert_ne!(updated.operation.to_v03().result["file"]["mtime"], "");
    assert!(!directory.path().join("runtime-facts.md").exists());
    let update_commit_id = prepared.commit_id().clone();
    drop(runtime);
    let reopened = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    assert_eq!(
        reopened
            .resolve_commit(&update_commit_id, &OperationContext::legacy())
            .unwrap(),
        Some(DurableCommitState::Committed {
            outcome: updated.clone()
        })
    );
    assert_eq!(
        reopened
            .resolve_claim(&claim, &OperationContext::legacy())
            .unwrap(),
        Some((
            update_commit_id,
            DurableCommitState::Committed { outcome: updated }
        ))
    );
}

#[test]
fn runtime_rename_replays_planned_facts_and_exact_changes_after_reopen() {
    let directory = collection();
    let source = b"---\nid: stable\n---\nSee [[source]].\n";
    let reference = b"See [[source]].\n";
    fs::write(directory.path().join("source.md"), source).unwrap();
    fs::write(directory.path().join("reference.md"), reference).unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    crate::mutation::reset_mutation_path_probes();
    let claim = HostClaimId::generate();
    let request = OperationRequest::new(
        OperationKind::Rename,
        json!({
            "from": "source.md",
            "to": "renamed.md",
            "update_refs": true,
            "include_document": true
        }),
    );
    let prepared = match runtime
        .prepare(&request, &claim, &OperationContext::legacy())
        .unwrap()
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        other => panic!("expected rename preparation: {other:?}"),
    };
    assert_eq!(
        crate::mutation::mutation_path_probes(),
        crate::mutation::MutationPathProbes {
            wire_request_decodes: 1,
            runtime_request_decodes: 1,
            full_shadows: 1,
            ..Default::default()
        }
    );
    crate::transactions::inject_post_commit_replacement(
        directory.path(),
        "renamed.md",
        Some(b"external replacement".to_vec()),
    );
    let renamed = match runtime
        .commit(&prepared, &OperationContext::legacy())
        .unwrap()
    {
        CommitAttempt::Committed(outcome) => outcome,
        other => panic!("expected committed rename: {other:?}"),
    };
    assert_eq!(renamed.operation.to_v03().result["from"], "source.md");
    assert_eq!(renamed.operation.to_v03().result["to"], "renamed.md");
    let renamed_wire = renamed.operation.to_v03();
    let planned_document = renamed_wire.result["document"].as_str().unwrap();
    assert_eq!(
        renamed.operation.to_v03().result["file"]["size"],
        planned_document.len()
    );
    assert_eq!(
        renamed.operation.to_v03().result["revision"],
        crate::v03::revision(planned_document.as_bytes())
    );
    assert_ne!(renamed.operation.to_v03().result["file"]["mtime"], "");
    assert!(planned_document.contains("[[renamed]]"));
    let ChangeSet::Exact(changes) = &renamed.changes else {
        panic!("runtime rename must retain exact changes")
    };
    assert_eq!(changes.items().len(), 2);
    let records = changes
        .items()
        .iter()
        .map(|change| match change {
            CanonicalChange::Record(record) => record,
            _ => panic!("rename effects must be record changes"),
        })
        .collect::<Vec<_>>();
    let primary = records
        .iter()
        .find(|record| record.kind == RecordChangeKind::Renamed)
        .unwrap();
    assert_eq!(primary.from.as_ref().unwrap().as_str(), "source.md");
    let reference_change = records
        .iter()
        .find(|record| record.kind == RecordChangeKind::Updated)
        .unwrap();
    assert_eq!(reference_change.path.as_str(), "reference.md");
    assert_eq!(
        fs::read(directory.path().join("renamed.md")).unwrap(),
        b"external replacement"
    );
    let commit_id = prepared.commit_id().clone();
    drop(runtime);
    let reopened = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    assert_eq!(
        reopened
            .resolve_commit(&commit_id, &OperationContext::legacy())
            .unwrap(),
        Some(DurableCommitState::Committed {
            outcome: renamed.clone()
        })
    );
    assert_eq!(
        reopened
            .resolve_claim(&claim, &OperationContext::legacy())
            .unwrap(),
        Some((
            commit_id,
            DurableCommitState::Committed { outcome: renamed }
        ))
    );
}

#[test]
fn runtime_delete_uses_sparse_stage_and_replays_planned_result_after_reopen() {
    let directory = collection();
    fs::create_dir(directory.path().join("_types")).unwrap();
    fs::write(
        directory.path().join("_types/runtime-delete.md"),
        "---\nkind: mdbase.type\nname: runtime-delete\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n",
    )
    .unwrap();
    let before = b"---\ntype: runtime-delete\n---\nbefore";
    fs::write(directory.path().join("runtime-delete.md"), before).unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    crate::mutation::reset_mutation_path_probes();
    let claim = HostClaimId::generate();
    let request = OperationRequest::new(
        OperationKind::Delete,
        json!({"path": "runtime-delete.md", "check_backlinks": true}),
    );
    let prepared = match runtime
        .prepare(&request, &claim, &OperationContext::legacy())
        .unwrap()
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        other => panic!("expected delete preparation: {other:?}"),
    };
    assert_eq!(
        crate::mutation::mutation_path_probes(),
        crate::mutation::MutationPathProbes {
            wire_request_decodes: 1,
            runtime_request_decodes: 1,
            sparse_shadows: 1,
            ..Default::default()
        }
    );
    crate::transactions::inject_post_commit_replacement(
        directory.path(),
        "runtime-delete.md",
        Some(b"external replacement".to_vec()),
    );
    let deleted = match runtime
        .commit(&prepared, &OperationContext::legacy())
        .unwrap()
    {
        CommitAttempt::Committed(outcome) => outcome,
        other => panic!("expected committed delete: {other:?}"),
    };
    assert_eq!(
        deleted.operation.to_v03().result["path"],
        "runtime-delete.md"
    );
    assert_eq!(deleted.operation.to_v03().result["deleted"], true);
    let ChangeSet::Exact(changes) = &deleted.changes else {
        panic!("runtime delete must retain one exact change")
    };
    let [CanonicalChange::Record(change)] = changes.items() else {
        panic!("runtime delete must retain one record change")
    };
    assert_eq!(change.kind, RecordChangeKind::Deleted);
    assert_eq!(
        change.before_revision.as_ref().unwrap().as_str(),
        crate::v03::revision(before)
    );
    assert_eq!(
        change.before_types.iter().collect::<Vec<_>>(),
        ["runtime-delete"]
    );
    assert!(change.after_revision.is_none());
    assert_eq!(change.after_types.iter().count(), 0);
    assert_eq!(
        fs::read(directory.path().join("runtime-delete.md")).unwrap(),
        b"external replacement"
    );
    let commit_id = prepared.commit_id().clone();
    drop(runtime);
    let reopened = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    assert_eq!(
        reopened
            .resolve_commit(&commit_id, &OperationContext::legacy())
            .unwrap(),
        Some(DurableCommitState::Committed {
            outcome: deleted.clone()
        })
    );
    assert_eq!(
        reopened
            .resolve_claim(&claim, &OperationContext::legacy())
            .unwrap(),
        Some((
            commit_id,
            DurableCommitState::Committed { outcome: deleted }
        ))
    );
}

#[test]
fn runtime_delete_dry_run_missing_and_stale_are_generation_neutral() {
    let directory = collection();
    let bytes = b"before";
    fs::write(directory.path().join("target.md"), bytes).unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let generation = runtime.current_generation().unwrap();

    for (input, valid, code) in [
        (json!({"path": "target.md", "dry_run": true}), true, None),
        (json!({"path": "missing.md"}), false, Some("file_not_found")),
        (
            json!({"path": "target.md", "if_revision": "sha256:stale"}),
            false,
            Some("concurrent_modification"),
        ),
    ] {
        crate::mutation::reset_mutation_path_probes();
        let outcome = runtime
            .prepare(
                &OperationRequest::new(OperationKind::Delete, input),
                &HostClaimId::generate(),
                &OperationContext::legacy(),
            )
            .unwrap();
        let PreparationOutcome::NoMutation(outcome) = outcome else {
            panic!("dry-run and rejected deletes must not stage a transaction")
        };
        assert_eq!(outcome.operation.valid, valid);
        if let Some(code) = code {
            assert_eq!(outcome.operation.to_v03().diagnostics[0].code, code);
        } else {
            assert_eq!(outcome.operation.to_v03().result["would_delete"], true);
        }
        assert_eq!(outcome.generation, generation);
        assert!(matches!(outcome.changes, ChangeSet::None));
        assert_eq!(
            crate::mutation::mutation_path_probes(),
            crate::mutation::MutationPathProbes {
                wire_request_decodes: 1,
                runtime_request_decodes: 1,
                sparse_shadows: 1,
                ..Default::default()
            }
        );
        assert_eq!(fs::read(directory.path().join("target.md")).unwrap(), bytes);
        assert_eq!(runtime.current_generation().unwrap(), generation);
    }
}

#[test]
fn runtime_sparse_mutations_reject_invalid_revisions_with_direct_wire_parity() {
    let directory = collection();
    let before = b"before";
    fs::write(directory.path().join("target.md"), before).unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let generation = runtime.current_generation().unwrap();
    let direct_collection = Collection::open(directory.path()).unwrap();
    let direct = crate::v03::Operations::new(&direct_collection).unwrap();
    let invalid_revisions = [
        Value::Null,
        json!(7),
        json!({}),
        json!([]),
        json!(""),
        json!(true),
    ];

    for (operation, kind) in [
        ("create", OperationKind::Create),
        ("update", OperationKind::Update),
        ("delete", OperationKind::Delete),
    ] {
        for revision in &invalid_revisions {
            let path = if operation == "create" {
                "created.md"
            } else {
                "target.md"
            };
            let mut input = json!({"path": path, "if_revision": revision});
            if operation == "update" {
                input["patch"] = json!({"changed": true});
            }
            let direct_result = match operation {
                "create" => direct.create(&input),
                "update" => direct.update(&input),
                "delete" => direct.delete(&input),
                _ => unreachable!(),
            };
            assert!(!direct_result.valid);
            assert_eq!(direct_result.diagnostics[0].code, "invalid_request");

            let prepared = runtime
                .prepare(
                    &OperationRequest::new(kind, input),
                    &HostClaimId::generate(),
                    &OperationContext::legacy(),
                )
                .unwrap();
            let PreparationOutcome::NoMutation(outcome) = prepared else {
                panic!("invalid revision must not prepare {operation}")
            };
            assert!(!outcome.operation.valid);
            assert_eq!(outcome.operation.to_v03().result, direct_result.result);
            assert_eq!(
                outcome.operation.to_v03().diagnostics,
                direct_result.diagnostics
            );
            assert_eq!(outcome.generation, generation);
            assert!(matches!(outcome.changes, ChangeSet::None));
            assert_eq!(
                fs::read(directory.path().join("target.md")).unwrap(),
                before
            );
            assert!(!directory.path().join("created.md").exists());
            assert_eq!(runtime.current_generation().unwrap(), generation);
        }
    }
}

#[test]
fn runtime_delete_invalid_record_matrix_retains_exact_revision_and_path_types() {
    for (name, bytes) in [
        ("malformed.md", b"---\ntitle: [bad\n---\nBody\n".as_slice()),
        ("nonmapping.md", b"---\n- item\n---\nBody\n".as_slice()),
        ("binary.md", b"bad\xffutf8".as_slice()),
    ] {
        let directory = collection();
        fs::create_dir(directory.path().join("_types")).unwrap();
        fs::write(
            directory.path().join("_types/path-record.md"),
            format!(
                "---\nkind: mdbase.type\nname: path-record\nmatch:\n  path_glob: '{name}'\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n"
            ),
        )
        .unwrap();
        fs::write(directory.path().join(name), bytes).unwrap();
        let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
        let prepared = match runtime
            .prepare(
                &OperationRequest::new(OperationKind::Delete, json!({"path": name})),
                &HostClaimId::generate(),
                &OperationContext::legacy(),
            )
            .unwrap()
        {
            PreparationOutcome::Prepared(prepared) => prepared,
            other => panic!("invalid record delete must prepare: {other:?}"),
        };
        let committed = match runtime
            .commit(&prepared, &OperationContext::legacy())
            .unwrap()
        {
            CommitAttempt::Committed(outcome) => outcome,
            other => panic!("invalid record delete must commit: {other:?}"),
        };
        let ChangeSet::Exact(changes) = committed.changes else {
            panic!("invalid record delete must be exact")
        };
        let [CanonicalChange::Record(change)] = changes.items() else {
            panic!("invalid record delete must contain one record change")
        };
        assert_eq!(change.kind, RecordChangeKind::Deleted);
        assert_eq!(
            change.before_revision.as_ref().unwrap().as_str(),
            crate::v03::revision(bytes)
        );
        assert_eq!(
            change.before_types.iter().collect::<Vec<_>>(),
            ["path-record"]
        );
        assert!(!directory.path().join(name).exists());
    }
}

#[test]
fn runtime_delete_backlinks_survive_planning_but_commit_conflicts_reject() {
    let directory = collection();
    fs::write(directory.path().join("target.md"), "before").unwrap();
    fs::write(directory.path().join("ref.md"), "See [[target]].\n").unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let generation = runtime.current_generation().unwrap();
    let preview = runtime
        .prepare(
            &OperationRequest::new(
                OperationKind::Delete,
                json!({"path": "target.md", "check_backlinks": true, "dry_run": true}),
            ),
            &HostClaimId::generate(),
            &OperationContext::legacy(),
        )
        .unwrap();
    let PreparationOutcome::NoMutation(preview) = preview else {
        panic!("delete preview must not stage")
    };
    assert_eq!(
        preview.operation.to_v03().result["broken_links"],
        json!([{"path": "ref.md"}])
    );

    let prepared = match runtime
        .prepare(
            &OperationRequest::new(
                OperationKind::Delete,
                json!({"path": "target.md", "check_backlinks": true}),
            ),
            &HostClaimId::generate(),
            &OperationContext::legacy(),
        )
        .unwrap()
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        other => panic!("delete must prepare before the conflict: {other:?}"),
    };
    fs::write(directory.path().join("target.md"), "external conflict").unwrap();
    let rejection = runtime
        .commit(&prepared, &OperationContext::legacy())
        .unwrap();
    let CommitAttempt::RejectedBeforeCommit { rejection } = rejection else {
        panic!("commit-time replacement must reject")
    };
    assert_eq!(
        rejection.operation.to_v03().diagnostics[0].code,
        "concurrent_modification"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("target.md")).unwrap(),
        "external conflict"
    );
    assert_eq!(runtime.current_generation().unwrap(), generation);
}

#[test]
fn runtime_commits_type_resources_without_a_host_side_mutation_path() {
    let directory = collection();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let request = OperationRequest::new(
        OperationKind::CreateType,
        json!({
            "document": "---\nkind: mdbase.type\nname: project\nversion: 1\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n"
        }),
    );
    let claim = HostClaimId::generate();
    let prepared = match runtime
        .prepare(&request, &claim, &OperationContext::legacy())
        .unwrap()
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        other => panic!("expected a prepared type mutation, got {other:?}"),
    };
    assert!(!directory.path().join("_types/project.md").exists());

    let outcome = match runtime
        .commit(&prepared, &OperationContext::legacy())
        .unwrap()
    {
        CommitAttempt::Committed(outcome) => outcome,
        other => panic!("expected a committed type mutation, got {other:?}"),
    };
    assert!(
        outcome.operation.valid,
        "{:?}",
        outcome.operation.to_v03().diagnostics
    );
    assert!(directory.path().join("_types/project.md").is_file());
    let ChangeSet::Exact(changes) = outcome.changes else {
        panic!("type mutation must return exact resource changes")
    };
    assert!(matches!(
        changes.items(),
        [CanonicalChange::Resource(ResourceChange {
            kind: ResourceChangeKind::TypeDefinition,
            ..
        })]
    ));
}

#[test]
fn runtime_prepared_claim_reattaches_after_restart_without_recovery_loop() {
    let directory = collection();
    let claim = HostClaimId::generate();
    let request = OperationRequest::new(
        OperationKind::Create,
        json!({"path": "restart.md", "frontmatter": {"title": "Restart"}}),
    );
    let commit_id = {
        let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
        match runtime
            .prepare(&request, &claim, &OperationContext::legacy())
            .unwrap()
        {
            PreparationOutcome::Prepared(prepared) => prepared.commit_id().clone(),
            other => panic!("expected a prepared mutation, got {other:?}"),
        }
    };

    let reopened = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let attached = reopened
        .attach_prepared(&claim, &OperationContext::legacy())
        .unwrap()
        .expect("durable prepared claim should reattach");
    assert_eq!(attached.commit_id(), &commit_id);
    assert!(!directory.path().join("restart.md").exists());
}

#[test]
fn runtime_cancel_and_commit_time_conflict_are_durable_final_states() {
    let directory = collection();
    fs::write(
        directory.path().join("task.md"),
        "---\ntitle: Original\n---\nBody\n",
    )
    .unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();

    let cancelled_claim = HostClaimId::generate();
    let cancelled_request = OperationRequest::new(
        OperationKind::Create,
        json!({"path": "cancelled.md", "frontmatter": {"title": "Cancelled"}}),
    );
    let cancelled = match runtime
        .prepare(
            &cancelled_request,
            &cancelled_claim,
            &OperationContext::legacy(),
        )
        .unwrap()
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        other => panic!("expected a prepared mutation, got {other:?}"),
    };
    assert_eq!(
        runtime
            .cancel(&cancelled, &OperationContext::legacy())
            .unwrap(),
        CancelOutcome::CancelledBeforeCommit
    );
    assert!(!directory.path().join("cancelled.md").exists());
    assert!(matches!(
        runtime
            .resolve_commit(cancelled.commit_id(), &OperationContext::legacy())
            .unwrap(),
        Some(DurableCommitState::CancelledBeforeCommit)
    ));

    let conflict_claim = HostClaimId::generate();
    let conflict_request = OperationRequest::new(
        OperationKind::Update,
        json!({
            "path": "task.md",
            "frontmatter": {"title": "Prepared"},
            "body": "Body\n"
        }),
    );
    let conflict = match runtime
        .prepare(
            &conflict_request,
            &conflict_claim,
            &OperationContext::legacy(),
        )
        .unwrap()
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        other => panic!("expected a prepared mutation, got {other:?}"),
    };
    fs::write(
        directory.path().join("task.md"),
        "---\ntitle: External\n---\nBody\n",
    )
    .unwrap();
    let rejection = match runtime
        .commit(&conflict, &OperationContext::legacy())
        .unwrap()
    {
        CommitAttempt::RejectedBeforeCommit { rejection } => rejection,
        other => panic!("expected a durable rejection, got {other:?}"),
    };
    assert!(!rejection.operation.valid);
    assert_eq!(
        rejection.operation.to_v03().diagnostics[0].code,
        "concurrent_modification"
    );
    assert!(matches!(
        runtime
            .resolve_claim(&conflict_claim, &OperationContext::legacy())
            .unwrap(),
        Some((_, DurableCommitState::RejectedBeforeCommit { .. }))
    ));
}

#[test]
fn runtime_rejects_reusing_a_claim_for_different_canonical_input() {
    let directory = collection();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let claim = HostClaimId::generate();
    let first = OperationRequest::new(
        OperationKind::Create,
        json!({"path": "one.md", "frontmatter": {"title": "One"}}),
    );
    assert!(matches!(
        runtime
            .prepare(&first, &claim, &OperationContext::legacy())
            .unwrap(),
        PreparationOutcome::Prepared(_)
    ));
    let different = OperationRequest::new(
        OperationKind::Create,
        json!({"path": "two.md", "frontmatter": {"title": "Two"}}),
    );
    assert!(matches!(
        runtime.prepare(&different, &claim, &OperationContext::legacy()),
        Err(ProviderError::ClaimMismatch)
    ));
    assert!(!directory.path().join("one.md").exists());
    assert!(!directory.path().join("two.md").exists());
}

#[test]
fn runtime_change_feed_is_paged_acknowledged_and_fenced() {
    let directory = collection();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let owner = ChangeFeedOwnerId::generate();
    let feed = runtime
        .open_change_feed(&owner, &OperationContext::legacy())
        .unwrap();
    let baseline = runtime
        .establish_change_feed_baseline(&feed, &OperationContext::legacy())
        .unwrap();
    assert_eq!(baseline.acknowledged_through.get(), 0);

    let claim = HostClaimId::generate();
    let request = OperationRequest::new(
        OperationKind::Create,
        json!({"path": "feed.md", "frontmatter": {"title": "Feed"}}),
    );
    let prepared = match runtime
        .prepare(&request, &claim, &OperationContext::legacy())
        .unwrap()
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        other => panic!("expected prepared mutation, got {other:?}"),
    };
    let committed = match runtime
        .commit(&prepared, &OperationContext::legacy())
        .unwrap()
    {
        CommitAttempt::Committed(outcome) => outcome,
        other => panic!("expected committed mutation, got {other:?}"),
    };
    let page = runtime
        .read_change_events(
            &feed,
            None,
            std::num::NonZeroUsize::new(32).unwrap(),
            &OperationContext::legacy(),
        )
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].commit_id, committed.commit_id);
    assert_eq!(page.events[0].origin, ChangeOrigin::KnownMutation);
    assert_eq!(page.feed_head.get(), 1);

    runtime
        .ack_change_events(&feed, page.feed_head, &OperationContext::legacy())
        .unwrap();
    runtime
        .ack_change_events(&feed, page.feed_head, &OperationContext::legacy())
        .unwrap();
    let empty = runtime
        .read_change_events(
            &feed,
            Some(page.feed_head),
            std::num::NonZeroUsize::new(1).unwrap(),
            &OperationContext::legacy(),
        )
        .unwrap();
    assert!(empty.events.is_empty());

    let replacement = runtime
        .open_change_feed(&owner, &OperationContext::legacy())
        .unwrap();
    assert!(matches!(
        runtime.establish_change_feed_baseline(&feed, &OperationContext::legacy()),
        Err(ProviderError::ChangeFeedFenced)
    ));
    assert_eq!(
        runtime
            .establish_change_feed_baseline(&replacement, &OperationContext::legacy())
            .unwrap()
            .acknowledged_through
            .get(),
        0
    );
}

#[test]
fn runtime_change_feed_replays_across_restart_and_transfer_is_idempotent() {
    let directory = collection();
    let owner = ChangeFeedOwnerId::generate();
    let (event_id, watermark, commit_id) = {
        let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
        let feed = runtime
            .open_change_feed(&owner, &OperationContext::legacy())
            .unwrap();
        runtime
            .establish_change_feed_baseline(&feed, &OperationContext::legacy())
            .unwrap();
        let claim = HostClaimId::generate();
        let request = OperationRequest::new(
            OperationKind::Create,
            json!({"path": "restart-feed.md", "frontmatter": {"title": "Replay"}}),
        );
        let prepared = match runtime
            .prepare(&request, &claim, &OperationContext::legacy())
            .unwrap()
        {
            PreparationOutcome::Prepared(prepared) => prepared,
            other => panic!("expected prepared mutation, got {other:?}"),
        };
        let outcome = match runtime
            .commit(&prepared, &OperationContext::legacy())
            .unwrap()
        {
            CommitAttempt::Committed(outcome) => outcome,
            other => panic!("expected committed mutation, got {other:?}"),
        };
        let identity = outcome.change_event.unwrap();
        (identity.id, identity.watermark, outcome.commit_id.unwrap())
    };

    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let feed = runtime
        .open_change_feed(&owner, &OperationContext::legacy())
        .unwrap();
    let replay = runtime
        .read_change_events(
            &feed,
            None,
            std::num::NonZeroUsize::new(8).unwrap(),
            &OperationContext::legacy(),
        )
        .unwrap();
    assert_eq!(replay.events.len(), 1);
    assert_eq!(replay.events[0].identity.id, event_id);
    assert_eq!(replay.events[0].identity.watermark, watermark);
    assert_eq!(replay.events[0].commit_id.as_ref(), Some(&commit_id));

    let next_owner = ChangeFeedOwnerId::generate();
    let intent = ChangeFeedTransferIntent::new(owner, next_owner, ChangeWatermark::from_stored(0));
    let transferred = runtime
        .transfer_change_feed(&intent, &OperationContext::legacy())
        .unwrap();
    let replayed_transfer = runtime
        .transfer_change_feed(&intent, &OperationContext::legacy())
        .unwrap();
    assert_eq!(transferred.receipt, replayed_transfer.receipt);
    assert!(matches!(
        runtime.read_change_events(
            &feed,
            None,
            std::num::NonZeroUsize::new(1).unwrap(),
            &OperationContext::legacy(),
        ),
        Err(ProviderError::ChangeFeedFenced)
    ));
    runtime
        .ack_change_feed_transfer(&intent.id, &OperationContext::legacy())
        .unwrap();
    runtime
        .ack_change_feed_transfer(&intent.id, &OperationContext::legacy())
        .unwrap();
}

#[test]
fn collection_fork_resets_host_claims_and_feed_without_touching_markdown() {
    let directory = collection();
    let committed_claim = HostClaimId::generate();
    let prepared_claim = HostClaimId::generate();
    {
        let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
        let owner = ChangeFeedOwnerId::generate();
        let feed = runtime
            .open_change_feed(&owner, &OperationContext::legacy())
            .unwrap();
        runtime
            .establish_change_feed_baseline(&feed, &OperationContext::legacy())
            .unwrap();

        let committed = OperationRequest::new(
            OperationKind::Create,
            json!({"path": "committed.md", "frontmatter": {"title": "Committed"}}),
        );
        let prepared = match runtime
            .prepare(&committed, &committed_claim, &OperationContext::legacy())
            .unwrap()
        {
            PreparationOutcome::Prepared(prepared) => prepared,
            other => panic!("expected prepared mutation, got {other:?}"),
        };
        assert!(matches!(
            runtime
                .commit(&prepared, &OperationContext::legacy())
                .unwrap(),
            CommitAttempt::Committed(_)
        ));

        let uncommitted = OperationRequest::new(
            OperationKind::Create,
            json!({"path": "prepared.md", "frontmatter": {"title": "Prepared"}}),
        );
        assert!(matches!(
            runtime
                .prepare(&uncommitted, &prepared_claim, &OperationContext::legacy())
                .unwrap(),
            PreparationOutcome::Prepared(_)
        ));
    }

    let provider = FilesystemProvider::open(directory.path()).unwrap();
    provider
        .reset_runtime_support_for_fork(&OperationContext::legacy())
        .unwrap();
    drop(provider);

    assert!(directory.path().join("committed.md").is_file());
    assert!(!directory.path().join("prepared.md").exists());
    let fork = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    assert!(fork
        .resolve_claim(&committed_claim, &OperationContext::legacy())
        .unwrap()
        .is_none());
    assert!(fork
        .resolve_claim(&prepared_claim, &OperationContext::legacy())
        .unwrap()
        .is_none());
    let feed = fork
        .open_change_feed(&ChangeFeedOwnerId::generate(), &OperationContext::legacy())
        .unwrap();
    let baseline = fork
        .establish_change_feed_baseline(&feed, &OperationContext::legacy())
        .unwrap();
    assert_eq!(baseline.acknowledged_through.get(), 0);
    let page = fork
        .read_change_events(
            &feed,
            None,
            std::num::NonZeroUsize::new(8).unwrap(),
            &OperationContext::legacy(),
        )
        .unwrap();
    assert!(page.events.is_empty());
    let read = fork
        .read(
            &OperationRequest::new(OperationKind::Read, json!({"path": "committed.md"})),
            &OperationContext::legacy(),
        )
        .unwrap();
    assert_eq!(
        read.operation.to_v03().result["frontmatter"]["title"],
        "Committed"
    );
}

#[test]
fn runtime_normalizes_external_changes_and_deduplicates_known_writes() {
    let directory = collection();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let owner = ChangeFeedOwnerId::generate();
    let feed = runtime
        .open_change_feed(&owner, &OperationContext::legacy())
        .unwrap();
    runtime
        .establish_change_feed_baseline(&feed, &OperationContext::legacy())
        .unwrap();

    fs::write(
        directory.path().join("external.md"),
        "---\ntitle: External\nnested:\n  status: open\n---\nBody\n",
    )
    .unwrap();
    runtime.synchronize().unwrap();
    let event = runtime
        .ingest_external_timeout(Duration::from_secs(1), &OperationContext::legacy())
        .unwrap()
        .expect("external write should become a durable event");
    assert_eq!(event.origin, ChangeOrigin::Filesystem);
    assert_eq!(event.identity.watermark.get(), 1);
    let ChangeSet::Exact(changes) = &event.changes else {
        panic!("external record create should be exact")
    };
    let CanonicalChange::Record(change) = &changes.items()[0] else {
        panic!("external record create should be a record change")
    };
    assert_eq!(change.kind, RecordChangeKind::Created);
    assert!(change.body_changed);
    let fields = change.changed_fields.iter().collect::<Vec<_>>();
    assert!(fields.contains(&"/nested/status"), "fields were {fields:?}");

    let claim = HostClaimId::generate();
    let request = OperationRequest::new(
        OperationKind::Create,
        json!({"path": "known.md", "frontmatter": {"title": "Known"}}),
    );
    let prepared = match runtime
        .prepare(&request, &claim, &OperationContext::legacy())
        .unwrap()
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        other => panic!("expected prepared mutation, got {other:?}"),
    };
    assert!(matches!(
        runtime
            .commit(&prepared, &OperationContext::legacy())
            .unwrap(),
        CommitAttempt::Committed(_)
    ));
    assert!(runtime
        .ingest_external_timeout(Duration::from_millis(50), &OperationContext::legacy(),)
        .unwrap()
        .is_none());

    let page = runtime
        .read_change_events(
            &feed,
            None,
            std::num::NonZeroUsize::new(8).unwrap(),
            &OperationContext::legacy(),
        )
        .unwrap();
    assert_eq!(page.events.len(), 2);
    assert_eq!(page.events[0].origin, ChangeOrigin::Filesystem);
    assert_eq!(page.events[1].origin, ChangeOrigin::KnownMutation);
}

#[test]
fn runtime_external_feed_reopens_with_stable_unacknowledged_replay() {
    let directory = collection();
    let owner = ChangeFeedOwnerId::generate();
    let expected = {
        let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(20)).unwrap();
        let feed = runtime
            .open_change_feed(&owner, &OperationContext::legacy())
            .unwrap();
        runtime
            .establish_change_feed_baseline(&feed, &OperationContext::legacy())
            .unwrap();

        fs::write(
            directory.path().join("external.md"),
            "---\ntitle: Created\n---\nBody\n",
        )
        .unwrap();
        let created = runtime
            .ingest_external_timeout(Duration::from_secs(3), &OperationContext::legacy())
            .unwrap()
            .expect("external create");
        fs::write(
            directory.path().join("external.md"),
            "---\ntitle: Modified\n---\nChanged body\n",
        )
        .unwrap();
        let modified = runtime
            .ingest_external_timeout(Duration::from_secs(3), &OperationContext::legacy())
            .unwrap()
            .expect("external modify");

        for (event, kind) in [
            (&created, RecordChangeKind::Created),
            (&modified, RecordChangeKind::Updated),
        ] {
            assert_eq!(event.origin, ChangeOrigin::Filesystem);
            let ChangeSet::Exact(changes) = &event.changes else {
                panic!("external record operation must remain exact")
            };
            let [CanonicalChange::Record(change)] = changes.items() else {
                panic!("external operation must contain one record change")
            };
            assert_eq!(change.kind, kind);
            assert_eq!(change.path.as_str(), "external.md");
        }
        assert_eq!(created.identity.watermark.get(), 1);
        assert_eq!(modified.identity.watermark.get(), 2);
        vec![created, modified]
    };

    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(20)).unwrap();
    let feed = runtime
        .open_change_feed(&owner, &OperationContext::legacy())
        .unwrap();
    let replay = runtime
        .read_change_events(
            &feed,
            None,
            std::num::NonZeroUsize::new(8).unwrap(),
            &OperationContext::legacy(),
        )
        .unwrap();
    assert_eq!(replay.feed_head.get(), 2);
    assert_eq!(replay.events, expected);

    let stable = runtime
        .read_change_events(
            &feed,
            None,
            std::num::NonZeroUsize::new(8).unwrap(),
            &OperationContext::legacy(),
        )
        .unwrap();
    assert_eq!(stable.events, replay.events);
    runtime
        .ack_change_events(&feed, replay.feed_head, &OperationContext::legacy())
        .unwrap();
    let empty = runtime
        .read_change_events(
            &feed,
            Some(replay.feed_head),
            std::num::NonZeroUsize::new(8).unwrap(),
            &OperationContext::legacy(),
        )
        .unwrap();
    assert!(empty.events.is_empty());
    assert!(runtime
        .ingest_external_timeout(Duration::from_millis(100), &OperationContext::legacy())
        .unwrap()
        .is_none());
}

#[cfg(unix)]
#[test]
fn runtime_external_recursive_changes_and_symlink_poison_replay_exactly_once() {
    use std::os::unix::fs::symlink;

    fn ingest_exact(runtime: &FilesystemRuntime, count: usize) -> Vec<super::RuntimeChangeEvent> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut events = Vec::new();
        while events.len() < count && std::time::Instant::now() < deadline {
            if let Some(event) = runtime
                .ingest_external_timeout(Duration::from_millis(500), &OperationContext::legacy())
                .unwrap()
            {
                assert_eq!(event.origin, ChangeOrigin::Filesystem);
                assert!(matches!(event.changes, ChangeSet::Exact(_)));
                events.push(event);
            }
        }
        assert_eq!(events.len(), count);
        events
    }

    fn record_effects(
        events: &[super::RuntimeChangeEvent],
    ) -> BTreeSet<(RecordChangeKind, String, Option<String>)> {
        events
            .iter()
            .map(|event| {
                let ChangeSet::Exact(changes) = &event.changes else {
                    unreachable!()
                };
                let [CanonicalChange::Record(change)] = changes.items() else {
                    panic!("filesystem observation must contain one exact record change")
                };
                (
                    change.kind,
                    change.path.as_str().to_string(),
                    change.from.as_ref().map(|path| path.as_str().to_string()),
                )
            })
            .collect()
    }

    let directory = collection();
    let outside = tempfile::tempdir().unwrap();
    let marker = "EXTERNAL_RUNTIME_MARKER_MUST_NOT_APPEAR";
    fs::write(
        outside.path().join("poison.md"),
        format!("---\ntitle: Poison\nmarker: {marker}\n---\n"),
    )
    .unwrap();
    fs::write(
        directory.path().join("tracked.md"),
        "---\ntitle: Tracked\n---\n",
    )
    .unwrap();

    let owner = ChangeFeedOwnerId::generate();
    let expected = {
        let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(25)).unwrap();
        let feed = runtime
            .open_change_feed(&owner, &OperationContext::legacy())
            .unwrap();
        runtime
            .establish_change_feed_baseline(&feed, &OperationContext::legacy())
            .unwrap();
        let mut events = Vec::new();

        fs::create_dir_all(directory.path().join("before/nested")).unwrap();
        fs::write(
            directory.path().join("before/nested/immediate.md"),
            "---\ntitle: Immediate\n---\n",
        )
        .unwrap();
        events.extend(ingest_exact(&runtime, 1));
        assert_eq!(
            record_effects(&events),
            BTreeSet::from([(
                RecordChangeKind::Created,
                "before/nested/immediate.md".to_string(),
                None
            )])
        );

        fs::write(
            directory.path().join("before/second.md"),
            "---\ntitle: Second\n---\n",
        )
        .unwrap();
        events.extend(ingest_exact(&runtime, 1));
        fs::rename(
            directory.path().join("before"),
            directory.path().join("after"),
        )
        .unwrap();
        let renamed = ingest_exact(&runtime, 2);
        assert_eq!(
            record_effects(&renamed),
            BTreeSet::from([
                (
                    RecordChangeKind::Renamed,
                    "after/nested/immediate.md".to_string(),
                    Some("before/nested/immediate.md".to_string())
                ),
                (
                    RecordChangeKind::Renamed,
                    "after/second.md".to_string(),
                    Some("before/second.md".to_string())
                ),
            ])
        );
        events.extend(renamed);

        fs::remove_dir_all(directory.path().join("after")).unwrap();
        let removed = ingest_exact(&runtime, 2);
        assert_eq!(
            record_effects(&removed),
            BTreeSet::from([
                (
                    RecordChangeKind::Deleted,
                    "after/nested/immediate.md".to_string(),
                    None
                ),
                (
                    RecordChangeKind::Deleted,
                    "after/second.md".to_string(),
                    None
                ),
            ])
        );
        events.extend(removed);

        fs::remove_file(directory.path().join("tracked.md")).unwrap();
        symlink(
            outside.path().join("poison.md"),
            directory.path().join("tracked.md"),
        )
        .unwrap();
        fs::write(directory.path().join("safe.md"), "---\ntitle: Safe\n---\n").unwrap();
        let poisoned = ingest_exact(&runtime, 2);
        assert_eq!(
            record_effects(&poisoned),
            BTreeSet::from([
                (RecordChangeKind::Deleted, "tracked.md".to_string(), None),
                (RecordChangeKind::Created, "safe.md".to_string(), None),
            ])
        );
        assert!(poisoned
            .iter()
            .all(|event| !format!("{event:?}").contains(marker)));
        events.extend(poisoned);

        for (index, event) in events.iter().enumerate() {
            assert_eq!(event.identity.watermark.get(), (index + 1) as u64);
        }
        assert!(runtime
            .ingest_external_timeout(Duration::from_millis(200), &OperationContext::legacy())
            .unwrap()
            .is_none());
        events
    };

    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(25)).unwrap();
    let feed = runtime
        .open_change_feed(&owner, &OperationContext::legacy())
        .unwrap();
    let replay = runtime
        .read_change_events(
            &feed,
            None,
            std::num::NonZeroUsize::new(32).unwrap(),
            &OperationContext::legacy(),
        )
        .unwrap();
    assert_eq!(replay.events, expected);
    assert_eq!(replay.events.len(), 8);
    runtime
        .ack_change_events(&feed, replay.feed_head, &OperationContext::legacy())
        .unwrap();
    let empty = runtime
        .read_change_events(
            &feed,
            Some(replay.feed_head),
            std::num::NonZeroUsize::new(32).unwrap(),
            &OperationContext::legacy(),
        )
        .unwrap();
    assert!(empty.events.is_empty());
    assert!(runtime
        .ingest_external_timeout(Duration::from_millis(200), &OperationContext::legacy())
        .unwrap()
        .is_none());
}

#[test]
fn runtime_read_cursor_pins_generation_replays_and_expires_on_release() {
    let directory = collection();
    for name in ["a", "b", "c"] {
        fs::write(
            directory.path().join(format!("{name}.md")),
            format!("---\ntitle: {name}\n---\n"),
        )
        .unwrap();
    }
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let query = OperationRequest::new(
        OperationKind::Query,
        json!({"limit": 1, "order_by": [{"field": "file.path", "direction": "asc"}]}),
    );
    let first = runtime
        .open_read(&query, &OperationContext::legacy())
        .unwrap();
    assert_eq!(
        first.outcome.operation.to_v03().result["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let cursor = first.next.expect("three records require another page");
    let retained = runtime.measurements().unwrap();
    assert_eq!(retained.active_read_snapshots, 1);
    assert!(retained.retained_read_snapshot_bytes > 0);
    let encoded = cursor.as_token().to_string();
    let decoded = ReadCursor::from_token(encoded.clone()).unwrap();
    assert_eq!(decoded, cursor);
    let mut tampered = encoded.clone().into_bytes();
    let last = tampered.last_mut().unwrap();
    *last = if *last == b'a' { b'b' } else { b'a' };
    let tampered = ReadCursor::from_token(String::from_utf8(tampered).unwrap()).unwrap();
    assert!(matches!(
        runtime.read_page(&tampered, &OperationContext::legacy()),
        Err(ProviderError::InvalidReadCursor)
    ));
    let pinned_generation = first.outcome.generation;

    let claim = HostClaimId::generate();
    let request = OperationRequest::new(
        OperationKind::Create,
        json!({"path": "d.md", "frontmatter": {"title": "d"}}),
    );
    let prepared = match runtime
        .prepare(&request, &claim, &OperationContext::legacy())
        .unwrap()
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        other => panic!("expected prepared mutation, got {other:?}"),
    };
    let committed = match runtime
        .commit(&prepared, &OperationContext::legacy())
        .unwrap()
    {
        CommitAttempt::Committed(outcome) => outcome,
        other => panic!("expected committed mutation, got {other:?}"),
    };
    assert_ne!(committed.generation, pinned_generation);

    let second = runtime
        .read_page(&cursor, &OperationContext::legacy())
        .unwrap();
    let replay = runtime
        .read_page(&cursor, &OperationContext::legacy())
        .unwrap();
    assert_eq!(second, replay);
    assert_eq!(second.outcome.generation, pinned_generation);
    assert_eq!(
        second.outcome.operation.to_v03().result["meta"]["total_count"],
        serde_json::json!(3)
    );
    let next = second.next.expect("the pinned third record remains");
    runtime
        .release_read(next.clone(), &OperationContext::legacy())
        .unwrap();
    let released = runtime.measurements().unwrap();
    assert_eq!(released.active_read_snapshots, 0);
    assert_eq!(released.retained_read_snapshot_bytes, 0);
    assert!(matches!(
        runtime.read_page(&next, &OperationContext::legacy()),
        Err(ProviderError::GenerationExpired)
    ));
    drop(runtime);
    let reopened = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    assert!(matches!(
        reopened.read_page(&decoded, &OperationContext::legacy()),
        Err(ProviderError::GenerationExpired)
    ));
}

#[test]
fn runtime_deadline_after_commit_boundary_returns_pending_while_settlement_finishes() {
    struct ResetDelay(CommitId);
    impl Drop for ResetDelay {
        fn drop(&mut self) {
            crate::transactions::set_runtime_settlement_delay(&self.0, Duration::ZERO);
        }
    }

    let directory = collection();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let claim = HostClaimId::generate();
    let request = OperationRequest::new(
        OperationKind::Create,
        json!({"path": "pending.md", "frontmatter": {"title": "Pending"}}),
    );
    let prepared = match runtime
        .prepare(&request, &claim, &OperationContext::legacy())
        .unwrap()
    {
        PreparationOutcome::Prepared(prepared) => prepared,
        other => panic!("expected prepared mutation, got {other:?}"),
    };
    crate::transactions::set_runtime_settlement_delay(prepared.commit_id(), Duration::from_secs(2));
    let _reset = ResetDelay(prepared.commit_id().clone());
    let cancellation = crate::OperationCancellation::new();
    let context = OperationContext::new(
        &cancellation,
        OperationDeadline::after(Duration::from_millis(500)),
    );
    let started = Instant::now();
    assert_eq!(
        runtime.commit(&prepared, &context).unwrap(),
        CommitAttempt::SettlementPending {
            commit_id: prepared.commit_id().clone()
        }
    );
    assert!(started.elapsed() < Duration::from_millis(1_500));

    let resolution_deadline = Instant::now() + Duration::from_secs(5);
    let resolved = loop {
        let resolved = runtime
            .resolve_commit(prepared.commit_id(), &OperationContext::legacy())
            .unwrap();
        if matches!(resolved, Some(DurableCommitState::Committed { .. })) {
            break resolved;
        }
        assert!(
            Instant::now() < resolution_deadline,
            "durable settlement did not finish: {resolved:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(matches!(
        resolved,
        Some(DurableCommitState::Committed { .. })
    ));
    assert!(directory.path().join("pending.md").exists());
}

#[test]
fn runtime_recovers_every_durable_commit_crash_boundary() {
    for point in 1..=4 {
        let directory = collection();
        let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
        let request = OperationRequest::new(
            OperationKind::Create,
            json!({
                "path": format!("recovered-{point}.md"),
                "frontmatter": {"id": format!("recovered-{point}")},
                "body": "durable body"
            }),
        );
        let prepared = match runtime
            .prepare(
                &request,
                &HostClaimId::generate(),
                &OperationContext::legacy(),
            )
            .unwrap()
        {
            PreparationOutcome::Prepared(prepared) => prepared,
            other => panic!("expected prepared mutation at point {point}, got {other:?}"),
        };
        crate::transactions::set_runtime_crash_point(prepared.commit_id(), point);
        let crashed = runtime.commit(&prepared, &OperationContext::legacy());
        assert!(
            crashed.is_err(),
            "crash point {point} unexpectedly returned {crashed:?}"
        );
        drop(runtime);

        let reopened = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
        let resolution = reopened
            .resolve_commit(prepared.commit_id(), &OperationContext::legacy())
            .unwrap()
            .expect("durable commit must remain resolvable");
        assert!(
            matches!(resolution, DurableCommitState::Committed { .. }),
            "crash point {point} recovered to {resolution:?}"
        );
        let read = reopened
            .read(
                &OperationRequest::new(
                    OperationKind::Read,
                    json!({"path": format!("recovered-{point}.md")}),
                ),
                &OperationContext::legacy(),
            )
            .unwrap();
        assert!(read.operation.valid, "crash point {point}: {read:?}");
        assert_eq!(read.operation.to_v03().result["body"], "durable body\n");
    }
}

#[test]
fn provider_serializes_conditional_writers() {
    let directory = collection();
    fs::write(
        directory.path().join("task.md"),
        "---\ntitle: Original\n---\n",
    )
    .unwrap();
    let provider = Arc::new(FilesystemProvider::open(directory.path()).unwrap());
    let read = provider
        .execute(&OperationRequest::new(
            OperationKind::Read,
            json!({"path": "task.md"}),
        ))
        .unwrap();
    let revision = read.result["revision"].as_str().unwrap().to_string();
    let barrier = Arc::new(Barrier::new(3));

    let handles = ["First", "Second"].map(|title| {
        let provider = provider.clone();
        let barrier = barrier.clone();
        let revision = revision.clone();
        thread::spawn(move || {
            barrier.wait();
            provider
                .execute(&OperationRequest::new(
                    OperationKind::Update,
                    json!({
                        "path": "task.md",
                        "fields": {"title": title},
                        "if_revision": revision,
                    }),
                ))
                .unwrap()
        })
    });
    barrier.wait();
    let results = handles.map(|handle| handle.join().unwrap());

    assert_eq!(results.iter().filter(|result| result.valid).count(), 1);
    assert_eq!(results.iter().filter(|result| !result.valid).count(), 1);
    assert!(results.iter().any(|result| {
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "concurrent_modification")
    }));
}

#[test]
fn provider_allows_read_only_compound_operations_to_overlap() {
    let directory = collection();
    let provider = Arc::new(FilesystemProvider::open(directory.path()).unwrap());
    let (entered, entered_rx) = mpsc::channel();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let handles = (0..2)
        .map(|_| {
            let provider = provider.clone();
            let entered = entered.clone();
            let release = release.clone();
            thread::spawn(move || {
                provider
                    .with_collection_read(|_| {
                        entered.send(()).unwrap();
                        let (released, ready) = &*release;
                        let guard = released.lock().unwrap();
                        drop(ready.wait_while(guard, |released| !*released).unwrap());
                        Ok::<_, ProviderError>(())
                    })
                    .unwrap();
            })
        })
        .collect::<Vec<_>>();
    drop(entered);

    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let overlapped = entered_rx.recv_timeout(Duration::from_millis(250)).is_ok();
    let (released, ready) = &*release;
    *released.lock().unwrap() = true;
    ready.notify_all();
    for handle in handles {
        handle.join().unwrap();
    }
    assert!(overlapped, "read-only operations were serialized");
}

#[test]
fn provider_deadline_cancels_work_waiting_for_the_runtime_gate() {
    let directory = collection();
    let provider = Arc::new(FilesystemProvider::open(directory.path()).unwrap());
    let (entered, entered_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let holder = {
        let provider = provider.clone();
        thread::spawn(move || {
            let cancellation = crate::OperationCancellation::new();
            let context = OperationContext::new(
                &cancellation,
                OperationDeadline::after(Duration::from_secs(5)),
            );
            provider
                .with_collection_context(&context, |_| {
                    entered.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok::<_, ProviderError>(())
                })
                .unwrap();
        })
    };
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    let cancellation = crate::OperationCancellation::new();
    let context = OperationContext::new(
        &cancellation,
        OperationDeadline::after(Duration::from_millis(25)),
    );
    let started = Instant::now();
    let result = provider.execute_with_context(
        &OperationRequest::new(OperationKind::Query, json!({"from": "records"})),
        &context,
    );

    assert!(matches!(result, Err(ProviderError::OperationDeadline)));
    assert!(started.elapsed() < Duration::from_millis(250));
    release.send(()).unwrap();
    holder.join().unwrap();
}

#[test]
fn provider_cancellation_releases_a_waiting_writer_without_running_it() {
    let directory = collection();
    let provider = Arc::new(FilesystemProvider::open(directory.path()).unwrap());
    let (entered, entered_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let holder = {
        let provider = provider.clone();
        thread::spawn(move || {
            let cancellation = crate::OperationCancellation::new();
            let context = OperationContext::new(
                &cancellation,
                OperationDeadline::after(Duration::from_secs(5)),
            );
            provider
                .with_collection_context(&context, |_| {
                    entered.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok::<_, ProviderError>(())
                })
                .unwrap();
        })
    };
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    let cancellation = crate::OperationCancellation::new();
    let context = OperationContext::new(
        &cancellation,
        OperationDeadline::after(Duration::from_secs(1)),
    );
    let waiting_provider = provider.clone();
    let waiting = thread::spawn(move || {
        waiting_provider.execute_with_context(
            &OperationRequest::new(
                OperationKind::Create,
                json!({"path": "must-not-exist.md", "frontmatter": {}}),
            ),
            &context,
        )
    });
    thread::sleep(Duration::from_millis(30));
    cancellation.cancel();

    assert!(matches!(
        waiting.join().unwrap(),
        Err(ProviderError::OperationCancelled)
    ));
    assert!(!directory.path().join("must-not-exist.md").exists());
    release.send(()).unwrap();
    holder.join().unwrap();
}

#[test]
fn runtime_durably_feeds_known_change_without_reemitting_it_through_watcher() {
    let directory = collection();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(40)).unwrap();
    let owner = ChangeFeedOwnerId::generate();
    let feed = runtime
        .open_change_feed(&owner, &OperationContext::legacy())
        .unwrap();
    runtime
        .establish_change_feed_baseline(&feed, &OperationContext::legacy())
        .unwrap();
    let created = runtime
        .execute(&OperationRequest::new(
            OperationKind::Create,
            json!({
                "path": "created.md",
                "frontmatter": {"title": "Created"},
            }),
        ))
        .unwrap();
    assert!(created.valid, "{created:#?}");

    assert!(runtime.recv_timeout(Duration::ZERO).unwrap().is_none());
    let page = runtime
        .read_change_events(
            &feed,
            None,
            std::num::NonZeroUsize::new(1).unwrap(),
            &OperationContext::legacy(),
        )
        .unwrap()
        .events
        .into_iter()
        .next()
        .expect("successful mutation must be durable before returning");
    assert_eq!(page.origin, ChangeOrigin::KnownMutation);
    let ChangeSet::Exact(changes) = page.changes else {
        panic!("known mutation must retain exact changes")
    };
    let CanonicalChange::Record(change) = &changes.items()[0] else {
        panic!("create must emit a record change")
    };
    assert_eq!(change.path.as_str(), "created.md");
}

#[test]
fn coordinated_provider_rejects_raw_mutations() {
    let directory = collection();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let error = runtime
        .provider()
        .execute(&OperationRequest::new(
            OperationKind::Create,
            json!({"path": "raw.md", "frontmatter": {"id": "raw"}}),
        ))
        .expect_err("coordinated providers must not expose a second writer");
    assert!(matches!(error, ProviderError::UnsupportedOperation(_)));
    assert!(!directory.path().join("raw.md").exists());
}

#[test]
fn runtime_commits_saved_view_resources_through_the_single_writer() {
    let directory = collection();
    fs::write(
        directory.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  default_validation: error\nx-obsidian:\n  bases:\n    include:\n      - views/**/*.base\n",
    )
    .unwrap();
    fs::create_dir(directory.path().join("views")).unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let documents = [
        ("views/inbox.base", "views:\n  - type: table\n    name: Inbox\n"),
        (
            "views/inbox.md",
            "---\ntype: view\nid: inbox.views\nversion: 1\nname: Inbox\nquery: {}\nviews:\n  - id: all\n    name: Inbox\n---\n",
        ),
    ];

    for (path, document) in documents {
        let created = runtime
            .execute(&OperationRequest::new(
                OperationKind::CreateViewSource,
                json!({"path": path, "document": document}),
            ))
            .unwrap();
        assert!(created.valid, "{created:?}");
        let revision = created.result["revision"].as_str().unwrap();

        let updated = runtime
            .execute(&OperationRequest::new(
                OperationKind::UpdateViewSource,
                json!({
                    "path": path,
                    "if_revision": revision,
                    "document": document.replace("Inbox", "Focused")
                }),
            ))
            .unwrap();
        assert!(updated.valid, "{updated:?}");

        let deleted = runtime
            .execute(&OperationRequest::new(
                OperationKind::DeleteViewSource,
                json!({
                    "path": path,
                    "if_revision": updated.result["revision"]
                }),
            ))
            .unwrap();
        assert!(deleted.valid, "{deleted:?}");
        assert!(!directory.path().join(path).exists());
    }
}

#[test]
fn public_runtime_prepare_rejects_erased_authority_without_staging_or_generation_change() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  explicit_type_keys: [kind]\n",
    )
    .unwrap();
    fs::create_dir(directory.path().join("_types")).unwrap();
    fs::write(
        directory.path().join("_types/note.md"),
        "---\nkind: mdbase.type\nname: note\nmatch: { fields_present: [title] }\nschema:\n  dialect: json-schema-2020-12\n  value: {type: object}\nlifecycle:\n  on_create:\n    set: { kind: { literal: null } }\n---\n",
    )
    .unwrap();
    let baseline = FilesystemProvider::open(directory.path())
        .unwrap()
        .snapshot()
        .unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let claim = HostClaimId::generate();
    let prepared = runtime
        .prepare(
            &OperationRequest::new(
                OperationKind::Create,
                json!({"path":"failed.md","type":"note","frontmatter":{"title":"same"}}),
            ),
            &claim,
            &OperationContext::legacy(),
        )
        .unwrap();
    let PreparationOutcome::NoMutation(outcome) = prepared else {
        panic!("authority-erasing public runtime request must not stage a mutation")
    };
    assert!(!outcome.operation.valid);
    assert_eq!(
        outcome.operation.to_v03().diagnostics[0].code,
        "type_membership_authority_changed"
    );
    assert_eq!(
        FilesystemProvider::open(directory.path())
            .unwrap()
            .snapshot()
            .unwrap()
            .revision,
        baseline.revision
    );
    assert!(matches!(outcome.changes, ChangeSet::None));
    assert!(outcome.commit_id.is_none());
    assert!(runtime
        .attach_prepared(&claim, &OperationContext::legacy())
        .unwrap()
        .is_none());
    assert!(!directory.path().join("failed.md").exists());
}

#[test]
fn runtime_uniqueness_uses_the_generation_bound_index() {
    let directory = collection();
    fs::write(
        directory.path().join("existing.md"),
        "---\nid: duplicate\ntitle: Existing\n---\n",
    )
    .unwrap();
    for index in 0..64 {
        fs::write(
            directory.path().join(format!("unrelated-{index}.md")),
            format!("---\nid: unrelated-{index}\n---\n"),
        )
        .unwrap();
    }
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let outcome = runtime
        .prepare(
            &OperationRequest::new(
                OperationKind::Create,
                json!({
                    "path": "candidate.md",
                    "frontmatter": {"id": "duplicate", "title": "Candidate"}
                }),
            ),
            &HostClaimId::generate(),
            &OperationContext::legacy(),
        )
        .unwrap();
    let PreparationOutcome::NoMutation(outcome) = outcome else {
        panic!("duplicate identity must be rejected before durable prepare")
    };
    assert!(!outcome.operation.valid);
    assert!(outcome
        .operation
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code.as_str() == "duplicate_id"));
    assert!(!directory.path().join("candidate.md").exists());
}

#[test]
fn runtime_query_omits_unneeded_bodies_from_resident_records() {
    let directory = collection();
    fs::write(
        directory.path().join("large.md"),
        format!(
            "---\nid: large\ntitle: Large\n---\n{}",
            "x".repeat(2 * 1024 * 1024)
        ),
    )
    .unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    let snapshot = runtime
        .provider()
        .with_collection_read(|collection| {
            collection
                .load_runtime_query_data_profiled_cancellable(
                    true,
                    false,
                    false,
                    &crate::OperationCancellation::new(),
                )
                .map_err(|error| ProviderError::CollectionOpen(error.to_string()))
        })
        .unwrap()
        .0;
    assert_eq!(snapshot.records.len(), 1);
    assert!(snapshot.records[0].body.is_empty());

    let query = runtime
        .read(
            &OperationRequest::new(
                OperationKind::Query,
                json!({"where": "title == 'Large'", "include_body": false}),
            ),
            &OperationContext::legacy(),
        )
        .unwrap();
    assert!(
        query.operation.valid,
        "{:?}",
        query.operation.to_v03().diagnostics
    );
    assert!(query.operation.to_v03().result["results"][0]
        .get("body")
        .is_none());
}

#[test]
fn cache_write_notifications_do_not_exhaust_reconciliation_ownership_retries() {
    let directory = collection();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_secs(60)).unwrap();
    fs::write(directory.path().join("invalid.md"), b"bad\xffutf8\n").unwrap();

    // Each actual maintenance write injects the coarse watched-root callback
    // emitted by macOS for hidden SQLite activity. Idempotent retries must not
    // write again and manufacture an endless source of their own invalidation.
    runtime.inject_cache_notifications_for_test(3);
    runtime.synchronize().unwrap();

    assert_eq!(runtime.maintenance_attempt_counts_for_test(), (1, 2));
    assert_eq!(
        runtime_query_paths(&runtime),
        BTreeSet::from(["invalid.md".to_string()])
    );
}

#[test]
fn validated_connection_closes_only_after_ack_and_later_callback_converges() {
    let directory = collection();
    fs::write(directory.path().join("invalid.md"), b"bad\xffutf8\n").unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_secs(60)).unwrap();

    // The first cache-write callback rejects attempt 1's acknowledgement. The
    // retained seal validates attempt 2; dropping it first rolls back and closes
    // SQLite, then invokes the exact installed watcher callback after attempt 2's
    // successful ack. Native close notification behavior requires hosted macOS;
    // this hook deterministically proves the lifecycle ordering.
    runtime.inject_cache_notifications_for_test(1);
    runtime.inject_installed_callback_on_validated_seal_drop_for_test();
    let revision_before_synchronize = runtime.watcher_revision_for_test();
    runtime.synchronize().unwrap();
    assert!(!runtime.drop_callback_preceded_ack_for_test());
    assert!(!runtime.drop_callback_preceded_connection_close_for_test());
    let attempts_after_close = runtime.maintenance_attempt_counts_for_test();
    assert_eq!(attempts_after_close, (1, 0));
    let revision_after_close = runtime.watcher_revision_for_test();
    let revision_advance_through_close = revision_after_close
        .checked_sub(revision_before_synchronize)
        .expect("watcher revision advanced monotonically through close");
    assert!(revision_advance_through_close >= 4);

    // The close callback is a later eventual event. It converges with one normal
    // maintenance pass and no callback/retry rewrite loop. Native backends may
    // add valid SQLite close/root revisions, so retain ordering rather than an
    // unjustified absolute revision count.
    runtime.synchronize().unwrap();
    let attempts_after_convergence = runtime.maintenance_attempt_counts_for_test();
    assert_eq!(attempts_after_convergence.0, attempts_after_close.0 + 1);
    assert_eq!(attempts_after_convergence.1, attempts_after_close.1);
    let revision_after_convergence = runtime.watcher_revision_for_test();
    let convergence_revision_advance = revision_after_convergence
        .checked_sub(revision_after_close)
        .expect("watcher revision advanced monotonically through convergence");
    assert!(convergence_revision_advance >= 1);
    assert_eq!(
        runtime_query_paths(&runtime),
        BTreeSet::from(["invalid.md".to_string()])
    );
}

#[test]
fn validation_reuses_committing_connection_without_manufacturing_invalidation() {
    let directory = collection();
    fs::write(directory.path().join("invalid.md"), b"bad\xffutf8\n").unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_secs(60)).unwrap();

    // Attempt 1 is Applied and its deterministic cache-write callback advances
    // the watcher. The next attempt observes that callback and must validate as
    // Current without opening another SQLite connection. Native backends may
    // contribute additional root-scoped revisions around those milestones.
    // The armed open hook models the extra macOS root callback that an open
    // against WAL/SHM used to produce; it must remain armed when acknowledgement
    // succeeds.
    runtime.inject_cache_notifications_for_test(1);
    runtime.inject_cache_notification_on_validation_open_for_test();
    let revision_before_synchronize = runtime.watcher_revision_for_test();
    assert_eq!(revision_before_synchronize, 0);
    runtime.synchronize().unwrap();

    assert_eq!(runtime.maintenance_attempt_counts_for_test(), (1, 0));
    let revision_after_synchronize = runtime.watcher_revision_for_test();
    let revision_advance = revision_after_synchronize
        .checked_sub(revision_before_synchronize)
        .expect("watcher revision advanced monotonically through validation");
    // The deterministic create/write/ack path advances at least three times.
    // Native backends may additionally report root-scoped SQLite lifecycle
    // notifications; the still-armed hook below proves validation itself did
    // not open another connection and manufacture one.
    assert!(revision_advance >= 3);
    assert!(runtime.clear_validation_open_notification_for_test());
    assert_eq!(
        runtime_query_paths(&runtime),
        BTreeSet::from(["invalid.md".to_string()])
    );
}

#[test]
fn runtime_reconciliation_keeps_all_classified_invalid_query_stubs() {
    let directory = collection();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_secs(60)).unwrap();
    fs::write(
        directory.path().join("sibling.md"),
        "---\ntitle: Sibling\n---\nVisible\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("malformed.md"),
        "---\ntitle: [broken\n---\nOpaque\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("non-mapping.md"),
        "---\n- item\n---\nOpaque\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("invalid-utf8.md"),
        b"---\ntitle: \xff\n---\nOpaque\n",
    )
    .unwrap();

    runtime.synchronize().unwrap();
    while runtime
        .ingest_external_timeout(Duration::ZERO, &OperationContext::legacy())
        .unwrap()
        .is_some()
    {}

    let query = runtime
        .read(
            &OperationRequest::new(
                OperationKind::Query,
                json!({"frontmatter_mode": "persisted", "include_body": true}),
            ),
            &OperationContext::legacy(),
        )
        .unwrap();
    assert!(
        query.operation.valid,
        "{:?}",
        query.operation.to_v03().diagnostics
    );
    let query_wire = query.operation.to_v03();
    let results = query_wire.result["results"].as_array().unwrap();
    let paths = results
        .iter()
        .filter_map(|record| record["path"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        paths,
        std::collections::BTreeSet::from([
            "invalid-utf8.md",
            "malformed.md",
            "non-mapping.md",
            "sibling.md",
        ])
    );
    for path in ["invalid-utf8.md", "malformed.md", "non-mapping.md"] {
        let stub = results
            .iter()
            .find(|record| record["path"] == path)
            .unwrap();
        assert!(stub.get("frontmatter").is_none());
        assert!(stub.get("body").is_none());
        assert!(stub["file"]["revision"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
    }
    let reasons = query
        .operation
        .diagnostics
        .iter()
        .map(|diagnostic| {
            (
                diagnostic.path.as_deref().unwrap(),
                diagnostic.details.as_ref().unwrap()["reason"]
                    .as_str()
                    .unwrap(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(
        reasons,
        std::collections::BTreeMap::from([
            ("invalid-utf8.md", "invalid_utf8"),
            ("malformed.md", "invalid_yaml"),
            ("non-mapping.md", "non_mapping_frontmatter"),
        ])
    );
}

fn assert_initial_invalid_deletion_disappears_from_query(full: bool) {
    let directory = collection();
    fs::write(directory.path().join("invalid.md"), b"bad\xffutf8\n").unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_secs(60)).unwrap();
    assert_eq!(
        runtime_query_paths(&runtime),
        BTreeSet::from(["invalid.md".to_string()])
    );

    fs::remove_file(directory.path().join("invalid.md")).unwrap();
    if full {
        runtime.synchronize().unwrap();
    } else {
        runtime.synchronize_paths_for_test(&["invalid.md"]).unwrap();
    }

    assert!(runtime_query_paths(&runtime).is_empty());
    assert!(runtime
        .ingest_external_timeout(Duration::ZERO, &OperationContext::legacy())
        .unwrap()
        .is_none());
}

#[test]
fn full_reconciliation_removes_initial_invalid_cache_stub_without_public_event() {
    assert_initial_invalid_deletion_disappears_from_query(true);
}

#[test]
fn incremental_reconciliation_removes_initial_invalid_cache_stub_without_public_event() {
    assert_initial_invalid_deletion_disappears_from_query(false);
}

#[test]
fn absence_recreation_before_cache_commit_rolls_back_and_reconciles_current_invalid_row() {
    let directory = collection();
    let original = b"original\xffinvalid\n";
    let recreated = b"recreated\xffinvalid\n";
    fs::write(directory.path().join("invalid.md"), original).unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_secs(60)).unwrap();
    assert_eq!(
        runtime_query_revision(&runtime, "invalid.md"),
        crate::v03::revision(original)
    );

    fs::remove_file(directory.path().join("invalid.md")).unwrap();
    crate::cache::runtime::set_maintenance_revalidation_replacement(
        directory.path(),
        "invalid.md",
        recreated.to_vec(),
    );
    runtime.synchronize().unwrap();

    assert_eq!(
        runtime_query_revision(&runtime, "invalid.md"),
        crate::v03::revision(recreated)
    );
    assert!(runtime
        .ingest_external_timeout(Duration::ZERO, &OperationContext::legacy())
        .unwrap()
        .is_none());
}

#[test]
fn invalid_replacement_before_cache_commit_rolls_back_and_indexes_final_revision() {
    let directory = collection();
    let original = b"original\xffinvalid\n";
    let replacement = b"replacement\xffinvalid\n";
    fs::write(directory.path().join("invalid.md"), original).unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_secs(60)).unwrap();
    crate::cache::runtime::set_maintenance_revalidation_replacement(
        directory.path(),
        "invalid.md",
        replacement.to_vec(),
    );
    // Deterministically exercise Stale (replacement hook), Applied (one cache
    // write/callback), then Current (retained-connection validation).
    runtime.inject_cache_notifications_for_test(1);

    runtime.synchronize().unwrap();

    assert_eq!(runtime.maintenance_attempt_counts_for_test(), (1, 0));
    assert_eq!(
        runtime_query_revision(&runtime, "invalid.md"),
        crate::v03::revision(replacement)
    );
    assert!(runtime
        .ingest_external_timeout(Duration::ZERO, &OperationContext::legacy())
        .unwrap()
        .is_none());
}

#[test]
fn persistent_refresh_failure_honors_context_deadline_and_releases_waiter() {
    let directory = collection();
    fs::write(
        directory.path().join("tracked.md"),
        "---\ntitle: Before\n---\n",
    )
    .unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
    crate::operations::set_record_open_failure(
        directory.path(),
        "tracked.md",
        Some(std::io::ErrorKind::Interrupted),
    );
    fs::write(
        directory.path().join("tracked.md"),
        "---\ntitle: After\n---\n",
    )
    .unwrap();
    let cancellation = crate::OperationCancellation::new();
    let context = OperationContext::new(
        &cancellation,
        OperationDeadline::after(Duration::from_millis(50)),
    );
    let started = Instant::now();
    assert!(matches!(
        runtime.synchronize_with_context(&context),
        Err(ProviderError::OperationDeadline)
    ));
    assert!(started.elapsed() < Duration::from_secs(1));

    let wait_until = Instant::now() + Duration::from_secs(1);
    while runtime.pending_rescan_count_for_test() != 0 && Instant::now() < wait_until {
        thread::yield_now();
    }
    assert_eq!(runtime.pending_rescan_count_for_test(), 0);
    crate::operations::set_record_open_failure(directory.path(), "tracked.md", None);
    runtime.synchronize().unwrap();
}

#[test]
fn poison_between_final_cache_check_and_commit_rolls_back_without_success() {
    let directory = collection();
    let original = b"original\xffinvalid\n";
    let replacement = b"replacement\xffinvalid\n";
    fs::write(directory.path().join("invalid.md"), original).unwrap();
    let runtime =
        Arc::new(FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap());
    let generation = runtime.current_generation().unwrap();
    let cached_revision = runtime_query_revision(&runtime, "invalid.md");
    fs::write(directory.path().join("invalid.md"), replacement).unwrap();

    let race = runtime.install_cache_commit_linearization_hook_for_test();
    let synchronizing = runtime.clone();
    let sync = thread::spawn(move || synchronizing.synchronize());
    race.wait_until_reached();
    runtime.poison_watcher_for_test();
    race.resume();

    assert!(matches!(
        sync.join().unwrap(),
        Err(ProviderError::Watch(
            crate::watch::WatchError::RevisionExhausted
        ))
    ));
    assert_eq!(runtime.current_generation().unwrap(), generation);
    assert_eq!(
        runtime_query_revision(&runtime, "invalid.md"),
        cached_revision
    );
    assert_eq!(cached_revision, crate::v03::revision(original));
}

#[test]
fn live_callback_revision_exhaustion_aborts_sync_without_cache_or_generation_mutation() {
    let directory = collection();
    let original = b"original\xffinvalid\n";
    let replacement = b"replacement\xffinvalid\n";
    fs::write(directory.path().join("invalid.md"), original).unwrap();
    fs::write(
        directory.path().join("tracked.md"),
        "---\ntitle: Before\n---\n",
    )
    .unwrap();
    let runtime =
        Arc::new(FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap());
    let generation = runtime.current_generation().unwrap();
    let cached_revision = runtime_query_revision(&runtime, "invalid.md");
    crate::operations::set_record_open_failure(
        directory.path(),
        "tracked.md",
        Some(std::io::ErrorKind::Interrupted),
    );
    fs::write(
        directory.path().join("tracked.md"),
        "---\ntitle: After\n---\n",
    )
    .unwrap();

    let synchronizing = runtime.clone();
    let sync = thread::spawn(move || synchronizing.synchronize());
    let wait_until = Instant::now() + Duration::from_secs(2);
    while runtime.pending_rescan_count_for_test() == 0 && Instant::now() < wait_until {
        thread::yield_now();
    }
    assert_eq!(runtime.pending_rescan_count_for_test(), 1);

    runtime.set_watcher_revision_for_test(u64::MAX);
    fs::write(directory.path().join("invalid.md"), replacement).unwrap();
    // Inject through the exact installed-callback path while the synchronization
    // waiter is retained; this avoids backend scheduling nondeterminism.
    runtime.invoke_installed_watcher_modify_callback_for_test("invalid.md");
    assert!(matches!(
        sync.join().unwrap(),
        Err(ProviderError::Watch(
            crate::watch::WatchError::RevisionExhausted
        ))
    ));
    crate::operations::set_record_open_failure(directory.path(), "tracked.md", None);

    assert_eq!(runtime.pending_rescan_count_for_test(), 0);
    assert_eq!(runtime.current_generation().unwrap(), generation);
    assert_eq!(
        runtime_query_revision(&runtime, "invalid.md"),
        cached_revision
    );
    assert_eq!(cached_revision, crate::v03::revision(original));
}

#[test]
fn invalid_maintenance_rejects_ambiguous_hints_and_repairs_exact_cache_shape() {
    use crate::cache::runtime::InvalidMaintenanceOutcome;

    let directory = collection();
    fs::write(directory.path().join("invalid.md"), b"bad\xffutf8\n").unwrap();
    fs::write(
        directory.path().join("parsed.md"),
        "---\ntitle: Parsed\n---\n[[invalid]]\n",
    )
    .unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_secs(60)).unwrap();
    let generation = runtime.current_generation().unwrap();
    let epoch = Arc::new(crate::watch::WatcherEpoch::new());
    let apply = |refresh: BTreeSet<String>, remove: BTreeSet<String>| {
        runtime
            .provider()
            .apply_runtime_invalid_maintenance(
                &refresh,
                &remove,
                epoch.as_ref(),
                &generation,
                crate::watch::ReconciliationToken::for_test(epoch.clone(), 1),
                &OperationContext::legacy(),
            )
            .unwrap()
    };

    assert_eq!(
        apply(BTreeSet::from(["parsed.md".to_string()]), BTreeSet::new()),
        InvalidMaintenanceOutcome::Stale
    );
    assert_eq!(
        apply(BTreeSet::from(["absent.md".to_string()]), BTreeSet::new()),
        InvalidMaintenanceOutcome::Stale
    );
    assert_eq!(
        apply(BTreeSet::new(), BTreeSet::from(["parsed.md".to_string()])),
        InvalidMaintenanceOutcome::Stale
    );
    assert_eq!(
        apply(
            BTreeSet::from(["invalid.md".to_string()]),
            BTreeSet::from(["invalid.md".to_string()]),
        ),
        InvalidMaintenanceOutcome::Stale
    );

    let collection = Collection::open(directory.path()).unwrap();
    let connection = crate::cache::sqlite::open_cache_db(
        collection.held_root().cache_storage_path(),
        &collection.settings.cache_folder,
    )
    .unwrap();
    connection
        .execute(
            "UPDATE files SET frontmatter_json = '{\"bad\":true}', body = 'leak', effective_json = '{\"bad\":true}' WHERE path = 'invalid.md'",
            [],
        )
        .unwrap();
    connection
        .execute("INSERT INTO file_types VALUES ('invalid.md', 'bogus')", [])
        .unwrap();
    connection
        .execute(
            "INSERT INTO links VALUES ('invalid.md', 'parsed.md', '', 0, 'body', NULL, 'parsed')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO unique_values VALUES ('bogus', 'id', 'value', 'invalid.md')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO identity_values VALUES ('value', 'invalid.md')",
            [],
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        apply(BTreeSet::from(["invalid.md".to_string()]), BTreeSet::new()),
        InvalidMaintenanceOutcome::Applied(_)
    ));
    let connection = crate::cache::sqlite::open_cache_db(
        collection.held_root().cache_storage_path(),
        &collection.settings.cache_folder,
    )
    .unwrap();
    let canonical = connection
        .query_row(
            "SELECT frontmatter_json, body, effective_json, parse_error, failure_reason FROM files WHERE path = 'invalid.md'",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?, row.get::<_, i64>(3)?, row.get::<_, String>(4)?)),
        )
        .unwrap();
    assert_eq!(canonical.0, "{}");
    assert_eq!(canonical.1, "");
    assert_eq!(canonical.2, None);
    assert_eq!(canonical.3, 1);
    assert_eq!(canonical.4, "invalid_utf8");
    for table in ["file_types", "links", "unique_values", "identity_values"] {
        let count: i64 = connection
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {table} WHERE {} = 'invalid.md'",
                    if table == "links" {
                        "source_path"
                    } else {
                        "path"
                    }
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "source-owned {table} contamination remained");
    }
    let incoming: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM links WHERE source_path = 'parsed.md' AND raw_target = 'invalid'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(incoming, 1, "incoming backlinks must remain represented");
}

#[test]
fn maintenance_seals_are_single_successor_epoch_and_cache_identity_bound() {
    use crate::cache::runtime::InvalidMaintenanceOutcome;

    let directory = collection();
    fs::write(directory.path().join("invalid.md"), b"bad\xffutf8\n").unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_secs(60)).unwrap();
    let generation = runtime.current_generation().unwrap();
    let epoch = Arc::new(crate::watch::WatcherEpoch::new());
    let refresh = BTreeSet::from(["invalid.md".to_string()]);
    let make_seal = |revision| match runtime
        .provider()
        .apply_runtime_invalid_maintenance(
            &refresh,
            &BTreeSet::new(),
            epoch.as_ref(),
            &generation,
            crate::watch::ReconciliationToken::for_test(epoch.clone(), revision),
            &OperationContext::legacy(),
        )
        .unwrap()
    {
        InvalidMaintenanceOutcome::Applied(seal) => seal,
        other => panic!("expected committed seal at revision {revision}, got {other:?}"),
    };
    let validate = |seal, token, candidate: &BTreeSet<String>| {
        runtime
            .provider()
            .validate_runtime_invalid_maintenance_seal(
                seal,
                candidate,
                &BTreeSet::new(),
                &generation,
                &token,
                &OperationContext::legacy(),
            )
            .unwrap()
            .is_some()
    };

    let collection = Collection::open(directory.path()).unwrap();
    let logical_state = || {
        let connection = crate::cache::sqlite::open_cache_db_read_only_existing(
            collection.held_root().cache_storage_path(),
            &collection.settings.cache_folder,
        )
        .unwrap();
        let snapshot: String = connection
            .query_row(
                "SELECT value FROM meta WHERE key = 'query_snapshot'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let schema: i64 = connection
            .query_row("PRAGMA schema_version", [], |row| row.get(0))
            .unwrap();
        let row: (String, i64) = connection
            .query_row(
                "SELECT source_revision, parse_error FROM files WHERE path = 'invalid.md'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        (snapshot, schema, row)
    };
    let valid_seal = make_seal(0);
    let before = logical_state();
    assert!(validate(
        valid_seal,
        crate::watch::ReconciliationToken::for_test(epoch.clone(), 1),
        &refresh,
    ));
    assert_eq!(logical_state(), before);

    assert!(!validate(
        make_seal(1),
        crate::watch::ReconciliationToken::for_test(epoch.clone(), 1),
        &refresh,
    ));
    let foreign = Arc::new(crate::watch::WatcherEpoch::new());
    assert!(!validate(
        make_seal(2),
        crate::watch::ReconciliationToken::for_test(foreign, 3),
        &refresh,
    ));
    assert!(!validate(
        make_seal(3),
        crate::watch::ReconciliationToken::for_test(epoch.clone(), 4),
        &BTreeSet::new(),
    ));

    let data_seal = make_seal(4);
    let writer_root = collection.held_root().cache_storage_path().to_path_buf();
    let writer_cache_folder = collection.settings.cache_folder.clone();
    crate::cache::runtime::set_seal_validation_hook(
        &collection,
        crate::cache::runtime::SealValidationBoundary::AfterPreTransactionIdentity,
        move || {
            // Commit deterministically after the first path identity check but
            // before validation acquires its BEGIN IMMEDIATE reservation.
            let connection =
                crate::cache::sqlite::open_cache_db(&writer_root, &writer_cache_folder).unwrap();
            connection
                .execute(
                    "INSERT OR REPLACE INTO meta (key, value) VALUES ('external_writer', 'changed')",
                    [],
                )
                .unwrap();
        },
    );
    assert!(!validate(
        data_seal,
        crate::watch::ReconciliationToken::for_test(epoch.clone(), 5),
        &refresh,
    ));

    // Once validation owns BEGIN IMMEDIATE, an external cooperative writer is
    // rejected at both remaining identity/data-version boundaries. The seal
    // remains valid and its reservation is released only when the guard drops.
    for (revision, boundary) in [
        (
            40,
            crate::cache::runtime::SealValidationBoundary::BeforeReservedDataVersion,
        ),
        (
            42,
            crate::cache::runtime::SealValidationBoundary::AfterReservedIdentity,
        ),
    ] {
        let seal = make_seal(revision);
        let blocked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_blocked = blocked.clone();
        let writer_path = crate::cache::sqlite::cache_db_path(
            &collection.root,
            &collection.settings.cache_folder,
        );
        crate::cache::runtime::set_seal_validation_hook(&collection, boundary, move || {
            let result = rusqlite::Connection::open(&writer_path).and_then(|connection| {
                connection.busy_timeout(Duration::ZERO)?;
                connection.execute(
                    "INSERT OR REPLACE INTO meta (key, value) VALUES ('reserved_writer', 'blocked')",
                    [],
                )
            });
            hook_blocked.store(result.is_err(), std::sync::atomic::Ordering::Release);
        });
        assert!(validate(
            seal,
            crate::watch::ReconciliationToken::for_test(epoch.clone(), revision + 1),
            &refresh,
        ));
        assert!(blocked.load(std::sync::atomic::Ordering::Acquire));
    }

    let schema_seal = make_seal(5);
    let connection = crate::cache::sqlite::open_cache_db(
        collection.held_root().cache_storage_path(),
        &collection.settings.cache_folder,
    )
    .unwrap();
    connection
        .execute_batch("ALTER TABLE files ADD COLUMN seal_schema_drift INTEGER;")
        .unwrap();
    drop(connection);
    assert!(!validate(
        schema_seal,
        crate::watch::ReconciliationToken::for_test(epoch.clone(), 6),
        &refresh,
    ));

    let second_runtime_seal = make_seal(6);
    let rebuild_seal = make_seal(7);
    let second_runtime =
        FilesystemRuntime::open(directory.path(), Duration::from_secs(60)).unwrap();
    assert!(!validate(
        second_runtime_seal,
        crate::watch::ReconciliationToken::for_test(epoch.clone(), 7),
        &refresh,
    ));
    drop(second_runtime);

    // Official rebuild and clear/recreate take the lifecycle lock exclusively,
    // so neither can cross a retained seal on any platform. Both remain bounded,
    // report the lock error, and succeed once acknowledgement would drop it.
    let blocked_rebuild = collection.cache_rebuild();
    assert_eq!(blocked_rebuild["success"], false);
    assert_eq!(blocked_rebuild["error"]["code"], "cache_rebuild_failed");
    drop(rebuild_seal);
    assert_eq!(collection.cache_rebuild()["success"], true);
}

#[test]
fn retained_seal_orders_official_clear_before_recreate() {
    use crate::cache::runtime::InvalidMaintenanceOutcome;

    let directory = collection();
    fs::write(directory.path().join("invalid.md"), b"bad\xffutf8\n").unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_secs(60)).unwrap();
    let generation = runtime.current_generation().unwrap();
    let epoch = Arc::new(crate::watch::WatcherEpoch::new());
    let refresh = BTreeSet::from(["invalid.md".to_string()]);
    let seal = match runtime
        .provider()
        .apply_runtime_invalid_maintenance(
            &refresh,
            &BTreeSet::new(),
            epoch.as_ref(),
            &generation,
            crate::watch::ReconciliationToken::for_test(epoch.clone(), 0),
            &OperationContext::legacy(),
        )
        .unwrap()
    {
        InvalidMaintenanceOutcome::Applied(seal) => seal,
        other => panic!("expected retained seal, got {other:?}"),
    };
    let collection = Collection::open(directory.path()).unwrap();
    let blocked = collection.cache_clear();
    assert_eq!(blocked["success"], false);
    assert_eq!(blocked["error"]["code"], "cache_clear_failed");
    drop(seal);
    assert_eq!(collection.cache_clear()["success"], true);
    assert_eq!(collection.cache_rebuild()["success"], true);
}

#[cfg(windows)]
#[test]
fn hosted_windows_retained_seal_lock_clear_ordering() {
    // Hosted Windows executes the same retained-SQLite-handle ordering with its
    // native deletion sharing semantics in addition to the advisory lock.
    retained_seal_orders_official_clear_before_recreate();
}

#[cfg(unix)]
#[test]
fn seal_identity_replacement_between_every_validation_boundary_is_rejected() {
    use crate::cache::runtime::{InvalidMaintenanceOutcome, SealValidationBoundary};

    for boundary in [
        SealValidationBoundary::AfterPreTransactionIdentity,
        SealValidationBoundary::AfterReservedIdentity,
    ] {
        let directory = collection();
        fs::write(directory.path().join("invalid.md"), b"bad\xffutf8\n").unwrap();
        let runtime = FilesystemRuntime::open(directory.path(), Duration::from_secs(60)).unwrap();
        let generation = runtime.current_generation().unwrap();
        let epoch = Arc::new(crate::watch::WatcherEpoch::new());
        let refresh = BTreeSet::from(["invalid.md".to_string()]);
        let seal = match runtime
            .provider()
            .apply_runtime_invalid_maintenance(
                &refresh,
                &BTreeSet::new(),
                epoch.as_ref(),
                &generation,
                crate::watch::ReconciliationToken::for_test(epoch.clone(), 0),
                &OperationContext::legacy(),
            )
            .unwrap()
        {
            InvalidMaintenanceOutcome::Applied(seal) => seal,
            other => panic!("expected seal before {boundary:?}, got {other:?}"),
        };
        let replacement = Collection::open(directory.path()).unwrap();
        let hook_collection = Collection::open(directory.path()).unwrap();
        crate::cache::runtime::set_seal_validation_hook(&hook_collection, boundary, move || {
            // Deliberately bypass the advisory lifecycle contract to model raw
            // Unix unlink/recreate. Official cache_clear/cache_rebuild cannot do
            // this while the seal holds its shared guard.
            let cache_root = replacement
                .held_root()
                .cache_storage_path()
                .join(&replacement.settings.cache_folder);
            for name in ["cache.db", "cache.db-wal", "cache.db-shm"] {
                match fs::remove_file(cache_root.join(name)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => panic!("raw cache replacement failed: {error}"),
                }
            }
            let mut connection = crate::cache::sqlite::open_cache_db(
                replacement.held_root().cache_storage_path(),
                &replacement.settings.cache_folder,
            )
            .unwrap();
            crate::cache::indexer::reindex_all(&mut connection, &replacement).unwrap();
        });
        assert!(runtime
            .provider()
            .validate_runtime_invalid_maintenance_seal(
                seal,
                &refresh,
                &BTreeSet::new(),
                &generation,
                &crate::watch::ReconciliationToken::for_test(epoch.clone(), 1),
                &OperationContext::legacy(),
            )
            .unwrap()
            .is_none());
    }
}

#[test]
fn stale_invalid_maintenance_cannot_rewind_or_contaminate_successor_cache() {
    let directory = collection();
    fs::write(directory.path().join("invalid.md"), b"first\xffrevision\n").unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_secs(60)).unwrap();
    let expected = runtime.current_generation().unwrap();
    let successor = expected.successor().unwrap();
    runtime
        .provider()
        .apply_runtime_cache_changes(&ChangeSet::None, &successor)
        .unwrap();

    let collection = Collection::open(directory.path()).unwrap();
    let connection = crate::cache::sqlite::open_cache_db(
        collection.held_root().cache_storage_path(),
        &collection.settings.cache_folder,
    )
    .unwrap();
    let old_revision = connection
        .query_row(
            "SELECT source_revision FROM files WHERE path = 'invalid.md'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    drop(connection);
    fs::write(directory.path().join("invalid.md"), b"second\xffrevision\n").unwrap();

    let unpoisoned_epoch = Arc::new(crate::watch::WatcherEpoch::new());
    let applied = runtime
        .provider()
        .apply_runtime_invalid_maintenance(
            &BTreeSet::from(["invalid.md".to_string()]),
            &BTreeSet::new(),
            unpoisoned_epoch.as_ref(),
            &expected,
            crate::watch::ReconciliationToken::for_test(unpoisoned_epoch.clone(), 1),
            &OperationContext::legacy(),
        )
        .unwrap();
    assert_eq!(
        applied,
        crate::cache::runtime::InvalidMaintenanceOutcome::Stale,
        "stale sequence must be rejected under the gate"
    );
    let foreign_epoch = CollectionGeneration::initial();
    assert_ne!(foreign_epoch.runtime_epoch(), successor.runtime_epoch());
    assert_eq!(
        runtime
            .provider()
            .apply_runtime_invalid_maintenance(
                &BTreeSet::new(),
                &BTreeSet::new(),
                unpoisoned_epoch.as_ref(),
                &foreign_epoch,
                crate::watch::ReconciliationToken::for_test(unpoisoned_epoch.clone(), 2),
                &OperationContext::legacy(),
            )
            .unwrap(),
        crate::cache::runtime::InvalidMaintenanceOutcome::Stale
    );

    assert!(crate::cache::runtime::matches_generation(&collection, &successor).unwrap());
    assert!(!crate::cache::runtime::matches_generation(&collection, &expected).unwrap());
    let connection = crate::cache::sqlite::open_cache_db(
        collection.held_root().cache_storage_path(),
        &collection.settings.cache_folder,
    )
    .unwrap();
    let revision = connection
        .query_row(
            "SELECT source_revision FROM files WHERE path = 'invalid.md'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    assert_eq!(revision, old_revision);
}

#[test]
fn runtime_reverse_link_index_tracks_resolved_targets_incrementally() {
    let directory = collection();
    fs::write(
        directory.path().join("source.md"),
        "---\nid: source\n---\nLinks to [[future]].\n",
    )
    .unwrap();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();

    let before = runtime
        .provider()
        .with_collection_read(|collection| {
            let connection = crate::cache::sqlite::open_cache_db(
                collection.held_root().cache_storage_path(),
                &collection.settings.cache_folder,
            )
            .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
            crate::cache::indexer::load_backlinks(&connection)
                .map_err(|error| ProviderError::CollectionOpen(error.to_string()))
        })
        .unwrap();
    assert!(before.is_empty());

    let created = runtime
        .execute(&OperationRequest::new(
            OperationKind::Create,
            json!({"path": "future.md", "frontmatter": {"id": "future"}}),
        ))
        .unwrap();
    assert!(created.valid, "{created:?}");

    let after = runtime
        .provider()
        .with_collection_read(|collection| {
            let connection = crate::cache::sqlite::open_cache_db(
                collection.held_root().cache_storage_path(),
                &collection.settings.cache_folder,
            )
            .map_err(|error| ProviderError::CollectionOpen(error.to_string()))?;
            crate::cache::indexer::load_backlinks(&connection)
                .map_err(|error| ProviderError::CollectionOpen(error.to_string()))
        })
        .unwrap();
    assert_eq!(after.get("future.md"), Some(&vec!["source.md".to_string()]));
}

#[test]
fn operation_kind_has_a_stable_wire_shape() {
    let request = OperationRequest::new(OperationKind::Rename, json!({"from": "a.md"}));
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        json!({"operation": "rename", "input": {"from": "a.md"}})
    );
    assert_eq!(
        "query".parse::<OperationKind>().unwrap(),
        OperationKind::Query
    );
    assert!("unknown".parse::<OperationKind>().is_err());
}

#[test]
fn mutation_affected_paths_include_rename_reference_rewrites() {
    let request = OperationRequest::new(
        OperationKind::Rename,
        json!({
            "from": "old.md",
            "to": "new.md",
            "simulate_before_ref_update": [{"path": "raced.md"}],
        }),
    );
    let result = OperationResult {
        valid: true,
        result: json!({
            "from": "old.md",
            "to": "new.md",
            "references_updated": [{"path": "linked.md", "location": "body"}],
        }),
        diagnostics: vec![],
    };
    assert_eq!(
        request.affected_paths(&result),
        ["linked.md", "new.md", "old.md", "raced.md"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
}

#[test]
fn observer_reports_payload_free_performance_and_opt_in_errors() {
    let directory = collection();
    let observer = Arc::new(RecordingObserver::default());
    let provider = FilesystemProvider::open_observed(
        directory.path(),
        observer.clone(),
        ObserverOptions {
            errors: ErrorReporting::Codes,
        },
    )
    .unwrap();

    let valid = provider
        .execute(&OperationRequest::new(
            OperationKind::Create,
            json!({
                "path": "safe.md",
                "frontmatter": {"private": "must-not-be-observed"},
            }),
        ))
        .unwrap();
    assert!(valid.valid);
    let invalid = provider
        .execute(&OperationRequest::new(
            OperationKind::Create,
            json!({
                "path": "../escape.md",
                "frontmatter": {"private": "must-not-be-observed"},
            }),
        ))
        .unwrap();
    assert!(!invalid.valid);
    let invalid_code = invalid.diagnostics[0].code.clone();

    let observations = observer.performance.lock().unwrap();
    assert_eq!(observations.len(), 2);
    assert_eq!(observations[0].operation, "create");
    assert!(observations[0].valid);
    assert!(!observations[1].valid);
    assert!(observations[1]
        .diagnostic_codes
        .iter()
        .any(|code| code == &invalid_code));
    let serialized = serde_json::to_string(&*observations).unwrap();
    assert!(!serialized.contains("safe.md"));
    assert!(!serialized.contains("must-not-be-observed"));
    drop(observations);

    let errors = observer.errors.lock().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].stage, "execute");
    assert!(errors[0].message.is_none());
}

#[test]
fn observer_reports_performance_when_provider_execution_fails_early() {
    let directory = collection();
    let observer = Arc::new(RecordingObserver::default());
    let provider = FilesystemProvider::open_observed(
        directory.path(),
        observer.clone(),
        ObserverOptions {
            errors: ErrorReporting::Codes,
        },
    )
    .unwrap();
    fs::write(
        directory.path().join("mdbase.yaml"),
        "spec_version: 99.0.0\n",
    )
    .unwrap();

    let error = provider
        .execute(&OperationRequest::new(
            OperationKind::Read,
            json!({"path": "private-name.md"}),
        ))
        .expect_err("reopening invalid configuration must fail");

    let performance = observer.performance.lock().unwrap();
    assert_eq!(performance.len(), 1);
    assert_eq!(performance[0].operation, "read");
    assert!(!performance[0].valid);
    assert_eq!(performance[0].diagnostic_count, 1);
    assert_eq!(performance[0].diagnostic_codes, [error.code()]);
    assert_eq!(performance[0].execute_us, 0);
    assert_eq!(performance[0].synchronize_us, 0);
    assert!(!serde_json::to_string(&*performance)
        .unwrap()
        .contains("private-name.md"));
    drop(performance);

    let errors = observer.errors.lock().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].stage, "open");
    assert_eq!(errors[0].code, error.code());
    assert!(errors[0].message.is_none());
}

#[cfg(unix)]
#[test]
fn held_resource_reads_reject_hardlinks_from_opened_nofollow_handles() {
    let config_root = collection();
    let config = Collection::open(config_root.path()).unwrap();
    fs::hard_link(
        config_root.path().join("mdbase.yaml"),
        config_root.path().join("config-link.yaml"),
    )
    .unwrap();
    assert_eq!(
        crate::config::load_config_for_open_held(config.held_root())["valid"],
        false
    );

    let resource_root = collection();
    fs::write(resource_root.path().join("schema.json"), "{}\n").unwrap();
    let resource = Collection::open(resource_root.path()).unwrap();
    fs::hard_link(
        resource_root.path().join("schema.json"),
        resource_root.path().join("schema-link.json"),
    )
    .unwrap();
    assert!(resource.held_root().read("schema.json").is_err());

    let shadow_root = collection();
    fs::write(shadow_root.path().join("record.md"), "record\n").unwrap();
    let shadow = Collection::open(shadow_root.path()).unwrap();
    fs::hard_link(
        shadow_root.path().join("record.md"),
        shadow_root.path().join("record-link.md"),
    )
    .unwrap();
    assert!(crate::mutation::shadow::shadow_collection(&shadow).is_err());
}

#[cfg(unix)]
#[test]
fn held_authority_never_adopts_a_replacement_root_across_refresh_snapshot_cache_and_legacy_mutation(
) {
    let directory = collection();
    let root = directory.path().to_path_buf();
    fs::write(root.join("authority.md"), "original\n").unwrap();
    let collection = Collection::open(&root).unwrap();
    let provider = FilesystemProvider::open(&root).unwrap();

    let held = root.with_extension("held-authority");
    let swap_root = root.clone();
    let swap_held = held.clone();
    crate::cache::set_cache_access_hook(&root, move || {
        fs::rename(&swap_root, &swap_held).unwrap();
        fs::create_dir(&swap_root).unwrap();
        fs::write(
            swap_root.join("mdbase.yaml"),
            "spec_version: 0.3.0\nx-replacement: true\n",
        )
        .unwrap();
        fs::write(swap_root.join("replacement-only.md"), "replacement\n").unwrap();
    });

    // The deterministic swap occurs after cache authority was acquired but
    // immediately before SQLite access. SQLite remains in identity-bound
    // private storage and cannot create files in the replacement collection.
    let cache = collection.cache_rebuild();
    assert_eq!(cache["success"], true, "{cache:?}");
    assert!(!root.join(".mdbase").exists());

    let _ = provider.refresh();
    let snapshot = provider.snapshot().unwrap();
    assert!(snapshot
        .records
        .iter()
        .any(|record| record.path == "authority.md"));
    assert!(!snapshot
        .records
        .iter()
        .any(|record| record.path == "replacement-only.md"));

    let shadow = crate::mutation::shadow::shadow_collection(&collection).unwrap();
    assert!(shadow.baseline.contains_key("authority.md"));
    assert!(!shadow.baseline.contains_key("replacement-only.md"));

    let batch = crate::v03::batch::execute(
        &collection,
        &serde_json::json!({
            "operations": [{"kind": "create", "input": {"path": "transaction.md", "body": "held"}}]
        }),
    );
    assert!(batch.valid, "{batch:?}");
    assert!(held.join("transaction.md").is_file());
    assert!(!root.join("transaction.md").exists());
    assert!(!root.join(".mdbase").exists());

    let created = collection.create(&serde_json::json!({"path": "created.md", "body": "held"}));
    assert!(created.get("error").is_none(), "{created:?}");
    assert!(held.join("created.md").is_file());
    assert!(!root.join("created.md").exists());
    let updated =
        collection.update(&serde_json::json!({"path": "authority.md", "body": "updated"}));
    assert!(updated.get("error").is_none(), "{updated:?}");
    assert_eq!(
        fs::read_to_string(held.join("authority.md")).unwrap(),
        "updated"
    );
    let deleted = collection.delete(&serde_json::json!({"path": "authority.md"}));
    assert!(deleted.get("error").is_none(), "{deleted:?}");
    assert!(!held.join("authority.md").exists());
    assert_eq!(
        fs::read_to_string(root.join("replacement-only.md")).unwrap(),
        "replacement\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("mdbase.yaml")).unwrap(),
        "spec_version: 0.3.0\nx-replacement: true\n"
    );
    assert_eq!(fs::read_dir(&root).unwrap().count(), 2);

    fs::remove_dir_all(&root).unwrap();
    fs::rename(&held, &root).unwrap();
}
