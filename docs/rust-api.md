# Typed Rust API

`mdbase` 0.4 makes the typed API the application-facing collection boundary.
JSON remains appropriate for dynamic frontmatter and query records, but callers
no longer assemble unvalidated operation envelopes.

## Open and inspect a collection

```rust,no_run
use std::path::Path;
use mdbase::{Collection, CompatibilityMode};

let collection = Collection::open(Path::new("./notes")).expect("open collection");
match collection.compatibility_mode() {
    CompatibilityMode::Canonical => {}
    CompatibilityMode::V02ReadOnly => {
        eprintln!("read/query are available; migrate before writing");
    }
}
let records = collection.typed().expect("typed collection");
```

`Collection` owns its loaded invariants. Its root, settings, profile, type
registry, and warnings are exposed through immutable accessors.

## Paths and revisions

Every record operation accepts `CollectionPath`, which:

- normalizes `\` to `/`;
- rejects absolute paths, drive prefixes, `.` and `..`;
- rejects empty components, NUL bytes, and non-Unicode platform paths;
- resolves only below the opened collection root.

Existing symlink components are checked at filesystem access time. Do not turn
untrusted strings into paths by joining them onto `Collection::root()`.

Successful reads and persisted mutations return an opaque `Revision`. Supply it
as `if_revision` to prevent overwriting a concurrently changed record:

```rust,no_run
use std::path::Path;
use mdbase::api::{CollectionPath, ReadRequest, UpdateRequest};
use mdbase::Collection;
use serde_json::json;

let collection = Collection::open(Path::new("./notes")).expect("open collection");
let records = collection.typed().expect("typed collection");
let read = records
    .read(ReadRequest::new("tasks/example.md").expect("valid path"))
    .expect("read");

let mut update = UpdateRequest::new(
    CollectionPath::new("tasks/example.md").expect("valid path"),
    json!({"status": "done"}),
);
update.if_revision = Some(read.value.revision);
let updated = records.update(update).expect("record was unchanged");
assert!(updated.value.revision.is_some());
```

Treat revisions as opaque values. Do not parse or manufacture their current
`sha256:` representation.

## Outcomes and errors

Successful operations return `OperationOutcome<T>`:

- `value` is the typed result;
- `diagnostics` contains non-fatal warnings or information.

Failures return `MdbaseError`. For `Operation` and `LossyMigration`, inspect
`MdbaseError::diagnostics()` for stable codes, severity, path, field, schema
location, and optional details. Match diagnostic codes for program behavior;
messages are for humans.

## Dry runs

Set `dry_run = true` on create, update, delete, or rename requests to execute
validation and planning without modifying authoritative files. Create and
update previews run in a disposable shadow collection, so they exercise the
same persistence validation without leaking writes.

## Queries

`QueryRequest` supports type filters, CEL filtering, invocation context,
projections, selection, deterministic ordering/grouping, pagination, body
inclusion, and raw/effective frontmatter modes.

```rust,no_run
use std::path::Path;
use mdbase::api::{FrontmatterMode, QueryDirection, QueryRequest};
use mdbase::Collection;

let collection = Collection::open(Path::new("./notes")).expect("open collection");
let records = collection.typed().expect("typed collection");
let mut request = QueryRequest::builder()
    .type_name("task")
    .where_expression("priority <= 2 && status != 'done'")
    .order_by("priority", QueryDirection::Asc)
    .order_by("file.path", QueryDirection::Asc)
    .limit(50);
request.frontmatter = FrontmatterMode::Both;

let page = records.query(request).expect("query");
if page.value.has_more {
    println!("continue with snapshot {:?}", page.value.snapshot);
}
```

Reusing `snapshot` on a later page guarantees consistency. If that cache
generation is unavailable, the query fails with `query_snapshot_expired`
instead of silently returning a page from different data.

## Recoverable batches

Non-partial batches are the default. They validate in a shadow collection,
stage the complete desired state, journal preconditions, and commit under the
collection write gate.

```rust,no_run
use std::path::Path;
use mdbase::api::{
    BatchOperation, BatchRequest, CollectionPath, CreateRequest, UpdateRequest,
};
use mdbase::Collection;
use serde_json::json;

let collection = Collection::open(Path::new("./notes")).expect("open collection");
let records = collection.typed().expect("typed collection");
let batch = BatchRequest::new(vec![
    BatchOperation::Update(UpdateRequest::new(
        CollectionPath::new("tasks/one.md").expect("valid path"),
        json!({"status": "done"}),
    )),
    BatchOperation::Create(CreateRequest::new(
        CollectionPath::new("tasks/two.md").expect("valid path"),
        json!({"type": "task", "title": "Two"}),
    )),
])
.expect("non-empty batch");

let outcome = records.batch(batch).expect("commit batch");
assert_eq!(outcome.value.failed, 0);
```

Set `allow_partial = true` only when itemized best-effort semantics are
acceptable. Set `dry_run = true` to keep the entire batch in preflight.

## Legacy v0.2 collections

`typed().read()` and `typed().query()` translate v0.2 data through the isolated
compatibility adapter. Create, update, delete, rename, batch, backfill, and
manifest migration return `MdbaseError::MigrationRequired`.

Use `migrate_v02(V02MigrationRequest { ... })` or the CLI workflow described in
[the migration guide](migration-v02-to-v03.md). Drop and reopen the
`Collection` after applying migration so the canonical profile and compiled
type plans are loaded.
