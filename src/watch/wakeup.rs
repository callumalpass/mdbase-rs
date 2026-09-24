use std::sync::{Arc, Mutex};

/// A payload-free readiness hint. Events remain owned by the watcher queue;
/// hints may be coalesced. Callbacks must be non-blocking and must not panic.
#[derive(Clone, Default)]
pub struct WatchWakeup(Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>);

impl std::fmt::Debug for WatchWakeup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WatchWakeup")
    }
}

impl WatchWakeup {
    /// Installing a listener also wakes it, covering events queued before
    /// installation. The callback is never called while holding the slot lock.
    pub fn set(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = Some(callback);
        self.wake();
    }

    pub fn wake(&self) {
        let callback = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(callback) = callback {
            callback();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[test]
    fn clones_share_registration_and_callbacks_run_outside_the_lock() {
        let wakeup = WatchWakeup::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let slot = wakeup.clone();
        wakeup.set(Arc::new(move || {
            assert!(slot.0.try_lock().is_ok());
            observed.fetch_add(1, Ordering::SeqCst);
        }));
        wakeup.clone().wake();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // Remove the test's deliberately self-referencing callback.
        *wakeup.0.lock().unwrap() = None;
    }
}
