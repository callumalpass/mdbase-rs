# Typed filesystem runtime outcomes — Phase 4a

Status: implemented.

## Contract

`FilesystemRuntime::execute_typed` and `execute_typed_with_context` are the
semantic execution entry points for Connect. They continue accepting
`OperationRequest` so the existing operation wire request remains the one
compatibility decoder, but decode migrated requests once into public typed
requests and return `ExecutionOutcome` whose `operation` is a closed
`CanonicalOperationOutcome`.

The closed value variants are `Read`, `Query`, `Create`, `Update`, `Delete`,
`Rename`, and `Batch`. Read/query/record-mutation variants contain their
canonical Rust types and never a generic JSON value. Diagnostics use the typed
`api::Diagnostic` contract. Delete and rename distinguish committed and
preflight values. Arbitrary query output is narrowed behind transparent
`ProjectedValue` and `QueryMetadata` domains; reference observations use
`ReferenceEvidence` rather than unclassified JSON envelope values.

The compatibility-only `execute` and `execute_with_context` methods call the
single `CanonicalOperationOutcome::to_v03` adapter. During the coordinated
Connect migration, `ExecutionOutcome.result` and `CommitRejection.result`
remain deprecated, ephemeral projections populated by that same adapter. They
are deliberately absent from version-3 journals. Remove both fields once the
Connect Phase 4 consumer migration lands. New Connect code should use
`operation` and must not decode `OperationResult.result` or infer shapes.

`prepare_typed` (and the compatibility-named `prepare` alias), commit,
commit/claim resolution, cancellation replay, and read cursors all carry the
same typed outcome alongside generation and canonical `ChangeSet`. Runtime
journal version 3 persists that outcome. New journals do not write
`OperationResult`; version 2 journals remain backward-readable through one
explicit legacy discriminator recovery path. Version-2 record operations are
identified only from exact result/change evidence. Ambiguous cleaned failures
and all resource journals become non-semantic `LegacyRecoveredV03` values;
resource paths are never guessed. Settlement upgrades a recovered version-2
operation before its next write. The prepare/commit/cancel/settle
state machine and durable boundary are unchanged.

Committed file facts replace planned size/mtime directly in typed record,
rename, and batch values. This is metadata captured from committed writes, not
a post-commit semantic read.

## Remaining wire-only families

The explicitly named `WireOnlyOperationValue` variants are:

- `Validation`
- `ViewResource` (list/execute/read/create/update/delete view source)
- `TypeResource` (list/read/create/update type resource)
- `TypePack` (assess/apply)
- `CollectionSetup` (assess/apply)

These families retain exact JSON solely because no canonical typed public value
exists for them yet. They cannot represent migrated reads, queries, record
mutations, or batches.

## Architecture budget

The semantic outcome and sole wire adapter are isolated in
`src/runtime/canonical_operation.rs`; dynamic public domains are split into
`src/api/dynamic.rs`. Journal compatibility remains colocated with the
durability state machine in `src/transactions/runtime.rs`. The reviewed
Phase 4 final ceilings are 171 Rust files and 93,200 Rust lines; the completed
stack measures 93,127 lines. The three existing runtime legacy-file ceilings
move to their measured formatted totals.
