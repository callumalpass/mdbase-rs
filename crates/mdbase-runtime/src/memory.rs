use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use ulid::Ulid;

use crate::error::{RuntimeError, RuntimeResult};
use crate::model::{
    ConcurrencyPolicy, EventJournalEntry, RunRecord, RunStatus, TimerRecord, TimerStatus,
};
use crate::store::{
    AdmitOutcome, Claim, EventPage, PreparedEvent, RuntimeStore, StoreSnapshot, TimerClaim,
    TimerReconcileOutcome,
};
use crate::timer::{next_timer_generation, next_timer_generation_value, timer_matches};

#[derive(Debug, Clone)]
struct Lease {
    worker: String,
    token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
struct MemoryState {
    next_cursor: u64,
    retained_after: u64,
    events: Vec<EventJournalEntry>,
    event_index: BTreeMap<(String, String), u64>,
    runs: BTreeMap<String, RunRecord>,
    run_leases: BTreeMap<String, Lease>,
    timers: BTreeMap<String, TimerRecord>,
    timer_leases: BTreeMap<String, Lease>,
}

#[derive(Debug, Default)]
pub struct InMemoryRuntimeStore {
    state: Mutex<MemoryState>,
}

impl InMemoryRuntimeStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RuntimeStore for InMemoryRuntimeStore {
    async fn admit_event(&self, event: PreparedEvent) -> RuntimeResult<AdmitOutcome> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;
        Ok(admit(&mut state, event))
    }

    async fn claim_run(
        &self,
        executor: &str,
        worker: &str,
        now: DateTime<Utc>,
        lease_for: Duration,
    ) -> RuntimeResult<Option<Claim>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;

        state.run_leases.retain(|_, lease| lease.expires_at > now);
        let candidate = state
            .runs
            .values()
            .filter(|run| {
                run.plan.executor == executor
                    && !run.status.terminal()
                    && run.status != RunStatus::Waiting
                    && run.plan.not_before <= now
                    && !state.run_leases.contains_key(&run.plan.id)
                    && group_is_runnable(&state, run)
            })
            .min_by(|left, right| {
                (&left.created_at, &left.plan.id).cmp(&(&right.created_at, &right.plan.id))
            })
            .map(|run| run.plan.id.clone());

