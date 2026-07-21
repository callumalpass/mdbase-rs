use super::*;
use serde_json::json;
use std::fs;
use std::sync::{Arc, Barrier, Mutex};
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
    assert_eq!(event.event_type, "mdbase.record.created");
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

#[test]
fn provider_loads_runtime_contracts_inside_its_authority_gate() {
    let directory = collection();
    fs::write(
        directory.path().join("event.md"),
        concat!(
            "---\ntype: event\nid: test.event\nversion: 1\n",
            "provider: test\nname: Test\nschemas:\n",
            "  dialect: json-schema-2020-12\n  payload:\n    type: object\n---\n"
        ),
    )
    .unwrap();
    let observer = Arc::new(RecordingObserver::default());
    let provider = FilesystemProvider::open_observed(
        directory.path(),
        observer.clone(),
        ObserverOptions::default(),
    )
    .unwrap();

    let loaded = provider
        .load_runtime_contracts(vec![], &crate::runtime_contracts::LoadOptions::default())
        .unwrap();
    assert!(loaded.registry.events.contains_key("test.event"));
    let performance = observer.performance.lock().unwrap();
    assert_eq!(performance.len(), 1);
    assert_eq!(performance[0].operation, "runtime_contracts.load");
}
