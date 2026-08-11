use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Cooperative cancellation for one collection operation.
///
/// The collection engine is synchronous, so hosts cancel work by sharing this
/// token with the worker thread. Long-running read paths check the token at
/// bounded intervals and release their operation-scoped snapshots promptly.
#[derive(Debug, Clone, Default)]
pub struct OperationCancellation {
    cancelled: Arc<AtomicBool>,
}

impl OperationCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn check(&self) -> Result<(), OperationCancelled> {
        if self.is_cancelled() {
            Err(OperationCancelled)
        } else {
            Ok(())
        }
    }
}

/// Returned when a host cancels a cooperative collection operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationCancelled;

impl std::fmt::Display for OperationCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("collection operation cancelled")
    }
}

impl std::error::Error for OperationCancelled {}
