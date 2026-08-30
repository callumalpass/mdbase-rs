# Phase 5: held collection authority

## Contract

`Collection::open(path)` acquires the collection directory once. The resulting
`CollectionRoot` retains a cloneable capability, the public display path, and
the acquired directory identity. Display paths are no longer reopen or
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

## Durability

Capability-relative publication writes and syncs a private file, publishes it
inside the held parent, and syncs that parent. Create uses a no-clobber hard-link
publication step; replacement uses a same-directory rename. The temporary link
is removed before success is returned.

## Guard

The architecture checker rejects ambient authority acquisition outside
`collection_root.rs`, display-path reopening in the filesystem provider, and
`self.root.join` in provider, snapshot, and shadow authority modules.

## Intentionally retained seams

- SQLite still requires a filesystem pathname through `rusqlite`. Its path is
  therefore a private temporary store keyed by the held collection identity,
  never a descendant of the collection display path. The cache is derived and
  lasts while at least one authority for that identity remains live.
- Watch registration necessarily names the display path to the platform watch
  service. Watch observations do not confer publication authority; provider
  refresh remains bound to the held root.
- Public standalone `config::load_config(path)` remains a path-based inspection
  compatibility API. Collection opening and provider reload do not call it.
