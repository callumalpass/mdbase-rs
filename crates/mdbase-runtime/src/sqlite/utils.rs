use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};

use crate::error::{RuntimeError, RuntimeResult};
use crate::model::{ConcurrencyPolicy, RunStatus};

pub(super) fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub(super) fn parse_timestamp(value: &str) -> RuntimeResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| RuntimeError::Store(error.to_string()))
}

pub(super) fn nonnegative_cursor(value: i64) -> RuntimeResult<u64> {
    u64::try_from(value).map_err(|_| RuntimeError::Store("negative event cursor".to_string()))
}

pub(super) fn add_duration(now: DateTime<Utc>, duration: Duration) -> RuntimeResult<DateTime<Utc>> {
    TimeDelta::from_std(duration)
        .map(|duration| now + duration)
        .map_err(|_| RuntimeError::Clock("lease duration is too large".to_string()))
}

pub(super) fn run_status(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Queued => "queued",
        RunStatus::Running => "running",
        RunStatus::Waiting => "waiting",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        RunStatus::Indeterminate => "indeterminate",
    }
}

pub(super) fn concurrency_policy(policy: ConcurrencyPolicy) -> &'static str {
    match policy {
        ConcurrencyPolicy::Skip => "skip",
        ConcurrencyPolicy::Queue => "queue",
        ConcurrencyPolicy::Replace => "replace",
        ConcurrencyPolicy::Allow => "allow",
    }
}

pub(super) fn store_error(error: rusqlite::Error) -> RuntimeError {
    RuntimeError::Store(error.to_string())
}
