use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Row, Transaction};
use ulid::Ulid;

use super::{add_duration, admit_tx, as_i64, store_error, PostgresRuntimeStore};
use crate::error::{RuntimeError, RuntimeResult};
use crate::model::{TimerRecord, TimerStatus};
use crate::store::{AdmitOutcome, PreparedEvent, TimerClaim, TimerReconcileOutcome};
use crate::timer::{next_timer_generation, next_timer_generation_value, timer_matches};

impl PostgresRuntimeStore {
    pub(super) async fn upsert_timer_impl(
        &self,
        mut timer: TimerRecord,
    ) -> RuntimeResult<TimerRecord> {
        let mut transaction = self.pool.begin().await.map_err(store_error)?;
        lock_timer_namespace(&mut transaction, &self.namespace).await?;
        let current = sqlx::query(
            "SELECT record_json FROM mdbase_runtime_timers
             WHERE namespace = $1 AND id = $2 FOR UPDATE",
        )
        .bind(&self.namespace)
        .bind(&timer.id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(store_error)?
        .map(|row| {
            let value: Value = row.try_get("record_json").map_err(store_error)?;
            serde_json::from_value::<TimerRecord>(value).map_err(RuntimeError::from)
        })
        .transpose()?;
        timer.generation = next_timer_generation_value(current.as_ref(), &timer.id)?;
        let mutation_at = postgres_mutation_time(&mut transaction).await?;
        timer.status = TimerStatus::Scheduled;
        timer.created_at = mutation_at;
        timer.updated_at = mutation_at;
        sqlx::query(
            "INSERT INTO mdbase_runtime_timers
                (namespace, id, generation, status, fire_at, record_json)
             VALUES ($1, $2, $3, 'scheduled', $4, $5)
             ON CONFLICT(namespace, id) DO UPDATE SET
                generation = excluded.generation, status = 'scheduled',
                fire_at = excluded.fire_at, lease_worker = NULL, lease_token = NULL,
                lease_expires_at = NULL, record_json = excluded.record_json",
        )
        .bind(&self.namespace)
        .bind(&timer.id)
        .bind(as_i64(timer.generation)?)
        .bind(timer.fire_at)
        .bind(serde_json::to_value(&timer)?)
        .execute(&mut *transaction)
        .await
        .map_err(store_error)?;
        transaction.commit().await.map_err(store_error)?;
        Ok(timer)
    }

    pub(super) async fn reconcile_timer_exact_impl(
        &self,
        desired: TimerRecord,
        _now: DateTime<Utc>,
    ) -> RuntimeResult<TimerRecord> {
        let mut transaction = self.pool.begin().await.map_err(store_error)?;
        lock_timer_namespace(&mut transaction, &self.namespace).await?;
        let current = sqlx::query(
            "SELECT record_json FROM mdbase_runtime_timers
             WHERE namespace = $1 AND id = $2 FOR UPDATE",
        )
        .bind(&self.namespace)
        .bind(&desired.id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(store_error)?
        .map(|row| {
            let value: Value = row.try_get("record_json").map_err(store_error)?;
            serde_json::from_value::<TimerRecord>(value).map_err(RuntimeError::from)
        })
        .transpose()?;
        if let Some(current) = current.as_ref() {
            if timer_matches(current, &desired) {
                transaction.commit().await.map_err(store_error)?;
                return Ok(current.clone());
            }
        }
        let mutation_at = postgres_mutation_time(&mut transaction).await?;
        let mut desired = desired;
        desired.created_at = mutation_at;
        let next = next_timer_generation(current.as_ref(), desired, mutation_at)?;
        sqlx::query(
            "INSERT INTO mdbase_runtime_timers
                (namespace, id, generation, status, fire_at, record_json)
             VALUES ($1, $2, $3, 'scheduled', $4, $5)
             ON CONFLICT(namespace, id) DO UPDATE SET
                generation = excluded.generation, status = 'scheduled',
                fire_at = excluded.fire_at, lease_worker = NULL, lease_token = NULL,
                lease_expires_at = NULL, record_json = excluded.record_json",
        )
        .bind(&self.namespace)
        .bind(&next.id)
        .bind(as_i64(next.generation)?)
        .bind(next.fire_at)
        .bind(serde_json::to_value(&next)?)
        .execute(&mut *transaction)
        .await
        .map_err(store_error)?;
        transaction.commit().await.map_err(store_error)?;
        Ok(next)
    }

    pub(super) async fn cancel_timer_impl(
        &self,
        id: &str,
        generation: Option<u64>,
        now: DateTime<Utc>,
    ) -> RuntimeResult<bool> {
        let mut transaction = self.pool.begin().await.map_err(store_error)?;
        let row = sqlx::query(
            "SELECT record_json FROM mdbase_runtime_timers
             WHERE namespace = $1 AND id = $2 FOR UPDATE",
        )
        .bind(&self.namespace)
        .bind(id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(store_error)?;
        let Some(row) = row else {
            transaction.commit().await.map_err(store_error)?;
            return Ok(false);
        };
        let mut timer: TimerRecord =
            serde_json::from_value(row.try_get("record_json").map_err(store_error)?)?;
        if generation.is_some_and(|expected| expected != timer.generation)
            || !matches!(timer.status, TimerStatus::Scheduled | TimerStatus::Firing)
        {
            transaction.commit().await.map_err(store_error)?;
            return Ok(false);
        }
        timer.status = TimerStatus::Cancelled;
        timer.updated_at = now;
        sqlx::query(
            "UPDATE mdbase_runtime_timers
             SET status = 'cancelled', lease_worker = NULL, lease_token = NULL,
                 lease_expires_at = NULL, record_json = $3
             WHERE namespace = $1 AND id = $2",
        )
        .bind(&self.namespace)
        .bind(id)
        .bind(serde_json::to_value(&timer)?)
        .execute(&mut *transaction)
        .await
        .map_err(store_error)?;
        transaction.commit().await.map_err(store_error)?;
        Ok(true)
    }

    pub(super) async fn reconcile_timers_impl(
        &self,
        id_prefix: &str,
        desired: Vec<TimerRecord>,
        _now: DateTime<Utc>,
    ) -> RuntimeResult<TimerReconcileOutcome> {
        let mut transaction = self.pool.begin().await.map_err(store_error)?;
        lock_timer_namespace(&mut transaction, &self.namespace).await?;
        let existing = sqlx::query(
            "SELECT record_json FROM mdbase_runtime_timers
             WHERE namespace = $1 AND left(id, char_length($2)) = $2
             ORDER BY id FOR UPDATE",
        )
        .bind(&self.namespace)
        .bind(id_prefix)
        .fetch_all(&mut *transaction)
        .await
        .map_err(store_error)?
        .into_iter()
        .map(|row| {
            let value: Value = row.try_get("record_json").map_err(store_error)?;
            let timer = serde_json::from_value::<TimerRecord>(value)?;
            Ok((timer.id.clone(), timer))
        })
        .collect::<RuntimeResult<std::collections::BTreeMap<_, _>>>()?;
        let desired_ids = desired
            .iter()
            .map(|timer| timer.id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let mutation_at = postgres_mutation_time(&mut transaction).await?;
        // Validate and materialize every new generation before the first SQL
        // mutation, so exhaustion cannot partially apply omission cancellation.
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
            timer.id.starts_with(id_prefix)
                && !desired_ids.contains(&timer.id)
                && matches!(timer.status, TimerStatus::Scheduled | TimerStatus::Firing)
        }) {
            let mut cancelled = timer.clone();
            cancelled.status = TimerStatus::Cancelled;
            cancelled.updated_at = mutation_at;
            sqlx::query(
                "UPDATE mdbase_runtime_timers
                 SET status = 'cancelled', lease_worker = NULL, lease_token = NULL,
                     lease_expires_at = NULL, record_json = $3
                 WHERE namespace = $1 AND id = $2",
            )
            .bind(&self.namespace)
            .bind(&cancelled.id)
            .bind(serde_json::to_value(&cancelled)?)
            .execute(&mut *transaction)
            .await
            .map_err(store_error)?;
            cancelled_ids.push(timer.id.clone());
        }
        let mut timers = Vec::with_capacity(planned.len());
        for (next, changed) in planned {
            if !changed {
                timers.push(next);
                continue;
            }
            sqlx::query(
                "INSERT INTO mdbase_runtime_timers
                    (namespace, id, generation, status, fire_at, record_json)
                 VALUES ($1, $2, $3, 'scheduled', $4, $5)
                 ON CONFLICT(namespace, id) DO UPDATE SET
                    generation = excluded.generation, status = 'scheduled',
                    fire_at = excluded.fire_at, lease_worker = NULL, lease_token = NULL,
                    lease_expires_at = NULL, record_json = excluded.record_json",
            )
            .bind(&self.namespace)
            .bind(&next.id)
            .bind(as_i64(next.generation)?)
            .bind(next.fire_at)
            .bind(serde_json::to_value(&next)?)
            .execute(&mut *transaction)
            .await
            .map_err(store_error)?;
            timers.push(next);
        }
        transaction.commit().await.map_err(store_error)?;
        timers.sort_by(|left, right| left.id.cmp(&right.id));
        cancelled_ids.sort();
        Ok(TimerReconcileOutcome {
            timers,
            cancelled_ids,
        })
    }

    pub(super) async fn timers_impl(&self, id_prefix: &str) -> RuntimeResult<Vec<TimerRecord>> {
        sqlx::query(
            "SELECT record_json FROM mdbase_runtime_timers
             WHERE namespace = $1 AND left(id, char_length($2)) = $2
             ORDER BY id",
        )
        .bind(&self.namespace)
        .bind(id_prefix)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?
        .into_iter()
        .map(|row| {
            let value: Value = row.try_get("record_json").map_err(store_error)?;
            serde_json::from_value::<TimerRecord>(value).map_err(Into::into)
        })
        .collect()
    }

    pub(super) async fn claim_due_timer_impl(
        &self,
        worker: &str,
        now: DateTime<Utc>,
        lease_for: Duration,
    ) -> RuntimeResult<Option<TimerClaim>> {
        // Expiration can touch many rows, while complete-set reconciliation
        // locks every matching row in id order. Acquire expired rows in that
        // same order and commit before selecting a candidate: otherwise a
        // later candidate (ordered by fire_at) could invert the row-lock order.
        // The candidate transaction still takes only one row lock and retains
        // SKIP LOCKED concurrency between workers.
        expire_timer_leases(&self.pool, &self.namespace, now).await?;

        let mut transaction = self.pool.begin().await.map_err(store_error)?;
        let row = sqlx::query(
            "SELECT record_json FROM mdbase_runtime_timers
             WHERE namespace = $1 AND status IN ('scheduled', 'firing')
               AND fire_at <= $2 AND lease_token IS NULL
             ORDER BY fire_at, id LIMIT 1 FOR UPDATE SKIP LOCKED",
        )
        .bind(&self.namespace)
        .bind(now)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(store_error)?;
        let Some(row) = row else {
            transaction.commit().await.map_err(store_error)?;
            return Ok(None);
        };
        let mut timer: TimerRecord =
            serde_json::from_value(row.try_get("record_json").map_err(store_error)?)?;
        timer.status = TimerStatus::Firing;
        timer.updated_at = now;
        let token = format!("timer_lease_{}", Ulid::new());
        let expires_at = add_duration(now, lease_for)?;
        sqlx::query(
            "UPDATE mdbase_runtime_timers
             SET status = 'firing', lease_worker = $3, lease_token = $4,
                 lease_expires_at = $5, record_json = $6
             WHERE namespace = $1 AND id = $2 AND lease_token IS NULL",
        )
        .bind(&self.namespace)
        .bind(&timer.id)
        .bind(worker)
        .bind(&token)
        .bind(expires_at)
        .bind(serde_json::to_value(&timer)?)
        .execute(&mut *transaction)
        .await
        .map_err(store_error)?;
        transaction.commit().await.map_err(store_error)?;
        Ok(Some(TimerClaim {
            timer,
            worker: worker.to_string(),
            token,
            expires_at,
        }))
    }

    pub(super) async fn fire_timer_impl(
        &self,
        claim: TimerClaim,
        fired: TimerRecord,
        event: PreparedEvent,
    ) -> RuntimeResult<AdmitOutcome> {
        let mut transaction = self.pool.begin().await.map_err(store_error)?;
        let changed = sqlx::query(
            "UPDATE mdbase_runtime_timers
             SET status = 'fired', lease_worker = NULL, lease_token = NULL,
                 lease_expires_at = NULL, record_json = $5
             WHERE namespace = $1 AND id = $2 AND lease_worker = $3 AND lease_token = $4",
        )
        .bind(&self.namespace)
        .bind(&claim.timer.id)
        .bind(&claim.worker)
        .bind(&claim.token)
        .bind(serde_json::to_value(&fired)?)
        .execute(&mut *transaction)
        .await
        .map_err(store_error)?
        .rows_affected();
        if changed != 1 {
            return Err(RuntimeError::diagnostic(
                "stale_timer_lease",
                "Timer lease changed before firing.",
            ));
        }
        let outcome = admit_tx(&mut transaction, &self.namespace, event).await?;
        transaction.commit().await.map_err(store_error)?;
        Ok(outcome)
    }
}

/// Serialize every operation that can allocate a timer generation or reconcile
/// a complete timer set. Existing rows are then locked by the operation's
/// SELECT/UPDATE, giving one global-before-row lock order while also fencing
/// absent rows that PostgreSQL predicate locks do not protect.
async fn postgres_mutation_time(
    transaction: &mut Transaction<'_, Postgres>,
) -> RuntimeResult<DateTime<Utc>> {
    sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **transaction)
        .await
        .map_err(store_error)
}

async fn expire_timer_leases(
    pool: &PgPool,
    namespace: &str,
    now: DateTime<Utc>,
) -> RuntimeResult<()> {
    let mut transaction = pool.begin().await.map_err(store_error)?;
    sqlx::query(
        "WITH expired AS MATERIALIZED (
             SELECT namespace, id
             FROM mdbase_runtime_timers
             WHERE namespace = $1 AND lease_expires_at <= $2
             ORDER BY id
             FOR UPDATE
         )
         UPDATE mdbase_runtime_timers AS timer
         SET lease_worker = NULL, lease_token = NULL, lease_expires_at = NULL
         FROM expired
         WHERE timer.namespace = expired.namespace AND timer.id = expired.id",
    )
    .bind(namespace)
    .bind(now)
    .execute(&mut *transaction)
    .await
    .map_err(store_error)?;
    transaction.commit().await.map_err(store_error)?;
    Ok(())
}

async fn lock_timer_namespace(
    transaction: &mut Transaction<'_, Postgres>,
    namespace: &str,
) -> RuntimeResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1 || ':timers', 0))")
        .bind(namespace)
        .execute(&mut **transaction)
        .await
        .map_err(store_error)?;
    Ok(())
}
