# Phase 3: typed create/update slice

`TypedCollection::create` and `TypedCollection::update` call the version-neutral
`mutation` service with public `CreateRequest` and `UpdateRequest` values. The
mutation module owns canonical membership, lifecycle, preparation, planned
record evidence, diagnostics, full-shadow implementation, and typed outcome projection.
It has no dependency on `v03` and does not construct or parse request `Value`s.

The v0.3 adapter alone parses aliases, raw create documents, revision strings,
and dry-run controls. Its staged entry calls the same mutation preparation and
create/update core without another shadow, then serializes the planned typed
outcome into the unchanged `OperationResult` envelope. `CreateInput` and
`UpdateInput` remain only at explicit legacy collection wrappers; the v0.3
wire decoder constructs typed requests directly. v0.2 typed mutations remain rejected.

Create/update retain one complete disposable collection shadow in this slice.
The shadow preserves generated values, collection validation, locking, exact
revision/CAS checks, transaction journals, and recovery semantics. The cores
return `PlannedRecord` with before revision, exact final bytes, persisted and
effective projections, body, types, and ordered diagnostics; they do not use
capture-out parameters or result hydration.

Transaction commit returns metadata facts captured from the committed file
entry while the transaction write lock is held and before the durable committed
marker. Mutation result semantics are evaluated from planned bytes before the
commit boundary. After durable commit, returned file facts are attached
infallibly, with no ambient stat, byte reread, or new error path. A test-only
post-commit replacement/removal seam proves the successful outcome retains the
planned revision, document, and size even if the path changes immediately after
the marker.

Boundary probes increment at the actual typed `into_wire`, legacy parser, v0.3
decoder, generic result decoder, hydration read, and shadow-construction sites.
Real typed create/update calls report zero bridge/hydration counters and one full
shadow each. Direct wire calls report one wire decode and one full shadow; runtime
calls report one runtime decode, one wire decode, and one sparse shadow, with no
nested full shadow. A source/call-graph guard rejects `crate::v03`, forwarding shadow
helpers, and legacy request builders under `src/mutation` and the crate root.

## Remaining paths and budgets

Delete is now covered by the follow-on
[`typed-delete-phase3.md`](typed-delete-phase3.md) slice. Rename, batch encoding,
runtime wire transport, and legacy v0.2 JSON CRUD remain outside this original
create/update slice. Rename alone retains compatibility hydration.
Runtime and batch create/update use the staged typed entry and do not nest a
second shadow. Committed facts replace shadow metadata in direct v0.3 and durable
runtime journal responses. Postcommit replacement/removal tests cover typed,
wire, and runtime create/update. Canonical failures are closed typed diagnostics;
JSON failure envelopes exist only in legacy collection wrappers and versioned
adapters. Canonical create/update execution accepts `PreparedCreate` and
`PreparedUpdate` directly; legacy DTOs are translated once in collection wrapper
edges and never enter planned/core functions. The workspace budget is 165 Rust files and 86,476
lines; both `src/v03/operations.rs` (862) and `src/v03/batch.rs` (941) are below
the general ceiling, and the facade-reference budget falls from 36 to 35.
