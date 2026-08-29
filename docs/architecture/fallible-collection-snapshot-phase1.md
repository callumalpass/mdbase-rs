# Fallible collection snapshot — Phase 1

Status: implemented on `consolidation/phase1-fallible-snapshot`; pre-release review record.

## Decision

Filesystem-backed operations now capture one authoritative, operation-scoped
collection generation. Discovery starts from the `cap_std::fs::Dir` retained
when `Collection` is opened, walks descendants with no-follow directory opens,
and loads every canonical record path once. The result owns sorted entries, an
O(1) path index, exact file facts/revisions, parsed or invalid record state, and
lazy operation-specific projections. Discovery, read, cache, and cancellation
failures are values rather than an empty collection or a partial success.

This deleted the old unchecked-scan mechanism instead of wrapping it. The
architecture budget `uncheckedCollectionScans` moved from **11 to 0**: callers
no longer use a best-effort full-tree scan that could turn an I/O failure into
zero records and then make a mutation decision from that false observation.
The checked scanner and its cancellation/error taxonomy are the sole operation
capture path.

## Two snapshot boundaries, not two authorities

`AuthoritativeCollectionSnapshot` is the mutation/planning boundary. Markdown
is authoritative; one capture discovers, no-follow opens, reads, classifies,
and indexes records. Operations may derive resolved-file and backlinks views
from that owned generation, but may not rescan to fill gaps.

The older `query::cache_source::CollectionSnapshot` remains a deliberately
separate **checked query cache boundary**. SQLite may supply its `FileRecord`s,
but cache lifecycle and staleness checks happen before construction and cache
failure is explicit. It is not an authoritative mutation snapshot and must not
be passed into operation planning. Phase 1 did not merge these types merely to
reduce the type count: their trust and freshness contracts differ.

## Capability and invalid-state model

`Collection` holds an opened root capability for its lifetime. Snapshot
discovery and record reads are relative to that handle and refuse symlinked
components. Ambient root paths remain only where a platform publication
primitive requires them, with public-root identity fences around that boundary.
Rename destination parents are prepared component-by-component from the held
root: open no-follow, create a missing directory relative to the current
capability, and reopen no-follow. Symlinks, non-directories, and replacement
races fail. Root identity is checked before preparation and immediately before
the ambient no-clobber rename.

A record load is closed over two owned outcomes:

- `Parsed` owns the exact document/layout plus raw and effective frontmatter.
- `Invalid::Frontmatter` owns valid-UTF-8 authored bytes as text/layout and an
  intrinsic malformed or non-mapping reason. Traversal still includes it with
  empty frontmatter, path-derived types, and its authored body.
- `Invalid::InvalidUtf8` has no invented text state. It remains an invalid,
  revision-addressable snapshot entry for validation and repair, while
  text-only traversal projections omit it.

Thus parse invalidity is not capture failure, and omission from a text
projection is not omission from the authoritative snapshot.

## Consumers and publication fences

The following consumers now plan from one capture:

- create/update generated values and validation corpora;
- batch selection, generated reservations, and final proposed-corpus
  validation;
- backfill selection and final validation;
- rename source selection, link resolution, and reference rewrite plans;
- validation, link resolution, backlinks, and `build_all_files_data`.

Batch type-only selection avoids resolved-file, computed-field, and backlinks
construction. Expression selection builds the resolved projection once and
constructs the link graph only when static expression analysis requires it.

A snapshot is evidence for planning, not a write lease. Consumers retain
publication fences: source and target existence/conflict checks, opaque byte
revisions, backfill per-write revision checks, batch transaction preconditions,
rename source/reference revision reloads, root identity checks, no-follow path
checks, and atomic no-clobber/create/replace primitives. Failures after rename
source publication are reported as bounded partial reference-update failures;
they do not cause stale planned bytes to be adopted.

## Known Phase 3 gaps

Phase 1 intentionally does not provide a collection-wide serializable
transaction for every legacy operation. Generated sequence allocation is still
snapshot-based before the collection write lock, so concurrent creates retain
the documented pre-Phase-3 sequence race. Multi-file rename plus reference
rewrites is not one atomic journal commit. Query-cache generations and
operation snapshots remain separate rather than sharing a durable generation
identity. Phase 3 must address allocation/commit serialization, unified durable
multi-file publication and recovery, and any cross-process generation fence;
none is implied by the Phase 1 snapshot name.

## Public and coordinated changes

`Collection::build_all_files_data` now returns
`Result<Vec<ResolvedFileData>, CollectionSnapshotError>`. The error and
`CollectionDiscoveryCause` are non-exhaustive and preserve I/O sources while
separating ambient filesystem paths from canonical collection paths.
`serialize_document` and `serialize_document_with_bom` also now return a typed
`Result` instead of panicking or substituting output.

Tasknotes TUI directly consumes `build_all_files_data`; its coordinated PR must
update both view-query and project-link call sites before this release is
promoted. The companion mdbase-spec PR supplies the invalid-record and
fallible-snapshot conformance expectations; this branch pins that reviewed spec
revision in both CI and `conformance/mdbase-spec-revision`. These coordinated
PRs are release prerequisites, not compatibility shims in mdbase-rs.

## Architecture budget review

The review baseline was **151 Rust files / 80,751 Rust source lines**. The final
Phase 1 budget is **156 files / 83,747 lines**. The five-file increase is
intentional ownership, not feature scattering: snapshot discovery is isolated
from snapshot state/projections, while rename planning, publication, hooks, and
focused tests are separated from link/body/frontmatter transformation. The
line increase pays for explicit error/state ownership, cancellation and cache
boundaries, operation migration, publication fences, hostile filesystem tests,
and regression/conformance coverage. Security checks and tests are code that
the former 11 best-effort scans did not own.

Incidental legacy-file budget deltas are recorded rather than hidden:

- serializer `Result` propagation through callers and fixtures;
- v0.3 duplicate-batch preflight before shadow/reservation work;
- watch and layout adaptations to owned invalid states;
- small command/profile and Phase 0 baseline propagation changes;
- a **reduction** in the resolver legacy budget after canonical resolution
  ownership moved into snapshot-backed indexes.

No new file may exceed the 1,000-line concentration ceiling. Existing files
above it retain per-file ceilings; only reviewed deltas changed, and the
resolver ceiling decreased. File counts, line counts, and concentration
budgets are review signals that force an ownership explanation. They are not
optimization goals, productivity targets, or permission to fill the available
headroom.
