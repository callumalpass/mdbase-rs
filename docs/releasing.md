# Release Checklist

The release gate is intentionally reproducible and fails closed.

## Prerequisites

- Exact Rust toolchain from `rust-toolchain.toml`
- Pinned `mdbase-spec` checkout at the revision in
  `conformance/mdbase-spec-revision`
- PostgreSQL 17 for live runtime contracts
- `cargo-deny` 0.19 or the pinned GitHub Action

## Local qualification

```bash
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo test --locked -p mdbase-runtime --no-default-features
cargo test --locked -p mdbase-runtime --no-default-features --features sqlite
cargo check --locked -p mdbase-runtime --no-default-features --features postgres
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --all-features --no-deps
cargo deny check
./scripts/benchmark-release.sh
cargo package --locked --workspace
```

Set `MDBASE_SPEC_REPO_DIR` and `MDBASE_SPEC_TESTS_DIR` when the sibling pinned
checkout is not at `../mdbase-spec`.

The historical runner requires exactly 78 fixture files and 1,794 cases. The
v0.3 adapters also assert their per-suite case counts. Missing or malformed
fixtures are fatal.

## Live PostgreSQL

Set:

```text
MDBASE_RUNTIME_REQUIRE_POSTGRES=1
MDBASE_RUNTIME_TEST_DATABASE_URL=postgres://...
```

Then run:

```bash
cargo test --locked -p mdbase-runtime --all-features \
  --test postgres --test store_contract
```

## Publication

1. Confirm `CHANGELOG.md` and the conformance manifest match package versions.
2. Confirm the migration guide describes every lossy diagnostic introduced by
   the release.
3. Inspect `cargo package --list` for each package.
4. Publish `mdbase` before `mdbase-runtime`, because runtime depends on the new
   collection crate version.
5. Tag only the commit qualified by CI and the recorded benchmark.

Do not publish from a dirty tree or ignore an advisory to make the gate green.
