use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::RuntimeResult;
use crate::model::{EventJournalEntry, PlannedRun, RunRecord, TimerRecord};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PreparedEvent {
    pub source_runtime: String,
    pub event_id: String,
    pub envelope: Value,
    pub received_at: DateTime<Utc>,
    pub runs: Vec<PlannedRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdmitOutcome {
    pub cursor: u64,
    pub duplicate: bool,
    pub admitted_run_ids: Vec<String>,
    pub skipped_run_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Claim {
    pub run: RunRecord,
    pub worker: String,
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TimerClaim {
    pub timer: TimerRecord,
    pub worker: String,
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StoreSnapshot {
    pub events: Vec<EventJournalEntry>,
    pub runs: Vec<RunRecord>,
    pub timers: Vec<TimerRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EventPage {
    pub events: Vec<EventJournalEntry>,
    pub retained_after: u64,
    pub head: u64,
    pub has_more: bool,
    pub reset_required: bool,
}

#[async_trait]
pub trait RuntimeStore: Send + Sync {
    async fn admit_event(&self, event: PreparedEvent) -> RuntimeResult<AdmitOutcome>;

    async fn claim_run(
        &self,
        executor: &str,
        worker: &str,
        now: DateTime<Utc>,
        lease_for: Duration,
    ) -> RuntimeResult<Option<Claim>>;

    /// Compare-and-set the claimed run and atomically admit any emitted events.
    async fn commit_run(&self, claim: Claim, emitted: Vec<PreparedEvent>) -> RuntimeResult<Claim>;

    async fn get_run(&self, id: &str) -> RuntimeResult<Option<RunRecord>>;

    async fn events_after(&self, after: u64, limit: usize) -> RuntimeResult<EventPage>;

    /// Remove journal envelopes through `cursor` while preserving dedupe tombstones.
    async fn prune_events_through(&self, cursor: u64) -> RuntimeResult<u64>;

    async fn request_cancel(&self, id: &str, now: DateTime<Utc>) -> RuntimeResult<bool>;

    async fn upsert_timer(&self, timer: TimerRecord) -> RuntimeResult<TimerRecord>;

    async fn cancel_timer(
        &self,
        id: &str,
        generation: Option<u64>,
        now: DateTime<Utc>,
    ) -> RuntimeResult<bool>;

    async fn claim_due_timer(
        &self,
        worker: &str,
        now: DateTime<Utc>,
        lease_for: Duration,
    ) -> RuntimeResult<Option<TimerClaim>>;

    /// Mark the claimed timer fired and atomically admit its event.
    async fn fire_timer(
        &self,
        claim: TimerClaim,
        fired: TimerRecord,
        event: PreparedEvent,
    ) -> RuntimeResult<AdmitOutcome>;

    async fn snapshot(&self) -> RuntimeResult<StoreSnapshot>;
}