        let Some(id) = candidate else {
            return Ok(None);
        };
        let token = format!("lease_{}", Ulid::new());
        let expires_at = add_std_duration(now, lease_for)?;
        let run = state
            .runs
            .get_mut(&id)
            .expect("selected run must remain present");
        if run.status == RunStatus::Queued {
            run.status = RunStatus::Running;
            run.started_at.get_or_insert(now);
            run.updated_at = now;
        }
        let run = run.clone();
        state.run_leases.insert(
            id,
            Lease {
                worker: worker.to_string(),
                token: token.clone(),
                expires_at,
            },
        );
        Ok(Some(Claim {
            run,
            worker: worker.to_string(),
            token,
            expires_at,
        }))
    }

    async fn commit_run(
        &self,
        mut claim: Claim,
        emitted: Vec<PreparedEvent>,
    ) -> RuntimeResult<Claim> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;
        validate_claim(&state, &claim)?;
        let stored = state
            .runs
            .get(&claim.run.plan.id)
            .expect("validated claim run must exist");
        if stored.revision != claim.run.revision {
            return Err(RuntimeError::diagnostic(
                "stale_lease",
                "The run changed after this worker claimed it.",
            ));
        }
        claim.run.revision += 1;
        state
            .runs
            .insert(claim.run.plan.id.clone(), claim.run.clone());
        for event in emitted {
            admit(&mut state, event);
        }
        if claim.run.status.terminal() {
            state.run_leases.remove(&claim.run.plan.id);
        }
        Ok(claim)
    }

    async fn get_run(&self, id: &str) -> RuntimeResult<Option<RunRecord>> {
        let state = self
            .state
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;
        Ok(state.runs.get(id).cloned())
    }

    async fn events_after(&self, after: u64, limit: usize) -> RuntimeResult<EventPage> {
        let state = self
            .state
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;
        let limit = limit.clamp(1, 10_000);
        let reset_required = after < state.retained_after;
        let mut events = if reset_required {
            Vec::new()
        } else {
            state
                .events
                .iter()
                .filter(|event| event.cursor > after)
                .take(limit + 1)
                .cloned()
                .collect::<Vec<_>>()
        };
        let has_more = events.len() > limit;
        events.truncate(limit);
        Ok(EventPage {
            events,
            retained_after: state.retained_after,
            head: state.next_cursor,
            has_more,
            reset_required,
        })
    }

    async fn prune_events_through(&self, cursor: u64) -> RuntimeResult<u64> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;
        let pruned_through = cursor.min(state.next_cursor).max(state.retained_after);
        state.events.retain(|event| event.cursor > pruned_through);
        state.retained_after = pruned_through;
        Ok(pruned_through)
    }

    async fn request_cancel(&self, id: &str, now: DateTime<Utc>) -> RuntimeResult<bool> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;
        let Some(run) = state.runs.get_mut(id) else {
            return Ok(false);
        };
        if !run.request_cancel(now) {
            return Ok(false);
        }
        run.revision += 1;
        if run.status.terminal() {
            state.run_leases.remove(id);
        }
        Ok(true)
    }

    async fn upsert_timer(&self, mut timer: TimerRecord) -> RuntimeResult<TimerRecord> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;
        timer.generation = next_timer_generation_value(state.timers.get(&timer.id), &timer.id)?;
        timer.status = TimerStatus::Scheduled;
        state.timer_leases.remove(&timer.id);
        state.timers.insert(timer.id.clone(), timer.clone());
        Ok(timer)
    }

    async fn reconcile_timer_exact(
        &self,
        desired: TimerRecord,
        now: DateTime<Utc>,
    ) -> RuntimeResult<TimerRecord> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;
        if let Some(current) = state.timers.get(&desired.id) {
            if timer_matches(current, &desired) {
                return Ok(current.clone());
            }
        }
        let next = next_timer_generation(state.timers.get(&desired.id), desired, now)?;
        state.timer_leases.remove(&next.id);
        state.timers.insert(next.id.clone(), next.clone());
        Ok(next)
    }

    async fn cancel_timer(
        &self,
        id: &str,
        generation: Option<u64>,
        now: DateTime<Utc>,
    ) -> RuntimeResult<bool> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;
        let Some(timer) = state.timers.get_mut(id) else {
            return Ok(false);
        };
        if generation.is_some_and(|expected| expected != timer.generation)
            || !matches!(timer.status, TimerStatus::Scheduled | TimerStatus::Firing)
        {
            return Ok(false);
        }
        timer.status = TimerStatus::Cancelled;
        timer.updated_at = now;
        state.timer_leases.remove(id);
        Ok(true)
    }

    async fn reconcile_timers(
        &self,
        id_prefix: &str,
        desired: Vec<TimerRecord>,
        now: DateTime<Utc>,
    ) -> RuntimeResult<TimerReconcileOutcome> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;
        let desired_ids = desired
            .iter()
            .map(|timer| timer.id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        // Build the complete desired result before changing omissions. This is
        // what gives the non-transactional memory store SQL-style rollback when
        // any member has exhausted its portable generation counter.
        let mut planned = Vec::with_capacity(desired.len());
        for desired in desired {
            let current = state.timers.get(&desired.id);
            if current.is_some_and(|current| timer_matches(current, &desired)) {
                planned.push((current.expect("checked above").clone(), false));
            } else {
                planned.push((next_timer_generation(current, desired, now)?, true));
            }
        }

        let mut cancelled_ids = Vec::new();
        let cancel = state
            .timers
            .values()
            .filter(|timer| {
                timer.id.starts_with(id_prefix)
                    && !desired_ids.contains(&timer.id)
                    && matches!(timer.status, TimerStatus::Scheduled | TimerStatus::Firing)
            })
            .map(|timer| timer.id.clone())
            .collect::<Vec<_>>();
        for id in cancel {
            if let Some(timer) = state.timers.get_mut(&id) {
                timer.status = TimerStatus::Cancelled;
                timer.updated_at = now;
            }
            state.timer_leases.remove(&id);
            cancelled_ids.push(id);
        }
        let mut timers = Vec::with_capacity(planned.len());
        for (next, changed) in planned {
            if changed {
                state.timer_leases.remove(&next.id);
                state.timers.insert(next.id.clone(), next.clone());
            }
            timers.push(next);
        }
        timers.sort_by(|left, right| left.id.cmp(&right.id));
        cancelled_ids.sort();
        Ok(TimerReconcileOutcome {
            timers,
            cancelled_ids,
        })
    }

    async fn timers(&self, id_prefix: &str) -> RuntimeResult<Vec<TimerRecord>> {
        let state = self
            .state
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;
        Ok(state
            .timers
            .values()
            .filter(|timer| timer.id.starts_with(id_prefix))
            .cloned()
            .collect())
    }

    async fn claim_due_timer(
        &self,
        worker: &str,
        now: DateTime<Utc>,
        lease_for: Duration,
    ) -> RuntimeResult<Option<TimerClaim>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;
        state.timer_leases.retain(|_, lease| lease.expires_at > now);
        let candidate = state
            .timers
            .values()
            .filter(|timer| {
                matches!(timer.status, TimerStatus::Scheduled | TimerStatus::Firing)
                    && timer.fire_at <= now
                    && !state.timer_leases.contains_key(&timer.id)
            })
            .min_by(|left, right| (&left.fire_at, &left.id).cmp(&(&right.fire_at, &right.id)))
            .map(|timer| timer.id.clone());
        let Some(id) = candidate else {
            return Ok(None);
        };
        let token = format!("timer_lease_{}", Ulid::new());
        let expires_at = add_std_duration(now, lease_for)?;
        let timer = state
            .timers
            .get_mut(&id)
            .expect("selected timer must remain present");
        timer.status = TimerStatus::Firing;
        timer.updated_at = now;
        let timer = timer.clone();
        state.timer_leases.insert(
            id,
            Lease {
                worker: worker.to_string(),
                token: token.clone(),
                expires_at,
            },
        );
        Ok(Some(TimerClaim {
            timer,
            worker: worker.to_string(),
            token,
            expires_at,
        }))
    }

    async fn fire_timer(
        &self,
        claim: TimerClaim,
        fired: TimerRecord,
        event: PreparedEvent,
    ) -> RuntimeResult<AdmitOutcome> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;
        let lease = state
            .timer_leases
            .get(&claim.timer.id)
            .ok_or_else(|| RuntimeError::diagnostic("stale_lease", "Timer lease is absent."))?;
        if lease.token != claim.token
            || lease.worker != claim.worker
            || state
                .timers
                .get(&claim.timer.id)
                .is_none_or(|timer| timer.generation != claim.timer.generation)
        {
            return Err(RuntimeError::diagnostic(
                "stale_timer_generation",
                "Timer generation or lease changed before firing.",
            ));
        }
        state.timers.insert(fired.id.clone(), fired);
        state.timer_leases.remove(&claim.timer.id);
        let outcome = admit(&mut state, event);
        Ok(outcome)
    }

    async fn snapshot(&self) -> RuntimeResult<StoreSnapshot> {
        let state = self
            .state
            .lock()
            .map_err(|_| RuntimeError::Store("in-memory store lock poisoned".to_string()))?;
        Ok(StoreSnapshot {
            events: state.events.clone(),
            runs: state.runs.values().cloned().collect(),
            timers: state.timers.values().cloned().collect(),
        })
    }
}

