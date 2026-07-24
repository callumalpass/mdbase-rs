# mdbase-runtime

`mdbase-runtime` is the durable workflow engine for mdbase Runtime profile 0.1.
It is independently versioned from the `mdbase` collection crate.

The crate provides:

- deterministic event-to-workflow planning using canonical mdbase CEL
- durable event journalling and deduplication
- idempotency, concurrency groups, leases, cancellation, and crash recovery
- stable action invocation IDs and explicit indeterminate outcomes
- provider-neutral dispatch with host-owned final authorization
- in-memory, SQLite, and namespace-fenced PostgreSQL runtime stores
- generation-safe one-shot timers with `fire_once` missed-run behaviour
- cursor-based journal consumption and bounded retention with dedupe tombstones

It deliberately does not contain Connect routing, Web Push credentials, local
filesystem grants, or application-specific actions.

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

Register action handlers through `ProviderRegistry`, then call
`Runtime::deliver_event` and run one or more workers with `Runtime::work_once`.
Use `ManualClock` for deterministic scheduling and recovery tests.

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

Schema installation is idempotent. PostgreSQL admission is serialized per
namespace so debounce, minimum interval, idempotency, and concurrency decisions
remain one atomic boundary while independent action execution stays parallel.

## Safety model

Preflight is advisory. Immediately before every provider call, the runtime
checks canonical policy and calls the embedding host's `DispatchAuthorizer`.
For mdbase Connect, that authorizer must enforce the locally cached exact grant.

Providers that declare invocation-ID idempotency receive the same
`invocation_id` after an ambiguous crash. Unsafe providers are never replayed
automatically; the run becomes `indeterminate`.

Admitted plans contain canonical action snapshots. Later registry changes can
tighten dispatch-time policy and hosts always re-authorize against current
grants, but they cannot silently replace the schemas or effect declaration of
an already admitted action.

`Runtime::cancel_run` durably records intent first. Queued work becomes
terminal immediately; an active cooperative provider receives a bounded
best-effort cancellation signal and the returned outcome reports whether that
signal was acknowledged.
