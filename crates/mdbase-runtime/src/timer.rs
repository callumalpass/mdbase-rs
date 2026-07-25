use chrono::{DateTime, Utc};
use mdbase::runtime_contracts::RuntimeRegistry;
use serde_json::{json, Value};

use crate::engine::{DeliveryOutcome, Runtime};
use crate::error::{RuntimeError, RuntimeResult};
use crate::model::{TimerRecord, TimerStatus};
use crate::planner::stable_id;
use crate::store::TimerReconcileOutcome;

#[derive(Debug, Clone, PartialEq)]
pub struct TimerRequest {
    pub id: String,
    pub fire_at: DateTime<Utc>,
    pub event_type: String,
    pub contract_version: u64,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimerReconcileRequest {
    pub id_prefix: String,
    pub timers: Vec<TimerRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerFireOutcome {
    Idle,
    Fired {
        timer_id: String,
        generation: u64,
        delivery: DeliveryOutcome,
    },
}

impl Runtime {
    pub async fn upsert_timer(&self, request: TimerRequest) -> RuntimeResult<TimerRecord> {
        let now = self.clock.now();
        self.store
            .upsert_timer(TimerRecord {
                id: request.id,
                generation: 0,
                status: TimerStatus::Scheduled,
                fire_at: request.fire_at,
                event_type: request.event_type,
                contract_version: request.contract_version,
                payload: request.payload,
                created_at: now,
                updated_at: now,
                fired_at: None,
            })
            .await
    }

    pub async fn cancel_timer(&self, id: &str, generation: Option<u64>) -> RuntimeResult<bool> {
        self.store
            .cancel_timer(id, generation, self.clock.now())
            .await
    }

    pub async fn reconcile_timers(
        &self,
        request: TimerReconcileRequest,
    ) -> RuntimeResult<TimerReconcileOutcome> {
        if request.id_prefix.is_empty() {
            return Err(RuntimeError::diagnostic(
                "invalid_timer_prefix",
                "Timer reconciliation requires a non-empty ID prefix.",
            ));
        }
        let mut ids = std::collections::BTreeSet::new();
        let now = self.clock.now();
        let mut desired = Vec::with_capacity(request.timers.len());
        for timer in request.timers {
            if !timer.id.starts_with(&request.id_prefix) {
                return Err(RuntimeError::diagnostic(
                    "timer_outside_reconciliation_prefix",
                    format!(
                        "Timer {} is outside reconciliation prefix {}.",
                        timer.id, request.id_prefix
                    ),
                ));
            }
            if !ids.insert(timer.id.clone()) {
                return Err(RuntimeError::diagnostic(
                    "duplicate_timer_id",
                    format!("Timer {} appears more than once.", timer.id),
                ));
            }
            desired.push(TimerRecord {
                id: timer.id,
                generation: 0,
                status: TimerStatus::Scheduled,
                fire_at: timer.fire_at,
                event_type: timer.event_type,
                contract_version: timer.contract_version,
                payload: timer.payload,
                created_at: now,
                updated_at: now,
                fired_at: None,
            });
        }
        self.store
            .reconcile_timers(&request.id_prefix, desired, now)
            .await
    }

    pub async fn timers(&self, id_prefix: &str) -> RuntimeResult<Vec<TimerRecord>> {
        if id_prefix.is_empty() {
            return Err(RuntimeError::diagnostic(
                "invalid_timer_prefix",
                "Timer listing requires a non-empty ID prefix.",
            ));
        }
        let mut timers = self.store.timers(id_prefix).await?;
        timers.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(timers)
    }

    pub async fn fire_due_timer(
        &self,
        registry: &RuntimeRegistry,
    ) -> RuntimeResult<TimerFireOutcome> {
        let now = self.clock.now();
        let Some(claim) = self
            .store
            .claim_due_timer(&self.config.worker_id, now, self.config.lease_duration)
            .await?
        else {
            return Ok(TimerFireOutcome::Idle);
        };
        let event_id = stable_id(
            "evt",
            &format!("timer:{}:{}", claim.timer.id, claim.timer.generation),
        );
        let late_by_ms = now
            .signed_duration_since(claim.timer.fire_at)
            .num_milliseconds()
            .max(0);
        let event = json!({
            "type": claim.timer.event_type,
            "contract_version": claim.timer.contract_version,
            "id": event_id,
            "occurred_at": claim.timer.fire_at.to_rfc3339(),
            "source": {
                "runtime": self.config.runtime_id,
                "provider": "mdbase.timer"
            },
            "payload": {
                "timer_id": claim.timer.id,
                "generation": claim.timer.generation,
                "scheduled_at": claim.timer.fire_at.to_rfc3339(),
                "fired_at": now.to_rfc3339(),
                "late_by_ms": late_by_ms,
                "data": claim.timer.payload
            },
            "trace": {
                "correlation_id": stable_id(
                    "corr",
                    &format!("timer:{}:{}", claim.timer.id, claim.timer.generation)
                )
            }
        });
        let prepared = self.prepare_event(registry, event, now)?;
        let mut fired = claim.timer.clone();
        fired.status = TimerStatus::Fired;
        fired.updated_at = now;
        fired.fired_at = Some(now);
        let timer_id = fired.id.clone();
        let generation = fired.generation;
        let delivery = self.store.fire_timer(claim, fired, prepared).await?;
        Ok(TimerFireOutcome::Fired {
            timer_id,
            generation,
            delivery: delivery.into(),
        })
    }
}

pub(crate) fn timer_matches(current: &TimerRecord, desired: &TimerRecord) -> bool {
    !matches!(current.status, TimerStatus::Cancelled)
        && current.fire_at == desired.fire_at
        && current.event_type == desired.event_type
        && current.contract_version == desired.contract_version
        && current.payload == desired.payload
}

pub(crate) fn next_timer_generation(
    current: Option<&TimerRecord>,
    mut desired: TimerRecord,
    now: DateTime<Utc>,
) -> RuntimeResult<TimerRecord> {
    desired.generation = current
        .map(|timer| timer.generation)
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| {
            RuntimeError::diagnostic(
                "timer_generation_exhausted",
                format!("Timer {} exhausted its generation counter.", desired.id),
            )
        })?;
    if let Some(current) = current {
        desired.created_at = current.created_at;
    }
    desired.status = TimerStatus::Scheduled;
    desired.updated_at = now;
    desired.fired_at = None;
    Ok(desired)
}
