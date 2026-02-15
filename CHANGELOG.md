# Changelog

All notable changes to this project are documented in this file.

## Unreleased

### Added
- Support for spec v0.2.x configuration parsing, including `settings.migrations_folder`, `settings.write_defaults`, and `settings.timezone`.
- Backfill operation (§12.8) for applying defaults and generated fields across files.
- Migrate operation (§12.13) to execute migration manifests with backfill steps.
- Generated fields can now source from `file.*` metadata (`file.name`, `file.basename`, `file.ext`, `file.path`, `file.folder`).

### Changed
- Collection opening now accepts forward minor spec versions (e.g., 0.3.x) for forward‑compatibility while `load_config` remains strict.
- Types loader excludes migration manifests from type discovery; migrations folder is also excluded from collection scans.
- Create/update persistence honors `settings.write_defaults` and `settings.write_nulls` more precisely.
- Init defaults to `spec_version: "0.2.0"`.

### Fixed
- Explicit `null` values prevent generated/default values on create, per spec.
- Backfill result accounting matches conformance expectations (success vs skipped behavior).

### Tests
- Conformance runner supports `backfill` and `migrate` operations.
- v0.2.0 conformance suite passes at Level 6 in the latest full run.
