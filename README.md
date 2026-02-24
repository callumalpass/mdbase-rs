# mdbase-rs

Rust implementation of the [mdbase spec](https://mdbase.dev), with:

- Typed frontmatter loading/validation
- Query execution (filters, formulas, grouping, summaries)
- Link parsing/resolution/traversal
- CRUD, batch, backfill, and migrate operations
- SQLite cache support
- Watch event simulation for conformance testing

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

- Conformance coverage is tracked in `tests/conformance.rs`.
- Operational status notes are in `PROGRESS.md`.
- Release-level changes are in `CHANGELOG.md`.