fn admit(state: &mut MemoryState, event: PreparedEvent) -> AdmitOutcome {
    let key = (event.source_runtime.clone(), event.event_id.clone());
    if let Some(cursor) = state.event_index.get(&key) {
        return AdmitOutcome {
            cursor: *cursor,
            duplicate: true,
            admitted_run_ids: Vec::new(),
            skipped_run_ids: Vec::new(),
            cancellation_requested_run_ids: Vec::new(),
        };
    }

    state.next_cursor += 1;
    let cursor = state.next_cursor;
    state.events.push(EventJournalEntry {
        cursor,
        source_runtime: event.source_runtime.clone(),
        event_id: event.event_id.clone(),
        envelope: event.envelope,
        received_at: event.received_at,
    });
    state.event_index.insert(key, cursor);

    let mut admitted = Vec::new();
    let mut skipped = Vec::new();
    let mut cancellation_requested = Vec::new();
    for mut plan in event.runs {
        plan.event_cursor = cursor;
        if minimum_interval_suppresses(state, &plan) {
            skipped.push(plan.id);
            continue;
        }
        if plan.not_before > event.received_at {
            let obsolete = state
                .runs
                .values()
                .filter(|run| {
                    run.status == RunStatus::Queued
                        && run.plan.workflow == plan.workflow
                        && run.plan.trigger == plan.trigger
                        && run.plan.executor == plan.executor
                        && run.plan.not_before > event.received_at
                })
                .map(|run| run.plan.id.clone())
                .collect::<Vec<_>>();
            for id in obsolete {
                state.runs.remove(&id);
                state.run_leases.remove(&id);
            }
        }
        if state.runs.values().any(|run| {
            run.plan.idempotency_scope == plan.idempotency_scope
                && run.plan.idempotency_key == plan.idempotency_key
        }) {
            skipped.push(plan.id);
            continue;
        }

        let active = active_group_runs(state, &plan.concurrency_group);
        if plan.concurrency_policy == ConcurrencyPolicy::Skip && !active.is_empty() {
            skipped.push(plan.id);
            continue;
        }
        if plan.concurrency_policy == ConcurrencyPolicy::Replace {
            plan.replacement_blockers = active.clone();
            for id in active {
                if let Some(run) = state.runs.get_mut(&id) {
                    if run.request_cancel(event.received_at) {
                        run.revision += 1;
                        cancellation_requested.push(id.clone());
                    }
                    if run.status.terminal() {
                        state.run_leases.remove(&id);
                    }
                }
            }
        }
        admitted.push(plan.id.clone());
        state
            .runs
            .insert(plan.id.clone(), RunRecord::admitted(plan));
    }

    cancellation_requested.sort();
    cancellation_requested.dedup();
    AdmitOutcome {
        cursor,
        duplicate: false,
        admitted_run_ids: admitted,
        skipped_run_ids: skipped,
        cancellation_requested_run_ids: cancellation_requested,
    }
}

