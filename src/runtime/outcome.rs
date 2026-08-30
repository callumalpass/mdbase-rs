use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{CanonicalOperationOutcome, ProviderError};
use crate::api::{CollectionPath, Revision};

/// Privacy-safe accounting for rebuildable state retained by one runtime.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeMeasurements {
    /// Parsed collection type definitions retained by the provider.
    pub loaded_type_definitions: usize,
    /// Active generation-pinned query snapshots.
    pub active_read_snapshots: usize,
    /// Serialized result bytes retained by active query snapshots.
    pub retained_read_snapshot_bytes: usize,
}

/// Opaque process-epoch and sequence identifying one readable runtime state.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct CollectionGeneration {
    runtime_epoch: String,
    sequence: u64,
}

impl CollectionGeneration {
    pub(crate) fn initial() -> Self {
        Self {
            runtime_epoch: uuid::Uuid::new_v4().to_string(),
            sequence: 0,
        }
    }

    /// Return the opaque process epoch.
    pub fn runtime_epoch(&self) -> &str {
        &self.runtime_epoch
    }

    /// Return the sequence within this process epoch.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub(crate) fn successor(&self) -> Result<Self, ProviderError> {
        Ok(Self {
            runtime_epoch: self.runtime_epoch.clone(),
            sequence: self
                .sequence
                .checked_add(1)
                .ok_or(ProviderError::GenerationExhausted)?,
        })
    }
}

macro_rules! opaque_uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[allow(dead_code)]
            pub(crate) fn generate() -> Self {
                Self(uuid::Uuid::new_v4().to_string())
            }

            #[allow(dead_code)]
            pub(crate) fn from_stored(value: String) -> Self {
                Self(value)
            }

            /// Return the opaque stable representation.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

opaque_uuid_id!(
    /// Opaque identity of one durable mdbase filesystem transaction.
    CommitId
);
opaque_uuid_id!(
    /// Durable identity of one normalized runtime change event.
    ChangeEventId
);

/// Unguessable capability identifying the single durable change-feed owner.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChangeFeedOwnerId(String);

impl ChangeFeedOwnerId {
    /// Generate a new owner capability.
    pub fn generate() -> Self {
        Self(format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ))
    }

    pub(crate) fn from_stored(value: String) -> Self {
        Self(value)
    }

    /// Return the stable secret representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

opaque_uuid_id!(
    /// Opaque identity of one crash-safe feed ownership transfer.
    ChangeFeedTransferId
);

/// Opaque host capability durably bound to one prepared mutation.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostClaimId(String);

impl HostClaimId {
    /// Generate an unguessable claim without embedding a host request identity.
    pub fn generate() -> Self {
        Self(format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ))
    }

    /// Return the opaque capability representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Monotonic durable position in the collection-local change feed.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChangeWatermark(u64);

impl ChangeWatermark {
    /// Return the numeric collection-local position.
    pub fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn successor(self) -> Result<Self, ProviderError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(ProviderError::WatermarkExhausted)
    }

    pub(crate) fn from_stored(value: u64) -> Self {
        Self(value)
    }
}

/// Durable identity and position of one runtime change event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeEventIdentity {
    /// Stable event identity used for replay deduplication.
    pub id: ChangeEventId,
    /// Monotonic feed position.
    pub watermark: ChangeWatermark,
}

/// Canonical deduplicated type membership before or after a record change.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CanonicalTypeSet(BTreeSet<String>);

impl CanonicalTypeSet {
    /// Normalize a set of type names into UTF-8 byte order.
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        Self(values.into_iter().collect())
    }

    /// Iterate over normalized type names.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

/// Canonical deduplicated JSON Pointers changed in record frontmatter.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CanonicalFieldChangeSet(BTreeSet<String>);

impl CanonicalFieldChangeSet {
    /// Validate and normalize changed frontmatter JSON Pointers.
    pub fn new(values: impl IntoIterator<Item = String>) -> Result<Self, ProviderError> {
        let values = values.into_iter().collect::<BTreeSet<_>>();
        if let Some(value) = values
            .iter()
            .find(|value| !value.starts_with('/') || value.contains('~') && !valid_pointer(value))
        {
            return Err(ProviderError::InvalidChangeSet(format!(
                "'{value}' is not a canonical JSON Pointer"
            )));
        }
        Ok(Self(values))
    }

