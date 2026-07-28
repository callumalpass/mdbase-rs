# mdbase-runtime

`mdbase-runtime` 0.3 is the durable workflow engine for mdbase Runtime
companion profile 0.2. It is independently versioned from the `mdbase`
collection crate.

The crate provides:

- admission from ordinary core contracts and projected workflow/policy records
- exact event-source/action-provider pinning through `mdbase-interop`
- deterministic event-to-workflow planning using canonical mdbase CEL
- durable event journalling and deduplication
- idempotency, concurrency groups, leases, cancellation, and crash recovery
- stable action invocation IDs and explicit indeterminate outcomes
- shared action invocation/outcome envelopes with host-owned final authorization
- in-memory, SQLite, and namespace-fenced PostgreSQL runtime stores
- generation-safe one-shot timers with `fire_once` missed-run behaviour
- cursor-based journal consumption and bounded retention with dedupe tombstones

It deliberately does not contain a second contract registry, Connect routing,
Web Push credentials, local filesystem grants, or application-specific
actions. Installing Markdown never registers executable code.

## Minimal host

```rust,no_run
use std::sync::Arc;
use mdbase_runtime::{InMemoryRuntimeStore, Runtime};

# fn example() -> Result<(), Box<dyn std::error::Error>> {
let store = Arc::new(InMemoryRuntimeStore::new());
let runtime = Runtime::builder(store).build()?;

// The builder denies every effectful dispatch until the embedding host
// installs an explicit DispatchAuthorizer.
# let _ = runtime;
# Ok(())
# }
```

Build an `AdmissionCatalog` from core-verified contract artifacts, projected
`runtime_workflow`/`runtime_policy` values, and verified interop declarations.
Register live handlers through `ProviderRegistry` with the exact
`ProviderBinding { provider_declaration_digest, handler_id }`. Then call
`Runtime::deliver_event(&catalog, cloud_event)` and run workers with
`Runtime::work_once()`.

An alternative authority such as Connect may prepare the exact immutable plan
and call `Runtime::deliver_prepared_event`. Neither path consults a live
registry during worker execution.

## Timers

`Runtime::upsert_timer` and `Runtime::cancel_timer` manage individual one-shot
timers. Hosts projecting application state should prefer
`Runtime::reconcile_timers`: it atomically makes every timer under an ID prefix
match a desired set. Identical scheduled or fired timers retain their
generation and status, changed timers receive the next generation, new timers
are scheduled, and active omitted timers are cancelled. `Runtime::timers`
lists only the requested prefix without loading unrelated runtime state.

Prefixes are a host authorization boundary, not an application credential.
Embedding hosts must derive them from their authenticated tenant or grant and
must reject desired timer IDs outside that prefix.

## Storage

The default `sqlite` feature is intended for one local authority process:

```rust,no_run
use mdbase_runtime::SqliteRuntimeStore;

let store = SqliteRuntimeStore::open(".mdbase/runtime/execution.sqlite")?;
# Ok::<(), mdbase_runtime::RuntimeError>(())
```

Enable `postgres` for horizontally scaled hosts. Each collection or tenant
must receive a stable, non-secret namespace; workers sharing that namespace
share its event cursors, idempotency reservations, leases, and timers.

```rust,no_run
use mdbase_runtime::PostgresRuntimeStore;

# async fn example(database_url: &str) -> Result<(), mdbase_runtime::RuntimeError> {
let store = PostgresRuntimeStore::connect(database_url, "collection:01J0").await?;
# let _ = store;
# Ok(())
# }
```

SQLite and PostgreSQL use explicit schema version 1 with transactional
migrations and reject unknown newer schemas. SQLite work runs on a bounded
dedicated thread so `rusqlite` never blocks Tokio executor workers. PostgreSQL
schema migration is serialized with an advisory lock; admission is serialized
per namespace so debounce, minimum interval, idempotency, and concurrency
decisions remain one atomic boundary while independent action execution stays
parallel.

See [`docs/runtime-store.md`](../../docs/runtime-store.md) for backend
selection, migration behavior, and the mandatory live PostgreSQL test.

## Safety model

Admission validates contract/source/provider compatibility and policy.
Immediately before every provider call, the runtime calls the embedding host's
`DispatchAuthorizer`; for Connect, that authorizer must enforce the current
locally cached exact grant.

Providers that declare request idempotency receive the same request,
invocation, and attempt evidence after an ambiguous crash. Unsafe providers
are never replayed automatically; the run becomes `indeterminate`.

Admitted plans contain exact action artifacts, provider identities,
declaration digests, and handler IDs. Later catalog changes cannot silently
replace any admitted dependency.

`Runtime::cancel_run` durably records intent first. Queued work becomes
terminal immediately; an active cooperative provider receives a bounded
best-effort cancellation signal and the returned outcome reports whether that
signal was acknowledged.

Queued runs occupy their concurrency group. `skip` therefore rejects work
behind a queued predecessor, `queue` preserves event-cursor order even when an
earlier run is not ready, and `replace` cancels queued predecessors
transactionally. `DeliveryOutcome::cancellation_requested_run_ids` identifies
active predecessors for which replacement recorded durable cancellation
intent. If a cancelled cooperative dispatch has no durable outcome after
recovery, it is not replayed: the predecessor becomes `indeterminate` and its
replacement remains queued until that ambiguity is explicitly resolved.
