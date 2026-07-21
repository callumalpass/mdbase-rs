---
type: task
title: Finish v0.3 conformance and runtime contracts
status: done
priority: urgent
tags:
  - v0.3
  - conformance
  - runtime-contracts
  - architecture
  - performance
  - mdbase-connect
contexts:
  - rust
  - mdbase
  - connect
projects:
  - '[[projects/mdbase-rs]]'
dateCreated: '2026-07-22T00:13:29+10:00'
dateModified: '2026-07-22T01:34:25+10:00'
recurrenceAnchor: scheduled
---

## Summary

Qualify the existing mdbase v0.3 core behavior, add a pure Runtime Contracts
0.1 registry and preflight layer, and refactor the crate into a maintainable,
observable, performant foundation for `mdbase-connect` and other long-running
Rust hosts.

The collection engine remains the source of filesystem semantics. Runtime
contracts must be optional, deterministic, deny-by-default at authorization
boundaries, and usable without a workflow executor.

## Starting State

- The checkout contains uncommitted v0.3 CEL, lifecycle, query/view schema, and
  runtime-provider work that predates this task.
- The published conformance declaration only claims `core_read` and
  `collection_semantics` for `0.3.0-alpha.1`, while the crate is now
  `0.3.0-rc.1`.
- Targeted v0.3 core, CEL, and lifecycle tests pass at task start.
- Runtime Contracts registry composition and workflow execution are not yet
  implemented in `mdbase-rs`.

## Plan

1. Audit and qualify the existing uncommitted v0.3 work; establish full test,
   formatting, lint, and package baselines.
2. Close implementation and fixture gaps for existing core profiles, then
   publish an exact-version evidence-scoped conformance claim.
3. Introduce typed runtime-contract records, canonical schemas, deterministic
   source/origin-aware registry composition, strict validation, preflight,
   event/action validation, and explicit materialization helpers.
4. Keep runtime contracts separate from workflow execution and expose stable
   embedding APIs suitable for `mdbase-connect` providers.
5. Add structured performance instrumentation and opt-in error reporting with
   no hidden global logger or payload leakage.
6. Add unit, integration, cross-language fixture, real-filesystem end-to-end,
   concurrency, fault-injection, property/adversarial, and benchmark coverage.
7. Refactor only behind green tests; commit each coherent stage.
8. Run full qualification, packaging, performance checks, and an adversarial
   final review before marking complete.

## Architectural Constraints

- `Collection::open` remains deterministic and runtime-neutral.
- Runtime contract sources are explicit: built-in, provider, installed pack,
  and collection.
- Synced runtime policy records never authorize operations by their mere
  presence.
- Contract materialization never writes without an explicit caller request.
- `mdbase-connect` remains the final authorization boundary for remote
  filesystem operations.
- Public APIs do not expose local collection paths or record payloads to any
  control-plane abstraction.
- Rust and TypeScript behavior stays aligned through canonical schemas and
  shared fixtures rather than duplicated protocol inventions.

## Acceptance

- Exact-version conformance claims validate and are backed by reproducible
  commands.
- Runtime Contracts 0.1 shared fixtures pass from the Rust adapter.
- Virtual event/workflow/action/provider/capability contracts compose without
  Markdown materialization.
- Registry conflicts, invalid schemas, missing references, policy mistakes,
  unsafe paths, duplicate IDs, and malformed envelopes fail deterministically.
- Real watcher/provider end-to-end tests prove cache-before-notification and
  request serialization behavior.
- Full tests, strict clippy, formatting, package smoke, benchmarks, and
  adversarial tests pass.

## Notes

Existing dirty source changes are treated as the starting implementation and
will be reviewed rather than discarded. Unrelated user state, including
transient `.ops/.mdbase/` cache changes, will not be committed.

## Delivered

- Claimed and validated every implemented non-execution v0.3 profile:
  `core_read`, `collection_semantics`, `cel`, `cel_match`, `cel_query`,
  `links`, `core_write`, `lifecycle`, `runtime_contracts/0.1`, and `watch`.
- Added a pure, typed Runtime Contracts engine with canonical vendored schemas,
  deterministic source composition, origin preservation, atomic provider
  registration, embedded schema caching, workflow preflight, event/action
  validation, virtual contracts, and explicit Markdown materialization.
- Kept workflow execution outside the claim and made embedding-host final
  authorization explicit in the API, tests, claim limits, and documentation.
- Split provider runtime, observer, filesystem runtime, operation, query
  execution, context, result, preflight, and diagnostic concerns into focused
  modules.
- Added payload-free per-stage provider performance observations and opt-in
  code/message error observations, including early provider failures and an
  optional `tracing` adapter.
- Added effective-registry revisions and runtime-aware Watch recomposition
  without materializing virtual provider contracts.
- Replaced check-then-write/create/rename races with no-clobber persistence and
  contained direct and indirect filesystem paths across CRUD, types, config,
  scans, queries, links, cache, migrations, batch, runtime loading, and watch.
- Scoped batch shadow copies to collection-visible records and required
  type/schema assets.

## Verification Evidence

- Shared v0.3 core, CEL, lifecycle, and Runtime Contracts fixtures pass.
- Full legacy conformance passes.
- Strict all-feature Clippy passes.
- Real filesystem, runtime-aware watch, provider serialization, concurrent
  writer, malformed schema, policy, authorization, and symlink escape tests
  pass.
- Release registry composition: 2,000 virtual event contracts in 25.7 ms on
  Linux x86_64 / Rust 1.94.0.
- Synthetic 2,000-record profile: read p50 0.030 ms, basic query p50 63.1 ms,
  update p50 24.8 ms, and cache rebuild p50 71.3 ms.
- mdbase-connect passes Rust workspace tests (35), TypeScript typechecking and
  unit/integration tests, production builds, and the local-agent end-to-end
  relay/request path.

## Handoff

The effective Runtime Contracts registry is intentionally not a workflow
executor. mdbase-connect can inject live provider contracts through
`ContractSource`, load them inside `FilesystemProvider`'s authority gate, and
use the returned preflight result as advisory input before applying its own
exact local grant immediately before dispatch.

Final qualification completed with clean formatting, warning-free Rustdoc,
strict all-target/all-feature Clippy, full all-feature tests with the shared
v0.3 conformance gate required, validated claim schema, verified Cargo package,
and the complete mdbase-connect downstream test/build/e2e matrix.
