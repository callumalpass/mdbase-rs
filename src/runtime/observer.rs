use serde::{Deserialize, Serialize};

/// Controls whether operation failures are delivered to an observer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ErrorReporting {
    /// Do not deliver error events. This is the privacy-preserving default.
    #[default]
    Disabled,
    /// Deliver stable error and diagnostic codes without human messages.
    Codes,
    /// Also deliver human-readable local error messages.
    Messages,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObserverOptions {
    pub errors: ErrorReporting,
}

/// Payload-free performance observation for one provider operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationPerformance {
    pub operation: String,
    pub queue_us: u64,
    pub open_us: u64,
    pub execute_us: u64,
    pub synchronize_us: u64,
    pub total_us: u64,
    pub valid: bool,
    pub diagnostic_count: usize,
    pub diagnostic_codes: Vec<String>,
}

/// Optional local error observation. Collection paths and request or record
/// payloads are intentionally absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationError {
    pub operation: String,
    pub stage: String,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Host hook for metrics and optional local error logging.
pub trait RuntimeObserver: Send + Sync {
    fn on_performance(&self, observation: &OperationPerformance);

    fn on_error(&self, _observation: &OperationError) {}
}

#[derive(Debug, Default)]
pub(crate) struct NoopObserver;

impl RuntimeObserver for NoopObserver {
    #[inline]
    fn on_performance(&self, _observation: &OperationPerformance) {}
}

/// Ready-to-use structured logger for hosts built with the `tracing` feature.
/// Performance records use `mdbase::performance`; opt-in error records use
/// `mdbase::errors` and contain no collection paths or operation payloads.
#[cfg(feature = "tracing")]
#[derive(Debug, Default)]
pub struct TracingObserver;

#[cfg(feature = "tracing")]
impl RuntimeObserver for TracingObserver {
    fn on_performance(&self, observation: &OperationPerformance) {
        tracing::debug!(
            target: "mdbase::performance",
            operation = %observation.operation,
            queue_us = observation.queue_us,
            open_us = observation.open_us,
            execute_us = observation.execute_us,
            synchronize_us = observation.synchronize_us,
            total_us = observation.total_us,
            valid = observation.valid,
            diagnostic_count = observation.diagnostic_count,
            diagnostic_codes = ?observation.diagnostic_codes,
            "mdbase operation completed"
        );
    }

    fn on_error(&self, observation: &OperationError) {
        tracing::error!(
            target: "mdbase::errors",
            operation = %observation.operation,
            stage = %observation.stage,
            code = %observation.code,
            message = observation.message.as_deref(),
            "mdbase operation failed"
        );
    }
}
