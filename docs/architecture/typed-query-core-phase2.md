# Typed query core — Phase 2

Phase 2 makes the parsed canonical query architecture version-neutral. The
model, semantic preflight, executor, query context, diagnostics, and result
serialization now live together under `src/query/canonical/`; shared CEL and
canonical diagnostic ownership live at `src/cel.rs` and `src/diagnostic.rs`.
Typed operations, runtime hosted queries, and v0.3 wire operations share that
single model and executor; there is no duplicate implementation.

`src/v03/query.rs` is only the v0.3 edge adapter. It validates the embedded v0.3
schema, decodes one parsed canonical `Query`, and creates the historical
`OperationResult` envelope. `TypedCollection::query` constructs the parsed model
directly and creates its typed result directly. Its path performs zero request
JSON encodes, JSON Schema validator calls, operation-facade dispatches, result
envelope constructions, or envelope decodes.

## Typed contract synchronization

The closed typed surface mirrors only constraints it can express: type-name and
field-name patterns, non-empty strings and arrays, unique type names, and wire
default omission. Those constraints have one helper inside the version-neutral
canonical model. Schema-sync tests parse the embedded `query.schema.json` and
assert every mirrored pattern, bound, `minLength`, `minItems`, uniqueness, and
default-omission rule.

`QueryRequest` deliberately still has public mutable fields. A caller can bypass
builder methods and create values such as `select: Some(vec![])` or an empty
expression, so direct typed preflight remains mandatory. Removing it requires a
breaking API that makes fields private and exposes only validated typed builders.
The direct traversal remains explicit to preserve wire diagnostic order without
a JSON round trip.

## Source-load accounting

`QueryPerformance` distinguishes:

- `records_loaded`: candidate records materialized by the bulk source;
- `record_source_loads`: bulk cache/snapshot sources opened;
- `context_record_loads`: point reads used to hydrate `context.this`;
- `total_source_loads`: bulk plus context source loads.

A contextual filesystem query therefore reports one bulk snapshot/cache source
and one successful point context read (`total_source_loads == 2`). Typed/wire
path probes assert the same counters while continuing to prove that typed
requests perform zero JSON encode or schema-validation probes. Invalid context
paths and failed point reads report zero context reads; totals use the count
returned by `load_context`. CLI profile output includes all three source counters.

## Legacy compatibility inventory

Production `Collection::read` callers are now limited to exactly:

1. `src/compat/v02.rs`, for public v0.2 read compatibility;
2. `src/query/engine.rs`, the v0.2 compatibility query context;
3. `v03::operations::hydrate_persisted_result`, temporary Phase 3 mutation
   result hydration.

Canonical saved-view source/context reads, watch record reads, data-contract
point reads, the Phase 0 read benchmark, v0.3 `get_types`, and canonical query
context hydration use `TypedCollection` or the sole typed read evaluator.
`Collection::read`/`read_document` remain public/internal only for the inventory
above. They can be removed after v0.2 compatibility is retired and Phase 3 moves
mutation hydration to the typed evaluator. Canonical saved-view query documents
continue through the v0.3 wire adapter because they can contain wire-only
selection expressions, summaries, and extensions; their record reads no longer
use the legacy facade.

## Architecture budget delta

The six core files moved from `src/v03/query/` to `src/query/canonical/` without
duplication. Canonical CEL moved from `src/v03/cel.rs` to `src/cel.rs`, and the
single diagnostic type moved from `src/v03/mod.rs` into the new
`src/diagnostic.rs`; v0.3 re-exports preserve public compatibility. The old v0.3
query module was replaced by one small wire-adapter file. The workspace is
**159 Rust files / 84,928 lines** (from **158 / 84,729**). No legacy per-file
allowance was raised. Every canonical query production file is below 1,000
lines (largest: `execute.rs`, 866); all ordinary production files remain below
the 1,000-line limit, with pre-existing legacy exceptions unchanged.

## Phase 3 remainder

Only mutation-result hydration remains a canonical consumer of the legacy JSON
read facade. Phase 3 must switch that hydration to `evaluate_typed_read`, retain
its special invalid-frontmatter fallback, then remove the facade when v0.2
compatibility is also retired.
