# Bounded filesystem cursor startup

`FilesystemRuntime::open_read` first attempts an eligible metadata query backed by a held,
read-only SQLite WAL transaction and an `Arc<Collection>` containing collection definitions.
It decodes and projects only the requested page, not every result. The snapshot is established
under the provider read gate; counts, record values, types, settings and definition behavior
remain pinned across later mutations. A captured evaluation clock is reused. Total count is
reused after the first page.

## Eligibility and fallback

The fast path uses the existing metadata-page planner: no invocation context, CEL filter,
projections, explicit selection, grouping, summaries or non-metadata ordering. It also requires
v0.3, no expression type matchers, no computed-field plans and no explicit query timezone.
Unsupported queries use the existing complete materialization path, preserving their semantics.
A mixed collection containing computed/expression type plans can therefore retain the old cost.

This bounds **decoded record count** for eligible pages; it does not eliminate cold filesystem
cache construction, SQLite count/offset work, large individual records or complex-query costs.
Unchanged setup assessment already uses definitions-only planning on engine main; this change
does not bypass reviewed changes, contract validation or mutation baseline verification.

## Page sizing and resource ownership

- Existing `read_page` preserves fixed-page callers. The additive `read_page_with_limit` method
  lets an authority supply a continuation size; the maximum is now 1,000 records.
- First effective size at each offset is remembered for deterministic replay. Conflicting
  explicit sizes fail. That small ledger is charged against retained-state capacity and the
  operation meter; replay does not add another entry.
- Cursor authentication, generation/scope binding, explicit release and failure reclamation
  remain intact. Successful terminal pages remain replayable until release or lease expiry.
- Existing limits remain: 32 active snapshots, 32 MiB retained-state capacity, 30-second idle
  leases and a five-minute hard lifetime.
- Expired readers are reaped on cursor activity and on normal reads, writes and watcher
  reconciliation. This prevents abandoned WAL readers obstructing checkpointing while the
  runtime continues activity. No extra background thread is introduced. Runtime drop also
  drops all held connections.
- Retained accounting measures definition/query state and the replay ledger, not complete
  result vectors. SQLite/WAL filesystem storage is not claimed as in-memory retained bytes.

Tests cover bounded retention, mutation-stable metadata, replay and conflicting sizes, release,
filtered offsets, and idle cleanup during read/prepare/commit/watcher activity. Existing general
query, scope, capacity, cancellation and mutation tests remain applicable.

## Local synthetic observation

```sh
cargo test --release --no-default-features --lib metadata_cursor_startup_benchmark -- --ignored --nocapture
```

This ignored test creates 10,000 synthetic notes in a temporary collection, establishes the cache
outside timing, then compares a 100-record cursor opening with full materialization. It prints
only record count, durations and retained-byte accounting. Three optimized runs on 2026-11-06:
median first page **1.64 ms**, median full materialization **160.39 ms**, retained cursor state
**4,237 bytes**. Full materialization is a comparator, not an instrumented older binary. These
are warm local engine measurements, not network, cold-start or production Reader timings.
