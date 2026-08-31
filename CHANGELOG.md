# Changelog

All notable changes to this project are documented in this file.

## Unreleased

### Breaking

- Context-free JSON `Collection` create/update/delete/rename/backfill/batch
  methods are isolated behind the default-on `legacy-collection-mutation` 0.4.x
  compatibility feature. They retain strict source compatibility, including
  under `deny(deprecated)`, while rustdoc records their planned 0.5.0 removal.
- Deprecated ephemeral `ExecutionOutcome::result` and `CommitRejection::result`
  projections now have a 0.5.0 removal gate; typed callers use `operation`.
- `Collection::build_all_files_data` now returns
  `Result<Vec<ResolvedFileData>, CollectionSnapshotError>` instead of silently
  treating discovery/read failures as an empty collection. This is a deliberate
  pre-release Rust API break. The external Tasknotes TUI is a known direct
  consumer and must migrate its view-query and project-link callers before the
  next release. `CollectionSnapshotError` is now a non-exhaustive typed error:
  discovery causes and record-read failures retain `std::io::Error` sources,
  and filesystem paths are distinct from canonical collection-relative paths.
  `CollectionDiscoveryCause` is publicly re-exported for exhaustive cause
  inspection within a wildcard match. Traversal projections include
  valid-UTF-8 authored records whose frontmatter is malformed or non-mapping:
  they contribute empty frontmatter, path-derived types, and their authored
  body. Invalid-UTF-8 records remain snapshot-invalid and repairable, but are
  omitted from text traversal projections.
- `frontmatter::serializer::serialize_document` and
  `serialize_document_with_bom` now return
  `Result<String, FrontmatterSerializationError>`. Callers must handle YAML
  emitter failures; serialization no longer panics or substitutes empty YAML.

### Added

- Added privacy-safe `legacy_journal_inventory` runtime/provider APIs so operators
  can prove that no version-2 journals remain before the 0.5.0 decoder removal.
- Added exact architecture ownership guards for the seven legacy Collection
  facade definitions and internal compatibility allowlist, plus non-growing
  guards for `OperationContext::legacy`, wire-only outcome variants/constructors,
  and ephemeral runtime result projections.

### Fixed

- Generated values now evaluate against effective defaults and dependencies,
  sequence starts are floors, and sequence exhaustion returns the typed
  `generated_sequence_overflow` error without wrapping or consuming a
  reservation.
- Explicit v0.3 batches reject adjacent and non-adjacent duplicate mutation
  paths with stable `duplicate_batch_path` diagnostics before shadow creation,
  generated-value reservation, dry-run, or real mutation in either partial
  mode.
- Authoritative snapshots retain sorted entries with a single canonical path
  owner and a one-time path-to-index map. Invalid record loads now use closed
  owned states: authored UTF-8 failures always retain their exact document and
  layout with a restricted frontmatter reason, while invalid UTF-8 has no text
  state and an intrinsic reason. Batch type-only selection constructs no
  resolved-file, computed-field, or backlinks projection; expression selection
  reuses one resolved-file projection and omits the link graph when the existing
  expression analyzer proves it unnecessary.
- The internal record loader now accepts only a capability-bound collection and
  collection-relative path; the obsolete ignored absolute-path argument was
  removed without a compatibility wrapper.
- Batch and backfill validate final effective proposed corpora, and backfill
  byte-revision fences every real write against edits made after planning.
- Rename and reference rewriting now plan from one authoritative snapshot;
  source and reference publication boundaries reject stale byte revisions,
  unavailable records, replaced collection roots, and no-follow destination
  parent races without adopting new data or creating directories outside the
  held root capability. Body and frontmatter rewrites share canonical
  source-relative link selection, preserve case-insensitive configured-ID links
  in every rewrite context, and
  dry-run includes projected self-reference updates exactly as execution plans
  them.

## 0.4.0-rc.4 - 2026-08-10

### Fixed

- Current application collection-setup assessments now prove configuration,
  type-pack, provision-lock, revision, and validation state without cloning the
  complete collection into a second preflight workspace. Applicable setup
  changes retain the existing staged, revision-safe review path; non-mutating
  conflicts are assessed directly.

## 0.4.0-rc.3 - 2026-08-07

### Breaking

- Removed the parallel `runtime_contracts` registry, runtime-specific schemas,
  provider loader, and runtime-aware watcher mode.
- Rebuilt `mdbase-runtime` as version `0.3.0-rc.1` for Runtime companion
  profile 0.2.
