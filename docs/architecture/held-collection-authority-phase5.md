# Phase 5: held collection authority

## Contract

`Collection::open(path)` acquires the collection directory once. The resulting
`CollectionRoot` retains a cloneable capability and the public display path.
The acquired directory identity is used only to select private derived cache
storage during acquisition. Display paths are no longer reopen, decision, or
publication authority.

Configuration, type, contract, schema, snapshot, and shadow resources are read
relative to the held directory with no-follow traversal. Type and contract
parsers receive private snapshots because their legacy parser APIs require
paths; those paths name only agent-owned temporary directories and never the
collection. Files with multiple hard links are rejected at the resource and
publication boundary on Unix.

`FilesystemProvider` owns the held root and refreshes with `reopen_held`.
Replacement of the public root name can therefore neither supply refreshed
configuration nor receive provider publication. Record create, update, delete,
and backfill publication use capability-relative parent handles and durable
file publication. Transaction locks, staging trees, payloads, journals, recovery,
and all record publication are capability-relative. SQLite's pathname-only API
is isolated in identity-keyed private storage outside the replaceable collection
name; live authorities for the same directory identity share that derived store.

## Atomic publication and durability

Capability-relative publication writes and syncs a private file, then publishes
it inside the held parent. Create uses a no-clobber hard-link publication step.
On Unix, replacement is `renameat` through cap-std with both names relative to
the same open parent, preserving atomic destination replacement. On Windows,
replacement uses `SetFileInformationByHandle(FileRenameInfoEx)` on the open
temporary-file handle, supplies the held destination-parent handle in
`FILE_RENAME_INFO.RootDirectory`, and requests
`FILE_RENAME_FLAG_REPLACE_IF_EXISTS`. Unsupported `FileRenameInfoEx` and all
other failures fail closed; there is no remove-then-rename fallback.

The file is synced before publication. Parent directories are synced after
publication on Unix. Windows directory handles do not provide the same portable
`sync_all` contract, so directory fsync is deliberately skipped there; the
atomic name replacement guarantee remains, but this implementation does not
claim Unix-equivalent directory-entry crash durability on Windows. CI executes
all tests on Windows and has an explicit Windows all-target compile gate; the
Windows-only replacement tests exercise the API layout and destination-replacing
publication.

### Caller audit

All collection record/resource publication callers terminate at
`CollectionRoot`:

- create-only: record create, type/view resource create, and transaction creation;
- destination-replacing: record update/backfill, type/view updates and rollback,
  rename reference publication, transaction journals, and runtime feed journals;
- no-clobber rename: record rename uses capability-relative hard-link publication
  followed by source unlink and never replaces an existing destination.

No audited caller performs ambient remove+rename or publishes through the
collection display path. Unix replacement retains `renameat` semantics; Windows
replacement retains one handle-rooted `FileRenameInfoEx` operation.

## Guard

The architecture checker scans every production Rust source in the workspace.
A closed `ambientIoAllowlist` inventories exact, non-growing per-file API
references for `std::fs` (including `File`/`OpenOptions`), `tokio::fs`,
`walkdir`, `tempfile`, and cap-std ambient acquisition. The checker parses Rust
with `syn`, recursively expands nested use trees, resolves renamed crates and
alias chains, and inspects ordinary, qualified, and macro-token paths. Its
owners are limited to low-level acquisition/adapters, private cache or
disposable workspace implementations, CLI/conformance path inputs, and watcher
registration. Any import or reference in an unowned domain or cross-file helper
fails regardless of alias or receiver spelling.

A separate display-root guard remains defense in depth: it rejects collection
display roots used with ambient filesystem calls, metadata/existence path
methods, `WalkDir`, root-scoped temporary files, collection reopening,
`CollectionPath::under`, aliases, and helpers. Synthetic tests cover arbitrary
aliases/receivers, unowned cross-file helpers, and display-root bypass classes.
Display-root use remains limited to acquisition, diagnostics/formatting, and OS
watcher registration.

## Intentionally retained seams

- SQLite still requires a filesystem pathname through `rusqlite`. Its path is
  therefore a private temporary store keyed by the held collection identity,
  never a descendant of the collection display path. The cache is derived and
  lasts while at least one authority for that identity remains live.
- Watch registration necessarily names the display path to the platform watch
  service. The real watcher acquires one `Collection` before registration and
  every full or incremental reconciliation reloads through that held authority.
  Root rename/replacement notifications can invalidate the snapshot but never
  cause the display path to be reopened.
- Public standalone `config::load_config(path)` remains a path-based inspection
  compatibility API. Collection opening and provider reload do not call it.
