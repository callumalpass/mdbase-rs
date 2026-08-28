use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use ulid::Ulid;

use super::{add_duration, admit_tx, parse_timestamp, store_error, timestamp, SqliteRuntimeStore};
use crate::error::{RuntimeError, RuntimeResult};
use crate::model::{TimerRecord, TimerStatus};
use crate::store::{AdmitOutcome, PreparedEvent, TimerClaim, TimerReconcileOutcome};
use crate::timer::{next_timer_generation, next_timer_generation_value, timer_matches};

impl SqliteRuntimeStore {
    pub(super) async fn upsert_timer_impl(
        &self,
        mut timer: TimerRecord,
    ) -> RuntimeResult<TimerRecord> {
        self.execute(move |connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let current = transaction
                .query_row(
                    "SELECT record_json FROM runtime_timers WHERE id = ?1",
                    params![timer.id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(store_error)?
                .map(|json| serde_json::from_str::<TimerRecord>(&json))
                .transpose()?;
            timer.generation = next_timer_generation_value(current.as_ref(), &timer.id)?;
            let mutation_at = sqlite_mutation_time(&transaction)?;
            timer.status = TimerStatus::Scheduled;
            timer.created_at = mutation_at;
            timer.updated_at = mutation_at;
            transaction
                .execute(
                    "INSERT INTO runtime_timers
                    (id, generation, status, fire_at, record_json)
                 VALUES (?1, ?2, 'scheduled', ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET
                    generation = excluded.generation,
                    status = 'scheduled',
                    fire_at = excluded.fire_at,
                    lease_worker = NULL,
                    lease_token = NULL,
                    lease_expires_at = NULL,
                    record_json = excluded.record_json",
                    params![
                        timer.id,
                        timer.generation,
                        timestamp(timer.fire_at),
                        serde_json::to_string(&timer)?
                    ],
                )
                .map_err(store_error)?;
            transaction.commit().map_err(store_error)?;
            Ok(timer)
        })
        .await
    }

    pub(super) async fn reconcile_timer_exact_impl(
        &self,
        desired: TimerRecord,
        _now: DateTime<Utc>,
    ) -> RuntimeResult<TimerRecord> {
        self.execute(move |connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let current = transaction
                .query_row(
                    "SELECT record_json FROM runtime_timers WHERE id = ?1",
                    params![desired.id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(store_error)?
                .map(|json| serde_json::from_str::<TimerRecord>(&json))
                .transpose()?;
            if let Some(current) = current.as_ref() {
                if timer_matches(current, &desired) {
                    transaction.commit().map_err(store_error)?;
                    return Ok(current.clone());
                }
            }
            let mutation_at = sqlite_mutation_time(&transaction)?;
            let mut desired = desired;
            desired.created_at = mutation_at;
            let next = next_timer_generation(current.as_ref(), desired, mutation_at)?;
            transaction
                .execute(
                    "INSERT INTO runtime_timers
                        (id, generation, status, fire_at, record_json)
                     VALUES (?1, ?2, 'scheduled', ?3, ?4)
                     ON CONFLICT(id) DO UPDATE SET
                        generation = excluded.generation,
                        status = 'scheduled',
                        fire_at = excluded.fire_at,
                        lease_worker = NULL,
                        lease_token = NULL,
                        lease_expires_at = NULL,
                        record_json = excluded.record_json",
                    params![
                        next.id,
                        next.generation,
                        timestamp(next.fire_at),
                        serde_json::to_string(&next)?
                    ],
                )
                .map_err(store_error)?;
            transaction.commit().map_err(store_error)?;
            Ok(next)
        })
        .await
    }

    pub(super) async fn cancel_timer_impl(
        &self,
        id: &str,
        generation: Option<u64>,
        now: DateTime<Utc>,
    ) -> RuntimeResult<bool> {
        let id = id.to_string();
        self.execute(move |connection| {
            let transaction = connection.transaction().map_err(store_error)?;
            let json = transaction
                .query_row(
                    "SELECT record_json FROM runtime_timers WHERE id = ?1",
                    params![id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(store_error)?;
            let Some(json) = json else {
                return Ok(false);
            };
            let mut timer: TimerRecord = serde_json::from_str(&json)?;
            if generation.is_some_and(|expected| expected != timer.generation)
                || !matches!(timer.status, TimerStatus::Scheduled | TimerStatus::Firing)
            {
                return Ok(false);
            }
            timer.status = TimerStatus::Cancelled;
            timer.updated_at = now;
            transaction
                .execute(
                    "UPDATE runtime_timers
                 SET status = 'cancelled', lease_worker = NULL, lease_token = NULL,
                     lease_expires_at = NULL, record_json = ?1
                 WHERE id = ?2",
                    params![serde_json::to_string(&timer)?, id],
                )
                .map_err(store_error)?;
            transaction.commit().map_err(store_error)?;
            Ok(true)
        })
        .await
    }

    pub(super) async fn reconcile_timers_impl(
        &self,
        id_prefix: &str,
        desired: Vec<TimerRecord>,
        _now: DateTime<Utc>,
    ) -> RuntimeResult<TimerReconcileOutcome> {
        let id_prefix = id_prefix.to_string();
        self.execute(move |connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store_error)?;
            let existing = {
                let mut statement = transaction
                    .prepare(
                        "SELECT record_json FROM runtime_timers
                     WHERE substr(id, 1, length(?1)) = ?1
                     ORDER BY id",
                    )
                    .map_err(store_error)?;
                let records = statement
                    .query_map(params![id_prefix], |row| row.get::<_, String>(0))
                    .map_err(store_error)?
                    .map(|value| {
                        let value = value.map_err(store_error)?;
                        let timer = serde_json::from_str::<TimerRecord>(&value)?;
                        Ok((timer.id.clone(), timer))
                    })
                    .collect::<RuntimeResult<std::collections::BTreeMap<_, _>>>()?;
                records
            };
            let desired_ids = desired
                .iter()
                .map(|timer| timer.id.clone())
                .collect::<std::collections::BTreeSet<_>>();
            let mutation_at = sqlite_mutation_time(&transaction)?;
            let mut planned = Vec::with_capacity(desired.len());
            for mut desired in desired {
                desired.created_at = mutation_at;
                let current = existing.get(&desired.id);
                if current.is_some_and(|current| timer_matches(current, &desired)) {
                    planned.push((current.expect("checked above").clone(), false));
                } else {
                    planned.push((next_timer_generation(current, desired, mutation_at)?, true));
                }
            }
            let mut cancelled_ids = Vec::new();
            for timer in existing.values().filter(|timer| {
                timer.id.starts_with(&id_prefix)
                    && !desired_ids.contains(&timer.id)
                    && matches!(timer.status, TimerStatus::Scheduled | TimerStatus::Firing)
            }) {
                let mut cancelled = timer.clone();
                cancelled.status = TimerStatus::Cancelled;
                cancelled.updated_at = mutation_at;
                transaction
                    .execute(
                        "UPDATE runtime_timers
                     SET status = 'cancelled', lease_worker = NULL, lease_token = NULL,
                         lease_expires_at = NULL, record_json = ?1
                     WHERE id = ?2",
                        params![serde_json::to_string(&cancelled)?, cancelled.id],
                    )
                    .map_err(store_error)?;
                cancelled_ids.push(timer.id.clone());
            }
            let mut timers = Vec::with_capacity(planned.len());
            for (next, changed) in planned {
                if !changed {
                    timers.push(next);
                    continue;
                }
                transaction
                    .execute(
                        "INSERT INTO runtime_timers
                        (id, generation, status, fire_at, record_json)
                     VALUES (?1, ?2, 'scheduled', ?3, ?4)
                     ON CONFLICT(id) DO UPDATE SET
                        generation = excluded.generation,
                        status = 'scheduled',
                        fire_at = excluded.fire_at,
                        lease_worker = NULL,
                        lease_token = NULL,
                        lease_expires_at = NULL,
                        record_json = excluded.record_json",
                        params![
                            next.id,
                            next.generation,
                            timestamp(next.fire_at),
                            serde_json::to_string(&next)?
                        ],
                    )
                    .map_err(store_error)?;
                timers.push(next);
            }
            transaction.commit().map_err(store_error)?;
            timers.sort_by(|left, right| left.id.cmp(&right.id));
            cancelled_ids.sort();
            Ok(TimerReconcileOutcome {
                timers,
                cancelled_ids,
            })
        })
        .await
    }

    pub(super) async fn timers_impl(&self, id_prefix: &str) -> RuntimeResult<Vec<TimerRecord>> {
        let id_prefix = id_prefix.to_string();
        self.execute(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT record_json FROM runtime_timers
                     WHERE substr(id, 1, length(?1)) = ?1
                     ORDER BY id",
                )
                .map_err(store_error)?;
            let timers = statement
                .query_map(params![id_prefix], |row| row.get::<_, String>(0))
                .map_err(store_error)?
                .map(|value| {
                    let value = value.map_err(store_error)?;
                    serde_json::from_str::<TimerRecord>(&value).map_err(Into::into)
                })
                .collect();
            timers
        })
        .await
    }

    pub(super) async fn claim_due_timer_impl(
        &self,
        worker: &str,
        now: DateTime<Utc>,
        lease_for: Duration,
    ) -> RuntimeResult<Option<TimerClaim>> {
        let worker = worker.to_string();
        self.execute(move |connection| {
            let transaction = connection.transaction().map_err(store_error)?;
            transaction
                .execute(
                    "UPDATE runtime_timers
                 SET lease_worker = NULL, lease_token = NULL, lease_expires_at = NULL
                 WHERE lease_expires_at <= ?1",
                    params![timestamp(now)],
                )
                .map_err(store_error)?;
            let json = transaction
                .query_row(
                    "SELECT record_json FROM runtime_timers
                 WHERE status IN ('scheduled', 'firing')
                   AND fire_at <= ?1
                   AND lease_token IS NULL
                 ORDER BY fire_at, id LIMIT 1",
                    params![timestamp(now)],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(store_error)?;
            let Some(json) = json else {
                transaction.commit().map_err(store_error)?;
                return Ok(None);
            };
            let mut timer: TimerRecord = serde_json::from_str(&json)?;
            timer.status = TimerStatus::Firing;
            timer.updated_at = now;
            let token = format!("timer_lease_{}", Ulid::new());
            let expires_at = add_duration(now, lease_for)?;
            let changed = transaction
                .execute(
                    "UPDATE runtime_timers
                 SET status = 'firing', lease_worker = ?1, lease_token = ?2,
                     lease_expires_at = ?3, record_json = ?4
                 WHERE id = ?5 AND generation = ?6 AND lease_token IS NULL",
                    params![
                        worker,
                        token,
                        timestamp(expires_at),
                        serde_json::to_string(&timer)?,
                        timer.id,
                        timer.generation
                    ],
                )
                .map_err(store_error)?;
            if changed != 1 {
                return Ok(None);
            }
            transaction.commit().map_err(store_error)?;
            Ok(Some(TimerClaim {
                timer,
                worker,
                token,
                expires_at,
            }))
        })
        .await
    }

    pub(super) async fn fire_timer_impl(
        &self,
        claim: TimerClaim,
        fired: TimerRecord,
        event: PreparedEvent,
    ) -> RuntimeResult<AdmitOutcome> {
        self.execute(move |connection| {
            let transaction = connection.transaction().map_err(store_error)?;
            let changed = transaction
                .execute(
                    "UPDATE runtime_timers
                 SET status = 'fired', lease_worker = NULL, lease_token = NULL,
                     lease_expires_at = NULL, record_json = ?1
                 WHERE id = ?2 AND generation = ?3 AND lease_worker = ?4 AND lease_token = ?5",
                    params![
                        serde_json::to_string(&fired)?,
                        claim.timer.id,
                        claim.timer.generation,
                        claim.worker,
                        claim.token
                    ],
                )
                .map_err(store_error)?;
            if changed != 1 {
                return Err(RuntimeError::diagnostic(
                    "stale_timer_generation",
                    "Timer generation or lease changed before firing.",
                ));
            }
            let outcome = admit_tx(&transaction, event)?;
            transaction.commit().map_err(store_error)?;
            Ok(outcome)
        })
        .await
    }
}

fn sqlite_mutation_time(transaction: &Transaction<'_>) -> RuntimeResult<DateTime<Utc>> {
    let value = transaction
        .query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |row| {
            row.get::<_, String>(0)
        })
        .map_err(store_error)?;
    parse_timestamp(&value)
}
