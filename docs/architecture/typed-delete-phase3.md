# Phase 3: typed delete/preflight slice

`TypedCollection::delete` and `TypedCollection::preflight_delete` now call the
version-neutral mutation service directly with `DeleteRequest`. The mutation
plan captures the canonical `CollectionPath`, exact pre-delete revision,
effective type membership, and authoritative backlink evidence before removal.
Malformed frontmatter remains deletable and invalid UTF-8 retains path-derived
type evidence. Canonical preparation and execution do not use `DeleteInput`,
`OperationResult`, request `Value` conversion, generic typed result decoding, or
post-commit hydration.

Direct typed and v0.3 filesystem mutations retain one complete recoverable
shadow. Preflight uses that shadow but performs no write or journal commit. A
real delete commits the exact shadow baseline/desired change through the
existing durable transaction journal, whose baseline revision provides the
publication CAS. The returned result is projected entirely from
`PlannedDelete`; no target read or stat occurs after the durable boundary.
Legacy `Collection::delete` alone parses `DeleteInput` and translates it at the
collection edge.

The v0.3 adapter decodes path, backlink, revision, and dry-run controls directly
into the typed request and serializes the unchanged delete envelope. Hosted
runtime preparation uses `mutation::plan_delete` with one sparse shadow. That
single API captures authoritative revision, frontmatter/body change evidence,
effective types, and backlinks, then applies the exact revision-bound plan to
the caller-owned sparse working set. The runtime adapter only decodes, calls,
and projects the plan; it does not replan or replace captured fields, and it
does not create a nested full shadow.

Boundary tests cover direct typed, wire, and runtime paths. Typed delete uses
one full shadow with no request/result/legacy/hydration bridge. Wire delete adds
one wire decode and one full shadow. Runtime delete adds one runtime decode, one
wire decode, and one sparse shadow with no full shadow. Differential tests cover
typed/wire backlink results and missing diagnostics. Post-commit replacement
and durable runtime reopen tests prove results and replay remain bound to the
planned deletion rather than ambient path state. Runtime `ChangeSet` deletion
evidence is built directly from the plan's exact before revision and effective
types, including malformed, non-mapping, and invalid-UTF-8 records.

Create, update, and delete share one `parse_optional_revision` wire decoder.
Absence alone means no CAS; null, non-string values, and empty strings are
ordered `invalid_request` failures with the request path. Direct operations and
runtime sparse preparation therefore return identical full diagnostics and
runtime can never erase a present malformed CAS value. Optional non-boolean `check_backlinks` and `dry_run` values retain the
established false-default compatibility behavior. Typed requests cannot express
syntactically unsafe paths or malformed field types; differential coverage
therefore compares every representable typed failure field-for-field and tests
those rejected wire-only shapes against their canonical edge diagnostics.

## Remaining Phase 3 work

Only rename and batch remain on the legacy mutation bridge. Runtime and batch
delete now use the typed staged core; legacy v0.2 JSON CRUD remains explicitly
outside this slice. The split delete test module brings the workspace budget to
166 Rust files and 87,561 lines; `src/v03/operations.rs` and `src/v03/batch.rs`
remain below 1,000 lines, and the v0.3 facade-reference budget falls from 35 to
34.
