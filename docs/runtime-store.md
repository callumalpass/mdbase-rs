# Runtime Store Operations

`mdbase-runtime` 0.2 provides one behavioral contract across memory, SQLite,
and PostgreSQL stores. Persistent stores install and migrate their own schema;
collection Markdown and query caches are separate concerns.

## Backend choice

- `InMemoryRuntimeStore` is deterministic and useful for tests or ephemeral
  hosts.
- `SqliteRuntimeStore` is the default for one local authority process.
- `PostgresRuntimeStore` supports horizontally scaled workers sharing a stable
  namespace.

SQLite is executed on a dedicated worker thread behind a bounded request
channel. No `rusqlite` call runs on a Tokio executor worker. Backpressure is
therefore explicit instead of appearing as unbounded blocking tasks.

## Schema versions

The current SQLite and PostgreSQL schema version is `1`, exported as
`SQLITE_SCHEMA_VERSION` and `POSTGRES_SCHEMA_VERSION`.

Store open:

1. creates the latest schema when the database is empty;
2. applies supported migrations transactionally;
3. records the new version only after commit;
4. rejects a schema newer than this crate understands.

SQLite uses `PRAGMA user_version`. PostgreSQL uses a global schema-version
table and an advisory migration lock, so multiple namespaces or hosts cannot
race schema installation.

Applications may call `schema_version()` for readiness reporting. Do not
modify version metadata independently of the schema.

## PostgreSQL namespaces

A namespace is the durable isolation and coordination key for event cursors,
deduplication, idempotency, leases, concurrency groups, and timers. Derive it
from the authenticated tenant/collection boundary. It is stable and
non-secret; it is not an authorization credential.

Workers sharing a namespace share runtime state. Independent tenants must not
reuse a namespace.

## Failure and recovery behavior

The shared backend contract covers:

- event admission and deduplication;
- skip, queue, and replace concurrency;
- claim leases and stale-token rejection;
- durable cancellation intent;
- journal retention and cursor reset;
- timer reconciliation, claim, and fire-once behavior.

Unsafe provider calls are never silently replayed after an ambiguous outcome.
Their run becomes `indeterminate`. Provider calls remain behind the embedding
host's final `DispatchAuthorizer`.

## Qualification

Run feature boundaries locally:

```bash
cargo test --locked -p mdbase-runtime --no-default-features
cargo test --locked -p mdbase-runtime --no-default-features --features sqlite
cargo check --locked -p mdbase-runtime --no-default-features --features postgres
```

Run the mandatory live PostgreSQL contracts against a disposable database:

```bash
MDBASE_RUNTIME_REQUIRE_POSTGRES=1 \
MDBASE_RUNTIME_TEST_DATABASE_URL=postgres://postgres:password@127.0.0.1/mdbase_runtime_test \
cargo test --locked -p mdbase-runtime --all-features \
  --test postgres --test store_contract
```

When `MDBASE_RUNTIME_REQUIRE_POSTGRES=1`, a missing database URL is a hard test
failure. Without that flag, PostgreSQL tests may skip for normal local
development.
