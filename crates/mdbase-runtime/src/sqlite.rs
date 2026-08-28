mod recovery;
mod schema;
mod timers;
mod utils;

pub use recovery::{inspect_sqlite_recovery, SqliteRecoveryState};
use schema::{migrate, validate_persisted_records};
use std::path::Path;
use std::thread;
use std::time::Duration;
use utils::{
    add_duration, concurrency_policy, nonnegative_cursor, parse_timestamp, run_status, store_error,
    timestamp,
};

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use tokio::sync::{mpsc, oneshot};
use ulid::Ulid;

use crate::error::{RuntimeError, RuntimeResult};
#[cfg(test)]
use crate::model::TimerStatus;
use crate::model::{ConcurrencyPolicy, EventJournalEntry, RunRecord, RunStatus, TimerRecord};
use crate::store::{
    AdmitOutcome, Claim, EventPage, PreparedEvent, RuntimeStore, StoreSnapshot, TimerClaim,
    TimerReconcileOutcome,
};

/// Version of the SQLite tables and every Rust value serialized into them.
///
/// Changing `RunRecord`, `TimerRecord`, or a stored event envelope incompatibly
/// requires a new migration here even when the SQL columns do not change.
pub const SQLITE_SCHEMA_VERSION: u32 = 2;
const SQLITE_WORK_QUEUE_CAPACITY: usize = 64;

type SqliteCommand = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

#[derive(Clone)]
pub struct SqliteRuntimeStore {
    worker: mpsc::Sender<SqliteCommand>,
}

impl SqliteRuntimeStore {
    pub fn open(path: impl AsRef<Path>) -> RuntimeResult<Self> {
        let connection =
            Connection::open(path).map_err(|error| RuntimeError::Store(error.to_string()))?;
        Self::from_connection(connection)
    }

    pub fn in_memory() -> RuntimeResult<Self> {
        let connection =
            Connection::open_in_memory().map_err(|error| RuntimeError::Store(error.to_string()))?;
        Self::from_connection(connection)
    }

    pub async fn schema_version(&self) -> RuntimeResult<u32> {
        self.execute(|connection| {
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
                .map_err(store_error)
        })
        .await
    }

    fn from_connection(mut connection: Connection) -> RuntimeResult<Self> {
        connection
            .execute_batch(
                "
                PRAGMA foreign_keys = ON;
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                ",
            )
            .map_err(store_error)?;
        migrate(&mut connection)?;
        validate_persisted_records(&connection)?;

        let (worker, mut receiver) = mpsc::channel::<SqliteCommand>(SQLITE_WORK_QUEUE_CAPACITY);
        thread::Builder::new()
            .name("mdbase-runtime-sqlite".to_string())
            .spawn(move || {
                while let Some(command) = receiver.blocking_recv() {
                    command(&mut connection);
                }
            })
            .map_err(|error| {
                RuntimeError::Store(format!("could not start SQLite worker: {error}"))
            })?;
        Ok(Self { worker })
    }

    async fn execute<T, F>(&self, operation: F) -> RuntimeResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> RuntimeResult<T> + Send + 'static,
    {
        let (result_sender, result_receiver) = oneshot::channel();
        self.worker
            .send(Box::new(move |connection| {
                let _ = result_sender.send(operation(connection));
            }))
            .await
            .map_err(|_| RuntimeError::Store("SQLite worker is unavailable".to_string()))?;
        result_receiver
            .await
            .map_err(|_| RuntimeError::Store("SQLite worker stopped unexpectedly".to_string()))?
    }
}

#[async_trait]
impl RuntimeStore for SqliteRuntimeStore {
    async fn admit_event(&self, event: PreparedEvent) -> RuntimeResult<AdmitOutcome> {
        self.execute(move |connection| {
            let transaction = connection.transaction().map_err(store_error)?;
            let outcome = admit_tx(&transaction, event)?;
            transaction.commit().map_err(store_error)?;
            Ok(outcome)
        })
        .await
    }

