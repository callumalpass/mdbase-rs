# Canonical operation outcomes

`CanonicalOperationOutcome` is the checked semantic result shared by runtime execution,
hosted APIs, cursors, and durable transactions. Its fields are private. Consumers use
`is_valid()`, `state()`, `family()`, `value()`, `query_value_mut()`,
`diagnostics()`, `operation_kind()`, and `to_v03()`.

## Construction and invariants

Public Rust callers use `try_completed`, `try_rejected`, or the explicit v0.3 adapter
`try_from_v03`. Successful read, query, create, update, delete, rename, batch, type-pack,
and collection-setup outcomes must contain a semantic value. Rejected outcomes may retain
partial values and diagnostics. Checked deserialization enforces the same rule.

Compatibility-only validation, view-resource, and type-resource results have named wire
constructors and round-trip exactly through outcome serde. They do not substitute for typed
record/query/mutation values and are forbidden in mutation journals. Resource mutations retain
a typed exact operation discriminator. Type-pack and collection-setup values likewise use distinct
assess/apply canonical discriminators; only apply may enter a mutation journal. Phase-4 generic
definition tags are recovered as apply only inside a resource-mutation journal context. Cursor release uses `CursorReleaseOutcome` rather than
a validation JSON envelope, has its own `CursorLifecycle` family, and can only be completed.

## Journal compatibility

Runtime journal versions are schema-strict. The current writer emits version 4 with
canonical `operation_outcome`/`operation_rejection` fields and authenticated transition
evidence. Version 2 accepts only legacy `operation_result`/`rejection` fields. Version 3 is
the Phase-4 canonical format: it accepts only canonical outcome/rejection fields and remains
readable only when its retained physical entries exactly prove the transition. Rejection
fields must agree with the phase, and readers never fall back across versions. Version-3 and
version-4 mutation journals authenticate the outcome and rejection operation against the
exact record or resource change family and reject wire-only, cursor-lifecycle, or
legacy-recovery state.

Version-2 journals continue to recover their exact v0.3 envelope. An ambiguous version-2
envelope is represented internally as legacy recovery state only in `transactions::runtime`;
it cannot be deserialized through the public outcome type or serialized into a new journal.

`ExecutionOutcome::result` and `CommitRejection::result` remain deprecated ephemeral v0.3
projections derived from `operation.to_v03()`. They are never independent authority and are
never persisted.

## Host migration

Hosts should read `ExecutionOutcome.operation`. Existing Connect code that directly accesses
`CanonicalOperationOutcome.valid`, `.value`, or `.diagnostics` must switch to the accessors.
`CollectionRuntime::release_read` now returns `CursorReleaseOutcome`; callers that discarded
`()` may continue to discard the returned value, while lifecycle-aware callers can inspect
`released` and construct the compatibility projection with
`CanonicalOperationOutcome::cursor_release`.
