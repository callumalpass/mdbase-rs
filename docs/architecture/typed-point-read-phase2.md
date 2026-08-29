# Phase 2: canonical typed point reads

Phase 2 commit 1 moves v0.3 point reads behind one typed evaluator in
`operations/read`. `TypedCollection::read` now passes `ReadRequest` directly to
that evaluator and receives `OperationOutcome<RecordDocument>` without a JSON
request/result conversion. `v03::Operations::read` is a wire adapter: it keeps
wire parsing and path diagnostics at the boundary, then serializes the typed
record and diagnostics into the unchanged `OperationResult` envelope.

The evaluator has two authoritative source forms. Filesystem reads use the
collection capability's component-by-component no-follow open and the existing
byte-first loader. One successful point read opens and loads the record once;
the old read-specific normalization hydration and its second read were deleted.
Provider-owned exact reads consume the supplied canonical identity, document,
and file facts. `CompiledCatalog::read_record` and not-found evaluation therefore
perform no collection filesystem read.

Raw and effective frontmatter remain distinct. Exact bytes determine revisions
and optional `document`; BOM-bearing source, body extraction, type membership,
defaults, coercion, computed fields, validation policy, file facts, strict
malformed/invalid-UTF-8 failures, and match-expression diagnostics retain their
canonical wire behavior. The v0.2 adapter still calls the explicit legacy read
path and was not changed. Mutation hydration remains intentionally in place for
Phase 3.

Differential tests cover typed and wire success with and without exact source,
BOM/revision/defaults, off/warn/error policy, missing, malformed YAML, invalid
UTF-8, exact hosted identity/not-found behavior, and traversal. Test-only loader
counters assert one load for typed filesystem reads, one for wire reads, and
zero for exact hosted reads. The record-document schema now declares the
optional emitted `document` member while retaining `additionalProperties:
false`; a schema-sync test validates both emitted forms.

## Architecture budget review

This change keeps the workspace at 156 Rust source files. The reviewed source
line ceiling moves from 83,747 to the measured post-format total recorded in
`config/architecture-budgets.json`; most added lines are the cohesive typed
source/evaluator and its adversarial differential/counter tests. No new facade
was introduced. The transitional v0.3 facade reference ceiling remains 36, and
`src/v03/operations.rs` shrinks because read normalization, read hydration,
provider normalization, and duplicate match-diagnostic attachment were
removed. `src/api/typed.rs` receives only the reviewed direct-call delta.

Query result evaluation is deliberately unresolved Phase 2 work: query still
has its own typed-core consolidation phase. This commit only migrates query CEL
context point-loading to the canonical read evaluator; it does not alter query
planning, result models, pagination, or runtime envelopes.
