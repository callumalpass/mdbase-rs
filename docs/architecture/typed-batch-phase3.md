# Phase 3: typed batch core

`TypedCollection::batch` now passes `BatchRequest` directly to the
version-neutral coordinator in `src/mutation/batch.rs`. It no longer constructs
a v0.3 JSON request, enters `Operations`, or deserializes an `OperationResult`.
The old `TypedCollection::operations` and generic `execute<T>` bridge are gone.

Non-partial and dry-run batches use one full collection shadow. Ordered staged
typed create, update, delete, and rename operations mutate that one working set,
so later plans observe earlier bytes and generated reservations. A successful
non-partial batch has one recoverable publication; dry runs and failed atomic
plans have no journal or authoritative write. Locked commit facts are attached
to every record result, including runtime results during durable settlement.

Direct `allow_partial` execution uses one independent shadow and recoverable
commit per successfully decoded attempted item. Items continue in request order
after failure. The v0.3 adapter decodes raw partial items lazily, so a late
malformed, unsupported, revision, or mtime failure is recorded in place while
later valid items still commit through the same canonical item primitive. The
pre-release `MdbaseError::PartialBatch` carries the complete `BatchResult` and
ordered diagnostics, ensuring already committed successes are inspectable.
Filesystem runtime batches reject `allow_partial = true` before creating a
shadow because one host claim cannot durably represent multiple independently
settled commits. Atomic runtime batch uses one canonical working shadow and one
runtime transaction. Its exact before/after snapshots produce one aggregate
`ChangeBatch`, including proven renames and reference updates.

The v0.3 batch adapter owns wire-only aliases, revision and mtime decoding,
option validation, and `OperationResult` serialization. The canonical mutation
module has no v0.3 facade or `OperationResult` dependency. As an intentional
pre-release Rust API correction, `BatchItemResult::result` is now the public
untagged `BatchOperationResult` enum rather than `serde_json::Value`. Its typed
record/delete/rename and preflight variants serialize to the unchanged flat
v0.3 result objects; failed items use the typed empty variant. Canonical batch
coordination therefore performs no mutation-result JSON encode/decode bridge.
Legacy `Collection::batch_update` and `Collection::batch_delete` remain documented
compatibility operations; neither is a production caller of the canonical
batch coordinator.

## Architecture inventory

The architecture ceiling is 169 Rust files and 90,466 lines. The transitional
`v03_operations` reference budget falls from 33 to 26. The exact counted
inventory is:

- architecture baselines and command tooling: `mdbase-command/lib.rs` (3),
  `mdbase-command/profile.rs` (6), and `phase0-baseline.rs` (3);
- remaining runtime/read/resource compatibility: `runtime/catalog.rs` (3),
  `runtime/hosted_mutation.rs` (2), `runtime/hosted_resource.rs` (2),
  `runtime/hosted_validation.rs` (1), `runtime/provider.rs` (1), and
  `runtime/tests.rs` (1);
- facade definition and non-migrated consumers: `v03/mod.rs` (1),
  `views/execute.rs` (1), and `watch/real.rs` (2).

No counted reference remains in the typed mutation service, typed batch path,
v0.3 batch adapter, or filesystem runtime batch path.
