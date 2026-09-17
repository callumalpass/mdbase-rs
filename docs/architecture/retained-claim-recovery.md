# Retained runtime claim recovery

Connect issue 443 reproduced exhaustion of the 128-transaction capacity by
committed, event-acknowledged local writes whose host claim was never settled.
Connect owns the local caller/replay distinction and its durable ownership and
recovery audit. The engine owns journal validation, phase interpretation, exact
current-revision checks, locking and acknowledgement.

`FilesystemRuntime::inspect_runtime_claims` returns bounded retained transaction
metadata, including relative paths but no record payloads. Its host capability
is available to the trusted in-process host and excluded from serialization.
`acknowledge_verified_runtime_claim` requires a committed transaction, completed
event acknowledgement and matching current after-image revisions under the
engine writer lock. Callers must independently establish that no external
response-replay owner needs the claim. This API does not infer ownership or
provide a force-delete path. Ordinary acknowledgement APIs and the cap remain
unchanged. A normal application must not expose this local-administration API.

The implementation uses the existing v2/v3/v4 journal readers and held-root I/O,
not a parallel JSON/filesystem adapter in Connect. Tests reject preparation,
pending events and changed current bytes, and preserve unrelated journals.

The architecture budget increases by exactly 143 Rust lines: 27 in the runtime
filesystem facade (1,462), 101 in transaction implementation/tests (2,746), and
15 in exports/inspection metadata (workspace 101,660). File count, ambient-I/O,
legacy-call and transitional-reference budgets are unchanged. The added API is
a concrete host/engine recovery boundary, not a new transaction representation.
