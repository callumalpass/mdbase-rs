# Collection runtime contract — Phase 0 API review

Status: accepted. This document freezes the provider boundary for the
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

The existing journal schema is not the final contract. Phase 1 must recover every
legacy v1 journal with current semantics before admitting a v2 prepare, then add the
claim/digest, event/batch descriptor, commit-time ordering metadata, cancelled state,
and acknowledged completed marker described below.

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

// An opaque, collection-local token generated and durably recorded by a host
// before it asks mdbase to prepare a mutation. It contains no host request ID.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct HostClaimId([u8; 32]);

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct ChangeEventId(Uuid);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ChangeWatermark(u64);

// Process-local, absolute monotonic deadline. It is never serialized.
pub struct OperationDeadline(Instant);

pub struct OperationContext<'a> {
    pub cancellation: &'a OperationCancellation,
    pub deadline: OperationDeadline,
}

pub struct ChangeEventIdentity {
    pub id: ChangeEventId,
    pub watermark: ChangeWatermark,
}

pub struct ExecutionOutcome {
    pub result: OperationResult,
    pub generation: CollectionGeneration,
    pub changes: ChangeSet,
    pub commit_id: Option<CommitId>,
    pub change_event: Option<ChangeEventIdentity>,
}

pub enum ChangeSet {
    None,
    Exact(ChangeBatch),
    CollectionWide { reason: RebuildReason },
}

pub enum RebuildReason {
    RecoveryReconciliation,
    ExternalChangeUncertain,
    ControlResourceChange,
    ChangeFeedRetentionGap,
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
    pub before_types: CanonicalTypeSet,
    pub after_types: CanonicalTypeSet,
    pub changed_fields: CanonicalFieldChangeSet,
    pub body_changed: bool,
}

pub enum RecordChangeKind { Created, Updated, Deleted, Renamed }

pub struct ResourceChange {
    pub kind: ResourceChangeKind,
    pub path: CollectionPath,
    pub before_revision: Option<Revision>,
    pub after_revision: Option<Revision>,
}

pub enum ResourceChangeKind {
    Configuration, TypeDefinition, Contract, ViewSource, File, Other,
}

pub struct RuntimeChangeEvent {
    pub identity: ChangeEventIdentity,
    pub generation: CollectionGeneration,
    pub changes: ChangeSet,
    pub origin: ChangeOrigin,
    pub commit_id: Option<CommitId>,
}

pub enum ChangeOrigin { KnownMutation, Filesystem, RecoveryReconciliation }

pub struct PreparedMutation { /* opaque, host cannot inspect or forge */ }

pub enum PreparationOutcome {
    NoMutation(ExecutionOutcome),
    Prepared(PreparedMutation),
}

pub struct ReadCursor { /* opaque generation pin plus ordering state */ }

pub struct ReadPage {
    pub outcome: ExecutionOutcome,
    pub next: Option<ReadCursor>,
}

// An unguessable secret capability, not a public label. Possession proves the
// current durable feed owner during open or transfer.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct ChangeFeedOwnerId([u8; 32]);

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct ChangeFeedTransferId(Uuid);

pub struct ChangeFeed { /* exclusive, fenced handle for one durable host owner */ }

pub struct ChangeFeedBaseline {
    pub fencing_epoch: u64,
    pub acknowledged_through: ChangeWatermark,
    pub feed_head: ChangeWatermark,
}

pub struct ChangeFeedTransferIntent {
    pub id: ChangeFeedTransferId,
    pub current: ChangeFeedOwnerId,
    pub next: ChangeFeedOwnerId,
    pub expected_acked_through: ChangeWatermark,
}

pub struct ChangeFeedTransferReceipt {
    pub id: ChangeFeedTransferId,
    pub fencing_epoch: u64,
    pub acknowledged_through: ChangeWatermark,
    pub feed_head: ChangeWatermark,
}

pub struct ChangeFeedTransfer {
    pub feed: ChangeFeed,
    pub receipt: ChangeFeedTransferReceipt,
}

impl PreparedMutation {
    // Available only after the prepared journal is durable.
    pub fn commit_id(&self) -> &CommitId;
}

