use super::*;
use crate::v03::OperationResult;
use crate::Collection;
use serde_json::json;
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
    assert!(outcome.result.valid);
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
    assert!(outcome.result.valid, "{:?}", outcome.result.diagnostics);
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
    assert!(!rejection.result.valid);
    assert_eq!(
        rejection.result.diagnostics[0].code,
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
    assert_eq!(read.result.result["frontmatter"]["title"], "Committed");
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
        first.outcome.result.result["results"]
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
        second.outcome.result.result["meta"]["total_count"],
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
        assert!(read.result.valid, "crash point {point}: {read:?}");
        assert_eq!(read.result.result["body"], "durable body\n");
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
    assert!(!outcome.result.valid);
    assert_eq!(
        outcome.result.diagnostics[0].code,
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
    assert!(!outcome.result.valid);
    assert!(outcome
        .result
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "duplicate_id"));
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
    assert!(query.result.valid, "{:?}", query.result.diagnostics);
    assert!(query.result.result["results"][0].get("body").is_none());
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
                &collection.root,
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
                &collection.root,
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