    async fn claim_run(
        &self,
        executor: &str,
        worker: &str,
        now: DateTime<Utc>,
        lease_for: Duration,
    ) -> RuntimeResult<Option<Claim>> {
        let executor = executor.to_string();
        let worker = worker.to_string();
        self.execute(move |connection| {
            let transaction = connection.transaction().map_err(store_error)?;
            transaction
                .execute(
                    "UPDATE runtime_runs
                 SET lease_worker = NULL, lease_token = NULL, lease_expires_at = NULL
                 WHERE lease_expires_at <= ?1",
                    params![timestamp(now)],
                )
                .map_err(store_error)?;
            let candidate_json = {
                let mut statement = transaction
                    .prepare(
                        "SELECT record_json
                     FROM runtime_runs
                     WHERE executor = ?1
                       AND status IN ('queued', 'running')
                       AND not_before <= ?2
                       AND lease_token IS NULL
                     ORDER BY created_at, id
                     LIMIT 100",
                    )
                    .map_err(store_error)?;
                let rows = statement
                    .query_map(params![executor, timestamp(now)], |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(store_error)?;
                let mut selected = None;
                for row in rows {
                    let json = row.map_err(store_error)?;
                    let run: RunRecord = serde_json::from_str(&json)?;
                    if group_is_runnable_tx(&transaction, &run)? {
                        selected = Some(json);
                        break;
                    }
                }
                selected
            };
            let Some(json) = candidate_json else {
                transaction.commit().map_err(store_error)?;
                return Ok(None);
            };
            let mut run: RunRecord = serde_json::from_str(&json)?;
            if run.status == RunStatus::Queued {
                run.status = RunStatus::Running;
                run.started_at.get_or_insert(now);
                run.updated_at = now;
            }
            let token = format!("lease_{}", Ulid::new());
            let expires_at = add_duration(now, lease_for)?;
            let changed = transaction
                .execute(
                    "UPDATE runtime_runs
                 SET status = ?1, lease_worker = ?2, lease_token = ?3,
                     lease_expires_at = ?4, record_json = ?5
                 WHERE id = ?6 AND lease_token IS NULL AND revision = ?7",
                    params![
                        run_status(run.status),
                        worker,
                        token,
                        timestamp(expires_at),
                        serde_json::to_string(&run)?,
                        run.plan.id,
                        run.revision
                    ],
                )
                .map_err(store_error)?;
            if changed != 1 {
                transaction.rollback().map_err(store_error)?;
                return Ok(None);
            }
            transaction.commit().map_err(store_error)?;
            Ok(Some(Claim {
                run,
                worker,
                token,
                expires_at,
            }))
        })
        .await
    }

    async fn commit_run(
        &self,
        mut claim: Claim,
        emitted: Vec<PreparedEvent>,
    ) -> RuntimeResult<Claim> {
        self.execute(move |connection| {
            let transaction = connection.transaction().map_err(store_error)?;
            let current: Option<(Option<String>, Option<String>, i64)> = transaction
                .query_row(
                    "SELECT lease_worker, lease_token, revision
                 FROM runtime_runs WHERE id = ?1",
                    params![claim.run.plan.id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(store_error)?;
            let Some((worker, token, revision)) = current else {
                return Err(RuntimeError::diagnostic(
                    "stale_lease",
                    "Claimed run no longer exists.",
                ));
            };
            if worker.as_deref() != Some(claim.worker.as_str())
                || token.as_deref() != Some(claim.token.as_str())
                || u64::try_from(revision).ok() != Some(claim.run.revision)
            {
                return Err(RuntimeError::diagnostic(
                    "stale_lease",
                    "Run lease or revision changed before commit.",
                ));
            }
            claim.run.revision += 1;
            let terminal = claim.run.status.terminal();
            let changed = transaction
                .execute(
                    "UPDATE runtime_runs
                 SET status = ?1, revision = ?2, record_json = ?3,
                     lease_worker = CASE WHEN ?4 THEN NULL ELSE lease_worker END,
                     lease_token = CASE WHEN ?4 THEN NULL ELSE lease_token END,
                     lease_expires_at = CASE WHEN ?4 THEN NULL ELSE lease_expires_at END
                 WHERE id = ?5 AND lease_token = ?6 AND revision = ?7",
                    params![
                        run_status(claim.run.status),
                        claim.run.revision,
                        serde_json::to_string(&claim.run)?,
                        terminal,
                        claim.run.plan.id,
                        claim.token,
                        revision
                    ],
                )
                .map_err(store_error)?;
            if changed != 1 {
                return Err(RuntimeError::diagnostic(
                    "stale_lease",
                    "Run changed during commit.",
                ));
            }
            for event in emitted {
                admit_tx(&transaction, event)?;
            }
            transaction.commit().map_err(store_error)?;
            Ok(claim)
        })
        .await
    }

    async fn get_run(&self, id: &str) -> RuntimeResult<Option<RunRecord>> {
        let id = id.to_string();
        self.execute(move |connection| {
            let json = connection
                .query_row(
                    "SELECT record_json FROM runtime_runs WHERE id = ?1",
                    params![id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(store_error)?;
            json.map(|value| serde_json::from_str(&value).map_err(Into::into))
                .transpose()
        })
        .await
    }

    async fn events_after(&self, after: u64, limit: usize) -> RuntimeResult<EventPage> {
        self.execute(move |connection| {
            let retained_after = connection
                .query_row(
                    "SELECT value FROM runtime_meta WHERE key = 'retained_after'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(store_error)
                .and_then(nonnegative_cursor)?;
            let head = connection
                .query_row(
                    "SELECT COALESCE(MAX(cursor), 0) FROM runtime_event_dedup",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(store_error)
                .and_then(nonnegative_cursor)?;
            let reset_required = after < retained_after;
            let limit = limit.clamp(1, 10_000);
            let mut events = Vec::new();
            if !reset_required {
                let mut statement = connection
                    .prepare(
                        "SELECT cursor, source_runtime, event_id, envelope_json, received_at
                     FROM runtime_events WHERE cursor > ?1 ORDER BY cursor LIMIT ?2",
                    )
                    .map_err(store_error)?;
                let rows = statement
                    .query_map(
                        params![
                            i64::try_from(after).unwrap_or(i64::MAX),
                            i64::try_from(limit + 1).unwrap_or(i64::MAX)
                        ],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                            ))
                        },
                    )
                    .map_err(store_error)?;
                for row in rows {
                    let (cursor, source_runtime, event_id, envelope, received_at) =
                        row.map_err(store_error)?;
                    events.push(EventJournalEntry {
                        cursor: nonnegative_cursor(cursor)?,
                        source_runtime,
                        event_id,
                        envelope: serde_json::from_str(&envelope)?,
                        received_at: parse_timestamp(&received_at)?,
                    });
                }
            }
            let has_more = events.len() > limit;
            events.truncate(limit);
            Ok(EventPage {
                events,
                retained_after,
                head,
                has_more,
                reset_required,
            })
        })
        .await
    }

    async fn prune_events_through(&self, cursor: u64) -> RuntimeResult<u64> {
        self.execute(move |connection| {
            let transaction = connection.transaction().map_err(store_error)?;
            let head = transaction
                .query_row(
                    "SELECT COALESCE(MAX(cursor), 0) FROM runtime_event_dedup",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(store_error)
                .and_then(nonnegative_cursor)?;
            let current = transaction
                .query_row(
                    "SELECT value FROM runtime_meta WHERE key = 'retained_after'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(store_error)
                .and_then(nonnegative_cursor)?;
            let pruned_through = cursor.min(head).max(current);
            transaction
                .execute(
                    "DELETE FROM runtime_events WHERE cursor <= ?1",
                    params![i64::try_from(pruned_through).unwrap_or(i64::MAX)],
                )
                .map_err(store_error)?;
            transaction
                .execute(
                    "UPDATE runtime_meta SET value = ?1 WHERE key = 'retained_after'",
                    params![i64::try_from(pruned_through).unwrap_or(i64::MAX)],
                )
                .map_err(store_error)?;
            transaction.commit().map_err(store_error)?;
            Ok(pruned_through)
        })
        .await
    }

    async fn request_cancel(&self, id: &str, now: DateTime<Utc>) -> RuntimeResult<bool> {
        let id = id.to_string();
        self.execute(move |connection| {
            let transaction = connection.transaction().map_err(store_error)?;
            let json = transaction
                .query_row(
                    "SELECT record_json FROM runtime_runs WHERE id = ?1",
                    params![id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(store_error)?;
            let Some(json) = json else {
                return Ok(false);
            };
            let mut run: RunRecord = serde_json::from_str(&json)?;
            if !run.request_cancel(now) {
                return Ok(false);
            }
            run.revision += 1;
            let terminal = run.status.terminal();
            transaction
                .execute(
                    "UPDATE runtime_runs
                 SET status = ?1, revision = ?2, record_json = ?3,
                     lease_worker = CASE WHEN ?4 THEN NULL ELSE lease_worker END,
                     lease_token = CASE WHEN ?4 THEN NULL ELSE lease_token END,
                     lease_expires_at = CASE WHEN ?4 THEN NULL ELSE lease_expires_at END
                 WHERE id = ?5",
                    params![
                        run_status(run.status),
                        run.revision,
                        serde_json::to_string(&run)?,
                        terminal,
                        id
                    ],
                )
                .map_err(store_error)?;
            transaction.commit().map_err(store_error)?;
            Ok(true)
        })
        .await
    }

    async fn upsert_timer(&self, timer: TimerRecord) -> RuntimeResult<TimerRecord> {
        self.upsert_timer_impl(timer).await
    }

    async fn reconcile_timer_exact(
        &self,
        desired: TimerRecord,
        now: DateTime<Utc>,
    ) -> RuntimeResult<TimerRecord> {
        self.reconcile_timer_exact_impl(desired, now).await
    }

    async fn cancel_timer(
        &self,
        id: &str,
        generation: Option<u64>,
        now: DateTime<Utc>,
    ) -> RuntimeResult<bool> {
        self.cancel_timer_impl(id, generation, now).await
    }

    async fn reconcile_timers(
        &self,
        id_prefix: &str,
        desired: Vec<TimerRecord>,
        now: DateTime<Utc>,
    ) -> RuntimeResult<TimerReconcileOutcome> {
        self.reconcile_timers_impl(id_prefix, desired, now).await
    }

    async fn timers(&self, id_prefix: &str) -> RuntimeResult<Vec<TimerRecord>> {
        self.timers_impl(id_prefix).await
    }

    async fn claim_due_timer(
        &self,
        worker: &str,
        now: DateTime<Utc>,
        lease_for: Duration,
    ) -> RuntimeResult<Option<TimerClaim>> {
        self.claim_due_timer_impl(worker, now, lease_for).await
    }

    async fn fire_timer(
        &self,
        claim: TimerClaim,
        fired: TimerRecord,
        event: PreparedEvent,
    ) -> RuntimeResult<AdmitOutcome> {
        self.fire_timer_impl(claim, fired, event).await
    }

    async fn snapshot(&self) -> RuntimeResult<StoreSnapshot> {
        self.execute(move |connection| {
            let events = {
                let mut statement = connection
                    .prepare(
                        "SELECT cursor, source_runtime, event_id, envelope_json, received_at
                     FROM runtime_events ORDER BY cursor",
                    )
                    .map_err(store_error)?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    })
                    .map_err(store_error)?;
                rows.map(|row| {
                    let (cursor, source_runtime, event_id, envelope, received_at) =
                        row.map_err(store_error)?;
                    Ok(EventJournalEntry {
                        cursor: u64::try_from(cursor).map_err(|_| {
                            RuntimeError::Store("negative event cursor".to_string())
                        })?,
                        source_runtime,
                        event_id,
                        envelope: serde_json::from_str(&envelope)?,
                        received_at: parse_timestamp(&received_at)?,
                    })
                })
                .collect::<RuntimeResult<Vec<_>>>()?
            };
            let runs = read_json_column::<RunRecord>(connection, "runtime_runs")?;
            let timers = read_json_column::<TimerRecord>(connection, "runtime_timers")?;
            Ok(StoreSnapshot {
                events,
                runs,
                timers,
            })
        })
        .await
    }
}

fn admit_tx(transaction: &Transaction<'_>, event: PreparedEvent) -> RuntimeResult<AdmitOutcome> {
    let existing = transaction
        .query_row(
            "SELECT cursor FROM runtime_event_dedup
             WHERE source_runtime = ?1 AND event_id = ?2",
            params![event.source_runtime, event.event_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(store_error)?;
    if let Some(cursor) = existing {
        return Ok(AdmitOutcome {
            cursor: u64::try_from(cursor)
                .map_err(|_| RuntimeError::Store("negative event cursor".to_string()))?,
            duplicate: true,
            admitted_run_ids: Vec::new(),
            skipped_run_ids: Vec::new(),
            cancellation_requested_run_ids: Vec::new(),
        });
    }
    transaction
        .execute(
            "INSERT INTO runtime_events
                (source_runtime, event_id, envelope_json, received_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                event.source_runtime,
                event.event_id,
                serde_json::to_string(&event.envelope)?,
                timestamp(event.received_at)
            ],
        )
        .map_err(store_error)?;
    let cursor = u64::try_from(transaction.last_insert_rowid())
        .map_err(|_| RuntimeError::Store("negative event cursor".to_string()))?;
    transaction
        .execute(
            "INSERT INTO runtime_event_dedup(source_runtime, event_id, cursor)
             VALUES (?1, ?2, ?3)",
            params![
                event.source_runtime,
                event.event_id,
                i64::try_from(cursor).unwrap_or(i64::MAX)
            ],
        )
        .map_err(store_error)?;

    let mut admitted = Vec::new();
    let mut skipped = Vec::new();
    let mut cancellation_requested = Vec::new();
    for mut plan in event.runs {
        plan.event_cursor = cursor;
        if minimum_interval_suppresses_tx(transaction, &plan)? {
            skipped.push(plan.id);
            continue;
        }
        if plan.not_before > event.received_at {
            transaction
                .execute(
                    "DELETE FROM runtime_runs
                     WHERE workflow = ?1 AND trigger_id = ?2 AND executor = ?3
                       AND status = 'queued' AND not_before > ?4",
                    params![
                        plan.workflow,
                        plan.trigger,
                        plan.executor,
                        timestamp(event.received_at)
                    ],
                )
                .map_err(store_error)?;
        }
        let duplicate = transaction
            .query_row(
                "SELECT 1 FROM runtime_runs
                 WHERE idempotency_scope = ?1 AND idempotency_key = ?2",
                params![plan.idempotency_scope, plan.idempotency_key],
                |_| Ok(()),
            )
            .optional()
            .map_err(store_error)?
            .is_some();
        if duplicate {
            skipped.push(plan.id);
            continue;
        }
        let active = active_group_runs_tx(transaction, &plan.concurrency_group)?;
        if plan.concurrency_policy == ConcurrencyPolicy::Skip && !active.is_empty() {
            skipped.push(plan.id);
            continue;
        }
        if plan.concurrency_policy == ConcurrencyPolicy::Replace {
            plan.replacement_blockers = active.clone();
            for id in active {
                let json: String = transaction
                    .query_row(
                        "SELECT record_json FROM runtime_runs WHERE id = ?1",
                        params![id],
                        |row| row.get(0),
                    )
                    .map_err(store_error)?;
                let mut run: RunRecord = serde_json::from_str(&json)?;
                if !run.request_cancel(event.received_at) {
                    continue;
                }
                run.revision += 1;
                let terminal = run.status.terminal();
                transaction
                    .execute(
                        "UPDATE runtime_runs
                         SET status = ?1, revision = ?2, record_json = ?3,
                             lease_worker = CASE WHEN ?4 THEN NULL ELSE lease_worker END,
                             lease_token = CASE WHEN ?4 THEN NULL ELSE lease_token END,
                             lease_expires_at = CASE WHEN ?4 THEN NULL ELSE lease_expires_at END
                         WHERE id = ?5",
                        params![
                            run_status(run.status),
                            run.revision,
                            serde_json::to_string(&run)?,
                            terminal,
                            run.plan.id
                        ],
                    )
                    .map_err(store_error)?;
                cancellation_requested.push(id);
            }
        }
        let run = RunRecord::admitted(plan);
        transaction
            .execute(
                "INSERT INTO runtime_runs
                    (id, executor, workflow, trigger_id, status, created_at, not_before,
                     idempotency_scope, idempotency_key, concurrency_group,
                     concurrency_policy, revision, record_json)
                 VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?6, ?7, ?8, ?9, ?10, 0, ?11)",
                params![
                    run.plan.id,
                    run.plan.executor,
                    run.plan.workflow,
                    run.plan.trigger,
                    timestamp(run.created_at),
                    timestamp(run.plan.not_before),
                    run.plan.idempotency_scope,
                    run.plan.idempotency_key,
                    run.plan.concurrency_group,
                    concurrency_policy(run.plan.concurrency_policy),
                    serde_json::to_string(&run)?
                ],
            )
            .map_err(store_error)?;
        admitted.push(run.plan.id);
    }
    cancellation_requested.sort();
    cancellation_requested.dedup();
    Ok(AdmitOutcome {
        cursor,
        duplicate: false,
        admitted_run_ids: admitted,
        skipped_run_ids: skipped,
        cancellation_requested_run_ids: cancellation_requested,
    })
}

fn minimum_interval_suppresses_tx(
    transaction: &Transaction<'_>,
    plan: &crate::model::PlannedRun,
) -> RuntimeResult<bool> {
    let Some(interval) = plan.minimum_interval_ms else {
        return Ok(false);
    };
    let earliest =
        plan.created_at - TimeDelta::milliseconds(i64::try_from(interval).unwrap_or(i64::MAX));
    Ok(transaction
        .query_row(
            "SELECT 1 FROM runtime_runs
             WHERE workflow = ?1 AND trigger_id = ?2 AND created_at > ?3
             LIMIT 1",
            params![plan.workflow, plan.trigger, timestamp(earliest)],
            |_| Ok(()),
        )
        .optional()
        .map_err(store_error)?
        .is_some())
}

fn active_group_runs_tx(transaction: &Transaction<'_>, group: &str) -> RuntimeResult<Vec<String>> {
    let mut statement = transaction
        .prepare(
            "SELECT id FROM runtime_runs
             WHERE concurrency_group = ?1 AND status IN ('queued', 'running', 'waiting')
             ORDER BY id",
        )
        .map_err(store_error)?;
    let result = statement
        .query_map(params![group], |row| row.get::<_, String>(0))
        .map_err(store_error)?
        .map(|row| row.map_err(store_error))
        .collect();
    result
}

fn group_is_runnable_tx(transaction: &Transaction<'_>, run: &RunRecord) -> RuntimeResult<bool> {
    if run.plan.concurrency_policy == ConcurrencyPolicy::Allow {
        return Ok(true);
    }
    let others = {
        let mut statement = transaction
            .prepare(
                "SELECT record_json FROM runtime_runs
                 WHERE concurrency_group = ?1 AND id != ?2",
            )
            .map_err(store_error)?;
        let rows = statement
            .query_map(params![run.plan.concurrency_group, run.plan.id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(store_error)?
            .map(|row| {
                row.map_err(store_error)
                    .and_then(|json| serde_json::from_str::<RunRecord>(&json).map_err(Into::into))
            })
            .collect::<RuntimeResult<Vec<_>>>()?;
        rows
    };
    let blocked_by_group = others.iter().any(|other| {
        matches!(other.status, RunStatus::Running | RunStatus::Waiting)
            || (other.status == RunStatus::Queued
                && (&other.plan.event_cursor, &other.plan.id)
                    < (&run.plan.event_cursor, &run.plan.id))
    });
    if blocked_by_group {
        return Ok(false);
    }
    Ok(!others.iter().any(|other| {
        run.plan.replacement_blockers.contains(&other.plan.id)
            && other.status == RunStatus::Indeterminate
    }))
}

fn read_json_column<T: serde::de::DeserializeOwned>(
    connection: &Connection,
    table: &str,
) -> RuntimeResult<Vec<T>> {
    let sql = format!("SELECT record_json FROM {table} ORDER BY id");
    let mut statement = connection.prepare(&sql).map_err(store_error)?;
    let result = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(store_error)?
        .map(|row| {
            let json = row.map_err(store_error)?;
            serde_json::from_str(&json).map_err(Into::into)
        })
        .collect();
    result
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
                implementation: "sqlite".to_string(),
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
        let store = SqliteRuntimeStore::in_memory().unwrap();
        let now = Utc::now();
        let exhausted = store.upsert_timer(timer("max:timer", now)).await.unwrap();
        store.upsert_timer(timer("max:omitted", now)).await.unwrap();
        let mut exhausted_at_max = exhausted;
        exhausted_at_max.generation = TIMER_GENERATION_MAX;
        let json = serde_json::to_string(&exhausted_at_max).unwrap();
        store
            .execute(move |connection| {
                connection
                    .execute(
                        "UPDATE runtime_timers SET generation = ?1, record_json = ?2 WHERE id = ?3",
                        params![i64::MAX, json, "max:timer"],
                    )
                    .map_err(store_error)?;
                Ok(())
            })
            .await
            .unwrap();
        let before = store.snapshot().await.unwrap().timers;
        let mut changed = exhausted_at_max;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_database_work_does_not_stall_the_async_executor() {
        let store = SqliteRuntimeStore::in_memory().unwrap();
        let database_work = tokio::spawn(async move {
            store
                .execute(|_| {
                    std::thread::sleep(Duration::from_millis(100));
                    Ok(())
                })
                .await
        });
        let heartbeat = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            "executor-responsive"
        };
        let heartbeat = tokio::time::timeout(Duration::from_millis(50), heartbeat)
            .await
            .expect("dedicated SQLite work must not starve the Tokio executor");
        assert_eq!(heartbeat, "executor-responsive");
        database_work.await.unwrap().unwrap();
    }

    #[test]
    fn recovery_inspection_is_read_only_and_reports_only_actionable_work() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.sqlite");
        assert_eq!(
            inspect_sqlite_recovery(&missing, Utc::now()).unwrap(),
            SqliteRecoveryState::default()
        );
        assert!(!missing.exists());

        let path = directory.path().join("runtime.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "PRAGMA user_version = 2;
                 CREATE TABLE runtime_timers(status TEXT NOT NULL, fire_at TEXT NOT NULL);
                 CREATE TABLE runtime_runs(
                    status TEXT NOT NULL,
                    not_before TEXT NOT NULL,
                    lease_token TEXT,
                    lease_expires_at TEXT
                 );",
            )
            .unwrap();
        let now = Utc::now();
        connection
            .execute(
                "INSERT INTO runtime_timers(status, fire_at) VALUES ('scheduled', ?1)",
                [timestamp(now + TimeDelta::minutes(5))],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO runtime_runs(status, not_before) VALUES ('queued', ?1)",
                [timestamp(now + TimeDelta::minutes(5))],
            )
            .unwrap();
        assert_eq!(
            inspect_sqlite_recovery(&path, now).unwrap(),
            SqliteRecoveryState::default()
        );
        connection
            .execute(
                "INSERT INTO runtime_timers(status, fire_at) VALUES ('firing', ?1)",
                [timestamp(now)],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO runtime_runs(status, not_before) VALUES ('queued', ?1)",
                [timestamp(now)],
            )
            .unwrap();
        assert_eq!(
            inspect_sqlite_recovery(&path, now).unwrap(),
            SqliteRecoveryState {
                due_timers: true,
                pending_runs: true,
            }
        );
    }
}