fn minimum_interval_suppresses(state: &MemoryState, plan: &crate::model::PlannedRun) -> bool {
    let Some(interval) = plan.minimum_interval_ms else {
        return false;
    };
    state.runs.values().any(|run| {
        run.plan.workflow == plan.workflow
            && run.plan.trigger == plan.trigger
            && run.plan.created_at
                + TimeDelta::milliseconds(i64::try_from(interval).unwrap_or(i64::MAX))
                > plan.created_at
    })
}

fn active_group_runs(state: &MemoryState, group: &str) -> Vec<String> {
    state
        .runs
        .values()
        .filter(|run| {
            run.plan.concurrency_group == group && run.status.occupies_concurrency_group()
        })
        .map(|run| run.plan.id.clone())
        .collect()
}

fn group_is_runnable(state: &MemoryState, run: &RunRecord) -> bool {
    if run.plan.concurrency_policy == ConcurrencyPolicy::Allow {
        return true;
    }
    let blocked_by_group = state.runs.values().any(|other| {
        other.plan.id != run.plan.id
            && other.plan.concurrency_group == run.plan.concurrency_group
            && (matches!(other.status, RunStatus::Running | RunStatus::Waiting)
                || (other.status == RunStatus::Queued
                    && (&other.plan.event_cursor, &other.plan.id)
                        < (&run.plan.event_cursor, &run.plan.id)))
    });
    let blocked_by_indeterminate_replacement = run.plan.replacement_blockers.iter().any(|id| {
        state
            .runs
            .get(id)
            .is_some_and(|blocker| blocker.status == RunStatus::Indeterminate)
    });
    !blocked_by_group && !blocked_by_indeterminate_replacement
}

fn validate_claim(state: &MemoryState, claim: &Claim) -> RuntimeResult<()> {
    let lease = state
        .run_leases
        .get(&claim.run.plan.id)
        .ok_or_else(|| RuntimeError::diagnostic("stale_lease", "Run lease is absent."))?;
    if lease.worker != claim.worker || lease.token != claim.token {
        return Err(RuntimeError::diagnostic(
            "stale_lease",
            "Run lease belongs to another worker.",
        ));
    }
    Ok(())
}

fn add_std_duration(now: DateTime<Utc>, duration: Duration) -> RuntimeResult<DateTime<Utc>> {
    let delta = TimeDelta::from_std(duration)
        .map_err(|_| RuntimeError::Clock("lease duration is too large".to_string()))?;
    Ok(now + delta)
}

#[cfg(test)]
mod timer_generation_tests {
    use super::*;
    use crate::timer::TIMER_GENERATION_MAX;
    use mdbase_interop::{ExactContractReference, ImplementationIdentity};
    use serde_json::json;

    fn timer(id: &str, now: DateTime<Utc>) -> TimerRecord {
        TimerRecord {
            id: id.to_string(),
            generation: 0,
            status: TimerStatus::Scheduled,
            fire_at: now,
            event_contract: ExactContractReference {
                id: "timer.test".to_string(),
                version: "1.0.0".to_string(),
                digest: format!("sha256:{}", "0".repeat(64)),
            },
            event_source: ImplementationIdentity {
                application: "test".to_string(),
                implementation: "memory".to_string(),
                version: "1.0.0".to_string(),
                instance_id: None,
            },
            source_uri: "urn:test".to_string(),
            subject: None,
            data: json!({}),
            created_at: now,
            updated_at: now,
            fired_at: None,
        }
    }

    #[tokio::test]
    async fn generation_exhaustion_rolls_back_every_scheduling_path() {
        let store = InMemoryRuntimeStore::new();
        let now = Utc::now();
        let mut exhausted = timer("max:timer", now);
        exhausted.generation = TIMER_GENERATION_MAX;
        let omitted = timer("max:omitted", now);
        {
            let mut state = store.state.lock().unwrap();
            state.timers.insert(exhausted.id.clone(), exhausted.clone());
            state.timers.insert(omitted.id.clone(), omitted);
        }
        let before = store.snapshot().await.unwrap().timers;
        let mut changed = exhausted;
        changed.data = json!({"changed": true});

        for error in [
            store.upsert_timer(changed.clone()).await.unwrap_err(),
            store
                .reconcile_timer_exact(changed.clone(), now)
                .await
                .unwrap_err(),
            store
                .reconcile_timers("max:", vec![changed], now)
                .await
                .unwrap_err(),
        ] {
            assert_eq!(error.code(), "timer_generation_exhausted");
            assert_eq!(store.snapshot().await.unwrap().timers, before);
        }
    }
}
