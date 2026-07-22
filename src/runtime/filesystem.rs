use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::observer::NoopObserver;
use super::{
    FilesystemProvider, ObserverOptions, OperationRequest, ProviderError, RuntimeObserver,
};
use crate::v03::OperationResult;
use crate::watch::{CollectionWatcher, WatchEvent};

/// Filesystem authority with consistent operation and notification ordering.
pub struct FilesystemRuntime {
    provider: Arc<FilesystemProvider>,
    watcher: CollectionWatcher,
}

impl FilesystemRuntime {
    pub fn open(root: impl AsRef<Path>, debounce: Duration) -> Result<Self, ProviderError> {
        Self::open_observed(
            root,
            debounce,
            Arc::new(NoopObserver),
            ObserverOptions::default(),
        )
    }

    pub fn open_observed(
        root: impl AsRef<Path>,
        debounce: Duration,
        observer: Arc<dyn RuntimeObserver>,
        observer_options: ObserverOptions,
    ) -> Result<Self, ProviderError> {
        let provider = Arc::new(FilesystemProvider::open_observed(
            root.as_ref(),
            observer,
            observer_options,
        )?);
        let watcher = CollectionWatcher::open(root, debounce)?;
        Ok(Self { provider, watcher })
    }

    pub fn provider(&self) -> Arc<FilesystemProvider> {
        self.provider.clone()
    }

    pub fn execute(&self, request: &OperationRequest) -> Result<OperationResult, ProviderError> {
        self.provider.execute_with_post(request, |result| {
            if request.operation.is_mutation() && result.valid {
                // Hold the provider gate until the watcher compares the
                // post-write snapshot, preserving operation/event order.
                self.watcher.rescan_paths(request.affected_paths(result))?;
            }
            Ok(())
        })
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<WatchEvent>, ProviderError> {
        self.watcher.recv_timeout(timeout).map_err(Into::into)
    }

    /// Complete a full watcher comparison before accepting benchmark or host
    /// traffic. Normal mutations use the incremental synchronization path.
    pub fn synchronize(&self) -> Result<(), ProviderError> {
        self.watcher.rescan().map_err(Into::into)
    }
}
