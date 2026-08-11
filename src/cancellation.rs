use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Cooperative cancellation for one collection operation.
///
/// The collection engine is synchronous, so hosts cancel work by sharing this
/// token with the worker thread. Long-running read paths check the token at
/// bounded intervals and release their operation-scoped snapshots promptly.
#[derive(Debug, Clone, Default)]
pub struct OperationCancellation {
    cancelled: Arc<AtomicBool>,
    deadline: Option<Instant>,
}

impl OperationCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Derive a token that observes the same explicit cancellation flag and
    /// also stops after an absolute monotonic deadline.
    pub fn with_deadline(&self, deadline: Instant) -> Self {
        Self {
            cancelled: self.cancelled.clone(),
            deadline: Some(deadline),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Return why cooperative work should stop, if either boundary was met.
    pub fn stop_reason(&self) -> Option<OperationStopReason> {
        if self.is_cancelled() {
            Some(OperationStopReason::Cancelled)
        } else if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            Some(OperationStopReason::Deadline)
        } else {
            None
        }
    }

    pub fn check(&self) -> Result<(), OperationCancelled> {
        if self.stop_reason().is_some() {
            Err(OperationCancelled)
        } else {
            Ok(())
        }
    }
}

/// The cooperative boundary that stopped an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationStopReason {
    /// A caller explicitly cancelled the shared token.
    Cancelled,
    /// The absolute monotonic deadline elapsed.
    Deadline,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn derived_deadline_preserves_shared_explicit_cancellation() {
        let root = OperationCancellation::new();
        let bounded = root.with_deadline(Instant::now() + Duration::from_secs(60));
        assert_eq!(bounded.stop_reason(), None);

        root.cancel();

        assert_eq!(bounded.stop_reason(), Some(OperationStopReason::Cancelled));
        assert_eq!(bounded.check(), Err(OperationCancelled));
    }

    #[test]
    fn deadline_stops_only_the_derived_token() {
        let root = OperationCancellation::new();
        let bounded = root.with_deadline(Instant::now());

        assert_eq!(bounded.stop_reason(), Some(OperationStopReason::Deadline));
        assert_eq!(bounded.check(), Err(OperationCancelled));
        assert_eq!(root.stop_reason(), None);
    }
}
