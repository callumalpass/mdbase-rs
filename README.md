# mdbase-rs

Rust implementation of the [mdbase spec](https://mdbase.dev), with:

- Typed frontmatter loading/validation
- Query execution (filters, formulas, grouping, summaries)
- Link parsing/resolution/traversal
- CRUD, batch, backfill, and migrate operations
- SQLite cache support
- Watch event simulation for conformance testing

The `0.3.0-alpha.1` crate dual-loads legacy v0.2 collections and v0.3 type
wrappers. v0.3 records are validated against their embedded JSON Schema
2020-12 schemas and report canonical structured diagnostics. The canonical
config, type-file, diagnostic, operation-result, and query-result schemas are
vendored under `schemas/v0.3/` and refreshed with
`scripts/sync-v03-schemas.sh`.

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
and emits opaque `sha256:` revisions. The verified, evidence-scoped claim for
`core_read` and `collection_semantics` is published under `conformance/`;
unlisted profiles are not claimed.

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
cargo test
```

## Performance Profiling

Run the profiler against a synthetic, deterministic dataset:

```bash
./scripts/profile.sh
```

Write results to a file with custom workload sizing:

```bash
./scripts/profile.sh --files 5000 --query-iters 500 --output .ops/profile/latest.json
```

The profiler reports latency percentiles (`p50`, `p95`, `p99`), averages, and throughput for core operations (`open`, `read`, `query`, `update`, `rename/update_refs`, `create`, `delete`, `cache_rebuild`).

## Notes

- v0.2 conformance coverage is tracked in `tests/conformance.rs`; the shared
  v0.3 adapter is `tests/v03_conformance.rs`.
- Operational status notes are in `PROGRESS.md`.
- Release-level changes are in `CHANGELOG.md`.
