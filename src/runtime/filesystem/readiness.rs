use super::*;

impl FilesystemRuntime {
    /// Install a non-blocking, payload-free watcher readiness hint. The durable
    /// feed and watcher queue remain authoritative; hosts may coalesce hints.
    pub fn set_event_waker(
        &self,
        callback: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(), ProviderError> {
        self.watcher
            .lock()
            .map_err(|_| ProviderError::LockPoisoned)?
            .set_event_waker(callback);
        Ok(())
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<WatchEvent>, ProviderError> {
        if let Some(event) = self
            .pending_watch
            .lock()
            .map_err(|_| ProviderError::LockPoisoned)?
            .pop_front()
        {
            return Ok(Some(event));
        }
        self.watcher
            .lock()
            .map_err(|_| ProviderError::LockPoisoned)?
            .recv_timeout(timeout)
            .map_err(Into::into)
    }
}