    /// Iterate over normalized JSON Pointers.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

fn valid_pointer(value: &str) -> bool {
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character == '~' && !matches!(characters.next(), Some('0' | '1')) {
            return false;
        }
    }
    true
}

/// Canonical record change kind.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordChangeKind {
    /// A record became present.
    Created,
    /// An existing record changed in place.
    Updated,
    /// A record became absent.
    Deleted,
    /// A proven rename retained record identity at a new path.
    Renamed,
}

/// Exact normalized effect on one canonical record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecordChange {
    /// Kind of record transition.
    pub kind: RecordChangeKind,
    /// Final path, or deleted path for a deletion.
    pub path: CollectionPath,
    /// Source path for a proven rename.
    pub from: Option<CollectionPath>,
    /// Revision before the transition.
    pub before_revision: Option<Revision>,
    /// Revision after the transition.
    pub after_revision: Option<Revision>,
    /// Canonical type membership before the transition.
    pub before_types: CanonicalTypeSet,
    /// Canonical type membership after the transition.
    pub after_types: CanonicalTypeSet,
    /// Exact changed frontmatter fields.
    pub changed_fields: CanonicalFieldChangeSet,
    /// Whether the Markdown body changed.
    pub body_changed: bool,
}

/// Canonical collection resource kind.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceChangeKind {
    /// `mdbase.yaml` or equivalent collection configuration.
    Configuration,
    /// Portable type definition.
    TypeDefinition,
    /// Portable data contract.
    Contract,
    /// Saved view source.
    ViewSource,
    /// Other explicitly managed file resource.
    File,
    /// Future or provider-specific resource.
    Other,
}

/// Exact normalized effect on one collection resource.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResourceChange {
    /// Resource class.
    pub kind: ResourceChangeKind,
    /// Canonical collection-relative path.
    pub path: CollectionPath,
    /// Revision before the transition.
    pub before_revision: Option<Revision>,
    /// Revision after the transition.
    pub after_revision: Option<Revision>,
}

/// One normalized canonical change.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "target", content = "change", rename_all = "snake_case")]
pub enum CanonicalChange {
    /// Record effect.
    Record(RecordChange),
    /// Collection resource effect.
    Resource(ResourceChange),
}

impl CanonicalChange {
    fn ordering_key(&self) -> (u8, &[u8], Option<&[u8]>, u8) {
        match self {
            Self::Record(change) => (
                0,
                change.path.as_str().as_bytes(),
                change.from.as_ref().map(|path| path.as_str().as_bytes()),
                change.kind as u8,
            ),
            Self::Resource(change) => (
                1 + change.kind as u8,
                change.path.as_str().as_bytes(),
                None,
                0,
            ),
        }
    }
}

/// Stable descriptor of one immutable exact change batch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeBatchDescriptor {
    /// Digest schema version.
    pub schema_version: u32,
    /// Exact item count.
    pub count: usize,
    /// SHA-256 digest over canonical JCS items in canonical order.
    pub digest: String,
}

/// Opaque replayable position within one exact change batch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangePageCursor {
    digest: String,
    next_index: usize,
}

impl ChangePageCursor {
    /// Return the next canonical item index.
    pub fn next_index(&self) -> usize {
        self.next_index
    }
}

/// One bounded page from an immutable exact change batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangePage {
    /// Canonically ordered items in this page.
    pub items: Vec<CanonicalChange>,
    /// Cursor for the next page, or `None` at the exact end.
    pub next: Option<ChangePageCursor>,
}

/// Immutable, digest-bound representation of exact canonical changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeBatch {
    descriptor: ChangeBatchDescriptor,
    items: Arc<[CanonicalChange]>,
}

impl ChangeBatch {
    /// Normalize, validate, order, and digest exact canonical changes.
    pub fn new(mut items: Vec<CanonicalChange>) -> Result<Self, ProviderError> {
        items.sort_by(|left, right| left.ordering_key().cmp(&right.ordering_key()));
        if items
            .windows(2)
            .any(|pair| pair[0].ordering_key() == pair[1].ordering_key())
        {
            return Err(ProviderError::InvalidChangeSet(
                "a canonical change batch contains duplicate target identities".to_string(),
            ));
        }
        let mut hasher = Sha256::new();
        for item in &items {
            let bytes = serde_jcs::to_vec(item)
                .map_err(|error| ProviderError::InvalidChangeSet(error.to_string()))?;
            hasher.update(bytes);
        }
        let digest = hasher.finalize();
        let mut encoded = String::with_capacity(7 + digest.len() * 2);
        encoded.push_str("sha256:");
        for byte in digest {
            write!(&mut encoded, "{byte:02x}").expect("writing to a string cannot fail");
        }
        Ok(Self {
            descriptor: ChangeBatchDescriptor {
                schema_version: 1,
                count: items.len(),
                digest: encoded,
            },
            items: items.into(),
        })
    }

