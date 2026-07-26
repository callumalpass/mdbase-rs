# mdbase-rs

Typed Rust implementation of the [mdbase specification](https://mdbase.dev).
The `0.4.0-rc.1` release is a deliberate breaking API release: canonical v0.3
collection semantics now sit behind typed requests, results, paths, revisions,
diagnostics, and errors.

It includes:

- Typed frontmatter loading/validation
- Query execution (filters, formulas, grouping, summaries)
- Link parsing/resolution/traversal
- Typed CRUD, crash-recoverable batch, backfill, and migration operations
- Runtime Contracts 0.1 registry composition and preflight
- Independently versioned durable workflow execution
- Fail-safe SQLite query caching with authoritative disk fallback
- Debounced filesystem watching with normalized collection events

Canonical v0.3 records are validated against JSON Schema 2020-12 and report
structured diagnostics. Legacy v0.2 collections remain readable and queryable
through an isolated read-only adapter. Mutations fail with
`migration_required` until an explicit, verified migration is applied.

## Typed Rust API

```rust,no_run
use std::path::Path;
use mdbase::api::{QueryDirection, QueryRequest, ReadRequest};
use mdbase::Collection;

let collection = Collection::open(Path::new("./notes")).expect("open collection");
let records = collection.typed().expect("typed API");

let record = records
    .read(ReadRequest::new("tasks/example.md").expect("valid path"))
    .expect("read record");
println!("revision: {}", record.value.revision);

let page = records
    .query(
        QueryRequest::builder()
            .type_name("task")
            .where_expression("status == 'open'")
            .order_by("file.path", QueryDirection::Asc)
            .limit(100),
    )
    .expect("query records");
assert!(page.value.total_count >= page.value.records.len());
```

`CollectionPath` rejects absolute, traversal, ambiguous, and platform-specific
paths before an operation starts. Mutations accept opaque `Revision`
preconditions. `allow_partial: false` batches stage and journal all changes,
then recover deterministically after interruption.

See [the Rust API guide](docs/rust-api.md) and
[the v0.2 migration guide](docs/migration-v02-to-v03.md).

## CLI

`mdb init` creates a canonical v0.3 collection. CRUD/query commands use the
same typed service as the Rust API and emit a consistent
`{ valid, result, diagnostics }` JSON envelope.

```bash
mdb -C ./notes read tasks/example.md
mdb -C ./notes update tasks/example.md \
  --fields '{"status":"done"}' \
  --if-revision sha256:...
mdb -C ./notes query --request query.json
mdb -C ./notes batch --request batch.json
```

Use `--dry-run` on mutations. Rename updates references by default; pass
`--no-update-refs` to opt out.

## Runtime Contracts

`mdbase::runtime_contracts::RuntimeContracts` is a pure registry and preflight
engine. It loads materialized contract records from normal collection scope and
composes them with built-in, provider, or pack contracts supplied in memory.
`ContractDocument::virtual_contract` is the first-class representation for a
non-materialized contract.

The contract engine validates strict provider, action, event, capability, policy, and
workflow shapes; compiles embedded schemas once; validates event and action
values; resolves workflow requirements; and renders contracts as Markdown when
materialization is requested. It grants no filesystem authority.

The independently versioned `mdbase-runtime` workspace crate adds durable event
admission, deterministic workflow planning, stable action invocation IDs,
leases, cancellation, crash recovery, cursor retention, and one-shot timers. It
supports in-memory and SQLite stores by default and a horizontally safe
PostgreSQL store behind the `postgres` feature. Provider calls remain neutral:
an embedding host such as mdbase-connect is the final authorization boundary
immediately before every dispatch. See
[`crates/mdbase-runtime/README.md`](crates/mdbase-runtime/README.md).

`FilesystemProvider::load_runtime_contracts` performs loading under the same
serialization gate as collection requests. A watcher opened with
`CollectionWatcher::open_with_runtime_contracts` recomposes the effective
registry and emits `runtime_registry_changed` only when its stable registry
revision changes; virtual sources never need to be written to the collection.

## Runtime Observability

`FilesystemProvider::open_observed` and `FilesystemRuntime::open_observed`
accept a `RuntimeObserver`. Every dispatched operation and Runtime Contracts
load reports payload-free timing for queue, collection open, execution,
synchronization, and total duration, including provider-level failures. Error
observations are disabled by default and can opt into stable codes or local
messages with `ErrorReporting`.

Enable the `tracing` Cargo feature to use `TracingObserver`. It writes timings
to the `mdbase::performance` target and optional errors to `mdbase::errors`.
Neither observation shape contains collection paths, request values, or record
payloads.

`mdbase::watch::CollectionWatcher` observes real filesystem changes, debounces
atomic-save sequences, and emits final-state record, type, and configuration
events. The legacy v0.2 conformance adapter retains a deterministic simulation
path for fixture-driven watch tests.

## Packages

- Library crate: `mdbase`
- Workflow crate: `mdbase-runtime` (Runtime profile 0.1)
- CLI binary: `mdb` (`init`, CRUD/query/validate, `backfill`, `migrate`, cache)
- Profiling binary: `mdb-profile` (synthetic workload profiler with JSON output)

## Build

```bash
cargo build --locked
```

Rust 1.94.0 is the minimum supported compiler and is pinned by
`rust-toolchain.toml`.

## Test

```bash
cargo test --locked --workspace --all-features
cargo test --locked -p mdbase-runtime --no-default-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo deny check
```

## Performance Profiling

Run the profiler against a synthetic, deterministic dataset:

```bash
./scripts/benchmark-release.sh
```

This executes the versioned 5,000-record release workload, writes
`target/benchmarks/release.json`, and fails if any p95 budget in
`benchmarks/release-v1.json` is exceeded. See
[the recorded release qualification](docs/performance/0.4.0-rc.1.md).

The default `queries` scenario is the fast feedback loop. It exercises the
canonical v0.3 query facade, reports payload-free phase timings, and includes
the editor's two-pass paginated index workload:

```bash
./scripts/profile.sh --scenario queries --files 5000
./scripts/profile.sh --scenario queries --files 5000 --json \
  --output .ops/profile/query.json
```

Profile metadata paging and the editor workload against an existing collection
without mutating records (the report redacts the collection path):

```bash
./scripts/profile.sh --collection /path/to/collection --editor-iters 3
```

Use `--scenario core` for CRUD, rename/reference updates, runtime startup, and
mutation-plus-watcher synchronization. Use `--scenario all` before handing off
a performance-sensitive change. Reports include latency percentiles (`p50`,
`p95`, `p99`), throughput, query phases and plan counters.

Set `MDBASE_WATCH_PROFILE=1` to print payload-free watcher invalidation mode,
record counts, and refresh time. For CPU sampling with symbols:

```bash
cargo build --profile profiling --bin mdb-profile
perf record -g --call-graph dwarf -- \
  target/profiling/mdb-profile --scenario queries --files 5000
perf report
```

The `profiling` Cargo profile retains debug symbols while keeping release
optimizations. `samply record` or `cargo flamegraph` can be used in place of
`perf record` on systems where those tools are preferred.

## Notes

- v0.2 conformance coverage is tracked in `tests/conformance.rs`; the shared
  v0.3 adapters are `tests/v03_conformance.rs` and
  `tests/v03_runtime_conformance.rs`.
- Release-level changes are in `CHANGELOG.md`.
