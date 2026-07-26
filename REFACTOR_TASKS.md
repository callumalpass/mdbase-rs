# Codebase Improvement Tasks

Status: archived by the 0.4 typed-core release.

This original checklist is retained for historical context. Its architectural
requirements were superseded by
[`docs/architecture/next-breaking-release.md`](docs/architecture/next-breaking-release.md),
which deliberately allowed breaking API changes instead of preserving the
legacy JSON boundary.

Delivered equivalents include typed public requests/results, private
collection invariants, `CollectionPath`, AST-derived compiled expression
plans, fallible snapshots, fail-safe caching, decomposed rename modules,
recoverable batches, isolated v0.2 migration, focused integration/fault tests,
and hermetic release qualification. Further work should be filed against the
current architecture rather than appended here.

The checklist below is the pre-0.4 plan.

## Task 1: Restore a Green Baseline
- Fix current conformance regressions so the main suite starts from known-good behavior.
- Confirm `cargo test -q` succeeds (except any explicitly documented spec conflicts, if present).
- Add/adjust targeted tests if needed for the fixed regressions.

Done criteria:
- The two known failing conformance scenarios are fixed.
- No new test regressions are introduced.

## Task 2: Tooling and Build Hygiene
- Enforce consistent formatting (`cargo fmt --all`).
- Ensure strict lint cleanliness (`cargo clippy --all-targets --all-features -- -D warnings`).
- Remove unused dependencies from `Cargo.toml`.
- Replace manual unsafe TTY detection with safe standard-library APIs.

Done criteria:
- Formatting is clean and reproducible.
- Clippy has zero warnings under `-D warnings`.
- No unnecessary unsafe blocks remain for TTY checks.

## Task 3: Unify Path Safety Rules Across Operations
- Eliminate per-operation path validation drift.
- Route create/read/update/delete/rename path checks through shared helpers.
- Preserve existing error code semantics where externally visible.

Done criteria:
- Path validation logic is centralized and reused.
- Traversal/absolute/invalid path handling is consistent across operations.

## Task 4: Introduce Typed Operation Inputs/Outputs Internally
- Stop using ad-hoc `serde_json::Value` parsing deep inside operation logic.
- Add typed request/response structures at operation boundaries.
- Keep external JSON wire format stable via conversion layer.

Done criteria:
- Core operations parse into typed inputs before business logic.
- JSON construction is reduced to API edge conversion.

## Task 5: Unify Error Handling Pipeline
- Promote typed internal errors (`MdbaseError`, `Issue`) across operations.
- Centralize JSON serialization of errors/responses.
- Remove duplicated hand-built error object patterns.

Done criteria:
- Error construction is standardized.
- Mapping from internal errors to JSON is handled in one place.

## Task 6: Modularize Field Validation
- Split `validation/fields.rs` into smaller modules by field type/concern.
- Keep behavior fully compatible.
- Add focused tests around extracted validators.

Done criteria:
- Field validation code is modular and easier to navigate.
- Existing behavior and conformance results are preserved.

## Task 7: Modularize Rename and Reference Rewriting
- Split `operations/rename.rs` into orchestration + focused submodules (rewrite, mtime checks, scanning).
- Maintain current semantics for reference updates and partial failures.

Done criteria:
- Rename logic is decomposed into clear units.
- Existing rename conformance behavior remains intact.

## Task 8: Use AST-Based Formula Dependency Analysis
- Replace string-substring dependency detection in formula planning.
- Extract formula references from parsed expression AST.
- Improve cycle detection reliability and determinism.

Done criteria:
- Formula dependency graph is AST-driven.
- False positives/false negatives from string matching are removed.

## Task 9: Reduce Repeated Full-Collection Scans in Write Paths
- Identify repeated `scan_collection_files + read_to_string` patterns in uniqueness/link/rename workflows.
- Reuse shared indexed/parsed snapshots where practical.
- Keep correctness for conflict detection and validation.

Done criteria:
- Repeated scanning/read loops are reduced in hotspots.
- Behavior remains correct under existing tests.

## Task 10: Improve Test Architecture and Add Focused Coverage
- Add focused unit/integration tests for critical modules (path safety, formulas, rename rewrite, validators).
- Reduce reliance on only large scenario tests for regression detection.
- Keep conformance suite as final contract check.

Done criteria:
- New targeted tests are present and passing.
- Regression localization is improved (failures point to smaller units).

## Validation Checklist (run after each task)
- `cargo fmt --all`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test -q`
