# Collection runtime contract — Phase 0 API review

Status: proposed. This document freezes the provider boundary for the
coordinated local collection runtime task. It intentionally contains no
implementation or wire-protocol change.

## Review evidence

The current provider exposes `CollectionProvider::execute` as
`Result<OperationResult, ProviderError>` (`src/runtime/provider.rs:18-25`).
`FilesystemProvider` serializes mutations with an `RwLock`, reloads the
collection when a control-resource stamp changes, and returns the portable
operation envelope (`src/runtime/provider.rs:135-243,245-297`). The runtime
then calls `OperationRequest::affected_paths()` and asks a second
`CollectionWatcher` to rescan those paths after a successful mutation
(`src/runtime/filesystem.rs:12-65`).

`affected_paths()` is an operation/result decoder: it reads `path`, `from`,
`to`, `references_updated`, and `partial_updates.failed` from JSON
(`src/runtime/operation.rs:84-136`). This is the ownership boundary to delete
after callers consume the canonical outcome. The current watcher is a
debounced snapshot comparison with full-rescan fallback and a separate worker
thread (`src/watch/real.rs:26-47,165-280`).

The filesystem transaction already has an opaque UUID journal identity,
durable `prepared`/`committing`/`committed` phases, exact before/after
revisions, and idempotent recovery (`src/transactions.rs:63-97,203-242,
287-384`). A Phase 0 API should expose that identity without exposing journal
paths or record bytes.

## Proposed provider API