    /// Return the immutable count and digest descriptor.
    pub fn descriptor(&self) -> &ChangeBatchDescriptor {
        &self.descriptor
    }

    pub(crate) fn items(&self) -> &[CanonicalChange] {
        &self.items
    }

    /// Read a bounded deterministic page, clamping caller limits to host policy.
    pub fn page(
        &self,
        after: Option<&ChangePageCursor>,
        requested: NonZeroUsize,
        maximum: NonZeroUsize,
    ) -> Result<ChangePage, ProviderError> {
        let start = match after {
            Some(cursor)
                if cursor.digest == self.descriptor.digest
                    && cursor.next_index <= self.items.len() =>
            {
                cursor.next_index
            }
            Some(_) => return Err(ProviderError::InvalidChangePage),
            None => 0,
        };
        let limit = requested.get().min(maximum.get());
        let end = start.saturating_add(limit).min(self.items.len());
        let items = self.items[start..end].to_vec();
        let next = (end < self.items.len()).then(|| ChangePageCursor {
            digest: self.descriptor.digest.clone(),
            next_index: end,
        });
        Ok(ChangePage { items, next })
    }
}

/// Reason an observation requires collection-wide derived-state rebuilding.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebuildReason {
    /// Startup recovered canonical writes or watcher state.
    RecoveryReconciliation,
    /// An external change could not be normalized exactly.
    ExternalChangeUncertain,
    /// Collection control resources changed.
    ControlResourceChange,
    /// A consumer requested history before retained change metadata.
    ChangeFeedRetentionGap,
}

/// Exact or explicitly broad canonical effects of one observation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ChangeSet {
    /// No canonical resource changed.
    #[default]
    None,
    /// Exact immutable canonical changes.
    Exact(ChangeBatch),
    /// Derived state must rebuild for the stated reason.
    CollectionWide { reason: RebuildReason },
}

/// Provider result plus local generation, transaction, and change metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionOutcome {
    /// Closed typed semantic operation outcome.
    pub operation: CanonicalOperationOutcome,
    /// Ephemeral v0.3 compatibility projection for coordinated host migration.
    ///
    /// This field is never journaled and is removed in 0.5.0 after the 0.4.x
    /// Connect compatibility window.
    #[deprecated(
        since = "0.4.0",
        note = "use operation (or operation.to_v03() only at a wire edge); removed in 0.5.0 after the 0.4.x Connect compatibility window"
    )]
    pub result: crate::v03::OperationResult,
    /// Runtime generation observed or produced.
    pub generation: CollectionGeneration,
    /// Canonical effects produced by the operation.
    pub changes: ChangeSet,
    /// Durable filesystem transaction identity for a committed mutation.
    pub commit_id: Option<CommitId>,
    /// Durable normalized event identity for a canonical change.
    pub change_event: Option<ChangeEventIdentity>,
}

impl ExecutionOutcome {
    pub(crate) fn new(
        operation: CanonicalOperationOutcome,
        generation: CollectionGeneration,
        changes: ChangeSet,
        commit_id: Option<CommitId>,
        change_event: Option<ChangeEventIdentity>,
    ) -> Self {
        let result = operation.to_v03();
        #[allow(deprecated)]
        Self {
            operation,
            result,
            generation,
            changes,
            commit_id,
            change_event,
        }
    }
}

/// Privacy-safe inventory of journals that still require the version-2
/// compatibility decoder. No transaction IDs, paths, claims, or payloads are exposed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyJournalInventory {
    /// Number of valid version-2 runtime journals under the held collection authority.
    pub version_2: usize,
}

impl LegacyJournalInventory {
    /// True when the collection has crossed the fixture-zero removal gate.
    pub fn is_zero(self) -> bool {
        self.version_2 == 0
    }
}