- Runtime admission now consumes ordinary core contract artifacts, projected
  workflow/policy records, and verified interoperability declarations.
- Provider registration now binds executable handlers to an exact provider
  declaration digest and handler ID; action names and Markdown records cannot
  activate code.
- Workers execute immutable admitted plans and exchange the shared
  interoperability action invocation/outcome envelopes.
- Removed the non-standard query snapshot request/result API to match the
  canonical v0.3 query schemas.

### Changed

- Watch events used by runtime adapters are structured CloudEvents with exact
  contract and implementation evidence.
- Timers pin an exact event contract and verified source identity.
- Core conformance no longer claims a runtime profile; durable runtime
  conformance is independently versioned.
- Queries evaluate date and datetime operations in an explicit collection
  authority timezone, including daylight-saving transitions.

### Added

- Transactional data-contract type-pack evolution with reviewed diffs,
  conflict detection, atomic installation, and recovery.
- Portable testbed coverage for shared contracts and durable runtime recovery.

### Fixed

- Saved-view logical `and`, `or`, and `not` filter arrays deserialize in their
  canonical forms.
- The real-filesystem debounce regression test now allows for scheduling pauses
  on shared CI runners while still asserting one final-state event.

## 0.4.0-rc.2 - 2026-07-28

### Breaking

- Make `mdbase.contract` a discriminated `record`, `event`, or `action`
  artifact and restrict type-file implementations to record contracts.
- Removed the standalone `mdb` and `mdb-profile` binaries. The final native
  executable is now the unified `mdbase` binary assembled by mdbase Connect.
- Removed the separately installed `mdb-fzf` helper; specialized interactive
  presentation is not a second core CLI surface.

### Added
- Added the transport-neutral `mdbase-command` crate for canonical CLI
  parsing, direct/daemon operation mapping, Watch-profile streaming, and
  deterministic engine profiling.
- Added canonical list/create/read/update type-resource commands and public
  wire encoders for query and batch requests.
- Exposed the embedded engine version for unified release diagnostics.

### Changed

- Expose every contract kind through the core registry while retaining record
  projection and binding semantics only for record contracts.
- Withdraw the legacy Runtime Contracts conformance claim until the durable
  runtime is rebuilt on the portable event/action interoperability profile.

### Tests
- Moved CLI lifecycle and security regressions onto the command adapter and
  added direct-versus-daemon parity coverage in the final executable.

## 0.4.0-rc.1 - 2026-07-26

### Breaking
- Replaced the application-facing JSON operation surface with typed paths,
  requests, results, diagnostics, revisions, outcomes, and errors under
  `mdbase::api`.
- Made `Collection` invariants private and exposed immutable inspection
  accessors plus `Collection::typed()`.
- Made canonical v0.3 the only writable profile. v0.2 collections are
  read/query compatible but return `migration_required` for mutations.
- Changed rename reference updates to the CLI default; use
  `--no-update-refs` to opt out.
- Raised the minimum supported Rust version to 1.94.0.
- Released `mdbase-runtime` 0.2.0-rc.1 with versioned store schemas.

### Added
- A verified, dry-runnable, crash-recoverable v0.2-to-v0.3 translator with
  explicit opt-in for lossy mappings.
- Typed recoverable batch requests/results and CLI JSON request files for
  complete queries and batches.
- Mutation `--if-revision` and `--dry-run` CLI controls.
- Portable `CollectionPath` and opaque `Revision` types.
- Fallible authoritative collection snapshots and cache fault recovery.
- Transaction journals, staging, recovery, and cross-process write gating for
  non-partial batches and migration.
- Precompiled type match, computed-field, lifecycle, and formula plans.
- Explicit SQLite/PostgreSQL runtime schema migrations and a bounded dedicated
  SQLite worker thread.
- One shared runtime store contract suite across memory, SQLite, and
  PostgreSQL.
- Hermetic pinned conformance counts, a cross-platform/feature/live-PostgreSQL
  CI matrix, and dependency advisory/license/source policy.
- Versioned, enforceable p95 release performance budgets.
- A real debounced `CollectionWatcher` backed by filesystem notifications and
  final-state collection snapshot diffs.
- Opaque `if_revision` preconditions for create, update, delete, and rename.
- Canonical v0.3 query and view schemas, including ordinary Markdown view-record
  validation through the existing type-file pipeline.
- Canonical CEL Match and Query execution, with explicit invocation
  context, ordered projections, selection, grouping, summaries, and pagination.
