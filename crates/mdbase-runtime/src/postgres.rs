use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Row, Transaction};
use ulid::Ulid;

use crate::error::{RuntimeError, RuntimeResult};
use crate::model::{
    ConcurrencyPolicy, EventJournalEntry, RunRecord, RunStatus, TimerRecord, TimerStatus,
};
use crate::store::{
    AdmitOutcome, Claim, EventPage, PreparedEvent, RuntimeStore, StoreSnapshot, TimerClaim,
    TimerReconcileOutcome,
};
use crate::timer::{next_timer_generation, timer_matches};

/// PostgreSQL runtime store scoped by an embedding-host authority namespace.
///
/// Multiple collections and workers may share one pool. Every durable key,
/// cursor, lease, idempotency reservation, and timer is fenced by `namespace`.
#[derive(Clone)]
pub struct PostgresRuntimeStore {
    pool: PgPool,
    namespace: String,
}

impl PostgresRuntimeStore {
    pub async fn connect(database_url: &str, namespace: impl Into<String>) -> RuntimeResult<Self> {
        let pool = PgPool::connect(database_url).await.map_err(store_error)?;
        Self::new(pool, namespace).await
    }

    pub async fn new(pool: PgPool, namespace: impl Into<String>) -> RuntimeResult<Self> {
        let namespace = namespace.into();
        if namespace.trim().is_empty() || namespace.len() > 200 {
            return Err(RuntimeError::diagnostic(
                "invalid_runtime_namespace",
                "A PostgreSQL runtime namespace must contain 1 to 200 characters.",
            ));
        }
        migrate(&pool).await?;
        Ok(Self { pool, namespace })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }
}

#[async_trait]
impl RuntimeStore for PostgresRuntimeStore {
    async fn admit_event(&self, event: PreparedEvent) -> RuntimeResult<AdmitOutcome> {
        let mut transaction = self.pool.begin().await.map_err(store_error)?;
        let outcome = admit_tx(&mut transaction, &self.namespace, event).await?;
        transaction.commit().await.map_err(store_error)?;
        Ok(outcome)
    }

