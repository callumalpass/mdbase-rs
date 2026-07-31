use super::*;
use crate::v03::OperationResult;
use serde_json::json;
use std::fs;
use std::sync::{mpsc, Arc, Barrier, Condvar, Mutex};
use std::thread;
use std::time::Duration;

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
fn provider_snapshot_reports_the_record_path_and_read_error() {
    let directory = collection();
    fs::write(
        directory.path().join("broken.md"),
        "---\ntitle: [unterminated\n---\n",
    )
    .unwrap();

    let error = FilesystemProvider::open(directory.path())
        .unwrap()
        .snapshot()
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "collection failed to open: failed to read collection record 'broken.md': Failed to parse YAML frontmatter"
    );
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
fn runtime_queues_change_before_successful_mutation_returns() {
    let directory = collection();
    let runtime = FilesystemRuntime::open(directory.path(), Duration::from_millis(40)).unwrap();
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

    let event = runtime
        .recv_timeout(Duration::ZERO)
        .unwrap()
        .expect("successful mutation must queue a change before returning");
    assert_eq!(
        event.event_type, "mdbase.record.created",
        "unexpected watcher event: {event:#?}"
    );
    assert_eq!(event.payload["path"], "created.md");
    assert_eq!(event.payload["after"]["title"], "Created");
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