pub enum DurableCommitState {
    Prepared,
    Committing,
    Committed { outcome: ExecutionOutcome },
    RejectedBeforeCommit { rejection: CommitRejection },
    CancelledBeforeCommit,
    NeedsManualRecovery,
}

// A durable, versioned semantic failure descriptor. It contains the exact
// provider result/problem needed to finish the original host request, but no
// record body or absolute filesystem path.
pub struct CommitRejection { /* code plus canonical operation failure */ }

pub enum CommitAttempt {
    Committed(ExecutionOutcome),
    RejectedBeforeCommit { rejection: CommitRejection },
    SettlementPending { commit_id: CommitId },
}

pub enum CancelOutcome {
    CancelledBeforeCommit,
    AlreadyCommitStarted,
    AlreadyCommitted(ExecutionOutcome),
    AlreadyRejected(CommitRejection),
    NeedsManualRecovery,
}

pub trait CollectionRuntime {
    fn read(&self, request: &OperationRequest,
            context: &OperationContext<'_>)
        -> Result<ExecutionOutcome, ProviderError>;
    fn open_read(&self, request: &OperationRequest,
                 context: &OperationContext<'_>)
        -> Result<ReadPage, ProviderError>;
    fn read_page(&self, cursor: &ReadCursor,
                 context: &OperationContext<'_>)
        -> Result<ReadPage, ProviderError>;
    fn release_read(&self, cursor: ReadCursor, context: &OperationContext<'_>)
        -> Result<(), ProviderError>;
    fn prepare(&self, request: &OperationRequest,
               claim: &HostClaimId,
               context: &OperationContext<'_>)
        -> Result<PreparationOutcome, ProviderError>;
    fn attach_prepared(&self, claim: &HostClaimId, context: &OperationContext<'_>)
        -> Result<Option<PreparedMutation>, ProviderError>;
    fn commit(&self, mutation: &PreparedMutation, context: &OperationContext<'_>)
        -> Result<CommitAttempt, ProviderError>;
    fn cancel(&self, mutation: &PreparedMutation, context: &OperationContext<'_>)
        -> Result<CancelOutcome, ProviderError>;
    fn resolve_commit(&self, commit_id: &CommitId, context: &OperationContext<'_>)
        -> Result<Option<DurableCommitState>, ProviderError>;
    fn resolve_claim(&self, claim: &HostClaimId, context: &OperationContext<'_>)
        -> Result<Option<(CommitId, DurableCommitState)>, ProviderError>;
    fn change_page(&self, batch: &ChangeBatch, after: Option<ChangePageCursor>,
                   limit: NonZeroUsize, context: &OperationContext<'_>)
        -> Result<ChangePage, ProviderError>;
    fn open_change_feed(&self, owner: &ChangeFeedOwnerId,
                        context: &OperationContext<'_>)
        -> Result<ChangeFeed, ProviderError>;
    fn transfer_change_feed(&self, intent: &ChangeFeedTransferIntent,
                            context: &OperationContext<'_>)
        -> Result<ChangeFeedTransfer, ProviderError>;
    fn ack_change_feed_transfer(&self, transfer: &ChangeFeedTransferId,
                                context: &OperationContext<'_>)
        -> Result<(), ProviderError>;
    fn establish_change_feed_baseline(&self, feed: &ChangeFeed,
                                      context: &OperationContext<'_>)
        -> Result<ChangeFeedBaseline, ProviderError>;
    fn read_change_events(&self, feed: &ChangeFeed,
                          after: Option<ChangeWatermark>,
                          limit: NonZeroUsize,
                          context: &OperationContext<'_>)
        -> Result<RuntimeChangeEventPage, ProviderError>;
    fn ack_change_events(&self, feed: &ChangeFeed,
                         through: ChangeWatermark,
                         context: &OperationContext<'_>)
        -> Result<(), ProviderError>;
    fn ack_commit_resolution(&self, commit_id: &CommitId,
                             context: &OperationContext<'_>)
        -> Result<(), ProviderError>;
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

`HostClaimId` is generated by an integrating host before preparation and is
persisted only as an opaque recovery key plus mutation digest. `ChangeEventId` and
`ChangeWatermark` identify durable delivery of normalized effects. None of these
identities is a collection generation: generation identifies a readable snapshot,
commit identifies a filesystem transaction, claim bridges a host crash window, and
change identity makes effect delivery replayable.

### Durable host claim, prepare, commit, and cancellation

Every potentially blocking provider call receives one `OperationContext`. Its
deadline is an absolute monotonic instant and its cooperative token may be set
independently; neither is durable state. The runtime gate, journal lock, snapshot
pin, feed-owner gate, page read, and acknowledgement path must use
cancellation-aware/timed acquisition and return a typed deadline/cancellation
result before entering their durable ownership boundary. The current blocking
`RwLock::read`/`write` calls do not satisfy this contract and must be replaced or
wrapped by a cancellable bounded gate during Phase 1. Connect's executor bounds
work before provider entry, but cannot be the only deadline owner because another
mdbase host may call the runtime directly.

Preparation validates inputs, resolves references, computes exact before/after
revisions, and stages the durable transaction. It checks the caller's
`OperationContext` at bounded points. A successful `prepare` has durable
evidence and exposes its opaque `CommitId`, but has not changed canonical
Markdown. The host must be able to persist its own request-to-commit mapping
before invoking `commit`; returning the identifier only in the post-commit
`ExecutionOutcome` would leave an unresolvable crash window.

Validation failure, failed precondition discovered before staging, dry run, and
semantic no-op return `PreparationOutcome::NoMutation` with the current generation,
`ChangeSet::None`, and no commit/event identity. Connect may then retire its unused
host claim. Only `Prepared` has a durable mdbase transaction.

The required hand-off is:

1. the host generates an unguessable `HostClaimId` and durably records
   application request ID -> host claim before calling mdbase;
2. mdbase durably prepares the transaction with that opaque claim and returns
   `PreparedMutation` plus its `CommitId`;
3. the host durably augments its entry with the returned `CommitId` and invokes
   `commit` with the opaque prepared handle; and
4. the host records its application-facing receipt after mdbase reports or
   recovers the durable commit state.

The claim closes both crash windows. If the process stops before mdbase prepares,
`resolve_claim` returns absent and the host may retry the same request and claim.
If it stops after mdbase prepares but before the host stores `CommitId`,
`resolve_claim` returns the prepared transaction and its identity. Reusing a claim
with a different canonical mutation digest is a hard `claim_mismatch` error. An
unclaimed convenience transaction, if the final API permits one, is never used by
a durable host and may be discarded on restart. Claimed prepared transactions are
not discarded merely because Connect has not yet copied the `CommitId`.

`resolve_claim` and `resolve_commit` must distinguish prepared, committing,
committed, rejected-before-commit, cancelled, and manual-recovery states for long
enough to finish the host's independent journal. The host calls
`ack_commit_resolution` only after its final application receipt is durable. The
mdbase resolution proves the filesystem write set or proves that its commit-time
recheck rejected the write set; it does not become an application receipt or acquire
host request semantics. `HostClaimId` is an opaque integration primitive, not a
Connect type; it never enters record data, an application response, a grant, or a
transport.

mdbase never age-evicts a claimed `Prepared`, `Committing`, unresolved committed, or
unacknowledged rejected transaction. Capacity pressure rejects later preparation
instead. A host may cancel an attached prepared transaction, acknowledge a resolved
transaction, or invoke a separately audited administrative abandonment path that
proves no canonical write began. A host claim for which `resolve_claim` is absent is
host-only state and may be garbage-collected only after the host has durably
finalized that request as `not_sent`.

The prepared journal is the recovery attachment point. Before `prepare` returns it
durably contains schema version, `HostClaimId`, `CommitId`, the versioned canonical
mutation digest, reserved `ChangeEventId`, and the exact change-batch descriptor
(count, digest, and inline or support-storage reference). At the winning
`Prepared -> Committing` transition, the runtime ordering gate rechecks filesystem
preconditions, assigns the then-next generation and change watermark, and fsyncs
them with `Committing` before the first canonical filesystem write. Preparation
therefore does not hold the runtime gate or create generation/watermark gaps while
waiting for the host. These fields are all recoverable before any canonical write.

If that commit-time recheck fails, the same journal lock atomically changes
`Prepared -> RejectedBeforeCommit`, stores a versioned canonical
`CommitRejection`, releases the reserved batch/event backing, and fsyncs the final
state without assigning a generation or watermark and without writing canonical
files. The claim and commit ID continue resolving to this final state until the host
durably stores and acknowledges the original request's semantic conflict receipt.
The host must not translate it to cancellation, `not_sent`, or an absent claim, and
must not retry it as a new mutation identity. Repeating `commit`, `cancel`,
`resolve_claim`, or `resolve_commit` returns the same rejection. This is distinct
from `NeedsManualRecovery`, which is possible only after `Committing` encountered
interference while settling canonical writes.

Generation is the exception to identity stability across restart. A generation
stored at `Committing` records ordering in that historical runtime epoch; it never
revives an old cursor. Recovery opens a new epoch and reports/replays the same commit,
event, watermark, and batch at a newly observed recovery generation. Consumers
deduplicate by event/commit identity, not generation equality. A durable application
receipt may therefore retain its historical generation while a post-restart recovery
observation carries the new epoch.
`attach_prepared(claim)` reconstructs a new opaque process-local handle for a durable
`Prepared` journal after restart; it never creates a second transaction. For later
states the host uses `resolve_claim`/`resolve_commit`. Recovery appends or finds the
same event identity from this journal before reporting a committed outcome, so a
crash after the last file write cannot lose or replace the exact known event.

`commit` is the ownership transfer. `commit` and `cancel` acquire the same durable
transaction lock with their `OperationContext`. Before any phase CAS, cancellation
or deadline expiry returns typed `commit_not_started` and leaves the durable state
`Prepared`, so the host can use a new bounded context to retry or cancel according to
its durable request state. Under the lock, `commit` rechecks preconditions and
compare-and-sets `Prepared` to `Committing` or `RejectedBeforeCommit`; `cancel`
compare-and-sets `Prepared` to `CancelledBeforeCommit`. Persisting and fsyncing one
of those phase transitions is the linearization point. The winner is idempotent. A
loser returns the durable winning state and never performs its action. Once
`Committing` wins, cancellation and the foreground deadline are ignored by the
filesystem writer: commit finishes or startup recovery finishes it. If the
foreground context expires during settlement, `commit` returns
`SettlementPending { commit_id }` and the durable worker continues without retaining
the caller task or its Connect permits. The host receives a committed outcome, a
durable rejection, or a typed pending/recovery state; it must not infer `not_sent`
after durable commit begins.

`cancel` is idempotent. Calling it after commit returns `AlreadyCommitted` (or
the committed outcome), and calling it after recovery returns the same durable
identity. A crash between prepare and commit leaves a claimed journal that is
resolved according to the host's durable request state. A crash
during/after commit uses each entry's exact before and after revision and stops
with manual recovery on interference, matching the current transaction checks
(`src/transactions.rs:325-384`).

Cancellation has two clocks and one explicit ownership boundary. The context's
execution token may stop provider gate acquisition, validation, preparation, feed
work, or a prepared transaction. Its absolute deadline bounds all those waits as
well as how long the caller waits for settlement, but it does not cancel recovery
after the journal durably enters `Committing`.

| Observed state at deadline/cancellation | Provider action | Host result |
| --- | --- | --- |
| not prepared | stop work and remove the unused host claim | `operation_cancelled`, `not_sent` |
| durable `Prepared` | race `cancel` against `commit`; resolve the journal | `not_sent` only after `CancelledBeforeCommit` is durable |
| durable `RejectedBeforeCommit` | replay the canonical conflict and retain it until receipt acknowledgement | final semantic failure; never cancellation or an absent claim |
| commit boundary unresolved | continue resolution in background | pending/`outcome_unknown`; never `not_sent` |
| durable `Committing` or `Committed` | finish or recover without the caller token | final receipt when known, otherwise pending/`outcome_unknown` |

Reads and dry runs remain cancellable throughout because they never cross the
filesystem commit boundary. Feed open/transfer/baseline/read/ack calls, change-batch
paging, cursor release, resolution, and acknowledgement use the same bounded
context. Waiting for a different feed owner is a typed ownership error rather than
an unbounded wait. A caller is never required to keep a timed-out request task alive
merely so durable settlement can finish.

## Change-set and watcher contract

Known changes are produced by the mdbase operation that owns the write. They
are not reconstructed from `OperationResult` by a host and are not sent back
through the filesystem watcher. The result remains the portable semantic
operation envelope; generation, commit, and change metadata remain provider
metadata.

Each `RecordChange` carries canonical before/after type sets and the exact changed
frontmatter field paths plus a body-change marker. Connect may intersect those
provider facts with a grant, but does not derive them from the portable result. For
create/delete, the absent side has an empty type set and the present side's fields
are the exact visible field set. If an external comparison cannot establish this
metadata, mdbase emits `CollectionWide` rather than inventing types or fields.

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
An external rename is `Renamed` only when mdbase can prove the identity across
the observation: one uninterrupted native watcher rename cookie must link old and
new paths, and the before/after snapshot must confirm the same non-reused platform
file identity. An inode/file ID without the native rename linkage is insufficient.
Otherwise the normative representation is a delete plus a create.

Known and external changes are ordered in one runtime generation stream. A
watcher event that is caused by a known commit is deduplicated by `CommitId` or
the resulting revisions; it is not emitted as a second mutation. Recovery
reconciliation may emit an external-origin event only for bytes not already
covered by the known commit outcome.

### Durable change delivery and bounded storage

Every known committed mutation and accepted external/recovery observation gets a
durable, collection-local `ChangeEventId` and monotonically increasing
`ChangeWatermark`. The completed transaction marker and its known-mutation event
become recoverable before mdbase reports success. On restart, mdbase recovers
transactions, compares the authoritative collection with its last acknowledged
watch snapshot, and appends either exact external changes or one explicit
`CollectionWide(RecoveryReconciliation)` event before declaring the feed caught
up. Event identity and watermark survive runtime epochs; they are deliberately
different from `CollectionGeneration`.

Known commit transitions and accepted watcher comparisons assign their generation
and watermark under the same runtime ordering gate. A watcher observation may queue
behind a prepared mutation's brief commit transition, but `prepare` itself never
holds the gate while waiting for a host decision.

Delivery is a pull/ack feed, not an unbounded channel. Any in-memory notification
is a capacity-one, coalescing wake-up only. A collection has exactly one durable
host feed owner in this contract. `open_change_feed` fences an older process handle
and accepts only the same persisted `ChangeFeedOwnerId`; transferring ownership is
an explicit lifecycle operation, not an opportunistic second consumer. The host
pages `read_change_events`, durably processes provider identities and batches, and
then calls `ack_change_events` with its fenced handle. A replay after a crash is
deduplicated by `ChangeEventId`.

Opening with the same owner increments a durable provider fencing epoch embedded in
the returned handle, so all older process handles fail with `change_feed_fenced`.
`ChangeFeedOwnerId` is an unguessable secret capability rather than a discoverable
identifier. Before transfer, the host durably records one unguessable
`ChangeFeedTransferId` plus the current/next owner capabilities and expected
acknowledged head. `transfer_change_feed` requires that intent, possession of the
current-owner secret, an exact acknowledged head, and no unresolved reconciliation
epoch. It atomically installs the next owner/fencing epoch and persists the exact
intent plus `ChangeFeedTransferReceipt` before returning.

Transfer is idempotent across both host crash windows. If the current owner is still
installed, retry performs the transfer once. If the next owner is already installed
and the retained transfer ID, both owner digests, and expected head match exactly,
retry returns the same receipt and a newly fenced handle without transferring again.
Any partial/mismatched tuple is `change_feed_transfer_mismatch`. The provider retains
that one completed receipt until `ack_change_feed_transfer`; the host acknowledges
only after its own owner/epoch update is durable. Acknowledgement durably replaces
the receipt with one bounded `last_acknowledged_transfer_id` tombstone before
returning. Repeating acknowledgement of that exact ID succeeds idempotently, so a
host crash after provider acknowledgement but before deletion of its intent resumes
forward. A new transfer is rejected while an unacknowledged receipt exists; after
acknowledgement it may proceed and eventually replace the prior tombstone, keeping
retention bounded. If the old owner secret is lost, a separate user-confirmed
administrative rebaseline creates a continuity barrier before installing a new
owner; normal runtime code cannot seize it.

The first `establish_change_feed_baseline` for an owner is valid only before that
owner's first event. Under the runtime ordering gate it starts native observation,
captures the authoritative watcher snapshot/feed head, and durably installs both as the
acknowledged baseline before returning. Its returned
`acknowledged_through == feed_head`; the first readable post-baseline watermark is
the checked successor of that head. Watermark exhaustion is a typed terminal runtime
condition and never wraps. Events racing initial observation startup are either
included in that snapshot or force a reconciliation epoch; there is no unobserved
gap. Once the baseline receipt is durable, calling baseline again with the same owner
returns that same watermark/snapshot receipt with the current handle's fencing epoch
even if later events already exist. It never treats those later events as a reason to
reject or recalculate the baseline.

Acknowledgement is monotonic and contiguous. Repeating the current acknowledgement
or an older one is an idempotent no-op. A watermark beyond feed head or across an
unprocessed gap is `invalid_change_ack`. Reading after head returns an empty page;
reading before retained history returns `change_feed_reset_required` plus the live
reconciliation barrier rather than silently skipping events.

Watcher startup, backend failure, scan failure, or wake-up overflow durably sets a
versioned reconciliation record with one stable epoch/event ID. Its state machine is
`Required -> Reconciling -> EventDurable -> Acknowledged`. Repeated failures retain
the same epoch and return `Reconciling` to `Required` with bounded backoff; they do
not append events. A successful comparison atomically installs the pending watcher
snapshot, appends the stable exact-or-collection-wide event, and records
`EventDurable`. Restart from `Reconciling` retries the same epoch; restart from
`EventDurable` replays the same event. Only acknowledgement promotes the pending
snapshot to the acknowledged watcher snapshot and removes the marker. External
activity arriving after an event is durable sets a following dirty epoch rather than
changing the existing event. Read and mutation service may remain available while
runtime health reports notification delivery as degraded; the obligation itself is
never reduced to a log line or silently dropped.

`ChangeBatch` is a handle to one immutable representation with an exact count and
digest. `change_page` clamps requests to a configured finite maximum. Small batches
may be inline; large batches use transaction/change-feed support storage. The
provider retains a claimed mutation's exact metadata until both its commit
resolution and associated change watermark are acknowledged. It retains external
metadata until the watermark is acknowledged. A referenced batch is never silently
evicted. A process-local page handle may expire on runtime close, but reopening the
still-unacknowledged event reconstructs it from durable backing. Only after the
required acknowledgements may backing be retired, after which a stale handle returns
typed `change_batch_expired` and forces explicit reconciliation.

Retention has configured limits for active batches, event count, metadata bytes,
and acknowledged-state age. Unacknowledged durable batches have no age eviction and
therefore apply backpressure. Known mutations reserve enough metadata capacity before durable prepare;
if capacity is unavailable they fail with `runtime_capacity_exhausted` before a
canonical write. The runtime reserves a separate fixed emergency slot and bounded
descriptor bytes for one reconciliation marker; ordinary batches can never consume
it. External edits cannot be rejected, so ordinary-capacity exhaustion coalesces
them behind that durable `CollectionWide` reconciliation marker. If the emergency
slot itself cannot be durably written, runtime health is terminal/unavailable and it
does not claim feed continuity. This is permitted for an
unknown external interval, but never as a way to discard the exact result of a
known committed mutation.

Digest and ordering rules are versioned. Claims are already namespaced by the
collection-local transaction support directory; no movable root path is hashed.
Version 1 mutation claims use SHA-256 over RFC 8785/JCS of spec profile, operation
name, normalized input, and explicit preconditions. Version 1 change batches order
resource kind, canonical UTF-8 path bytes, optional source-path bytes, and change
kind, then hash the JCS representation of every normalized item in that order.
Canonical type sets are deduplicated and UTF-8 byte sorted; changed fields are
deduplicated canonical JSON Pointers in the same ordering. Every normalized record
item also contains the explicit JSON boolean `body_changed`; it is never encoded as a
field-name sentinel. Pages preserve this order. A page cursor lower than the next
position is a deterministic replay; one beyond it is `invalid_change_page`. The
rolling item count and digest must equal the journal descriptor before an event may
be acknowledged. `CollectionWide` has zero
items. Its version 1 descriptor is exactly the JCS object
`{"schema_version":1,"kind":"collection_wide","reason":<reason>}`. The closed
version 1 reason strings are `recovery_reconciliation`,
`external_change_uncertain`, `control_resource_change`, and
`change_feed_retention_gap`; unknown future reasons require a new descriptor schema
version rather than changing a version 1 digest.

## Generation-pinned reads

An optional read cursor carries an opaque `CollectionGeneration`, a stable
ordering key, and a cursor token owned by mdbase. Creating it atomically pins the
immutable query/index snapshot used by the first page. Every later page is
evaluated against that snapshot even while mutations advance the active generation.
Cursors never contain paths or record payloads beyond the normal query cursor
contract.

Pins have configured finite limits for count, retained bytes, idle lease, and hard
lifetime. Creation fails with `cursor_capacity_exhausted` before returning a first
page when capacity is unavailable; an active pin is not evicted to admit another
cursor. The idle lease begins when the first page is returned. Replaying the same
cursor position before expiry returns the same page and next token; each successful
page renews only the idle lease while the hard deadline remains fixed. Explicit release, lease
expiry, hard-lifetime expiry, runtime close, or a non-retainable rebuild releases
the snapshot, after which the exact typed result is `generation_expired`. There is
no silent fallback to current state. Mutations do not expire a healthy pin merely
because they advance the generation.

## State ownership

| State | Owner | Classification | Recovery rule |
| --- | --- | --- | --- |
| Markdown records, config, types, contracts, view sources | mdbase | Authoritative | Filesystem bytes and exact revisions are the source of truth. |
| `.mdbase/transactions` journal, stage/backup files, opaque host claim, and bounded pending/completed `CommitId` resolution state | mdbase | Durable support | Recover, finish, or cancel according to before/after revisions and durable claim state; retain resolution until host acknowledgement; never treat a partial write as success. |
| Durable change-event identity, watermark, reconciliation marker, exact batch metadata, and acknowledged watcher snapshot | mdbase | Durable support | Replay until monotonic acknowledgement; rebuild/coalesce external uncertainty explicitly; never silently drop an obligation. |
| Runtime epoch and active generation sequence | mdbase runtime | Rebuildable/ephemeral | Reopen creates a new epoch; old pinned reads expire. |
| Parsed collection, query/link indexes, compiled plans, watcher snapshot | mdbase runtime | Rebuildable cache | Rebuild from canonical bytes; corruption never authorizes content edits. |
| Application grant, request identity, receipt, outcome | Connect | External authority | Connect journals these independently and may record opaque `CommitId` metadata. |

Connect must not persist `RecordChange` paths or document bytes in its application
request journal. It may persist provider event identity in private delivery support
state and append privacy-reviewed, scope-filtered metadata to its existing local
change journal. Commit IDs, host claims, absolute paths, and record snapshots never
enter application-visible events or notification signals.

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
| cursor survives mutation | two or more query pages | same pinned snapshot | later mutation advances active generation without mixing pages |
| cursor expiry | page after lease/restart | typed `generation_expired` | no current-generation fallback |
| cancellation before commit | cancelled prepared mutation | `None` | durable `CancelledBeforeCommit`; `not_sent` is safe |
| cancellation at/after commit | caller stops waiting | recoverable exact outcome | pending/`outcome_unknown`, never `not_sent` |
| crash after prepare | resolve by host claim | prepared commit identity | same claim/digest resumes; mismatch is rejected |
| crash after canonical write | journal recovery | original exact known event | same commit/event/watermark/batch; new runtime-epoch generation; never collection-wide fallback |
| cancel/commit race | one durable phase CAS | one winner | loser resolves durable winner; no partial commit |
| partial batch consumption | replay bounded pages | hidden staging until complete | item indexes/digests deduplicate; acknowledge after exact finalization |
| external watcher failure | reconciliation event | `CollectionWide` | durable marker remains until event acknowledgement |
| repeated reconciliation failure | one durable epoch | no event until scan succeeds | restart/retry preserves identity; one acknowledged event |
| unknown external rename | watcher observation | delete plus create | never guesses rename identity |

These mappings are contract fixtures, not executable behavior in Phase 0.
