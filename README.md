# mdbase-rs

Rust implementation of the [mdbase spec](https://mdbase.dev), with:

- Typed frontmatter loading/validation
- Query execution (filters, formulas, grouping, summaries)
- Link parsing/resolution/traversal
- CRUD, batch, backfill, and migrate operations
- SQLite cache support
- Watch event simulation for conformance testing

## Packages

- Library crate: `mdbase`
- CLI binary: `mdb` (`init`, CRUD/query/validate, `backfill`, `migrate`, cache)

## Build

```bash
cargo build
```

## Test

```bash
cargo test
```

## Notes

- Conformance coverage is tracked in `tests/conformance.rs`.
- Operational status notes are in `PROGRESS.md`.
- Release-level changes are in `CHANGELOG.md`.
