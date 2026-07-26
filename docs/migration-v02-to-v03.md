# Migrating v0.2 Collections to v0.3

`mdbase` 0.4 opens v0.2 collections in `V02ReadOnly` compatibility mode. Reads
and queries continue to work, but mutations are blocked until the collection
is explicitly migrated. This keeps legacy translation out of the canonical
write pipeline.

## Before migrating

Commit the collection to version control or take a filesystem snapshot. The
migration itself is crash-recoverable, but a user-owned backup is still the
right boundary for approving semantic changes.

Run the read-only checks you depend on:

```bash
mdb -C ./notes validate
mdb -C ./notes query --types task --limit 10
```

## 1. Inspect the verified plan

```bash
mdb -C ./notes migrate-v02 --dry-run --pretty
```

The plan reports:

- every canonical config/type artifact to create, replace, or remove;
- before and after revisions;
- the number of records read through both adapters and compared;
- translation diagnostics;
- the recovery/provenance manifest path.

Dry-run does not write the collection or migration manifest.

## 2. Review lossy diagnostics

Most v0.2 fields map directly to JSON Schema plus v0.3 collection lifecycle
metadata. A feature that cannot preserve future write behavior is reported as
`migration_lossy`.

The apply command refuses lossy plans by default. Do not use `--allow-lossy`
until each diagnostic has been reviewed:

```bash
mdb -C ./notes migrate-v02 --dry-run --pretty
mdb -C ./notes migrate-v02 --allow-lossy
```

Existing record Markdown is not rewritten merely to change the type format.
The verifier reads every record through the v0.2 adapter and a temporary
canonical v0.3 collection and rejects the plan if effective reads differ.

## 3. Apply and reopen

For a lossless plan:

```bash
mdb -C ./notes migrate-v02
mdb -C ./notes validate
```

The apply step:

1. writes a canonical `spec_version: 0.3.0` config;
2. writes `mdbase.type` wrappers with embedded JSON Schema;
3. removes superseded legacy type sources;
4. writes `.mdbase/migrations/v02-to-v03-<id>.json`;
5. commits the complete change through the transaction journal.

Long-running applications must drop and reopen `Collection` after apply.

## Crash recovery

Migration uses the same staged transaction protocol as non-partial batches.
An interrupted commit leaves a journal under `.mdbase/transactions/`. The next
`Collection::open` recovers before loading configuration or types:

- prepared transactions with no writes are discarded;
- safe in-progress commits are completed deterministically;
- external edits that invalidate preconditions fail closed with a manual
  recovery diagnostic;
- committed transactions are finalized idempotently.

Do not manually remove transaction artifacts unless the diagnostic explicitly
requires operator recovery and you have inspected the journal and files.

## Rust API

```rust,no_run
use std::path::Path;
use mdbase::api::V02MigrationRequest;
use mdbase::Collection;

let collection = Collection::open(Path::new("./notes")).expect("open v0.2 collection");
let records = collection.typed().expect("typed adapter");

let plan = records
    .migrate_v02(V02MigrationRequest {
        dry_run: true,
        allow_lossy: false,
    })
    .expect("verified plan");
assert!(!plan.applied);

let applied = records
    .migrate_v02(V02MigrationRequest {
        dry_run: false,
        allow_lossy: false,
    })
    .expect("apply migration");
assert!(applied.applied);
```

If `MdbaseError::LossyMigration` is returned, inspect its diagnostics and issue
a second explicit request with `allow_lossy: true` only after approval.

## Rollback

Before the first write, rollback means abandoning the dry-run. During an
interrupted commit, let `Collection::open` run automatic recovery first.

After a completed migration, rollback is a source-control operation: restore
the pre-migration config and type files together. Do not restore only
`mdbase.yaml`; mixed v0.2/v0.3 definitions are intentionally rejected.
