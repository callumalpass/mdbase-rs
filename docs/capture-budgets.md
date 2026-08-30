# Capture budgets and cancellation

Every runtime operation with an `OperationContext` has caller-owned cancellation,
a monotonic deadline, and finite `CaptureLimits`. The default limits are:

| Dimension | Default | Meaning |
|---|---:|---|
| entries | 100,000 | discovered record/file entries in one capture |
| file bytes | 64 MiB | bytes in one record or resource |
| aggregate bytes | 4 GiB | cumulative bytes actually read during the operation |
| depth | 128 | descendant directory depth in one capture |
| resource entries | 10,000 | resources in one capture |
| retained bytes | 4 GiB | cumulative bytes retained by snapshots, shadows, query results, or cursors |

Limits are inclusive. Arithmetic and limits are checked before capacity
reservation and for every 64 KiB streaming chunk. A violation returns
`ProviderError::CaptureLimitExceeded` with stable code
`capture_limit_exceeded`, a typed dimension, the limit, and the attempted
value. Capture never truncates and never reports partial success.

Entry, resource-entry, depth, and per-file ceilings apply independently to each
capture. Aggregate-read and retained-byte counters are operation-wide and are
shared by context clones and nested canonical adapters. Projecting or indexing
one already-captured authoritative snapshot does not charge another filesystem
read. A genuine second read, such as creating and reopening a mutation shadow,
is charged because it is additional work. Returned query rows are measured by
deterministic JSON serialization whether they came from disk, cache, a
non-paginated read, or a pinned cursor.

The 4 GiB operation defaults are an explicit eight-times multiplier over the
historical 512 MiB capture envelope. This allows a collection at the documented
100,000-entry ceiling to pass ordinary before/shadow/after mutation phases
without multiplying the per-capture entry limit; unusually large records or
extra phases remain bounded.

Use `CaptureLimits::builder()` and `OperationContext::with_capture_limits()` to
set host policy. Context-free provider and collection methods are compatibility
entry points: they construct a finite 24-hour legacy context with default
budgets. New long-running host code should always use a `*_with_context` API.

Cancellation is checked around discovery, every entry/chunk, snapshot
materialization, shadow copy, and before durable transaction prepare. Once a
durable commit boundary starts, runtime settlement owns completion and reports
its exact durable state rather than relabeling cancellation as rollback.

## Intentional legacy seams

The context-free typed `Collection` API, watcher background refresh transport,
and v0.2 compatibility adapter do not expose caller cancellation. They use a
finite legacy context where they enter budgeted authority capture. The watcher
command protocol currently checks cancellation while callers wait, but its
already-dispatched background filesystem refresh cannot be interrupted by that
caller's token. These seams remain for wire/API compatibility and should be
deprecated only in a compatibility release.

## Source guard inventory

Authority-capture code is expected to use no-follow handles and chunked reads.
Review changes with:

```sh
rg 'OperationCancellation::new\(\)|OperationContext::legacy\(\)' src \
  --glob '*.rs' --glob '!**/*tests.rs'
rg 'fs::read\(|fs::read_to_string\(' \
  src/snapshot.rs src/snapshot src/record_load.rs src/runtime/snapshot.rs \
  src/mutation src/v03/batch.rs
```

Fresh tokens are permitted only in documented context-free adapters and tests;
unbounded ambient reads are not permitted in authority capture, runtime shadow,
or synchronization snapshot paths.
