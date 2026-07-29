# mdbase-rs

Typed Rust implementation of the [mdbase specification](https://mdbase.dev).
The `0.4.0-rc.3` release is a deliberate breaking API release: canonical v0.3
collection semantics now sit behind typed requests, results, paths, revisions,
diagnostics, and errors.

It includes:

- Typed frontmatter loading/validation
- Query execution (filters, formulas, grouping, summaries)
- Link parsing/resolution/traversal
- Typed CRUD, crash-recoverable batch, backfill, and migration operations
- One core contract/type implementation model for record, event, and action contracts
- Independently versioned durable Runtime companion profile 0.2
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

## Unified CLI integration

This repository intentionally does not publish a competing executable.
`mdbase-command` is the transport-neutral command adapter used by the final
`mdbase` executable in the adjacent `mdbase-connect` repository. It owns
argument-to-operation mapping, canonical output envelopes, watch streaming,
and deterministic engine workloads, but no daemon, service, or process
lifecycle.

The unified executable can open a filesystem root directly or send the same
portable operation through Connect:

```bash
mdbase --root ./notes read tasks/example.md
mdbase --root ./notes update tasks/example.md \
  --fields '{"status":"done"}' \
  --if-revision sha256:...
mdbase --root ./notes query --request query.json
mdbase --root ./notes batch --request batch.json
mdbase --collection <uuid> query --request query.json
```

Use `--dry-run` on mutations. Rename updates references by default; pass
`--no-update-refs` to opt out.

## Contracts and durable runtime

The core `mdbase` crate owns the single contract identity, SemVer, digest,
schema, type-implementation, projection, and pack model. Event and action
contracts use that same registry. Core collection loading remains passive: a
type, contract, pack, workflow, or provider-registration record can never
activate executable code.

The independently versioned `mdbase-runtime` workspace crate implements
durable Runtime companion profile 0.2. `AdmissionCatalog` consumes verified
core contract artifacts, ordinary projected workflow/policy records, and live
event-source/action-provider declarations from `mdbase-interop`. Admission
pins exact contract digests, implementation identities, declaration digests,
and handler IDs. Workers execute that immutable plan through ordinary
interoperability action invocations; they never re-resolve a live registry.

The crate adds atomic event/run admission, leases, concurrency, cancellation,
crash recovery, cursor retention, and generation-safe timers. It supports
in-memory and SQLite stores by default and PostgreSQL behind the `postgres`
feature. The embedding host remains the final authorization boundary. See
[`crates/mdbase-runtime/README.md`](crates/mdbase-runtime/README.md).

## Runtime Observability

`FilesystemProvider::open_observed` and `FilesystemRuntime::open_observed`
accept a `RuntimeObserver`. Every dispatched collection operation reports
payload-free timing for queue, collection open, execution, synchronization,
and total duration, including provider-level failures. Error
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
- Workflow crate: `mdbase-runtime` (Runtime companion profile 0.2)
- Command adapter crate: `mdbase-command` (embedded by the unified CLI)
- Private verification crate: `mdbase-testbed-adapter` (black-box contract,
  crash-recovery, and lease-fencing scenarios; never shipped as a runtime API)

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

CI also runs the spec-owned interoperability testbed through
`mdbase-testbed-adapter`. The adapter uses the public `Collection`, `Runtime`,
and `RuntimeStore` boundaries and emits canonical transcripts, so the Rust core
and durable runtime can be compared directly with implementations in other
languages.

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
./scripts/profile.sh --root /path/to/collection --editor-iters 3
```

Use `--scenario core` for CRUD, rename/reference updates, runtime startup, and
mutation-plus-watcher synchronization. Use `--scenario all` before handing off
a performance-sensitive change. Reports include latency percentiles (`p50`,
`p95`, `p99`), throughput, query phases and plan counters.

Set `MDBASE_WATCH_PROFILE=1` to print payload-free watcher invalidation mode,
record counts, and refresh time. For CPU sampling with symbols:

```bash
cargo build --profile profiling \
  --manifest-path ../mdbase-connect/Cargo.toml -p mdbase-cli
perf record -g --call-graph dwarf -- \
  ../mdbase-connect/target/profiling/mdbase profile engine \
  --scenario queries --files 5000
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
