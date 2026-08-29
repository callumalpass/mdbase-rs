# Typed query core — Phase 2

Phase 2 moves `TypedCollection::query` off the canonical JSON operation facade.
The typed request is converted explicitly into `v03::query::model::Query`; the
parsed query executor returns dynamic records, metadata, and one lossless v0.3
diagnostic list. The typed adapter constructs `QueryResult` and
`OperationOutcome` directly. It performs no query wire validation, request JSON
encoding, `v03::Operations` dispatch, `OperationResult` construction, or result
envelope decoding.

The v0.3 operation methods remain edge adapters. They validate the query schema,
decode exactly one `Query`, execute the same parsed core, and preserve the
canonical `{results, meta, diagnostics}` result plus outer diagnostic list.
Cancellation remains out-of-band, and the runtime-cache and local cache/disk
load choices remain parameters of the shared executor.

## Compatibility and retained paths

- `QueryRequest::to_wire` remains for v0.2 compatibility, CLI, provider, and
  transport callers.
- The v0.2 query engine and its legacy envelope decoder are unchanged.
- Canonical saved views still originate as schema-validated JSON because saved
  views can express wire-only selection expressions, summaries, and extensions.
- Obsidian Bases and hosted query execution retain their specialized plans.
- Record hydration, body/link-graph loading, metadata-page pagination, invalid
  record stubs, and cache fallback remain in the shared v0.3 executor.

## Architecture budget review

Two focused Rust modules were extracted rather than increasing legacy-file
ceilings: typed query API definitions now live in `src/api/query.rs`, and schema
diagnostic translation lives in `src/v03/schema_diagnostics.rs`. This removes
the legacy allowances for both `src/api/typed.rs` (**1,113 to 885 lines**) and
`src/v03/mod.rs` (**1,067 to 999 lines**). The workspace actual is **158 files /
84,729 lines** after direct validation, path probes, and differential/schema
coverage. `src/v03/query/execute.rs` is 884 lines and
`src/v03/query/model.rs` is 372 lines; every changed Rust file is below the
1,000-line ordinary-file limit.
