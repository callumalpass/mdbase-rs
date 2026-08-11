use std::time::{Duration, Instant};

use crate::{OperationCancellation, OperationStopReason};

use super::ProviderError;

const LEGACY_DEADLINE: Duration = Duration::from_secs(24 * 60 * 60);
const CANCELLATION_POLL: Duration = Duration::from_millis(10);

/// Absolute monotonic deadline for one provider/runtime operation.
///
/// Deadlines are process-local and are never serialized into durable state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationDeadline(Instant);

impl OperationDeadline {
    /// Construct a deadline at an exact monotonic instant.
    pub fn at(instant: Instant) -> Self {
        Self(instant)
    }

    /// Construct a deadline relative to the current monotonic instant.
    pub fn after(duration: Duration) -> Self {
        Self(Instant::now() + duration)
    }

    /// Return the underlying process-local monotonic instant.
    pub fn instant(self) -> Instant {
        self.0
    }

    /// Return the remaining duration, saturating at zero after expiry.
    pub fn remaining(self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }

    /// Whether the deadline has elapsed.
    pub fn is_elapsed(self) -> bool {
        Instant::now() >= self.0
    }
}

/// Cooperative cancellation and deadline ownership for one runtime call.
#[derive(Clone, Debug)]
pub struct OperationContext {
    cancellation: OperationCancellation,
    deadline: OperationDeadline,
}

impl OperationContext {
    /// Bind an existing caller cancellation token to an absolute deadline.
    pub fn new(cancellation: &OperationCancellation, deadline: OperationDeadline) -> Self {
        Self {
            cancellation: cancellation.with_deadline(deadline.instant()),
            deadline,
        }
    }

    /// Return the cooperative token used by long-running collection work.
    pub fn cancellation(&self) -> &OperationCancellation {
        &self.cancellation
    }

    /// Return the operation's absolute monotonic deadline.
    pub fn deadline(&self) -> OperationDeadline {
        self.deadline
    }

    /// Fail with the typed reason when cancellation or deadline has won.
    pub fn check(&self) -> Result<(), ProviderError> {
        match self.cancellation.stop_reason() {
            Some(OperationStopReason::Cancelled) => Err(ProviderError::OperationCancelled),
            Some(OperationStopReason::Deadline) => Err(ProviderError::OperationDeadline),
            None => Ok(()),
        }
    }

    pub(crate) fn next_wait(&self) -> Result<Duration, ProviderError> {
        self.check()?;
        Ok(self.deadline.remaining().min(CANCELLATION_POLL))
    }

    pub(crate) fn legacy() -> Self {
        Self::new(
            &OperationCancellation::new(),
            OperationDeadline::after(LEGACY_DEADLINE),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_distinguishes_deadline_from_explicit_cancellation() {
        let cancellation = OperationCancellation::new();
        let expired = OperationContext::new(
            &cancellation,
            OperationDeadline::at(Instant::now() - Duration::from_millis(1)),
        );
        assert!(matches!(
            expired.check(),
            Err(ProviderError::OperationDeadline)
        ));

        let cancellation = OperationCancellation::new();
        let active = OperationContext::new(
            &cancellation,
            OperationDeadline::after(Duration::from_secs(1)),
        );
        cancellation.cancel();
        assert!(matches!(
            active.check(),
            Err(ProviderError::OperationCancelled)
        ));
    }
}
