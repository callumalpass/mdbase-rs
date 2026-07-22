# mdbase-rs

Rust implementation of the [mdbase spec](https://mdbase.dev), with:

- Typed frontmatter loading/validation
- Query execution (filters, formulas, grouping, summaries)
- Link parsing/resolution/traversal
- CRUD, batch, backfill, and migrate operations
- Pure Runtime Contracts 0.1 registry composition and preflight
- SQLite cache support
- Debounced filesystem watching with normalized collection events

The `0.3.0-rc.1` crate dual-loads legacy v0.2 collections and v0.3 type
wrappers. v0.3 records are validated against their embedded JSON Schema
2020-12 schemas and report canonical structured diagnostics. The canonical
config, type-file, diagnostic, operation-result, query, query-result, and view
schemas are vendored under `schemas/v0.3/` and refreshed with
`scripts/sync-v03-schemas.sh`.

The canonical `view` schema is handled through the normal v0.3 type-file and
record-validation pipeline. The v0.3 operation facade implements canonical CEL
matching and query objects, including invocation context, projections,
selection, deterministic grouping and summaries, pagination, and query result
envelopes. It does not advertise the optional `view_records` execution feature:
named-view resolution and merge behavior remain outside its verified claim.

`mdb init` creates a minimal stable `0.3.0` collection by default. Supplying an
explicit v0.2 version retains the legacy initializer and generated meta type.
The bundled profiler also exercises canonical v0.3 type wrappers.

For a v0.3 collection, use `Collection::v03_operations()` for the normative
operation envelope:

```rust
let collection = mdbase::Collection::open(root)?;
let operations = collection.v03_operations()?;
let read = operations.read(&serde_json::json!({ "path": "tasks/example.md" }));
assert!(read.valid);
assert!(read.result["revision"].as_str().is_some());
```

The existing `Collection` operation methods retain their legacy native result
shape for v0.2 consumers. The v0.3 facade returns `{ valid, result,
diagnostics }`, canonicalizes diagnostics, reports persisted mutation state,
and emits opaque `sha256:` revisions. Mutations enforce optional
`if_revision` preconditions, and queries use the same envelope. The verified,
evidence-scoped profile claim is published under `conformance/`; unlisted
profiles and optional features are not claimed.

## Runtime Contracts

`mdbase::runtime_contracts::RuntimeContracts` is a pure registry and preflight
engine. It loads materialized contract records from normal collection scope and
composes them with built-in, provider, or pack contracts supplied in memory.
`ContractDocument::virtual_contract` is the first-class representation for a
non-materialized contract.

The engine validates strict provider, action, event, capability, policy, and
workflow shapes; compiles embedded schemas once; validates event and action
values; resolves workflow requirements; and renders contracts as Markdown when
materialization is requested. It deliberately does not execute workflows or
grant filesystem authority. An embedding host such as mdbase-connect remains
the final authorization boundary immediately before dispatch.

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
- CLI binary: `mdb` (`init`, CRUD/query/validate, `backfill`, `migrate`, cache)
- Profiling binary: `mdb-profile` (synthetic workload profiler with JSON output)

## Build

```bash
cargo build
```

## Test

```bash
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## Performance Profiling

Run the profiler against a synthetic, deterministic dataset:

```bash
./scripts/profile.sh
```

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
- Operational status notes are in `PROGRESS.md`.
- Release-level changes are in `CHANGELOG.md`.