    async fn claim_run(
        &self,
        executor: &str,
        worker: &str,
        now: DateTime<Utc>,
        lease_for: Duration,
    ) -> RuntimeResult<Option<Claim>> {
        let mut transaction = self.pool.begin().await.map_err(store_error)?;
        sqlx::query(
            "UPDATE mdbase_runtime_runs
             SET lease_worker = NULL, lease_token = NULL, lease_expires_at = NULL
             WHERE namespace = $1 AND lease_expires_at <= $2",
        )
        .bind(&self.namespace)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(store_error)?;
        let candidates = sqlx::query(
            "SELECT record_json
             FROM mdbase_runtime_runs
             WHERE namespace = $1 AND executor = $2
               AND status IN ('queued', 'running') AND not_before <= $3
               AND lease_token IS NULL
             ORDER BY created_at, id
             LIMIT 100
             FOR UPDATE SKIP LOCKED",
        )
        .bind(&self.namespace)
        .bind(executor)
        .bind(now)
        .fetch_all(&mut *transaction)
        .await
        .map_err(store_error)?;
        let mut selected = None;
        for row in candidates {
            let value: Value = row.try_get("record_json").map_err(store_error)?;
            let run: RunRecord = serde_json::from_value(value)?;
            if run.plan.concurrency_policy != ConcurrencyPolicy::Allow {
                let locked = sqlx::query_scalar::<_, bool>(
                    "SELECT pg_try_advisory_xact_lock(
                        hashtextextended($1 || ':' || $2, 0)
                     )",
                )
                .bind(&self.namespace)
                .bind(&run.plan.concurrency_group)
                .fetch_one(&mut *transaction)
                .await
                .map_err(store_error)?;
                if !locked {
                    continue;
                }
            }
            if group_is_runnable_tx(&mut transaction, &self.namespace, &run).await? {
                selected = Some(run);
                break;
            }
        }
        let Some(mut run) = selected else {
            transaction.commit().await.map_err(store_error)?;
            return Ok(None);
        };
        if run.status == RunStatus::Queued {
            run.status = RunStatus::Running;
            run.started_at.get_or_insert(now);
            run.updated_at = now;
        }
        let token = format!("lease_{}", Ulid::new());
        let expires_at = add_duration(now, lease_for)?;
        let changed = sqlx::query(
            "UPDATE mdbase_runtime_runs
             SET status = $1, lease_worker = $2, lease_token = $3,
                 lease_expires_at = $4, record_json = $5
             WHERE namespace = $6 AND id = $7 AND lease_token IS NULL AND revision = $8",
        )
        .bind(run_status(run.status))
        .bind(worker)
        .bind(&token)
        .bind(expires_at)
        .bind(serde_json::to_value(&run)?)
        .bind(&self.namespace)
        .bind(&run.plan.id)
        .bind(as_i64(run.revision)?)
        .execute(&mut *transaction)
        .await
        .map_err(store_error)?
        .rows_affected();
        if changed != 1 {
            transaction.rollback().await.map_err(store_error)?;
            return Ok(None);
        }
        transaction.commit().await.map_err(store_error)?;
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
        let mut transaction = self.pool.begin().await.map_err(store_error)?;
        let current = sqlx::query(
            "SELECT lease_worker, lease_token, revision
             FROM mdbase_runtime_runs WHERE namespace = $1 AND id = $2 FOR UPDATE",
        )
        .bind(&self.namespace)
        .bind(&claim.run.plan.id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(store_error)?;
        let Some(current) = current else {
            return Err(RuntimeError::diagnostic(
                "stale_lease",
                "Claimed run no longer exists.",
            ));
        };
        let lease_worker: Option<String> = current.try_get("lease_worker").map_err(store_error)?;
        let lease_token: Option<String> = current.try_get("lease_token").map_err(store_error)?;
        let revision: i64 = current.try_get("revision").map_err(store_error)?;
        if lease_worker.as_deref() != Some(claim.worker.as_str())
            || lease_token.as_deref() != Some(claim.token.as_str())
            || as_u64(revision)? != claim.run.revision
        {
            return Err(RuntimeError::diagnostic(
                "stale_lease",
                "Run lease or revision changed before commit.",
            ));
        }
        claim.run.revision += 1;
        let terminal = claim.run.status.terminal();
        let changed = sqlx::query(
            "UPDATE mdbase_runtime_runs
             SET status = $1, revision = $2, record_json = $3,
                 lease_worker = CASE WHEN $4 THEN NULL ELSE lease_worker END,
                 lease_token = CASE WHEN $4 THEN NULL ELSE lease_token END,
                 lease_expires_at = CASE WHEN $4 THEN NULL ELSE lease_expires_at END
             WHERE namespace = $5 AND id = $6 AND lease_token = $7 AND revision = $8",
        )
        .bind(run_status(claim.run.status))
        .bind(as_i64(claim.run.revision)?)
        .bind(serde_json::to_value(&claim.run)?)
        .bind(terminal)
        .bind(&self.namespace)
        .bind(&claim.run.plan.id)
        .bind(&claim.token)
        .bind(revision)
        .execute(&mut *transaction)
        .await
        .map_err(store_error)?
        .rows_affected();
        if changed != 1 {
            return Err(RuntimeError::diagnostic(
                "stale_lease",
                "Run changed during commit.",
            ));
        }
        for event in emitted {
            admit_tx(&mut transaction, &self.namespace, event).await?;
        }
        transaction.commit().await.map_err(store_error)?;
        Ok(claim)
    }

    async fn get_run(&self, id: &str) -> RuntimeResult<Option<RunRecord>> {
        let row = sqlx::query(
            "SELECT record_json FROM mdbase_runtime_runs WHERE namespace = $1 AND id = $2",
        )
        .bind(&self.namespace)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        row.map(|row| {
            let value: Value = row.try_get("record_json").map_err(store_error)?;
            serde_json::from_value(value).map_err(Into::into)
        })
        .transpose()
    }

