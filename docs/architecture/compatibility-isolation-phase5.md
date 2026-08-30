# Phase 5: compatibility isolation and retirement gates

## 0.4.x ambient Collection mutations

The released context-free JSON methods `Collection::{create, update, delete,
rename, backfill, batch_update, batch_delete}` remain source-compatible behind
the default-on `legacy-collection-mutation` feature. Their seven public
definitions are owned exclusively by `src/compat/legacy_mutation.rs`;
implementation helpers and legacy DTO use are crate-private and frozen by an
explicit file allowlist in `config/architecture-budgets.json`.

The 0.4.x methods deliberately have no Rust `#[deprecated]` attributes because
released consumers may use `deny(deprecated)`. Their rustdoc marks them as
Deprecated compatibility APIs with typed replacements and planned 0.5.0
removal. Attribute-based deprecation and the breaking removal begin only on the
0.5 release line.

Canonical hosts build with `--no-default-features` and use
`Collection::typed()` with `CreateRequest`, `UpdateRequest`, `DeleteRequest`,
`RenameRequest`, or `BatchRequest`. Backfill has no canonical operation, so the
0.4.x CLI explicitly selects compatibility; new automation must query/read via
the typed service and submit a recoverable typed batch. The phase-0 benchmark
binary and compatibility-dependent tests are feature-gated.

The architecture checker does not infer calls from generic `.create` or
`.update` syntax. It instead enforces exactly seven facade definitions in the
compatibility owner, rejects unregistered compatibility-module, legacy DTO,
legacy-helper, and `Collection::method` UFCS references, and compares all
intentional internal compatibility references with an exact file allowlist.
Compile fixtures prove that all seven methods are present with default features,
compile under `deny(deprecated)`, and are absent without default features.

`OperationContext::legacy` is test-only. Production runtime, typed, hosted, and
provider sources have zero callers and use explicit caller contexts or the
bounded private internal lifecycle context. Test/support has 167 lexical
callers.

## Wire-only values

`WireOnlyOperationValue` remains private to `runtime/canonical_operation.rs`.
Cursor release is typed (`CursorReleaseOutcome`). The checker parses the full
enum body and requires the exact allowlist `Validation`, `ViewResource`, and
`TypeResource`; an added or renamed variant fails without relying on a
hand-written variant regex.

Removal criteria and owners are:

- `Validation`: runtime API owner; replace after validation has a closed typed
  result model and filesystem/hosted parity tests no longer require
  `validation_wire`.
- `ViewResource`: views owner; replace after list, execute, and source-read each
  have closed typed value models. View mutations already recover to typed
  resource mutation values.
- `TypeResource`: types owner; replace after list/read have closed typed value
  models. Type mutations already recover to typed resource mutation values.

Production constructor references use non-growing name/count ceilings and a
closed file allowlist: `runtime/canonical_operation.rs`,
`runtime/filesystem.rs`, and `v03/batch.rs`. Deletions are intentionally allowed
without raising a budget; any new production file or increase fails. Test and
support references are excluded from this production-debt metric. The paired
Connect PR adds the external repository guard. This local checker makes no
claim that current Connect callers are zero.

## Version-2 runtime journals

`LegacyRecoveredV03` is journal-read-only. Checked serde rejects it and current version-4
persistence cannot write it. `FilesystemProvider::legacy_journal_inventory`
and `FilesystemRuntime::legacy_journal_inventory` return only a version-2
count; they expose no IDs, paths, claims, or payloads. Operators must record
`is_zero() == true` before upgrading beyond the supported 0.4.x window.

There is no general eager-upgrade API. A version-2 journal is upgraded to the current
version-4 typed shape when ordinary recovery settles a committing transaction;
prepared or already-final journals remain inventory-visible until their normal
resolution/acknowledgement lifecycle removes them. The decoder and
`LegacyRecoveredV03` can be removed only after the supported settlement and
acknowledgement window, operator inventories, and all supported fixtures are
zero. Migration tests prove that exact version-2 and retained-evidence version-3 journals
remain readable while every new journal starts at version 4.

## Ephemeral runtime projections and paired Connect migration

`ExecutionOutcome::result` and `CommitRejection::result` retain their existing
0.4.x source compatibility. Replace them with `operation`; call
`operation.to_v03()` only at the wire edge. Production mdbase has zero callers;
four local compile/test fixtures remain under a non-growing ceiling. Deleting a
fixture is intentionally allowed; adding one fails. Removal is a 0.5.0 change
after the Connect migration window.

This repository does not enforce Connect source today. The paired Connect PR
will add its external guard and migrate field reads. If Connect disables
mdbase default features before replacing ambient Collection calls, its expected
failure is Rust `E0599` (`no method named create`, `update`, `delete`, `rename`,
`backfill`, `batch_update`, or `batch_delete`) at each remaining call. If a
Connect branch removes compatibility-field handling ahead of its coordinated
adapter changes, its expected failures are missing/incorrect projection uses at
`ExecutionOutcome::result` or `CommitRejection::result`; those fields still
exist locally throughout 0.4.x. Such Connect-only failures are tracked in the
paired PR, not reported as mdbase-local gate failures.
