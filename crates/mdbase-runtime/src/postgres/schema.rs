use serde_json::Value;
use sqlx::{PgPool, Row};

use super::{store_error, POSTGRES_MIGRATION_LOCK, POSTGRES_SCHEMA_VERSION};
use crate::error::{RuntimeError, RuntimeResult};
use crate::model::{RunRecord, TimerRecord};

pub(super) async fn migrate(pool: &PgPool) -> RuntimeResult<()> {
    let mut transaction = pool.begin().await.map_err(store_error)?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(POSTGRES_MIGRATION_LOCK)
        .execute(&mut *transaction)
        .await
        .map_err(store_error)?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mdbase_runtime_schema (
            singleton boolean PRIMARY KEY DEFAULT TRUE CHECK (singleton),
            version integer NOT NULL CHECK (version >= 1)
        )",
    )
    .execute(&mut *transaction)
    .await
    .map_err(store_error)?;
    let installed = sqlx::query_scalar::<_, i32>(
        "SELECT version FROM mdbase_runtime_schema WHERE singleton = TRUE",
    )
    .fetch_optional(&mut *transaction)
    .await
    .map_err(store_error)?
    .unwrap_or(0);
    let installed = u32::try_from(installed)
        .map_err(|_| RuntimeError::Store("negative PostgreSQL schema version".to_string()))?;
    if installed > POSTGRES_SCHEMA_VERSION {
        transaction.rollback().await.map_err(store_error)?;
        return Err(RuntimeError::diagnostic(
            "runtime_schema_too_new",
            format!(
                "PostgreSQL runtime schema version {installed} is newer than supported version {POSTGRES_SCHEMA_VERSION}."
            ),
        ));
    }
    let mut version = installed;
    if version == 0 {
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
        .execute(&mut *transaction)
        .await
        .map_err(store_error)?;
        version = 1;
    }
    if version == 1 {
        // Runtime profile 0.2 made the persisted run and timer model
        // incompatible with profile 0.1 (including exact string versions and
        // implementation identities). Those values cannot be reconstructed
        // safely from the old JSON. This prerelease boundary deliberately
        // discards the runtime-owned execution journal; embedding-host source
        // outboxes remain authoritative and can replay work that was not
        // marked processed.
        sqlx::raw_sql(
            "
            DELETE FROM mdbase_runtime_timers;
            DELETE FROM mdbase_runtime_runs;
            DELETE FROM mdbase_runtime_event_dedup;
            DELETE FROM mdbase_runtime_events;
            DELETE FROM mdbase_runtime_meta;
            ",
        )
        .execute(&mut *transaction)
        .await
        .map_err(store_error)?;
        version = 2;
    }
    if version != POSTGRES_SCHEMA_VERSION {
        transaction.rollback().await.map_err(store_error)?;
        return Err(RuntimeError::Store(format!(
            "PostgreSQL runtime migration stopped at version {version}; expected {POSTGRES_SCHEMA_VERSION}."
        )));
    }
    sqlx::query(
        "INSERT INTO mdbase_runtime_schema(singleton, version)
         VALUES (TRUE, $1)
         ON CONFLICT(singleton) DO UPDATE SET version = excluded.version",
    )
    .bind(i32::try_from(POSTGRES_SCHEMA_VERSION).expect("schema version fits PostgreSQL integer"))
    .execute(&mut *transaction)
    .await
    .map_err(store_error)?;
    transaction.commit().await.map_err(store_error)
}

pub(super) async fn validate_persisted_records(pool: &PgPool) -> RuntimeResult<()> {
    for row in sqlx::query("SELECT namespace, id, record_json FROM mdbase_runtime_runs")
        .fetch_all(pool)
        .await
        .map_err(store_error)?
    {
        let namespace: String = row.try_get("namespace").map_err(store_error)?;
        let id: String = row.try_get("id").map_err(store_error)?;
        let value: Value = row.try_get("record_json").map_err(store_error)?;
        serde_json::from_value::<RunRecord>(value).map_err(|error| {
            RuntimeError::diagnostic(
                "invalid_persisted_runtime_record",
                format!("Runtime run {namespace}/{id} is incompatible with this build: {error}"),
            )
        })?;
    }
    for row in sqlx::query("SELECT namespace, id, record_json FROM mdbase_runtime_timers")
        .fetch_all(pool)
        .await
        .map_err(store_error)?
    {
        let namespace: String = row.try_get("namespace").map_err(store_error)?;
        let id: String = row.try_get("id").map_err(store_error)?;
        let value: Value = row.try_get("record_json").map_err(store_error)?;
        serde_json::from_value::<TimerRecord>(value).map_err(|error| {
            RuntimeError::diagnostic(
                "invalid_persisted_runtime_record",
                format!("Runtime timer {namespace}/{id} is incompatible with this build: {error}"),
            )
        })?;
    }
    Ok(())
}