/// Source of one durable runtime change event.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOrigin {
    /// Change produced by an mdbase mutation.
    KnownMutation,
    /// Change observed from the filesystem.
    Filesystem,
    /// Change produced while restoring continuity after restart/failure.
    RecoveryReconciliation,
}

/// Durable generation-aware event returned by the collection runtime feed.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeChangeEvent {
    /// Durable identity and watermark.
    pub identity: ChangeEventIdentity,
    /// Runtime generation assigned to this observation.
    pub generation: CollectionGeneration,
    /// Exact or explicitly collection-wide effects.
    pub changes: ChangeSet,
    /// Observation source.
    pub origin: ChangeOrigin,
    /// Durable transaction identity for a known mutation.
    pub commit_id: Option<CommitId>,
}

/// Exclusive fenced handle for one durable change-feed owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeFeed {
    pub(crate) owner: ChangeFeedOwnerId,
    pub(crate) fencing_epoch: u64,
}

/// Durable feed position observed after opening or transferring ownership.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeFeedBaseline {
    pub fencing_epoch: u64,
    pub acknowledged_through: ChangeWatermark,
    pub feed_head: ChangeWatermark,
}

/// Host-durable request to transfer the single feed owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeFeedTransferIntent {
    pub id: ChangeFeedTransferId,
    pub current: ChangeFeedOwnerId,
    pub next: ChangeFeedOwnerId,
    pub expected_acked_through: ChangeWatermark,
}

impl ChangeFeedTransferIntent {
    /// Create a new transfer intent with an unguessable identity.
    pub fn new(
        current: ChangeFeedOwnerId,
        next: ChangeFeedOwnerId,
        expected_acked_through: ChangeWatermark,
    ) -> Self {
        Self {
            id: ChangeFeedTransferId::generate(),
            current,
            next,
            expected_acked_through,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeFeedTransferReceipt {
    pub id: ChangeFeedTransferId,
    pub fencing_epoch: u64,
    pub acknowledged_through: ChangeWatermark,
    pub feed_head: ChangeWatermark,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeFeedTransfer {
    pub feed: ChangeFeed,
    pub receipt: ChangeFeedTransferReceipt,
}

/// One bounded page from the durable runtime change feed.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeChangeEventPage {
    pub events: Vec<RuntimeChangeEvent>,
    pub next: Option<ChangeWatermark>,
    pub feed_head: ChangeWatermark,
}

/// Opaque token for one replayable position in a generation-pinned read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadCursor {
    token: String,
}

impl ReadCursor {
    pub fn from_token(token: impl Into<String>) -> Result<Self, ProviderError> {
        let token = token.into();
        if token.is_empty() || token.len() > 256 || !token.is_ascii() {
            return Err(ProviderError::InvalidReadCursor);
        }
        Ok(Self { token })
    }

    pub fn as_token(&self) -> &str {
        &self.token
    }

    pub(crate) fn issued(token: String) -> Self {
        Self { token }
    }
}

/// One bounded page from a generation-pinned read.
#[derive(Clone, Debug, PartialEq)]
pub struct ReadPage {
    pub outcome: ExecutionOutcome,
    pub next: Option<ReadCursor>,
}

/// Typed result of explicitly releasing generation-pinned cursor state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CursorReleaseOutcome {
    /// `true` when retained state existed and was released; `false` for an
    /// authenticated cursor that had already been released.
    pub released: bool,
}

/// Opaque process-local handle for one durable prepared mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedMutation {
    pub(crate) commit_id: CommitId,
    pub(crate) claim: HostClaimId,
}

impl PreparedMutation {
    /// Return the durable transaction identity exposed after preparation.
    pub fn commit_id(&self) -> &CommitId {
        &self.commit_id
    }
}

/// Result of mutation preparation before the durable commit boundary.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum PreparationOutcome {
    /// Validation failure, dry-run, or semantic no-op; no transaction exists.
    NoMutation(ExecutionOutcome),
    /// Exact filesystem effects are durably staged and claimed.
    Prepared(PreparedMutation),
}

/// Durable semantic rejection produced by the commit-time CAS recheck.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommitRejection {
    /// Typed canonical operation failure.
    pub operation: CanonicalOperationOutcome,
    /// Ephemeral v0.3 compatibility projection; never persisted. Removed in
    /// 0.5.0 after the 0.4.x Connect compatibility window.
    #[deprecated(
        since = "0.4.0",
        note = "use operation (or operation.to_v03() only at a wire edge); removed in 0.5.0 after the 0.4.x Connect compatibility window"
    )]
    pub result: crate::v03::OperationResult,
}

