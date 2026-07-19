# Changelog

All notable changes to this project are documented in this file.

## Unreleased

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