    async fn events_after(&self, after: u64, limit: usize) -> RuntimeResult<EventPage> {
        let meta = sqlx::query(
            "SELECT next_cursor, retained_after
             FROM mdbase_runtime_meta WHERE namespace = $1",
        )
        .bind(&self.namespace)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        let (head, retained_after) = match meta {
            Some(row) => (
                as_u64(row.try_get("next_cursor").map_err(store_error)?)?,
                as_u64(row.try_get("retained_after").map_err(store_error)?)?,
            ),
            None => (0, 0),
        };
        let reset_required = after < retained_after;
        let limit = limit.clamp(1, 10_000);
        let rows = if reset_required {
            Vec::new()
        } else {
            sqlx::query(
                "SELECT cursor, source_runtime, event_id, envelope_json, received_at
                 FROM mdbase_runtime_events
                 WHERE namespace = $1 AND cursor > $2
                 ORDER BY cursor LIMIT $3",
            )
            .bind(&self.namespace)
            .bind(as_i64(after)?)
            .bind(i64::try_from(limit + 1).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)?
        };
        let mut events = rows
            .into_iter()
            .map(|row| {
                Ok(EventJournalEntry {
                    cursor: as_u64(row.try_get("cursor").map_err(store_error)?)?,
                    source_runtime: row.try_get("source_runtime").map_err(store_error)?,
                    event_id: row.try_get("event_id").map_err(store_error)?,
                    envelope: row.try_get("envelope_json").map_err(store_error)?,
                    received_at: row.try_get("received_at").map_err(store_error)?,
                })
            })
            .collect::<RuntimeResult<Vec<_>>>()?;
        let has_more = events.len() > limit;
        events.truncate(limit);
        Ok(EventPage {
            events,
            retained_after,
            head,
            has_more,
            reset_required,
        })
    }

    async fn prune_events_through(&self, cursor: u64) -> RuntimeResult<u64> {
        let mut transaction = self.pool.begin().await.map_err(store_error)?;
        let meta = sqlx::query(
            "SELECT next_cursor, retained_after FROM mdbase_runtime_meta
             WHERE namespace = $1 FOR UPDATE",
        )
        .bind(&self.namespace)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(store_error)?;
        let Some(meta) = meta else {
            transaction.commit().await.map_err(store_error)?;
            return Ok(0);
        };
        let head = as_u64(meta.try_get("next_cursor").map_err(store_error)?)?;
        let current = as_u64(meta.try_get("retained_after").map_err(store_error)?)?;
        let pruned_through = cursor.min(head).max(current);
        sqlx::query("DELETE FROM mdbase_runtime_events WHERE namespace = $1 AND cursor <= $2")
            .bind(&self.namespace)
            .bind(as_i64(pruned_through)?)
            .execute(&mut *transaction)
            .await
            .map_err(store_error)?;
        sqlx::query("UPDATE mdbase_runtime_meta SET retained_after = $2 WHERE namespace = $1")
            .bind(&self.namespace)
            .bind(as_i64(pruned_through)?)
            .execute(&mut *transaction)
            .await
            .map_err(store_error)?;
        transaction.commit().await.map_err(store_error)?;
        Ok(pruned_through)
    }

