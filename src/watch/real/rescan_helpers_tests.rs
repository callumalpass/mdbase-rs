fn test_pending_rescan(watcher: &CollectionWatcher, ready: ReconciliationSender) -> PendingRescan {
    let ticket = reserve_rescan_slot(watcher.pending_rescans.clone()).unwrap();
    let id = watcher.next_rescan_id.fetch_add(1, Ordering::AcqRel);
    increment_revision(
        &watcher.invalidation_revision,
        &watcher.epoch,
        &watcher.commands,
    )
    .unwrap();
    PendingRescan { id, ready, ticket }
}

fn bounded_rescan(watcher: &CollectionWatcher, paths: Option<&[&str]>) {
    let (ready, receiver) = mpsc::channel();
    let pending = test_pending_rescan(watcher, ready);
    let command = match paths {
        Some(paths) => Command::RescanPaths(paths.iter().map(PathBuf::from).collect(), pending),
        None => Command::Rescan(pending),
    };
    watcher
        .commands
        .send(WorkerInput::Command(command))
        .expect("watcher worker remains available");
    receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("reconciliation completes within its bounded test budget")
        .expect("reconciliation succeeds");
}