impl CommitRejection {
    pub(crate) fn new(operation: CanonicalOperationOutcome) -> Self {
        let result = operation.to_v03();
        #[allow(deprecated)]
        Self { operation, result }
    }
}

/// Recoverable state of one claimed mutation.
#[derive(Clone, Debug, PartialEq)]
pub enum DurableCommitState {
    Prepared,
    Committing,
    Committed { outcome: ExecutionOutcome },
    RejectedBeforeCommit { rejection: CommitRejection },
    CancelledBeforeCommit,
    NeedsManualRecovery,
}

/// Result of attempting the durable commit ownership transfer.
#[derive(Clone, Debug, PartialEq)]
pub enum CommitAttempt {
    Committed(ExecutionOutcome),
    RejectedBeforeCommit { rejection: CommitRejection },
    SettlementPending { commit_id: CommitId },
    NeedsManualRecovery { commit_id: CommitId },
}

/// Idempotent result of cancelling a prepared mutation.
#[derive(Clone, Debug, PartialEq)]
pub enum CancelOutcome {
    CancelledBeforeCommit,
    AlreadyCommitStarted,
    AlreadyCommitted(ExecutionOutcome),
    AlreadyRejected(CommitRejection),
    NeedsManualRecovery,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(path: &str, body_changed: bool) -> CanonicalChange {
        CanonicalChange::Record(RecordChange {
            kind: RecordChangeKind::Updated,
            path: CollectionPath::new(path).unwrap(),
            from: None,
            before_revision: Revision::parse("sha256:before").ok(),
            after_revision: Revision::parse("sha256:after").ok(),
            before_types: CanonicalTypeSet::new(["task".to_string()]),
            after_types: CanonicalTypeSet::new(["task".to_string()]),
            changed_fields: CanonicalFieldChangeSet::new(["/status".to_string()]).unwrap(),
            body_changed,
        })
    }

    #[test]
    fn exact_batches_are_order_and_body_marker_bound() {
        let ordered = ChangeBatch::new(vec![update("a.md", false), update("b.md", false)]).unwrap();
        let reversed =
            ChangeBatch::new(vec![update("b.md", false), update("a.md", false)]).unwrap();
        assert_eq!(ordered.descriptor(), reversed.descriptor());

        let body_changed =
            ChangeBatch::new(vec![update("a.md", true), update("b.md", false)]).unwrap();
        assert_ne!(
            ordered.descriptor().digest,
            body_changed.descriptor().digest
        );
    }

    #[test]
    fn paging_is_bounded_replayable_and_digest_fenced() {
        let batch = ChangeBatch::new(vec![
            update("c.md", false),
            update("a.md", false),
            update("b.md", false),
        ])
        .unwrap();
        let first = batch
            .page(
                None,
                NonZeroUsize::new(10).unwrap(),
                NonZeroUsize::new(2).unwrap(),
            )
            .unwrap();
        assert_eq!(first.items.len(), 2);
        let cursor = first.next.unwrap();
        let replay = batch
            .page(
                Some(&cursor),
                NonZeroUsize::new(1).unwrap(),
                NonZeroUsize::new(2).unwrap(),
            )
            .unwrap();
        assert_eq!(replay.items, vec![update("c.md", false)]);
        assert!(replay.next.is_none());

        let forged = ChangePageCursor {
            digest: "sha256:other".to_string(),
            next_index: 0,
        };
        assert!(matches!(
            batch.page(
                Some(&forged),
                NonZeroUsize::new(1).unwrap(),
                NonZeroUsize::new(1).unwrap()
            ),
            Err(ProviderError::InvalidChangePage)
        ));
    }

    #[test]
    fn generation_and_watermark_never_wrap() {
        let generation = CollectionGeneration {
            runtime_epoch: "epoch".to_string(),
            sequence: u64::MAX,
        };
        assert!(matches!(
            generation.successor(),
            Err(ProviderError::GenerationExhausted)
        ));
        assert!(matches!(
            ChangeWatermark(u64::MAX).successor(),
            Err(ProviderError::WatermarkExhausted)
        ));
    }
}