    async fn request_cancel(&self, id: &str, now: DateTime<Utc>) -> RuntimeResult<bool> {
        let mut transaction = self.pool.begin().await.map_err(store_error)?;
        let row = sqlx::query(
            "SELECT record_json FROM mdbase_runtime_runs
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
        let mut run: RunRecord =
            serde_json::from_value(row.try_get("record_json").map_err(store_error)?)?;
        if !run.request_cancel(now) {
            transaction.commit().await.map_err(store_error)?;
            return Ok(false);
        }
        run.revision += 1;
        sqlx::query(
            "UPDATE mdbase_runtime_runs SET status = $3, revision = $4, record_json = $5
             WHERE namespace = $1 AND id = $2",
        )
        .bind(&self.namespace)
        .bind(id)
        .bind(run_status(run.status))
        .bind(as_i64(run.revision)?)
        .bind(serde_json::to_value(&run)?)
        .execute(&mut *transaction)
        .await
        .map_err(store_error)?;
        transaction.commit().await.map_err(store_error)?;
        Ok(true)
    }

    async fn upsert_timer(&self, mut timer: TimerRecord) -> RuntimeResult<TimerRecord> {
        let mut transaction = self.pool.begin().await.map_err(store_error)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1 || ':timer:' || $2, 0))")
            .bind(&self.namespace)
            .bind(&timer.id)
            .execute(&mut *transaction)
            .await
            .map_err(store_error)?;
        let generation = sqlx::query(
            "SELECT generation FROM mdbase_runtime_timers
             WHERE namespace = $1 AND id = $2 FOR UPDATE",
        )
        .bind(&self.namespace)
        .bind(&timer.id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(store_error)?
        .map(|row| row.try_get::<i64, _>("generation").map_err(store_error))
        .transpose()?
        .map(as_u64)
        .transpose()?
        .unwrap_or(0)
            + 1;
        timer.generation = generation;
        timer.status = TimerStatus::Scheduled;
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

    async fn cancel_timer(
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

    async fn reconcile_timers(
        &self,
        id_prefix: &str,
        desired: Vec<TimerRecord>,
        now: DateTime<Utc>,
    ) -> RuntimeResult<TimerReconcileOutcome> {
        let mut transaction = self.pool.begin().await.map_err(store_error)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1 || ':timers', 0))")
            .bind(&self.namespace)
            .execute(&mut *transaction)
            .await
            .map_err(store_error)?;
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
        let mut cancelled_ids = Vec::new();
        for timer in existing.values().filter(|timer| {
            timer.id.starts_with(id_prefix)
                && !desired_ids.contains(&timer.id)
                && matches!(timer.status, TimerStatus::Scheduled | TimerStatus::Firing)
        }) {
            let mut cancelled = timer.clone();
            cancelled.status = TimerStatus::Cancelled;
            cancelled.updated_at = now;
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
        let mut timers = Vec::with_capacity(desired.len());
        for desired in desired {
            let current = existing.get(&desired.id);
            if current.is_some_and(|current| timer_matches(current, &desired)) {
                timers.push(current.expect("checked above").clone());
                continue;
            }
            let next = next_timer_generation(current, desired, now)?;
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

    async fn timers(&self, id_prefix: &str) -> RuntimeResult<Vec<TimerRecord>> {
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

    async fn claim_due_timer(
        &self,
        worker: &str,
        now: DateTime<Utc>,
        lease_for: Duration,
    ) -> RuntimeResult<Option<TimerClaim>> {
        let mut transaction = self.pool.begin().await.map_err(store_error)?;
        sqlx::query(
            "UPDATE mdbase_runtime_timers
             SET lease_worker = NULL, lease_token = NULL, lease_expires_at = NULL
             WHERE namespace = $1 AND lease_expires_at <= $2",
        )
        .bind(&self.namespace)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(store_error)?;
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

    async fn fire_timer(
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

    async fn snapshot(&self) -> RuntimeResult<StoreSnapshot> {
        let retained_after = sqlx::query_scalar::<_, i64>(
            "SELECT retained_after FROM mdbase_runtime_meta WHERE namespace = $1",
        )
        .bind(&self.namespace)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?
        .map(as_u64)
        .transpose()?
        .unwrap_or(0);
        let events = self.events_after(retained_after, 10_000).await?.events;
        let runs =
            read_records::<RunRecord>(&self.pool, "mdbase_runtime_runs", &self.namespace).await?;
        let timers =
            read_records::<TimerRecord>(&self.pool, "mdbase_runtime_timers", &self.namespace)
                .await?;
        Ok(StoreSnapshot {
            events,
            runs,
            timers,
        })
    }
}

async fn admit_tx(
    transaction: &mut Transaction<'_, Postgres>,
    namespace: &str,
    event: PreparedEvent,
) -> RuntimeResult<AdmitOutcome> {
    // Admission decisions (idempotency, debounce, and concurrency) are one
    // serializable boundary per authority namespace. Dispatch and independent
    // run execution remain concurrent.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(namespace)
        .execute(&mut **transaction)
        .await
        .map_err(store_error)?;
    let existing = sqlx::query(
        "SELECT cursor FROM mdbase_runtime_event_dedup
         WHERE namespace = $1 AND source_runtime = $2 AND event_id = $3",
    )
    .bind(namespace)
    .bind(&event.source_runtime)
    .bind(&event.event_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(store_error)?;
    if let Some(row) = existing {
        return Ok(AdmitOutcome {
            cursor: as_u64(row.try_get("cursor").map_err(store_error)?)?,
            duplicate: true,
            admitted_run_ids: Vec::new(),
            skipped_run_ids: Vec::new(),
        });
    }
    let cursor: i64 = sqlx::query(
        "INSERT INTO mdbase_runtime_meta(namespace, next_cursor, retained_after)
         VALUES ($1, 1, 0)
         ON CONFLICT(namespace) DO UPDATE
           SET next_cursor = mdbase_runtime_meta.next_cursor + 1
         RETURNING next_cursor",
    )
    .bind(namespace)
    .fetch_one(&mut **transaction)
    .await
    .map_err(store_error)?
    .try_get("next_cursor")
    .map_err(store_error)?;
    sqlx::query(
        "INSERT INTO mdbase_runtime_events
            (namespace, cursor, source_runtime, event_id, envelope_json, received_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(namespace)
    .bind(cursor)
    .bind(&event.source_runtime)
    .bind(&event.event_id)
    .bind(&event.envelope)
    .bind(event.received_at)
    .execute(&mut **transaction)
    .await
    .map_err(store_error)?;
    sqlx::query(
        "INSERT INTO mdbase_runtime_event_dedup
            (namespace, source_runtime, event_id, cursor)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(namespace)
    .bind(&event.source_runtime)
    .bind(&event.event_id)
    .bind(cursor)
    .execute(&mut **transaction)
    .await
    .map_err(store_error)?;
    let cursor = as_u64(cursor)?;

    let mut admitted = Vec::new();
    let mut skipped = Vec::new();
    for mut plan in event.runs {
        plan.event_cursor = cursor;
        if minimum_interval_suppresses_tx(transaction, namespace, &plan).await? {
            skipped.push(plan.id);
            continue;
        }
        if plan.not_before > event.received_at {
            sqlx::query(
                "DELETE FROM mdbase_runtime_runs
                 WHERE namespace = $1 AND workflow = $2 AND trigger_id = $3 AND executor = $4
                   AND status = 'queued' AND not_before > $5",
            )
            .bind(namespace)
            .bind(&plan.workflow)
            .bind(&plan.trigger)
            .bind(&plan.executor)
            .bind(event.received_at)
            .execute(&mut **transaction)
            .await
            .map_err(store_error)?;
        }
        let duplicate = sqlx::query(
            "SELECT 1 FROM mdbase_runtime_runs
             WHERE namespace = $1 AND idempotency_scope = $2 AND idempotency_key = $3",
        )
        .bind(namespace)
        .bind(&plan.idempotency_scope)
        .bind(&plan.idempotency_key)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(store_error)?
        .is_some();
        if duplicate {
            skipped.push(plan.id);
            continue;
        }
        let active = active_group_runs_tx(transaction, namespace, &plan.concurrency_group).await?;
        if plan.concurrency_policy == ConcurrencyPolicy::Skip && !active.is_empty() {
            skipped.push(plan.id);
            continue;
        }
        if plan.concurrency_policy == ConcurrencyPolicy::Replace {
            for id in active {
                let row = sqlx::query(
                    "SELECT record_json FROM mdbase_runtime_runs
                     WHERE namespace = $1 AND id = $2 FOR UPDATE",
                )
                .bind(namespace)
                .bind(&id)
                .fetch_one(&mut **transaction)
                .await
                .map_err(store_error)?;
                let mut run: RunRecord =
                    serde_json::from_value(row.try_get("record_json").map_err(store_error)?)?;
                run.cancel_requested_at.get_or_insert(event.received_at);
                run.updated_at = event.received_at;
                run.revision += 1;
                sqlx::query(
                    "UPDATE mdbase_runtime_runs SET revision = $3, record_json = $4
                     WHERE namespace = $1 AND id = $2",
                )
                .bind(namespace)
                .bind(&id)
                .bind(as_i64(run.revision)?)
                .bind(serde_json::to_value(&run)?)
                .execute(&mut **transaction)
                .await
                .map_err(store_error)?;
            }
        }
        let run = RunRecord::admitted(plan);
        sqlx::query(
            "INSERT INTO mdbase_runtime_runs
                (namespace, id, executor, workflow, trigger_id, status, created_at, not_before,
                 idempotency_scope, idempotency_key, concurrency_group, concurrency_policy,
                 revision, record_json)
             VALUES ($1, $2, $3, $4, $5, 'queued', $6, $7, $8, $9, $10, $11, 0, $12)",
        )
        .bind(namespace)
        .bind(&run.plan.id)
        .bind(&run.plan.executor)
        .bind(&run.plan.workflow)
        .bind(&run.plan.trigger)
        .bind(run.created_at)
        .bind(run.plan.not_before)
        .bind(&run.plan.idempotency_scope)
        .bind(&run.plan.idempotency_key)
        .bind(&run.plan.concurrency_group)
        .bind(concurrency_policy(run.plan.concurrency_policy))
        .bind(serde_json::to_value(&run)?)
        .execute(&mut **transaction)
        .await
        .map_err(store_error)?;
        admitted.push(run.plan.id);
    }
    Ok(AdmitOutcome {
        cursor,
        duplicate: false,
        admitted_run_ids: admitted,
        skipped_run_ids: skipped,
    })
}

async fn minimum_interval_suppresses_tx(
    transaction: &mut Transaction<'_, Postgres>,
    namespace: &str,
    plan: &crate::model::PlannedRun,
) -> RuntimeResult<bool> {
    let Some(interval) = plan.minimum_interval_ms else {
        return Ok(false);
    };
    let earliest =
        plan.created_at - TimeDelta::milliseconds(i64::try_from(interval).unwrap_or(i64::MAX));
    Ok(sqlx::query(
        "SELECT 1 FROM mdbase_runtime_runs
         WHERE namespace = $1 AND workflow = $2 AND trigger_id = $3 AND created_at > $4
         LIMIT 1",
    )
    .bind(namespace)
    .bind(&plan.workflow)
    .bind(&plan.trigger)
    .bind(earliest)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(store_error)?
    .is_some())
}

async fn active_group_runs_tx(
    transaction: &mut Transaction<'_, Postgres>,
    namespace: &str,
    group: &str,
) -> RuntimeResult<Vec<String>> {
    let rows = sqlx::query(
        "SELECT id FROM mdbase_runtime_runs
         WHERE namespace = $1 AND concurrency_group = $2 AND status IN ('running', 'waiting')",
    )
    .bind(namespace)
    .bind(group)
    .fetch_all(&mut **transaction)
    .await
    .map_err(store_error)?;
    rows.into_iter()
        .map(|row| row.try_get("id").map_err(store_error))
        .collect()
}

async fn group_is_runnable_tx(
    transaction: &mut Transaction<'_, Postgres>,
    namespace: &str,
    run: &RunRecord,
) -> RuntimeResult<bool> {
    if run.plan.concurrency_policy == ConcurrencyPolicy::Allow {
        return Ok(true);
    }
    Ok(sqlx::query(
        "SELECT 1 FROM mdbase_runtime_runs
         WHERE namespace = $1 AND concurrency_group = $2 AND id != $3
           AND status IN ('running', 'waiting') LIMIT 1",
    )
    .bind(namespace)
    .bind(&run.plan.concurrency_group)
    .bind(&run.plan.id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(store_error)?
    .is_none())
}

async fn read_records<T: serde::de::DeserializeOwned>(
    pool: &PgPool,
    table: &str,
    namespace: &str,
) -> RuntimeResult<Vec<T>> {
    let rows = match table {
        "mdbase_runtime_runs" => sqlx::query(
            "SELECT record_json FROM mdbase_runtime_runs
                 WHERE namespace = $1 ORDER BY id",
        )
        .bind(namespace)
        .fetch_all(pool)
        .await
        .map_err(store_error)?,
        "mdbase_runtime_timers" => sqlx::query(
            "SELECT record_json FROM mdbase_runtime_timers
                 WHERE namespace = $1 ORDER BY id",
        )
        .bind(namespace)
        .fetch_all(pool)
        .await
        .map_err(store_error)?,
        _ => {
            return Err(RuntimeError::Store(format!(
                "unsupported PostgreSQL runtime record table: {table}"
            )))
        }
    };
    rows.into_iter()
        .map(|row| {
            let value: Value = row.try_get("record_json").map_err(store_error)?;
            serde_json::from_value(value).map_err(Into::into)
        })
        .collect()
}

async fn migrate(pool: &PgPool) -> RuntimeResult<()> {
    sqlx::raw_sql(
        "
        CREATE TABLE IF NOT EXISTS mdbase_runtime_meta (
            namespace text PRIMARY KEY,
            next_cursor bigint NOT NULL DEFAULT 0 CHECK (next_cursor >= 0),
            retained_after bigint NOT NULL DEFAULT 0 CHECK (retained_after >= 0)
        );
        CREATE TABLE IF NOT EXISTS mdbase_runtime_events (
            namespace text NOT NULL,
            cursor bigint NOT NULL CHECK (cursor > 0),
            source_runtime text NOT NULL,
            event_id text NOT NULL,
            envelope_json jsonb NOT NULL,
            received_at timestamptz NOT NULL,
            PRIMARY KEY(namespace, cursor)
        );
        CREATE TABLE IF NOT EXISTS mdbase_runtime_event_dedup (
            namespace text NOT NULL,
            source_runtime text NOT NULL,
            event_id text NOT NULL,
            cursor bigint NOT NULL CHECK (cursor > 0),
            PRIMARY KEY(namespace, source_runtime, event_id)
        );
        CREATE TABLE IF NOT EXISTS mdbase_runtime_runs (
            namespace text NOT NULL,
            id text NOT NULL,
            executor text NOT NULL,
            workflow text NOT NULL,
            trigger_id text NOT NULL,
            status text NOT NULL,
            created_at timestamptz NOT NULL,
            not_before timestamptz NOT NULL,
            idempotency_scope text NOT NULL,
            idempotency_key text NOT NULL,
            concurrency_group text NOT NULL,
            concurrency_policy text NOT NULL,
            lease_worker text,
            lease_token text,
            lease_expires_at timestamptz,
            revision bigint NOT NULL,
            record_json jsonb NOT NULL,
            PRIMARY KEY(namespace, id),
            UNIQUE(namespace, idempotency_scope, idempotency_key)
        );
        CREATE INDEX IF NOT EXISTS mdbase_runtime_runs_claim
            ON mdbase_runtime_runs(namespace, executor, status, not_before, created_at);
        CREATE INDEX IF NOT EXISTS mdbase_runtime_runs_group
            ON mdbase_runtime_runs(namespace, concurrency_group, status);
        CREATE INDEX IF NOT EXISTS mdbase_runtime_runs_trigger
            ON mdbase_runtime_runs(namespace, workflow, trigger_id, created_at);
        CREATE TABLE IF NOT EXISTS mdbase_runtime_timers (
            namespace text NOT NULL,
            id text NOT NULL,
            generation bigint NOT NULL,
            status text NOT NULL,
            fire_at timestamptz NOT NULL,
            lease_worker text,
            lease_token text,
            lease_expires_at timestamptz,
            record_json jsonb NOT NULL,
            PRIMARY KEY(namespace, id)
        );
        CREATE INDEX IF NOT EXISTS mdbase_runtime_timers_due
            ON mdbase_runtime_timers(namespace, status, fire_at);
        ",
    )
    .execute(pool)
    .await
    .map_err(store_error)?;
    Ok(())
}

fn add_duration(now: DateTime<Utc>, duration: Duration) -> RuntimeResult<DateTime<Utc>> {
    TimeDelta::from_std(duration)
        .map(|duration| now + duration)
        .map_err(|_| RuntimeError::Clock("lease duration is too large".to_string()))
}

fn run_status(status: RunStatus) -> &'static str {
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

fn concurrency_policy(policy: ConcurrencyPolicy) -> &'static str {
    match policy {
        ConcurrencyPolicy::Skip => "skip",
        ConcurrencyPolicy::Queue => "queue",
        ConcurrencyPolicy::Replace => "replace",
        ConcurrencyPolicy::Allow => "allow",
    }
}

fn as_i64(value: u64) -> RuntimeResult<i64> {
    i64::try_from(value)
        .map_err(|_| RuntimeError::Store("runtime counter exceeds PostgreSQL bigint".to_string()))
}

fn as_u64(value: i64) -> RuntimeResult<u64> {
    u64::try_from(value).map_err(|_| RuntimeError::Store("negative runtime counter".to_string()))
}

fn store_error(error: sqlx::Error) -> RuntimeError {
    RuntimeError::Store(error.to_string())
}
