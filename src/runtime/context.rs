use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{OperationCancellation, OperationStopReason};

use super::ProviderError;

const LEGACY_DEADLINE: Duration = Duration::from_secs(24 * 60 * 60);
const CANCELLATION_POLL: Duration = Duration::from_millis(10);

/// Finite capture budgets applied to one operation.
///
/// The defaults permit ordinary large collections while preventing accidental
/// unbounded authority reads. Entry/resource limits apply per capture; actual
/// reads and newly retained results are charged to operation-wide counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureLimits {
    pub max_entries: u64,
    pub max_file_bytes: u64,
    pub max_aggregate_bytes: u64,
    pub max_depth: u64,
    pub max_resource_entries: u64,
    pub max_retained_bytes: u64,
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_file_bytes: 64 * 1024 * 1024,
            // Eight full retained-capture equivalents cover the ordinary
            // before/shadow/after phases of a 100k-entry mutation.
            max_aggregate_bytes: 4 * 1024 * 1024 * 1024,
            max_depth: 128,
            max_resource_entries: 10_000,
            max_retained_bytes: 4 * 1024 * 1024 * 1024,
        }
    }
}

impl CaptureLimits {
    pub fn builder() -> CaptureLimitsBuilder {
        CaptureLimitsBuilder(Self::default())
    }
}

/// Builder for [`CaptureLimits`]. All values are exact inclusive maxima.
#[derive(Clone, Copy, Debug)]
pub struct CaptureLimitsBuilder(CaptureLimits);

impl CaptureLimitsBuilder {
    pub fn max_entries(mut self, value: u64) -> Self {
        self.0.max_entries = value;
        self
    }
    pub fn max_file_bytes(mut self, value: u64) -> Self {
        self.0.max_file_bytes = value;
        self
    }
    pub fn max_aggregate_bytes(mut self, value: u64) -> Self {
        self.0.max_aggregate_bytes = value;
        self
    }
    pub fn max_depth(mut self, value: u64) -> Self {
        self.0.max_depth = value;
        self
    }
    pub fn max_resource_entries(mut self, value: u64) -> Self {
        self.0.max_resource_entries = value;
        self
    }
    pub fn max_retained_bytes(mut self, value: u64) -> Self {
        self.0.max_retained_bytes = value;
        self
    }
    pub fn build(self) -> CaptureLimits {
        self.0
    }
}

/// Stable budget dimension reported when capture stops without partial success.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureLimitKind {
    Entries,
    FileBytes,
    AggregateBytes,
    Depth,
    ResourceEntries,
    RetainedBytes,
    ArithmeticOverflow,
}

impl CaptureLimitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Entries => "entries",
            Self::FileBytes => "file_bytes",
            Self::AggregateBytes => "aggregate_bytes",
            Self::Depth => "depth",
            Self::ResourceEntries => "resource_entries",
            Self::RetainedBytes => "retained_bytes",
            Self::ArithmeticOverflow => "arithmetic_overflow",
        }
    }
}

/// Typed diagnostic for a capture budget violation.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("capture limit exceeded ({kind}): attempted {attempted}, limit {limit}", kind = .kind.as_str())]
pub struct CaptureLimitExceeded {
    pub kind: CaptureLimitKind,
    pub limit: u64,
    pub attempted: u64,
}

#[derive(Debug, Default)]
struct CaptureUsage {
    aggregate: AtomicU64,
    retained: AtomicU64,
    exceeded: std::sync::Mutex<Option<CaptureLimitExceeded>>,
}

/// Absolute monotonic deadline for one provider/runtime operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationDeadline(Instant);

impl OperationDeadline {
    pub fn at(instant: Instant) -> Self {
        Self(instant)
    }
    pub fn after(duration: Duration) -> Self {
        Self(Instant::now() + duration)
    }
    pub fn instant(self) -> Instant {
        self.0
    }
    pub fn remaining(self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }
    pub fn is_elapsed(self) -> bool {
        Instant::now() >= self.0
    }
}

/// Cooperative cancellation, deadline, and capture-budget ownership for one runtime call.
#[derive(Clone, Debug)]
pub struct OperationContext {
    cancellation: OperationCancellation,
    deadline: OperationDeadline,
    limits: CaptureLimits,
    usage: Arc<CaptureUsage>,
}

thread_local! {
    static ACTIVE_CONTEXTS: RefCell<Vec<OperationContext>> = const { RefCell::new(Vec::new()) };
}

struct ActiveContextGuard;
impl Drop for ActiveContextGuard {
    fn drop(&mut self) {
        ACTIVE_CONTEXTS.with(|contexts| {
            contexts.borrow_mut().pop();
        });
    }
}

impl OperationContext {
    /// Bind a caller token to a deadline and the documented default budgets.
    pub fn new(cancellation: &OperationCancellation, deadline: OperationDeadline) -> Self {
        Self::with_capture_limits(cancellation, deadline, CaptureLimits::default())
    }

    /// Bind a caller token to a deadline and explicit capture budgets.
    pub fn with_capture_limits(
        cancellation: &OperationCancellation,
        deadline: OperationDeadline,
        limits: CaptureLimits,
    ) -> Self {
        Self {
            cancellation: cancellation.with_deadline(deadline.instant()),
            deadline,
            limits,
            usage: Arc::new(CaptureUsage::default()),
        }
    }

    pub fn cancellation(&self) -> &OperationCancellation {
        &self.cancellation
    }
    pub fn deadline(&self) -> OperationDeadline {
        self.deadline
    }
    pub fn capture_limits(&self) -> CaptureLimits {
        self.limits
    }

