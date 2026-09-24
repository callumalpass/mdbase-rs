use super::*;
use std::sync::{mpsc, Arc};
use std::time::Duration;

#[test]
fn readiness_hints_coalesce_without_consuming_events_and_cover_late_subscription() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("mdbase.yaml"),
        "spec_version: 0.3.0\nsettings:\n  validation: warn\n",
    )
    .unwrap();
    let watcher = CollectionWatcher::open(root.path(), Duration::from_millis(20)).unwrap();
    for index in 0..3 {
        std::fs::write(
            root.path().join(format!("note-{index}.md")),
            "---\ntitle: Visible\n---\n",
        )
        .unwrap();
    }
    watcher.rescan().unwrap();
    // The queue already contains events. Installing a listener must still wake
    // the host; callbacks are hints and must never dequeue these events.
    let (ready, receiver) = mpsc::sync_channel(1);
    watcher.set_event_waker(Arc::new(move || {
        let _ = ready.try_send(());
    }));
    receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    let mut count = 0;
    while let Some(event) = watcher.recv_timeout(Duration::ZERO).unwrap() {
        if event.event_type == "mdbase.record.created" {
            count += 1;
        }
    }
    assert_eq!(count, 3);
    std::fs::write(root.path().join("next.md"), "---\ntitle: Next\n---\n").unwrap();
    watcher.rescan().unwrap();
    receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        watcher
            .recv_timeout(Duration::ZERO)
            .unwrap()
            .unwrap()
            .payload["path"],
        "next.md"
    );
}
