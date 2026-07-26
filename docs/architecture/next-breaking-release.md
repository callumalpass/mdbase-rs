# Next Breaking Release Architecture

Status: implemented for `mdbase` 0.4.0-rc.1
Date: 2026-07-26

## Purpose

The next `mdbase-rs` release is a deliberate breaking release. Its job is to
turn the conformance-driven implementation into a typed, reliable Rust
foundation without carrying the current JSON-shaped API or legacy execution
branches forward.

The mdbase specification version and the Rust crate version are independent:

- canonical collection semantics remain mdbase v0.3;
- the Rust crates may take a new pre-1.0 breaking version;
- v0.2.0 collections remain readable through a compatibility adapter and can
  be migrated explicitly.

## Design Principles

1. Markdown records and type files are authoritative.
2. SQLite indexes are derived accelerators, never silent alternate authority.
3. JSON is an edge format. Dynamic frontmatter remains JSON-like, while paths,
   requests, results, revisions, plans, diagnostics, and errors are typed.
4. A canonical v0.3 model drives all current operations.
5. v0.2 support is isolated at the load/import boundary.
6. Every mutation is planned from a consistent snapshot and guarded by
   revisions immediately before persistence.
7. Runtime durability claims are backed by versioned schemas and backend
   contract tests.
8. CLI, providers, and future network hosts share the same typed operation
   service.

## Scope

This release includes:

- a typed public collection API;
- private collection invariants;
- portable validated collection paths;
- fallible collection discovery and snapshots;
- fail-safe cache behavior;
- canonical v0.3 CLI operations;
- crash-recoverable multi-file mutations;
- precompiled type, match, computed-field, and formula plans;
- a read/import/migrate adapter for v0.2.0 collections;
- versioned runtime-store migrations;
- non-blocking async integration for the SQLite runtime store;
- hermetic conformance and backend qualification;
- public API and migration documentation.

This release does not require a large workspace split. Crate boundaries should
be extracted only after the internal seams are stable and measured.

## Canonical Public API

The public API will no longer accept arbitrary operation objects or return
hand-built JSON envelopes.

Illustrative shape:

```rust
let collection = Collection::open(root)?;
let records = collection.typed()?;
let record = records.read(ReadRequest::new("tasks/example.md")?)?;
let result = records.query(QueryRequest::builder().type_name("task"))?;
```

Core types:

```text
Collection
CollectionPath
Revision
ReadRequest / ReadResult
CreateRequest / CreateResult
UpdateRequest / UpdateResult
DeleteRequest / DeleteResult
RenameRequest / RenameResult
QueryRequest / QueryResult
BatchRequest / BatchResult
Diagnostic / DiagnosticCode / Severity
MdbaseError / MdbaseResult<T>
```

`serde_json::Value` remains appropriate for:

- raw and effective frontmatter;
- schema values;
- extension values;
- expression inputs and outputs where the specification is dynamic.

The external v0.3 `{ valid, result, diagnostics }` envelope remains available
as a wire adapter for CLI/provider consumers. It is not the core Rust error
model.

`Collection` fields become private. Read-only accessors expose the root,
settings, specification version, type registry, and compatibility mode without
allowing callers to invalidate path or schema guarantees after open.

## Portable Paths

`CollectionPath` is the only operation-facing record path type.

It:

- stores normalized forward-slash components;
- rejects empty, absolute, prefixed, traversal, NUL, and non-Unicode paths;
- has explicit conversion to a filesystem path below an authorized root;
- performs symlink-boundary validation at filesystem access time;
- produces the same logical value on Unix and Windows.

No core operation joins an unchecked string directly onto the collection root.

## Collection Snapshots

An operation builds or obtains one `CollectionSnapshot` with a stable
generation. A snapshot contains:

- discovered portable paths and metadata;
- parsed raw frontmatter;
- effective type membership;
- lazily requested body data;
- optional link/backlink and uniqueness indexes;
- diagnostics for unavailable or malformed resources;
- the configuration/type-registry revision used to interpret the files.

Snapshot requirements are planned before loading so metadata-only queries do
not eagerly retain every body. Writes reuse the same parsed snapshot for
validation, uniqueness, references, and final planning.

Discovery and loading are fallible. Permission, metadata, encoding, and
filesystem traversal errors cannot become an apparently valid empty result.

## Cache Policy

The index has one invariant:

```text
Successful cached results are equivalent to results from the authoritative
Markdown snapshot for the same generation.
```

Rules:

- cache schema, transaction, decode, staleness, or indexing failure invalidates
  that cache attempt;
- a cache snapshot token changes only after a complete successful transaction;
- partial refreshes are rolled back;
- explicit cache commands report failure honestly;
- library mode falls back to a disk snapshot and reports degraded cache health;
- runtime mode may either fall back or fail readiness according to host policy;
- an expected pagination snapshot that is unavailable returns
  `query_snapshot_expired`, never a different page from disk.

Fault tests cover malformed schemas, corrupt JSON rows, lock contention,
read-only databases, interrupted refresh, and deleted cache files.

## Multi-file Mutation Semantics

`allow_partial: false` means crash-recoverable non-partial intent, not merely
“validate first and stop after the first live error.”

The transaction protocol is:

1. build a complete typed operation plan from one snapshot;
2. stage new contents and backups under `.mdbase/transactions/<id>/`;
3. persist and fsync a journal containing target paths, before revisions,
   intended revisions, and transaction phase;