    pub fn check(&self) -> Result<(), ProviderError> {
        match self.cancellation.stop_reason() {
            Some(OperationStopReason::Cancelled) => Err(ProviderError::OperationCancelled),
            Some(OperationStopReason::Deadline) => Err(ProviderError::OperationDeadline),
            None => Ok(()),
        }
    }

    pub(crate) fn check_depth(&self, depth: u64) -> Result<(), ProviderError> {
        if depth > self.limits.max_depth {
            return Err(limit(CaptureLimitKind::Depth, self.limits.max_depth, depth));
        }
        Ok(())
    }

    pub(crate) fn check_file_bytes(&self, bytes: u64) -> Result<(), ProviderError> {
        if bytes > self.limits.max_file_bytes {
            return Err(limit(
                CaptureLimitKind::FileBytes,
                self.limits.max_file_bytes,
                bytes,
            ));
        }
        Ok(())
    }

    /// Check the number of entries retained by one capture. Entry and resource
    /// ceilings reset for each capture; byte/work counters are operation-wide.
    pub(crate) fn check_entries(&self, entries: u64) -> Result<(), ProviderError> {
        self.record_limit(check_limit(
            entries,
            self.limits.max_entries,
            CaptureLimitKind::Entries,
        ))
    }

    pub(crate) fn check_resource_entries(&self, entries: u64) -> Result<(), ProviderError> {
        self.record_limit(check_limit(
            entries,
            self.limits.max_resource_entries,
            CaptureLimitKind::ResourceEntries,
        ))
    }

    pub(crate) fn charge_read(&self, bytes: u64) -> Result<(), ProviderError> {
        self.record_limit(charge(
            &self.usage.aggregate,
            bytes,
            self.limits.max_aggregate_bytes,
            CaptureLimitKind::AggregateBytes,
        ))
    }

    pub(crate) fn charge_retained(&self, bytes: u64) -> Result<(), ProviderError> {
        self.record_limit(charge(
            &self.usage.retained,
            bytes,
            self.limits.max_retained_bytes,
            CaptureLimitKind::RetainedBytes,
        ))
    }

    pub(crate) fn capture_limit_error(&self) -> Option<ProviderError> {
        self.usage
            .exceeded
            .lock()
            .ok()
            .and_then(|error| error.clone())
            .map(ProviderError::CaptureLimitExceeded)
    }

    fn record_limit(&self, result: Result<(), ProviderError>) -> Result<(), ProviderError> {
        if let Err(ProviderError::CaptureLimitExceeded(error)) = &result {
            if let Ok(mut exceeded) = self.usage.exceeded.lock() {
                exceeded.get_or_insert_with(|| error.clone());
            }
        }
        result
    }

    /// Run synchronous canonical work with this caller context available to
    /// legacy internal adapters. Nested scopes retain the outer shared budget.
    pub(crate) fn scope<T>(&self, operation: impl FnOnce() -> T) -> T {
        ACTIVE_CONTEXTS.with(|contexts| contexts.borrow_mut().push(self.clone()));
        let _guard = ActiveContextGuard;
        operation()
    }

    pub(crate) fn current() -> Option<Self> {
        ACTIVE_CONTEXTS.with(|contexts| contexts.borrow().last().cloned())
    }

    pub(crate) fn current_or_legacy() -> Self {
        Self::current().unwrap_or_else(Self::internal)
    }

    pub(crate) fn next_wait(&self) -> Result<Duration, ProviderError> {
        self.check()?;
        Ok(self.deadline.remaining().min(CANCELLATION_POLL))
    }

    /// Bounded context for internal lifecycle work that has no external caller.
    pub(crate) fn internal() -> Self {
        Self::new(
            &OperationCancellation::new(),
            OperationDeadline::after(LEGACY_DEADLINE),
        )
    }

    /// Context retained only for test/support inventory.
    #[cfg(test)]
    pub(crate) fn legacy() -> Self {
        Self::internal()
    }
}

fn limit(kind: CaptureLimitKind, limit: u64, attempted: u64) -> ProviderError {
    ProviderError::CaptureLimitExceeded(CaptureLimitExceeded {
        kind,
        limit,
        attempted,
    })
}

fn check_limit(attempted: u64, maximum: u64, kind: CaptureLimitKind) -> Result<(), ProviderError> {
    if attempted > maximum {
        Err(limit(kind, maximum, attempted))
    } else {
        Ok(())
    }
}

fn charge(
    counter: &AtomicU64,
    amount: u64,
    maximum: u64,
    kind: CaptureLimitKind,
) -> Result<(), ProviderError> {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let attempted = current
            .checked_add(amount)
            .ok_or_else(|| limit(CaptureLimitKind::ArithmeticOverflow, u64::MAX, u64::MAX))?;
        if attempted > maximum {
            return Err(limit(kind, maximum, attempted));
        }
        match counter.compare_exchange_weak(
            current,
            attempted,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(()),
            Err(actual) => current = actual,
        }
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

    #[test]
    fn capture_boundaries_are_inclusive_and_shared_by_clones() {
        let limits = CaptureLimits::builder()
            .max_entries(1)
            .max_aggregate_bytes(3)
            .build();
        let context = OperationContext::with_capture_limits(
            &OperationCancellation::new(),
            OperationDeadline::after(Duration::from_secs(1)),
            limits,
        );
        context.check_entries(1).unwrap();
        context.clone().check_entries(1).unwrap();
        context.charge_read(3).unwrap();
        assert!(matches!(
            context.charge_read(1),
            Err(ProviderError::CaptureLimitExceeded(_))
        ));
    }
}