The names are review targets, not a request to implement them in this phase.
The types are mdbase types and must not import Connect grants, applications,
request IDs, relay concepts, or retry policy.

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct CollectionGeneration {
    // Opaque to hosts. The epoch changes when a runtime is reopened.
    pub(crate) runtime_epoch: Uuid,
    pub(crate) sequence: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct CommitId(Uuid);

pub struct ExecutionOutcome {
    pub result: OperationResult,
    pub generation: CollectionGeneration,
    pub changes: ChangeSet,
    pub commit_id: Option<CommitId>,
}

pub enum ChangeSet {
    None,
    Exact(ChangeBatch),
    CollectionWide { reason: RebuildReason },
}

// Immutable exact changes. Small batches may be inline; large batches are
// paged from mdbase-owned transaction/runtime support state.
pub struct ChangeBatch { /* len, digest, bounded page reader */ }

pub enum CanonicalChange {
    Record(RecordChange),
    Resource(ResourceChange),
}

pub struct RecordChange {
    pub kind: RecordChangeKind,
    pub path: CollectionPath,
    pub from: Option<CollectionPath>,
    pub before_revision: Option<Revision>,
    pub after_revision: Option<Revision>,
}

pub enum RecordChangeKind { Created, Updated, Deleted, Renamed }

pub struct ResourceChange {
    pub kind: ResourceChangeKind,
    pub path: CollectionPath,
    pub before_revision: Option<Revision>,
    pub after_revision: Option<Revision>,
}

pub enum ResourceChangeKind {
    Configuration, TypeDefinition, Contract, ViewSource, Other,
}

pub struct ExternalChange {
    pub generation: CollectionGeneration,
    pub changes: ChangeSet,
    pub origin: ExternalChangeOrigin,
}

pub enum ExternalChangeOrigin { Filesystem, RecoveryReconciliation }

pub struct PreparedMutation { /* opaque, host cannot inspect or forge */ }

impl PreparedMutation {
    // Available only after the prepared journal is durable.
    pub fn commit_id(&self) -> &CommitId;
}

pub enum DurableCommitState {
    Prepared,
    Committing,
    Committed { changes: ChangeSet },
    CancelledBeforeCommit,
    NeedsManualRecovery,
}

pub trait CollectionRuntime {
    fn read(&self, request: &OperationRequest,
            cancellation: &OperationCancellation)
        -> Result<ExecutionOutcome, ProviderError>;
    fn prepare(&self, request: &OperationRequest,
               cancellation: &OperationCancellation)
        -> Result<PreparedMutation, ProviderError>;
    fn commit(&self, mutation: PreparedMutation)
        -> Result<ExecutionOutcome, ProviderError>;
    fn cancel(&self, mutation: PreparedMutation)
        -> Result<CancelOutcome, ProviderError>;
    fn resolve_commit(&self, commit_id: &CommitId)
        -> Result<Option<DurableCommitState>, ProviderError>;
    fn recv_external_change(&self, timeout: Duration)
        -> Result<Option<ExternalChange>, ProviderError>;
}
```

The final public API may use a generic `ExecutionOutcome<T>` or a separate
`PreparedOperation`; the invariants below are normative. A convenience
`execute` may call `prepare` and `commit`, but it must not create a second
transaction model.

### Generation and commit identity

`CollectionGeneration` orders observations in one runtime. A successful
known mutation or accepted external change advances its sequence exactly once.
Reads return the generation observed, including invalid/no-op results, but do
not advance it. A generation contains an opaque runtime epoch, so a cursor
from before a process restart fails with `generation_expired` rather than
silently mixing snapshots. Retaining generations across restart would require
durable snapshot state and is deliberately not part of Phase 0.

`CommitId` is different from an application request ID. It is created by
mdbase for a filesystem transaction and must survive process termination while
the transaction journal or its bounded completed marker is recoverable. It is
an opaque UUID (or equivalent) and contains no path, record content, grant, or
application identity. A host may durably associate it with its own request,
but the two identifiers must never be substituted for one another.

### Prepare, commit, and cancellation

Preparation validates inputs, resolves references, computes exact before/after
revisions, and stages the durable transaction. It checks the caller's
`OperationCancellation` at bounded points. A successful `prepare` has durable
evidence and exposes its opaque `CommitId`, but has not changed canonical
Markdown. The host must be able to persist its own request-to-commit mapping
before invoking `commit`; returning the identifier only in the post-commit
`ExecutionOutcome` would leave an unresolvable crash window.

The required hand-off is:

1. mdbase durably prepares the transaction and returns `PreparedMutation` plus
   its `CommitId`;
2. the host durably records its independent request-to-commit association;
3. the host invokes `commit` with the opaque prepared handle; and
4. the host records its application-facing receipt after mdbase reports or
   recovers the durable commit state.

If the process stops after step 1 but before step 2, startup may discard the
unowned prepared transaction because no canonical write began. After step 2,
`resolve_commit` must distinguish prepared, committing, committed, cancelled,
and manual-recovery states for long enough to finish the host's independent
journal. The mdbase resolution proves the filesystem write set; it does not
become an application receipt or acquire host request semantics.

`commit` is the ownership transfer. Before the first canonical filesystem
write, `cancel` may discard the prepared transaction and return
`CancelledBeforeCommit`. Once commit begins, cancellation is ignored by the
filesystem writer: commit finishes or startup recovery finishes it. The host
receives either a committed `ExecutionOutcome` or a typed recovery/error state;
it must not infer `not_sent` after durable commit begins.

`cancel` is idempotent. Calling it after commit returns `AlreadyCommitted` (or
the committed outcome), and calling it after recovery returns the same durable
identity. A crash between prepare and commit leaves a journal that recovery
can safely discard only when no host has claimed it; a host-claimed prepared
transaction is resolved according to that host's durable request state. A crash
during/after commit uses each entry's exact before and after revision and stops
with manual recovery on interference, matching the current transaction checks
(`src/transactions.rs:325-384`).

## Change-set and watcher contract

Known changes are produced by the mdbase operation that owns the write. They
are not reconstructed from `OperationResult` by a host and are not sent back
through the filesystem watcher. The result remains the portable semantic
operation envelope; generation, commit, and change metadata remain provider
metadata.

An exact change set has one immutable mdbase-owned representation. Consumers
iterate it in explicitly bounded pages; they do not receive an unbounded copied
`Vec` at every layer. Small changes may be stored inline. A large atomic batch
or reference-updating rename may use transaction-backed/rebuildable support
state identified by a digest and exact count. `CollectionWide` is a recovery or
unknown-external-change result, not an overflow escape hatch for a known
mutation: a known committed mutation must retain its exact canonical changes.

External changes use the same normalized `ChangeSet` shape plus an explicit
origin. The watcher may debounce and coalesce OS notifications, but it must
emit the final observed revisions. A create, update, delete, rename, or
collection-control edit is represented without losing the revisions required
to invalidate a cache. If an exact change set cannot be established safely,
the event is `CollectionWide` with a rebuild reason, never a guessed path list.

Known and external changes are ordered in one runtime generation stream. A
watcher event that is caused by a known commit is deduplicated by `CommitId` or
the resulting revisions; it is not emitted as a second mutation. Recovery
reconciliation may emit an external-origin event only for bytes not already
covered by the known commit outcome.

## Generation-pinned reads

An optional read cursor carries an opaque `CollectionGeneration`, a stable
ordering key, and a cursor token owned by mdbase. Every page is evaluated
against the same retained generation. If the runtime has rebuilt or evicted
that generation, the operation returns typed `generation_expired`; it does not
silently fall back to the current generation. Cursors never contain paths or
record payloads beyond the normal query cursor contract.

The first implementation may retain only the active generation and therefore
expire all cursors on a runtime restart or rebuild. A later bounded snapshot
retention policy may retain more than one generation, but it must expose the
same explicit expiry outcome.

## State ownership

| State | Owner | Classification | Recovery rule |
| --- | --- | --- | --- |
| Markdown records, config, types, contracts, view sources | mdbase | Authoritative | Filesystem bytes and exact revisions are the source of truth. |
| `.mdbase/transactions` journal, stage/backup files, and bounded pending/completed `CommitId` resolution state | mdbase | Durable support | Recover, finish, or discard according to before/after revisions; retain resolution long enough for a claimed host request to finish; never treat a partial write as success. |
| Runtime epoch and active generation sequence | mdbase runtime | Rebuildable/ephemeral | Reopen creates a new epoch; old pinned reads expire. |
| Parsed collection, query/link indexes, compiled plans, watcher snapshot | mdbase runtime | Rebuildable cache | Rebuild from canonical bytes; corruption never authorizes content edits. |
| Application grant, request identity, receipt, outcome | Connect | External authority | Connect journals these independently and may record opaque `CommitId` metadata. |

Connect must not persist `RecordChange` paths or document bytes in its
application request journal. It may append a privacy-reviewed local change
notification containing the normalized change metadata needed by its own
consumer API.

## API-review fixture mapping

The companion JSON fixture at
`docs/architecture/collection-runtime-api-fixtures.json` defines the expected
metadata for the following cases:

| Case | Result | Change set | Commit/generation rule |
| --- | --- | --- | --- |
| create | valid record result | one `Created` record with `after_revision` | commit ID present; generation advances once |
| update | valid record result | one `Updated` record with before/after revisions | commit ID present; failed precondition has no advance |
| delete | valid delete result | one `Deleted` record with `before_revision` | commit ID present; invalid/not-found has no advance |
| reference-updating rename | valid rename result | `Renamed(from,to)` plus one `Updated` entry per verified reference rewrite | one commit ID and one generation advance for the atomic set |
| view-source mutation | valid view result | one `ResourceChange(ViewSource)` | one commit ID; no record change inference |
| external edit | watcher observation | normalized external create/update/delete/rename or `CollectionWide` | external origin; generation advances once; no mdbase commit ID |
| dry run/no-op/error | semantic result | `None` | no commit ID and no generation advance |

These mappings are contract fixtures, not executable behavior in Phase 0.