4. acquire the authoritative write gate;
5. recheck every affected revision and path boundary;
6. atomically replace individual files while advancing the journal;
7. fsync affected directories;
8. mark committed, refresh derived state, and remove recovery artifacts.

Recovery rules:

- a prepared transaction with no applied writes is discarded;
- a committing transaction is deterministically completed when all unchanged
  preconditions still hold;
- if external edits make automatic completion unsafe, recovery fails closed
  with a structured manual-recovery diagnostic;
- a committed transaction is idempotently finalized;
- no recovered transaction may silently leave an unreported mixed state.

This does not promise instantaneous atomic visibility to unrelated processes
reading multiple files directly. Hosts requiring that visibility must read
through the authoritative runtime.

`allow_partial: true` remains an explicitly itemized best-effort mode.

## Expressions And Type Plans

Type loading parses and compiles every expression exactly once. Invalid
computed or matching expressions reject the type definition rather than
falling back to substring analysis or disappearing at read time.

Compiled plans include:

- inheritance and merged field plans;
- match expressions;
- computed-field dependency order;
- generated/default lifecycle behavior;
- formula dependency graphs;
- strict/uniqueness/link validation metadata.

The Bases-compatible saved-view language may retain a distinct parser where
its syntax differs, but lexer/parser, value operations, dates, regex limits,
and diagnostics should be split into focused modules and tested against the
shared oracle.

## v0.2.0 Compatibility And Migration

Opening a v0.2.0 collection selects `CompatibilityMode::V02ReadOnly`.

The compatibility adapter:

1. parses legacy config and type files;
2. translates them into the canonical type registry;
3. records source-version and lossy/unsupported migration diagnostics;
4. supports typed read, query, validation, and migration planning;
5. rejects mutation with `migration_required`.

The canonical engine contains no `if v0.2` behavior branches.

Migration supports dry-run and transactional apply:

```text
mdb migrate-v02 --dry-run
mdb migrate-v02
```

It writes canonical v0.3 config/type wrappers, preserves a recovery manifest,
does not rewrite ordinary records unless required and requested, and verifies
that canonical reads match compatibility reads before commit.

## Sync And Async Boundaries

Filesystem collection operations remain synchronous and deterministic. Async
hosts move them onto their blocking execution boundary.

`mdbase-runtime` remains async because PostgreSQL and providers are async. Its
SQLite backend must not lock and execute `rusqlite` work directly on Tokio
executor threads. It will use a dedicated database worker boundary with
bounded request flow.

## Runtime Store Evolution

SQLite and PostgreSQL stores have explicit schema versions and ordered,
transactional migrations.

Opening a store:

- creates the latest schema when empty;
- migrates supported older versions;
- rejects unknown newer versions;
- records the installed version only after successful migration.

One backend contract suite runs against memory, SQLite, and live PostgreSQL.
It covers admission, deduplication, claims, stale leases, cancellation,
retention, queue ordering, replacement, timer reconciliation, firing, crash
recovery, and namespace fencing.

## CLI

For v0.3 collections, every CLI command uses the canonical typed operation
service and emits one consistent wire envelope.

The CLI adds:

- typed request input from JSON files/stdin for complete query and batch
  features;
- revision preconditions for mutations;
- dry-run and recoverable batch commands;
- cache status/rebuild/clear commands;
- v0.2 migration planning and apply;
- consistent diagnostic-derived exit codes.

## Conformance And CI

Release tests are hermetic:

- the exact specification fixture revision is pinned or vendored;
- mandatory suites fail when fixtures are missing;
- every adapter asserts an expected executed-case count;
- moving upstream conformance runs separately from the release gate.

CI includes:

- formatting and strict Clippy;
- all-feature and no-default-feature tests;
- Linux, macOS, and Windows core tests;
- live PostgreSQL backend tests;
- an explicit supported Rust version;
- Rustdoc with public missing-doc enforcement;
- package tests for every publishable crate;
- dependency advisory/license/source policy;
- a versioned public API baseline from which semver checks can be added after
  publication.

## Performance Qualification

Correctness failures may fall back to slower authoritative paths, but healthy
steady-state behavior must be measured.

The versioned release workload tracks:

- collection open;
- cold and warm metadata queries;
- filtered/formula/link queries;
- rename with reference updates;
- cache rebuild and no-op refresh;
- filesystem runtime startup and update/watch synchronization.

Runtime event admission, claims, timers, non-partial batch recovery, and
concurrent cache behavior are covered by deterministic regression and
backend-contract tests rather than unstable wall-clock budgets.

Expected improvements:

- fewer full collection scans per operation;
- no repeated expression parsing;
- less repeated frontmatter decoding;
- lower async tail latency under SQLite runtime load;
- one batch plan and one set of indexes per transaction.

Durability overhead is reported separately rather than hidden. No performance
optimization may reintroduce silent cache or filesystem failure.

## Delivery Order

1. Architecture and migration contract.
2. Fallible snapshots and cache-safe fallback.
3. Typed public API and private invariants.
4. Canonical CLI.
5. Recoverable multi-file transactions.
6. Compiled plans and v0.2 adapter/migration.
7. Runtime migrations and backend parity.
8. Hermetic CI, documentation, performance, and release qualification.

Each stage is committed independently and must pass its proportionate focused
tests, formatting, and strict Clippy checks. Full workspace qualification runs
at major boundaries and before release.
