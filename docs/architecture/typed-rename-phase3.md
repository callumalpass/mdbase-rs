# Phase 3: typed rename/preflight slice

`TypedCollection::rename` and `TypedCollection::preflight_rename` now enter the
version-neutral mutation service directly with `RenameRequest`. Preparation
captures canonical source and destination `CollectionPath` values, source
revision, effective types and stable ID, exact source bytes, and one
collection-wide reference-rewrite plan. Each reference write retains its
captured revision and exact desired document. `PlannedRename` keeps the exact
destination record, reference details, warnings, and partial failures across
publication.

Direct typed and v0.3 filesystem calls use one complete recoverable shadow. The
staged rename applies the captured plan directly to its caller-owned working
set; it does not construct another shadow or replan. The outer
`commit_shadow` publication supplies exact baseline CAS, destination no-clobber,
source revision checks, root-capability fencing, durable journals, and locked
committed file facts. Preflight evaluates the same plan but does not publish.

The v0.3 adapter alone decodes wire aliases, optional revision, `update_refs`,
`dry_run`, mtime compatibility, and simulation rejection. It serializes the
existing `OperationResult` shape from the typed plan. `Collection::rename` is
the sole `RenameInput` parser and translates once at the legacy edge. The old
v0.3 direct legacy dispatch and persisted-result hydration path have been
removed.

Rename outcomes are projected from planned exact bytes. No successful rename
rereads or stats the authoritative destination. Transaction-locked facts attach
size and mtime after durable publication, so post-commit replacement or removal
cannot change the returned revision, document, or size. BOM/CRLF source,
malformed opaque frontmatter, self references, canonical basename resolution,
aliases, anchors, embeds, stable IDs, warnings, and partial reference failures
continue through the shared planner.

Hosted runtime rename retains one full shadow because incoming-reference
planning is collection-wide. It uses the same staged typed plan and the existing
durable runtime journal. The exact before/after working-set evidence produces a
primary `Renamed` change plus `Updated` reference changes and is persisted for
claim attachment, commit resolution, restart recovery, and feed replay.

The architecture budget is 167 Rust files and 88,773 lines. Rename adds no
nested shadow and lowers the v0.3 facade-reference budget to 33; batch remains
the final Phase 3 typed mutation bridge.
