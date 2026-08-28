use chrono::{DateTime, Utc};
use mdbase_interop::{ExactContractReference, ImplementationIdentity};
use serde_json::{json, Value};

use crate::admission::AdmissionCatalog;
use crate::engine::{DeliveryOutcome, Runtime};
use crate::error::{RuntimeError, RuntimeResult};
use crate::model::{TimerRecord, TimerStatus};
use crate::planner::stable_id;
use crate::store::TimerReconcileOutcome;

#[derive(Debug, Clone, PartialEq)]
pub struct TimerRequest {
    pub id: String,
    pub fire_at: DateTime<Utc>,
    pub contract: ExactContractReference,
    pub source: ImplementationIdentity,
    pub source_uri: String,
    pub subject: Option<String>,
    pub data: Value,
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
    /// Unconditionally schedule one timer as a new generation.
    ///
    /// Use [`Runtime::reconcile_timer_exact`] when an identical non-cancelled
    /// timer must retain its generation and lifecycle state.
    pub async fn upsert_timer(&self, request: TimerRequest) -> RuntimeResult<TimerRecord> {
        let now = self.clock.now();
        self.store
            .upsert_timer(TimerRecord {
                id: request.id,
                generation: 0,
                status: TimerStatus::Scheduled,
                fire_at: request.fire_at,
                event_contract: request.contract,
                event_source: request.source,
                source_uri: request.source_uri,
                subject: request.subject,
                data: request.data,
                created_at: now,
                updated_at: now,
                fired_at: None,
            })
            .await
    }

    /// Reconcile exactly one timer without cancelling prefix-related siblings.
    ///
    /// This has the desired-member semantics of [`Runtime::reconcile_timers`]
    /// without complete-set omission semantics: identical non-cancelled timers
    /// preserve their generation and lifecycle state, while missing, changed,
    /// or cancelled timers are scheduled at the next checked generation.
    pub async fn reconcile_timer_exact(&self, request: TimerRequest) -> RuntimeResult<TimerRecord> {
        let now = self.clock.now();
        self.store
            .reconcile_timer_exact(timer_record(request, now), now)
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
            desired.push(timer_record(timer, now));
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
        catalog: &AdmissionCatalog,
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
            "specversion": "1.0",
            "id": event_id,
            "source": claim.timer.source_uri,
            "type": claim.timer.event_contract.id,
            "time": claim.timer.fire_at.to_rfc3339(),
            "datacontenttype": "application/json",
            "dataschema": format!(
                "urn:mdbase:contract:{}:{}:{}",
                claim.timer.event_contract.id,
                claim.timer.event_contract.version,
                claim.timer.event_contract.digest,
            ),
            "data": {
                "timer_id": claim.timer.id,
                "generation": claim.timer.generation,
                "scheduled_for": claim.timer.fire_at.to_rfc3339(),
                "fired_at": now.to_rfc3339(),
                "late_by_ms": late_by_ms,
                "data": claim.timer.data
            },
            "mdbaseprofile": "0.1",
            "mdbasecontractversion": claim.timer.event_contract.version,
            "mdbasecontractdigest": claim.timer.event_contract.digest,
            "mdbaseapplication": claim.timer.event_source.application,
            "mdbaseimplementation": claim.timer.event_source.implementation,
            "mdbaseimplementationversion": claim.timer.event_source.version,
            "correlationid": stable_id(
                "corr",
                &format!("timer:{}:{}", claim.timer.id, claim.timer.generation)
            )
        });
        let mut event = event;
        if let Some(instance_id) = &claim.timer.event_source.instance_id {
            event["mdbaseinstanceid"] = Value::String(instance_id.clone());
        }
        if let Some(subject) = &claim.timer.subject {
            event["subject"] = Value::String(subject.clone());
        }
        let prepared = self.prepare_event(catalog, event, now)?;
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

fn timer_record(request: TimerRequest, now: DateTime<Utc>) -> TimerRecord {
    TimerRecord {
        id: request.id,
        generation: 0,
        status: TimerStatus::Scheduled,
        fire_at: request.fire_at,
        event_contract: request.contract,
        event_source: request.source,
        source_uri: request.source_uri,
        subject: request.subject,
        data: request.data,
        created_at: now,
        updated_at: now,
        fired_at: None,
    }
}

pub(crate) fn timer_matches(current: &TimerRecord, desired: &TimerRecord) -> bool {
    !matches!(current.status, TimerStatus::Cancelled)
        && current.fire_at == desired.fire_at
        && current.event_contract == desired.event_contract
        && current.event_source == desired.event_source
        && current.source_uri == desired.source_uri
        && current.subject == desired.subject
        && current.data == desired.data
}

/// Largest timer generation representable by every runtime store.
///
/// SQLite and PostgreSQL persist generations in signed 64-bit integer columns,
/// so the in-memory store deliberately uses the same ceiling.
pub const TIMER_GENERATION_MAX: u64 = i64::MAX as u64;

pub(crate) fn next_timer_generation_value(
    current: Option<&TimerRecord>,
    id: &str,
) -> RuntimeResult<u64> {
    let generation = current.map(|timer| timer.generation).unwrap_or(0);
    if generation >= TIMER_GENERATION_MAX {
        return Err(RuntimeError::diagnostic(
            "timer_generation_exhausted",
            format!("Timer {id} exhausted its generation counter."),
        ));
    }
    Ok(generation + 1)
}

pub(crate) fn next_timer_generation(
    current: Option<&TimerRecord>,
    mut desired: TimerRecord,
    now: DateTime<Utc>,
) -> RuntimeResult<TimerRecord> {
    desired.generation = next_timer_generation_value(current, &desired.id)?;
    if let Some(current) = current {
        desired.created_at = current.created_at;
    }
    desired.status = TimerStatus::Scheduled;
    desired.updated_at = now;
    desired.fired_at = None;
    Ok(desired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn exact_timer_generation_exhaustion_fails_closed() {
        let now = Utc.with_ymd_and_hms(2026, 8, 29, 0, 0, 0).single().unwrap();
        let current = TimerRecord {
            id: "exact".to_string(),
            generation: TIMER_GENERATION_MAX,
            status: TimerStatus::Cancelled,
            fire_at: now,
            event_contract: ExactContractReference {
                id: "timer.test".to_string(),
                version: "1.0.0".to_string(),
                digest: format!("sha256:{}", "0".repeat(64)),
            },
            event_source: ImplementationIdentity {
                application: "test".to_string(),
                implementation: "test".to_string(),
                version: "1.0.0".to_string(),
                instance_id: None,
            },
            source_uri: "urn:test".to_string(),
            subject: None,
            data: json!({}),
            created_at: now,
            updated_at: now,
            fired_at: None,
        };
        let mut desired = current.clone();
        desired.status = TimerStatus::Scheduled;

        let error = next_timer_generation(Some(&current), desired, now).unwrap_err();

        assert_eq!(error.code(), "timer_generation_exhausted");
        assert_eq!(current.generation, TIMER_GENERATION_MAX);
        assert_eq!(current.status, TimerStatus::Cancelled);
    }
}