- Runtime Contracts 0.1 loading, deterministic registry composition, workflow
  preflight, event/action validation, virtual contracts, and materialization.
- The independently versioned `mdbase-runtime` crate with atomic event/run
  admission, leases, pinned execution plans, stable invocation receipts,
  cancellation, honest indeterminate outcomes, one-shot timers, journal
  retention, and in-memory, SQLite, and PostgreSQL stores.
- A provider/runtime boundary with payload-free performance observations,
  opt-in error observations, and optional structured `tracing` output.
- Portable Watch notifications and runtime-aware effective-registry changes.

### Changed
- v0.3 queries now return the canonical operation envelope.
- Watch events use stable sequence numbers, timestamps, revisions, and changed
  frontmatter field metadata.
- Runtime schemas and embedded action/event schemas are compiled once and
  reused across registry loads and validation.
- Dispatch validates against the admitted action snapshot while rechecking
  current runtime policy and host authorization immediately before every
  provider call.
- Batch preflight shadows only collection-visible files and required type/schema
  assets rather than caches, excluded trees, or nested collections.

### Fixed
- Cache corruption, lock contention, decode failures, and refresh failures now
  fall back to authoritative disk snapshots or fail explicitly; they cannot
  become successful empty/stale query results.
- Stale SQLite runtime claim tokens now return the same `stale_lease`
  diagnostic as memory and PostgreSQL.
- Updated `rand` to 0.8.7 to address RUSTSEC-2026-0097 without suppressing the
  advisory.
- Collection operations and discovery reject symlink escapes, including config,
  type, cache, validation, link, migration, batch, runtime, query, and watch
  paths.
- Record and type creation and record rename use atomic no-clobber persistence
  under concurrent writers.
- Provider performance observations are emitted for early failures as well as
  successful and validation-failing operations.

### Tests
- Require all 78 pinned historical fixture files and 1,794 cases, plus exact
  canonical v0.3 suite counts; missing or malformed fixtures are fatal.
- Added migration equivalence, interrupted-commit recovery, cache
  fault-injection, typed API/CLI, backend schema rollback, and shared store
  contract tests.
- Qualified live PostgreSQL 17 and the release performance workload.
- Added shared Runtime Contracts fixture execution, filesystem-backed registry
  end-to-end tests, runtime-aware watch tests, a 2,000-contract performance
  regression, concurrent writer tests, and adversarial boundary tests.
- Added fake-clock execution tests, property-based event deduplication, SQLite
  restart recovery, live PostgreSQL namespace/race/timer coverage, and bounded
  admission throughput profiles.

## 0.3.0-rc.1 - 2026-07-19

### Added
- v0.3 config and `mdbase.type` wrapper loading alongside the v0.2 adapter.
- JSON Schema 2020-12 record validation with canonical `schema_*` diagnostics.
- Canonical v0.3 schema artifacts and collection inspection APIs under `mdbase::v03`.
- A `Collection::v03_operations()` facade with canonical operation envelopes,
  structured diagnostics, persisted mutation results, and SHA-256 revisions.
- Support for spec v0.2.x configuration parsing, including `settings.migrations_folder`, `settings.write_defaults`, and `settings.timezone`.
- Backfill operation (§12.8) for applying defaults and generated fields across files.
- Migrate operation (§12.13) to execute migration manifests with backfill steps.
- Generated fields can now source from `file.*` metadata (`file.name`, `file.basename`, `file.ext`, `file.path`, `file.folder`).

### Changed
- New collections and the profiler use the stable `0.3.0` protocol marker;
  the earlier `0.3.0-alpha.1` marker and v0.2.x remain readable through
  explicit compatibility paths.
- Types loader excludes migration manifests from type discovery; migrations folder is also excluded from collection scans.
- Create/update persistence honors `settings.write_defaults` and `settings.write_nulls` more precisely.

### Fixed
- Explicit `null` values prevent generated/default values on create, per spec.
- Backfill result accounting matches conformance expectations (success vs skipped behavior).
- v0.3 `collection.links` rules apply to JSON Schema strings and arrays rather
  than requiring the legacy `link` field type.
- v0.3 traversal failures use the canonical `path_traversal` diagnostic while
  v0.2 retains `invalid_path`.

### Tests
- Conformance runner supports `backfill` and `migrate` operations.
- v0.2.0 conformance suite passes at Level 6 in the latest full run.
- The Rust adapter executes the shared v0.3 core-collection fixture.
