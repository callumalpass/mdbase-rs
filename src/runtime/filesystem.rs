use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::diff::canonical_changes;
use super::observer::NoopObserver;
use super::{
    CancelOutcome, CanonicalOperationOutcome, CanonicalOperationValue, ChangeBatch,
    ChangeEventIdentity, ChangeFeed, ChangeFeedBaseline, ChangeFeedOwnerId, ChangeFeedTransfer,
    ChangeFeedTransferId, ChangeFeedTransferIntent, ChangePage, ChangePageCursor, ChangeSet,
    ChangeWatermark, CollectionGeneration, CommitAttempt, CommitId, CommitRejection,
    DurableCommitState, ExecutionOutcome, FilesystemProvider, HostClaimId, ObserverOptions,
    OperationContext, OperationRequest, PreparationOutcome, PreparedMutation, ProviderError,
    ReadCursor, ReadPage, RuntimeChangeEvent, RuntimeChangeEventPage, RuntimeObserver,
};
use crate::transactions::{
    self, RuntimeCommitAttempt, RuntimePrepareOutcome, RuntimeResolution, TransactionError,
};
use crate::v03::OperationResult;
use crate::watch::{CollectionWatcher, WatchEvent};

/// Filesystem authority with consistent operation and notification ordering.
pub struct FilesystemRuntime {
    provider: Arc<FilesystemProvider>,
    watcher: Arc<Mutex<CollectionWatcher>>,
    pending_watch: Arc<Mutex<VecDeque<WatchEvent>>>,
    order: Arc<Mutex<RuntimeOrder>>,
    settlement: Arc<SettlementCoordinator>,
    cursors: Mutex<super::cursor::CursorStore>,
    #[cfg(test)]
    watcher_test_control: crate::watch::WatcherTestControl,
    #[cfg(test)]
    maintenance_notification_injections: AtomicUsize,
    #[cfg(test)]
    maintenance_transaction_commits: AtomicUsize,
    #[cfg(test)]
    validation_drop_callback_injections: AtomicUsize,
    #[cfg(test)]
    acknowledgement_completions: Arc<AtomicUsize>,
    #[cfg(test)]
    drop_callback_preceded_ack: Arc<AtomicBool>,
    #[cfg(test)]
    drop_callback_preceded_connection_close: Arc<AtomicBool>,
}

struct RuntimeOrder {
    generation: CollectionGeneration,
    watermark: ChangeWatermark,
}

struct SettlementCoordinator {
    active: Mutex<Option<CommitId>>,
    changed: Condvar,
}

impl FilesystemRuntime {
    pub fn open(root: impl AsRef<Path>, debounce: Duration) -> Result<Self, ProviderError> {
        Self::open_observed(
            root,
            debounce,
            Arc::new(NoopObserver),
            ObserverOptions::default(),
        )
    }

    /// Count valid version-2 runtime journals without exposing journal contents.
    /// A zero result is the operator-facing gate for removing the decoder in 0.5.0.
    pub fn legacy_journal_inventory(
        &self,
        context: &OperationContext,
    ) -> Result<super::LegacyJournalInventory, ProviderError> {
        self.provider.legacy_journal_inventory(context)
    }

    pub fn open_observed(
        root: impl AsRef<Path>,
        debounce: Duration,
        observer: Arc<dyn RuntimeObserver>,
        observer_options: ObserverOptions,
    ) -> Result<Self, ProviderError> {
        let provider = Arc::new(FilesystemProvider::open_runtime_observed(
            root.as_ref(),
            observer,
            observer_options,
        )?);
        let initial_generation = CollectionGeneration::initial();
        let reconciled = provider.with_collection_boundary_context(
            &OperationContext::internal(),
            |collection| {
                super::feed::reconcile(
                    collection,
                    initial_generation.clone(),
                    &OperationContext::internal(),
                )
            },
        )?;
        provider.initialize_runtime_cache(&reconciled.generation)?;
        let watcher = CollectionWatcher::open(root, debounce)?;
        #[cfg(test)]
        let watcher_test_control = watcher.test_control();
        let runtime_epoch = reconciled.generation.runtime_epoch().to_string();
        Ok(Self {
            provider,
            watcher: Arc::new(Mutex::new(watcher)),
            pending_watch: Arc::new(Mutex::new(VecDeque::new())),
            order: Arc::new(Mutex::new(RuntimeOrder {
                generation: reconciled.generation,
                watermark: reconciled.watermark,
            })),
            settlement: Arc::new(SettlementCoordinator {
                active: Mutex::new(None),
                changed: Condvar::new(),
            }),
            cursors: Mutex::new(super::cursor::CursorStore::new(runtime_epoch)),
            #[cfg(test)]
            watcher_test_control,
            #[cfg(test)]
            maintenance_notification_injections: AtomicUsize::new(0),
            #[cfg(test)]
            maintenance_transaction_commits: AtomicUsize::new(0),
            #[cfg(test)]
            validation_drop_callback_injections: AtomicUsize::new(0),
            #[cfg(test)]
            acknowledgement_completions: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            drop_callback_preceded_ack: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            drop_callback_preceded_connection_close: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn provider(&self) -> Arc<FilesystemProvider> {
        self.provider.clone()
    }

    /// Return privacy-safe accounting for rebuildable state retained in memory.
    pub fn measurements(&self) -> Result<super::RuntimeMeasurements, ProviderError> {
        let (active_read_snapshots, retained_read_snapshot_bytes) = self
            .cursors
            .lock()
            .map_err(|_| ProviderError::LockPoisoned)?
            .measurements();
        Ok(super::RuntimeMeasurements {
            loaded_type_definitions: self.provider.loaded_type_definitions()?,
            active_read_snapshots,
            retained_read_snapshot_bytes,
        })
    }

    pub fn execute(&self, request: &OperationRequest) -> Result<OperationResult, ProviderError> {
        self.execute_with_context(request, &OperationContext::internal())
    }

    /// Compatibility wrapper which serializes the typed runtime result exactly
    /// once at the v0.3 wire edge.
    pub fn execute_with_context(
        &self,
        request: &OperationRequest,
        context: &OperationContext,
    ) -> Result<OperationResult, ProviderError> {
        self.execute_typed_with_context(request, context)
            .map(|outcome| outcome.operation.to_v03())
    }

    pub fn execute_typed(
        &self,
        request: &OperationRequest,
    ) -> Result<ExecutionOutcome, ProviderError> {
        self.execute_typed_with_context(request, &OperationContext::internal())
    }

    /// Execute the compatibility-decoded request and return the closed typed
    /// runtime outcome.
    pub fn execute_typed_with_context(
        &self,
        request: &OperationRequest,
        context: &OperationContext,
    ) -> Result<ExecutionOutcome, ProviderError> {
        if !request.operation.is_mutation() {
            return self.read(request, context);
        }
        let claim = HostClaimId::generate();
        match self.prepare_typed(request, &claim, context)? {
            PreparationOutcome::NoMutation(outcome) => Ok(outcome),
            PreparationOutcome::Prepared(prepared) => match self.commit(&prepared, context)? {
                CommitAttempt::Committed(outcome) => Ok(outcome),
                CommitAttempt::RejectedBeforeCommit { rejection } => Ok(ExecutionOutcome::new(
                    rejection.operation,
                    self.current_generation()?,
                    ChangeSet::None,
                    None,
                    None,
                )),
                CommitAttempt::SettlementPending { commit_id } => Err(ProviderError::Transaction {
                    code: "outcome_unknown",
                    message: format!(
                        "mutation settlement is pending for commit {}",
                        commit_id.as_str()
                    ),
                }),
                CommitAttempt::NeedsManualRecovery { commit_id } => {
                    Err(ProviderError::Transaction {
                        code: "manual_recovery_required",
                        message: format!(
                            "mutation requires manual recovery for commit {}",
                            commit_id.as_str()
                        ),
                    })
                }
            },
        }
    }

    /// Execute a read and bind its result to the runtime generation observed.
    pub fn read(
        &self,
        request: &OperationRequest,
        context: &OperationContext,
    ) -> Result<ExecutionOutcome, ProviderError> {
        self.read_with_result_charge(request, context, true)
    }

    fn read_with_result_charge(
        &self,
        request: &OperationRequest,
        context: &OperationContext,
        charge_query_result: bool,
    ) -> Result<ExecutionOutcome, ProviderError> {
        if request.operation.is_mutation() {
            return Err(ProviderError::UnsupportedOperation(
                "read requires a non-mutation operation".to_string(),
            ));
        }
        self.wait_for_settlement(context)?;
        let expected = self.current_generation()?;
        self.provider.ensure_runtime_cache(&expected, context)?;
        let operation = match request.operation {
            super::OperationKind::Read | super::OperationKind::Query => self
                .provider
                .with_collection_read_context(context, |collection| {
                    execute_typed_read_operation(collection, request, context)
                })?,
            _ => {
                let result = self
                    .provider
                    .execute_with_post_context(request, context, |_| Ok(()))?;
                CanonicalOperationOutcome::wire_only(request.operation, result)
            }
        };
        let generation = self.current_generation()?;
        if charge_query_result {
            charge_returned_query(&operation, context)?;
        }
        Ok(ExecutionOutcome::new(
            operation,
            generation,
            ChangeSet::None,
            None,
            None,
        ))
    }

    /// Open a bounded generation-pinned read page.
    pub fn open_read(
        &self,
        request: &OperationRequest,
        context: &OperationContext,
    ) -> Result<ReadPage, ProviderError> {
        if request.operation.is_mutation() {
            return Err(ProviderError::UnsupportedOperation(
                "open_read requires a non-mutation operation".to_string(),
            ));
        }
        let mut expanded = request.clone();
        let page_items = expanded
            .input
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok());
        if let Some(input) = expanded.input.as_object_mut() {
            input.remove("limit");
        }
        let outcome = self.read_with_result_charge(&expanded, context, false)?;
        self.cursor_lock(context)?
            .open(outcome, page_items, context)
    }

    /// Read or deterministically replay one page from a pinned generation.
    pub fn read_page(
        &self,
        cursor: &ReadCursor,
        context: &OperationContext,
    ) -> Result<ReadPage, ProviderError> {
        self.cursor_lock(context)?.page(cursor, context)
    }

    /// Explicitly release a pinned read and its bounded retained state.
    pub fn release_read(
        &self,
        cursor: ReadCursor,
        context: &OperationContext,
    ) -> Result<super::CursorReleaseOutcome, ProviderError> {
        context.check()?;
        let released = self.cursor_lock(context)?.release(cursor)?;
        context.check()?;
        Ok(super::CursorReleaseOutcome { released })
    }

    /// Validate and durably stage one exact mutation under an opaque host claim.
    pub fn prepare(
        &self,
        request: &OperationRequest,
        claim: &HostClaimId,
        context: &OperationContext,
    ) -> Result<PreparationOutcome, ProviderError> {
        self.prepare_typed(request, claim, context)
    }

    pub fn prepare_typed(
        &self,
        request: &OperationRequest,
        claim: &HostClaimId,
        context: &OperationContext,
    ) -> Result<PreparationOutcome, ProviderError> {
        if !request.operation.is_mutation() {
            return Err(ProviderError::UnsupportedOperation(
                "prepare requires a mutation operation".to_string(),
            ));
        }
        let generation = self.current_generation()?;
        self.provider.ensure_runtime_cache(&generation, context)?;
        let digest = mutation_digest(request)?;
        self.provider
            .with_collection_boundary_context(context, |collection| {
                super::feed::ensure_capacity(collection)?;
                if request.operation == super::OperationKind::Batch {
                    return super::batch::prepare(
                        self, collection, request, claim, &digest, context,
                    );
                }
                let preparation = crate::v03::batch::prepare_single_runtime(
                    collection,
                    request.operation.as_str(),
                    &request.input,
                    context,
                )?;
                match preparation {
                    crate::v03::batch::RuntimeSinglePreparation::NoMutation(operation) => {
                        Ok(PreparationOutcome::NoMutation(ExecutionOutcome::new(
                            operation,
                            self.current_generation()?,
                            ChangeSet::None,
                            None,
                            None,
                        )))
                    }
                    crate::v03::batch::RuntimeSinglePreparation::Prepared(plan) => {
                        let changes = canonical_changes(&plan.before, &plan.after, Some(request))?;
                        let event_id = super::ChangeEventId::generate();
                        match transactions::prepare_runtime_transaction(
                            collection,
                            transactions::RuntimePrepareInput {
                                baseline: &plan.baseline,
                                desired: &plan.desired,
                                claim,
                                mutation_digest: &digest,
                                operation: &plan.operation,
                                changes: &changes,
                                event_id: &event_id,
                            },
                            context,
                        )
                        .map_err(transaction_error)?
                        {
                            RuntimePrepareOutcome::NoMutation(operation) => {
                                Ok(PreparationOutcome::NoMutation(ExecutionOutcome::new(
                                    operation,
                                    self.current_generation()?,
                                    ChangeSet::None,
                                    None,
                                    None,
                                )))
                            }
                            RuntimePrepareOutcome::Prepared(commit_id) => {
                                Ok(PreparationOutcome::Prepared(PreparedMutation {
                                    commit_id,
                                    claim: claim.clone(),
                                }))
                            }
                            RuntimePrepareOutcome::Existing(_) => Err(ProviderError::Transaction {
                                code: "claim_already_finalized",
                                message: "host claim already resolves to a final durable state"
                                    .to_string(),
                            }),
                        }
                    }
                }
            })
    }

    /// Reattach a claimed prepared mutation after a host restart.
    pub fn attach_prepared(
        &self,
        claim: &HostClaimId,
        context: &OperationContext,
    ) -> Result<Option<PreparedMutation>, ProviderError> {
        self.provider
            .with_collection_boundary_context(context, |collection| {
                transactions::attach_runtime_prepared(collection, claim, context)
                    .map(|commit_id| {
                        commit_id.map(|commit_id| PreparedMutation {
                            commit_id,
                            claim: claim.clone(),
                        })
                    })
                    .map_err(transaction_error)
            })
    }

    /// Atomically transfer ownership from prepared to durable settlement.
    pub fn commit(
        &self,
        mutation: &PreparedMutation,
        context: &OperationContext,
    ) -> Result<CommitAttempt, ProviderError> {
        self.wait_for_settlement(context)?;
        let attempt = self
            .provider
            .with_collection_boundary_context(context, |collection| {
                let mut order = self.order.lock().map_err(|_| ProviderError::LockPoisoned)?;
                let reconciled =
                    super::feed::reconcile(collection, order.generation.clone(), context)?;
                order.generation = reconciled.generation;
                order.watermark = reconciled.watermark;
                let generation = order.generation.successor()?;
                let watermark = order.watermark.successor()?;
                transactions::commit_runtime_prepared(
                    collection,
                    &mutation.commit_id,
                    &generation,
                    watermark,
                    context,
                )
                .map_err(transaction_error)
            })?;
        match attempt {
            RuntimeCommitAttempt::Committed(resolution) => self.finalize_resolution(&resolution),
            RuntimeCommitAttempt::RejectedBeforeCommit(resolution) => {
                Ok(CommitAttempt::RejectedBeforeCommit {
                    rejection: rejected(&resolution)?,
                })
            }
            RuntimeCommitAttempt::AlreadyCancelled => Err(ProviderError::Transaction {
                code: "commit_cancelled_before_start",
                message: "prepared mutation was durably cancelled before commit".to_string(),
            }),
            RuntimeCommitAttempt::SettlementRequired(commit_id)
            | RuntimeCommitAttempt::SettlementPending(commit_id) => {
                self.start_settlement(commit_id, context)
            }
            RuntimeCommitAttempt::NeedsManualRecovery(commit_id) => {
                Ok(CommitAttempt::NeedsManualRecovery { commit_id })
            }
        }
    }

    /// Idempotently cancel a mutation only if durable commit has not started.
    pub fn cancel(
        &self,
        mutation: &PreparedMutation,
        context: &OperationContext,
    ) -> Result<CancelOutcome, ProviderError> {
        self.provider
            .with_collection_boundary_context(context, |collection| {
                let resolution =
                    transactions::cancel_runtime_prepared(collection, &mutation.commit_id, context)
                        .map_err(transaction_error)?;
                cancel_outcome(&resolution)
            })
    }

    pub fn resolve_commit(
        &self,
        commit_id: &CommitId,
        context: &OperationContext,
    ) -> Result<Option<DurableCommitState>, ProviderError> {
        self.provider
            .with_collection_context(context, |collection| {
                transactions::resolve_runtime_commit(collection, commit_id, context)
                    .map_err(transaction_error)?
                    .as_ref()
                    .map(durable_state)
                    .transpose()
            })
    }

    pub fn resolve_claim(
        &self,
        claim: &HostClaimId,
        context: &OperationContext,
    ) -> Result<Option<(CommitId, DurableCommitState)>, ProviderError> {
        self.provider
            .with_collection_context(context, |collection| {
                transactions::resolve_runtime_claim(collection, claim, context)
                    .map_err(transaction_error)?
                    .map(|(commit_id, resolution)| {
                        durable_state(&resolution).map(|state| (commit_id, state))
                    })
                    .transpose()
            })
    }

    pub fn ack_commit_resolution(
        &self,
        commit_id: &CommitId,
        context: &OperationContext,
    ) -> Result<(), ProviderError> {
        self.provider
            .with_collection_boundary_context(context, |collection| {
                transactions::ack_runtime_resolution(collection, commit_id, context)
                    .map_err(transaction_error)
            })
    }

    pub fn change_page(
        &self,
        batch: &ChangeBatch,
        after: Option<&ChangePageCursor>,
        limit: NonZeroUsize,
        context: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        context.check()?;
        let page = batch.page(after, limit, NonZeroUsize::new(256).unwrap())?;
        context.check()?;
        Ok(page)
    }

    pub fn open_change_feed(
        &self,
        owner: &ChangeFeedOwnerId,
        context: &OperationContext,
    ) -> Result<ChangeFeed, ProviderError> {
        self.provider
            .with_collection_boundary_context(context, |collection| {
                context.check()?;
                super::feed::open(collection, owner)
            })
    }

    pub fn establish_change_feed_baseline(
        &self,
        feed: &ChangeFeed,
        context: &OperationContext,
    ) -> Result<ChangeFeedBaseline, ProviderError> {
        self.provider
            .with_collection_boundary_context(context, |collection| {
                context.check()?;
                let plan = super::feed::baseline_plan(collection, feed)?;
                if !plan.needs_commit {
                    return Ok(plan.baseline);
                }
                let settlement = OperationContext::internal();
                for commit_id in plan.commits {
                    transactions::ack_runtime_change_event(collection, &commit_id, &settlement)
                        .map_err(transaction_error)?;
                }
                super::feed::commit_baseline(collection, feed)
            })
    }

    pub fn read_change_events(
        &self,
        feed: &ChangeFeed,
        after: Option<ChangeWatermark>,
        limit: NonZeroUsize,
        context: &OperationContext,
    ) -> Result<RuntimeChangeEventPage, ProviderError> {
        self.provider
            .with_collection_read_context(context, |collection| {
                super::feed::read(collection, feed, after, limit, context)
            })
    }

    pub fn ack_change_events(
        &self,
        feed: &ChangeFeed,
        through: ChangeWatermark,
        context: &OperationContext,
    ) -> Result<(), ProviderError> {
        self.provider
            .with_collection_boundary_context(context, |collection| {
                context.check()?;
                let commits = super::feed::commits_through(collection, feed, through)?;
                // Once acknowledgement starts it owns settlement. Marking each
                // transaction first is crash-safe because the durable feed still
                // retains the event until its own final atomic acknowledgement.
                let settlement = OperationContext::internal();
                for commit_id in commits {
                    transactions::ack_runtime_change_event(collection, &commit_id, &settlement)
                        .map_err(transaction_error)?;
                }
                super::feed::ack(collection, feed, through).map(|_| ())
            })
    }

    pub fn transfer_change_feed(
        &self,
        intent: &ChangeFeedTransferIntent,
        context: &OperationContext,
    ) -> Result<ChangeFeedTransfer, ProviderError> {
        self.provider
            .with_collection_boundary_context(context, |collection| {
                context.check()?;
                super::feed::transfer(collection, intent)
            })
    }

    pub fn ack_change_feed_transfer(
        &self,
        transfer: &ChangeFeedTransferId,
        context: &OperationContext,
    ) -> Result<(), ProviderError> {
        self.provider
            .with_collection_boundary_context(context, |collection| {
                context.check()?;
                super::feed::ack_transfer(collection, transfer)
            })
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<WatchEvent>, ProviderError> {
        if let Some(event) = self
            .pending_watch
            .lock()
            .map_err(|_| ProviderError::LockPoisoned)?
            .pop_front()
        {
            return Ok(Some(event));
        }
        self.watcher
            .lock()
            .map_err(|_| ProviderError::LockPoisoned)?
            .recv_timeout(timeout)
            .map_err(Into::into)
    }

    /// Normalize and durably append one external filesystem observation.
    pub fn ingest_external_timeout(
        &self,
        timeout: Duration,
        context: &OperationContext,
    ) -> Result<Option<RuntimeChangeEvent>, ProviderError> {
        self.wait_for_settlement(context)?;
        context.check()?;
        let wait = timeout.min(context.deadline().remaining());
        let Some(observed) = self.recv_timeout(wait)? else {
            context.check()?;
            return Ok(None);
        };
        context.check()?;
        self.provider
            .with_collection_boundary_context(context, |collection| {
                let mut order = self.order.lock().map_err(|_| ProviderError::LockPoisoned)?;
                let reconciled =
                    super::feed::reconcile(collection, order.generation.clone(), context)?;
                order.generation = reconciled.generation;
                order.watermark = reconciled.watermark;
                let generation = order.generation.successor()?;
                let watermark = order.watermark.successor()?;
                let changes =
                    super::external::normalize(&observed).unwrap_or(ChangeSet::CollectionWide {
                        reason: super::RebuildReason::ExternalChangeUncertain,
                    });
                let event = RuntimeChangeEvent {
                    identity: ChangeEventIdentity {
                        id: super::ChangeEventId::generate(),
                        watermark,
                    },
                    generation: generation.clone(),
                    changes,
                    origin: super::ChangeOrigin::Filesystem,
                    commit_id: None,
                };
                super::feed::append_external(collection, &event)?;
                order.generation = generation;
                order.watermark = watermark;
                self.provider
                    .apply_runtime_cache_changes(&event.changes, &event.generation)?;
                Ok(Some(event))
            })
    }

    /// Complete a full watcher comparison before accepting benchmark or host
    /// traffic. Normal mutations use the incremental synchronization path.
    pub fn synchronize(&self) -> Result<(), ProviderError> {
        self.synchronize_with_context(&OperationContext::internal())
    }

    /// Complete a full comparison within an explicit operation boundary.
    pub fn synchronize_with_context(
        &self,
        context: &OperationContext,
    ) -> Result<(), ProviderError> {
        self.synchronize_reconciliation(None, context)
    }

    #[cfg(test)]
    pub(crate) fn set_watcher_revision_for_test(&self, value: u64) {
        self.watcher_test_control.set_invalidation_revision(value);
    }

    #[cfg(test)]
    pub(crate) fn invoke_installed_watcher_modify_callback_for_test(&self, path: &str) {
        self.watcher_test_control
            .invoke_installed_modify_callback(&self.provider.root().join(path));
    }

    #[cfg(test)]
    pub(crate) fn poison_watcher_for_test(&self) {
        self.watcher_test_control.poison();
    }

    #[cfg(test)]
    pub(crate) fn install_cache_commit_linearization_hook_for_test(
        &self,
    ) -> crate::watch::LinearizationRace {
        self.watcher_test_control
            .install_cache_commit_linearization_hook()
    }

    #[cfg(test)]
    pub(crate) fn pending_rescan_count_for_test(&self) -> usize {
        self.watcher_test_control.pending_rescan_count()
    }

    #[cfg(test)]
    pub(crate) fn synchronize_paths_for_test(&self, paths: &[&str]) -> Result<(), ProviderError> {
        self.synchronize_reconciliation(Some(paths), &OperationContext::internal())
    }

    #[cfg(test)]
    pub(crate) fn inject_cache_notifications_for_test(&self, count: usize) {
        self.maintenance_notification_injections
            .store(count, Ordering::Release);
        self.maintenance_transaction_commits
            .store(0, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn inject_cache_notification_on_validation_open_for_test(&self) {
        let root = self.provider.root().to_path_buf();
        let collection = crate::Collection::open(&root).unwrap();
        let cache_folder = collection.settings.cache_folder.clone();
        let callback = self.watcher_test_control.clone();
        let callback_root = root.clone();
        crate::cache::sqlite::set_read_only_open_hook(&root, &cache_folder, move || {
            callback.invoke_installed_modify_callback(&callback_root);
        });
    }

    #[cfg(test)]
    pub(crate) fn clear_validation_open_notification_for_test(&self) -> bool {
        let root = self.provider.root();
        let collection = crate::Collection::open(root).unwrap();
        crate::cache::sqlite::clear_read_only_open_hook(root, &collection.settings.cache_folder)
    }

    #[cfg(test)]
    pub(crate) fn inject_installed_callback_on_validated_seal_drop_for_test(&self) {
        self.validation_drop_callback_injections
            .store(1, Ordering::Release);
        self.drop_callback_preceded_ack
            .store(false, Ordering::Release);
        self.drop_callback_preceded_connection_close
            .store(false, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn drop_callback_preceded_ack_for_test(&self) -> bool {
        self.drop_callback_preceded_ack.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn drop_callback_preceded_connection_close_for_test(&self) -> bool {
        self.drop_callback_preceded_connection_close
            .load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn watcher_revision_for_test(&self) -> u64 {
        self.watcher_test_control.invalidation_revision()
    }

    #[cfg(test)]
    pub(crate) fn maintenance_attempt_counts_for_test(&self) -> (usize, usize) {
        (
            self.maintenance_transaction_commits.load(Ordering::Acquire),
            self.maintenance_notification_injections
                .load(Ordering::Acquire),
        )
    }

    #[cfg(test)]
    fn inject_cache_notification_after_write_for_test(&self) {
        if self
            .maintenance_notification_injections
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                (remaining > 0).then(|| remaining - 1)
            })
            .is_ok()
        {
            // macOS can report a recursive hidden-cache write at the watched
            // root. Exercise that installed-callback path deterministically.
            self.watcher_test_control
                .invoke_installed_modify_callback(self.provider.root());
        }
    }

    fn synchronize_reconciliation(
        &self,
        paths: Option<&[&str]>,
        context: &OperationContext,
    ) -> Result<(), ProviderError> {
        const MAX_STALE_RETRIES: usize = 3;
        let mut maintenance_seal = None;
        for _ in 0..MAX_STALE_RETRIES {
            context.check()?;
            // Capture ownership before observation. The provider compares this
            // exact epoch+sequence again only after acquiring its write gate.
            let expected_generation = self.current_generation()?;
            let observation = {
                let watcher = self.watcher_lock(context)?;
                match paths {
                    Some(paths) => watcher
                        .rescan_paths_observation_with_context(paths.iter().copied(), context)?,
                    None => watcher.rescan_observation_with_context(context)?,
                }
            };
            context.check()?;
            let observation_token = observation.token();
            // A seal is single-use and belongs only to the immediately next
            // observation. Taking it before validation prevents a mismatched
            // state from becoming valid again after any intervening retry.
            #[allow(unused_mut)]
            let mut validated_seal = match maintenance_seal.take() {
                Some(seal) => self.provider.validate_runtime_invalid_maintenance_seal(
                    seal,
                    observation.invalid_records.as_ref(),
                    observation.removed_invalid_records.as_ref(),
                    &expected_generation,
                    &observation_token,
                    context,
                )?,
                None => None,
            };
            #[cfg(test)]
            if let Some(seal) = validated_seal.as_mut() {
                if self
                    .validation_drop_callback_injections
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                        (remaining > 0).then(|| remaining - 1)
                    })
                    .is_ok()
                {
                    let callback = self.watcher_test_control.clone();
                    let callback_root = self.provider.root().to_path_buf();
                    let completions = self.acknowledgement_completions.clone();
                    let completed_before = completions.load(Ordering::Acquire);
                    let preceded_ack = self.drop_callback_preceded_ack.clone();
                    let preceded_close = self.drop_callback_preceded_connection_close.clone();
                    let connection_closed = Arc::new(AtomicBool::new(false));
                    let hook_connection_closed = connection_closed.clone();
                    seal.set_drop_hook(connection_closed, move || {
                        if completions.load(Ordering::Acquire) <= completed_before {
                            preceded_ack.store(true, Ordering::Release);
                        }
                        if !hook_connection_closed.load(Ordering::Acquire) {
                            preceded_close.store(true, Ordering::Release);
                        }
                        callback.invoke_installed_modify_callback(&callback_root);
                    });
                }
            }
            let maintenance = if validated_seal.is_some() {
                crate::cache::runtime::InvalidMaintenanceOutcome::Current
            } else {
                self.provider.apply_runtime_invalid_maintenance(
                    observation.invalid_records.as_ref(),
                    observation.removed_invalid_records.as_ref(),
                    observation.epoch.as_ref(),
                    &expected_generation,
                    observation_token,
                    context,
                )?
            };
            match maintenance {
                crate::cache::runtime::InvalidMaintenanceOutcome::Stale => continue,
                crate::cache::runtime::InvalidMaintenanceOutcome::Current => {}
                crate::cache::runtime::InvalidMaintenanceOutcome::Applied(seal) => {
                    maintenance_seal = Some(seal);
                    #[cfg(test)]
                    {
                        self.maintenance_transaction_commits
                            .fetch_add(1, Ordering::AcqRel);
                        self.inject_cache_notification_after_write_for_test();
                    }
                }
            }
            let acknowledgement = self
                .watcher_lock(context)?
                .acknowledge_observation_with_context(observation, context);
            #[cfg(test)]
            self.acknowledgement_completions
                .fetch_add(1, Ordering::AcqRel);
            // The validated seal owns the retained SQLite connection. Closing it
            // only after the acknowledgement attempt prevents a close callback
            // from invalidating the operation it just validated.
            drop(validated_seal);
            let acknowledged = acknowledgement?;
            if acknowledged {
                return context.check();
            }
        }
        Err(ProviderError::Transaction {
            code: "stale_reconciliation",
            message: "watcher reconciliation repeatedly lost runtime generation ownership"
                .to_string(),
        })
    }

    pub(crate) fn current_generation(&self) -> Result<CollectionGeneration, ProviderError> {
        self.order
            .lock()
            .map(|order| order.generation.clone())
            .map_err(|_| ProviderError::LockPoisoned)
    }

    fn wait_for_settlement(&self, context: &OperationContext) -> Result<(), ProviderError> {
        let mut active = self.settlement_lock(context)?;
        while active.is_some() {
            let wait = context.next_wait()?;
            let (next, _) = self
                .settlement
                .changed
                .wait_timeout(active, wait)
                .map_err(|_| ProviderError::LockPoisoned)?;
            active = next;
        }
        Ok(())
    }

    fn settlement_lock(
        &self,
        context: &OperationContext,
    ) -> Result<std::sync::MutexGuard<'_, Option<CommitId>>, ProviderError> {
        loop {
            context.check()?;
            match self.settlement.active.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::sleep(context.next_wait()?);
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(ProviderError::LockPoisoned)
                }
            }
        }
    }

    fn start_settlement(
        &self,
        commit_id: CommitId,
        context: &OperationContext,
    ) -> Result<CommitAttempt, ProviderError> {
        {
            // Committing is already durable. Internal settlement ownership can
            // no longer be cancelled by the application deadline.
            let mut active = self.settlement_lock(&OperationContext::internal())?;
            if active.is_some() {
                return Ok(CommitAttempt::SettlementPending { commit_id });
            }
            *active = Some(commit_id.clone());
        }
        let provider = self.provider.clone();
        let watcher = self.watcher.clone();
        let pending_watch = self.pending_watch.clone();
        let order = self.order.clone();
        let settlement = self.settlement.clone();
        let worker_id = commit_id.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("mdbase-settlement".to_string())
            .spawn(move || {
                let settlement_context = OperationContext::internal();
                let result =
                    provider.with_collection_boundary_context(&settlement_context, |collection| {
                        let resolution =
                            transactions::settle_runtime_commit(collection, &worker_id)
                                .map_err(transaction_error)?;
                        finish_resolution_inside(
                            collection,
                            provider.as_ref(),
                            watcher.as_ref(),
                            pending_watch.as_ref(),
                            order.as_ref(),
                            &resolution,
                        )
                    });
                if let Ok(mut active) = settlement.active.lock() {
                    if active.as_ref() == Some(&worker_id) {
                        *active = None;
                    }
                    settlement.changed.notify_all();
                }
                let _ = sender.send(result);
            });
        if let Err(error) = spawned {
            if let Ok(mut active) = self.settlement.active.lock() {
                *active = None;
                self.settlement.changed.notify_all();
            }
            self.provider.report_error(
                "commit",
                "settlement_spawn",
                &ProviderError::Transaction {
                    code: "settlement_spawn_failed",
                    message: error.to_string(),
                },
            );
            return Ok(CommitAttempt::SettlementPending { commit_id });
        }

        loop {
            match receiver.try_recv() {
                Ok(result) => return result,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Ok(CommitAttempt::SettlementPending { commit_id })
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            if context.check().is_err() {
                return Ok(CommitAttempt::SettlementPending { commit_id });
            }
            let Ok(wait) = context.next_wait() else {
                return Ok(CommitAttempt::SettlementPending { commit_id });
            };
            std::thread::sleep(wait);
        }
    }

    fn finalize_resolution(
        &self,
        resolution: &RuntimeResolution,
    ) -> Result<CommitAttempt, ProviderError> {
        self.provider.with_collection_boundary_context(
            &OperationContext::internal(),
            |collection| {
                finish_resolution_inside(
                    collection,
                    self.provider.as_ref(),
                    self.watcher.as_ref(),
                    self.pending_watch.as_ref(),
                    self.order.as_ref(),
                    resolution,
                )
            },
        )
    }

    fn cursor_lock(
        &self,
        context: &OperationContext,
    ) -> Result<std::sync::MutexGuard<'_, super::cursor::CursorStore>, ProviderError> {
        loop {
            context.check()?;
            match self.cursors.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::sleep(context.next_wait()?);
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(ProviderError::LockPoisoned)
                }
            }
        }
    }

    fn watcher_lock(
        &self,
        context: &OperationContext,
    ) -> Result<std::sync::MutexGuard<'_, CollectionWatcher>, ProviderError> {
        loop {
            context.check()?;
            match self.watcher.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::sleep(context.next_wait()?);
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(ProviderError::LockPoisoned)
                }
            }
        }
    }
}

fn finish_resolution_inside(
    collection: &crate::Collection,
    provider: &FilesystemProvider,
    watcher: &Mutex<CollectionWatcher>,
    pending_watch: &Mutex<VecDeque<WatchEvent>>,
    order: &Mutex<RuntimeOrder>,
    resolution: &RuntimeResolution,
) -> Result<CommitAttempt, ProviderError> {
    match resolution {
        RuntimeResolution::Committed { commit_id, .. } => {
            let outcome = committed_outcome(resolution)?;
            let identity = outcome
                .change_event
                .as_ref()
                .expect("a committed runtime outcome has an event identity");
            {
                let mut current = order.lock().map_err(|_| ProviderError::LockPoisoned)?;
                if identity.watermark > current.watermark {
                    if identity.watermark != current.watermark.successor()? {
                        return Err(ProviderError::Transaction {
                            code: "runtime_order_conflict",
                            message: "committed watermark is not the next runtime position"
                                .to_string(),
                        });
                    }
                    current.generation = outcome.generation.clone();
                    current.watermark = identity.watermark;
                }
            }
            if let Err(error) =
                provider.apply_runtime_cache_changes(&outcome.changes, &outcome.generation)
            {
                provider.report_error("commit", "cache", &error);
            }
            if let Err(error) = synchronize_known_shared(watcher, pending_watch, &outcome) {
                provider.report_error("commit", "watch_synchronize", &error);
            }
            if let Err(error) = super::feed::append_known(collection, &outcome) {
                provider.report_error("commit", "change_feed", &error);
                return Ok(CommitAttempt::SettlementPending {
                    commit_id: commit_id.clone(),
                });
            }
            Ok(CommitAttempt::Committed(outcome))
        }
        RuntimeResolution::RejectedBeforeCommit { .. } => Ok(CommitAttempt::RejectedBeforeCommit {
            rejection: rejected(resolution)?,
        }),
        RuntimeResolution::NeedsManualRecovery { commit_id } => {
            Ok(CommitAttempt::NeedsManualRecovery {
                commit_id: commit_id.clone(),
            })
        }
        RuntimeResolution::Prepared { commit_id } | RuntimeResolution::Committing { commit_id } => {
            Ok(CommitAttempt::SettlementPending {
                commit_id: commit_id.clone(),
            })
        }
        RuntimeResolution::CancelledBeforeCommit { .. } => Err(ProviderError::Transaction {
            code: "commit_cancelled_before_start",
            message: "prepared mutation was durably cancelled before commit".to_string(),
        }),
    }
}

fn synchronize_known_shared(
    watcher: &Mutex<CollectionWatcher>,
    pending_watch: &Mutex<VecDeque<WatchEvent>>,
    outcome: &ExecutionOutcome,
) -> Result<(), ProviderError> {
    let ChangeSet::Exact(changes) = &outcome.changes else {
        return Ok(());
    };
    let mut paths = Vec::new();
    let mut full = false;
    for change in changes.items() {
        match change {
            super::CanonicalChange::Record(change) => {
                paths.push(change.path.as_str().to_string());
                if let Some(from) = &change.from {
                    paths.push(from.as_str().to_string());
                }
            }
            super::CanonicalChange::Resource(_) => full = true,
        }
    }
    let watcher = watcher.lock().map_err(|_| ProviderError::LockPoisoned)?;
    if full {
        watcher.rescan()?;
    } else {
        watcher.rescan_paths(paths)?;
    }
    let mut pending = pending_watch
        .lock()
        .map_err(|_| ProviderError::LockPoisoned)?;
    while let Some(event) = watcher.recv_timeout(Duration::ZERO)? {
        if !super::external::matches_known(&event, changes) {
            pending.push_back(event);
        }
    }
    Ok(())
}

fn execute_typed_read_operation(
    collection: &crate::Collection,
    request: &OperationRequest,
    context: &OperationContext,
) -> Result<CanonicalOperationOutcome, ProviderError> {
    let typed = match collection.typed() {
        Ok(typed) => typed,
        Err(error) => return Ok(CanonicalOperationOutcome::failure(request.operation, error)),
    };
    let operation = match request.operation {
        super::OperationKind::Read => {
            match serde_json::from_value::<crate::api::ReadRequest>(request.input.clone()) {
                Ok(request) => typed
                    .read(request)
                    .map(CanonicalOperationOutcome::read)
                    .unwrap_or_else(|error| {
                        CanonicalOperationOutcome::failure(super::OperationKind::Read, error)
                    }),
                Err(error) => CanonicalOperationOutcome::failure(
                    super::OperationKind::Read,
                    crate::api::MdbaseError::InvalidRequest {
                        message: error.to_string(),
                    },
                ),
            }
        }
        super::OperationKind::Query => {
            match crate::api::QueryRequest::decode_wire(request.input.clone()) {
                Ok(request) => typed
                    .query_runtime(request, context.cancellation())
                    .map(CanonicalOperationOutcome::query)
                    .unwrap_or_else(|error| {
                        CanonicalOperationOutcome::failure(super::OperationKind::Query, error)
                    }),
                Err(error) => CanonicalOperationOutcome::failure(
                    super::OperationKind::Query,
                    crate::api::MdbaseError::InvalidRequest {
                        message: error.to_string(),
                    },
                ),
            }
        }
        _ => unreachable!("only migrated read operations use the typed read executor"),
    };
    if let Some(error) = context.capture_limit_error() {
        return Err(error);
    }
    context.check()?;
    Ok(operation)
}

fn charge_returned_query(
    operation: &CanonicalOperationOutcome,
    context: &OperationContext,
) -> Result<(), ProviderError> {
    let CanonicalOperationValue::Query(Some(query)) = &operation.value else {
        return Ok(());
    };
    if query.records.is_empty() {
        return Ok(());
    }
    let bytes = super::cursor::measured_json_bytes(
        &query.records,
        context.capture_limits().max_retained_bytes,
    )?;
    context.charge_retained(bytes)
}

fn mutation_digest(request: &OperationRequest) -> Result<String, ProviderError> {
    let bytes = serde_jcs::to_vec(request)
        .map_err(|error| ProviderError::InvalidChangeSet(error.to_string()))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

pub(super) fn transaction_error(error: TransactionError) -> ProviderError {
    match error {
        TransactionError::ClaimMismatch => ProviderError::ClaimMismatch,
        TransactionError::RuntimeCapacityExhausted => ProviderError::RuntimeCapacityExhausted,
        TransactionError::OperationBoundary {
            code: "operation_cancelled",
        } => ProviderError::OperationCancelled,
        TransactionError::OperationBoundary {
            code: "operation_deadline",
        } => ProviderError::OperationDeadline,
        other => ProviderError::Transaction {
            code: other.code(),
            message: other.to_string(),
        },
    }
}

fn committed_outcome(resolution: &RuntimeResolution) -> Result<ExecutionOutcome, ProviderError> {
    let RuntimeResolution::Committed {
        commit_id,
        result,
        generation,
        watermark,
        event_id,
        changes,
    } = resolution
    else {
        return Err(ProviderError::Transaction {
            code: "transaction_state_invalid",
            message: "expected a committed runtime transaction".to_string(),
        });
    };
    Ok(ExecutionOutcome::new(
        result.clone(),
        generation.clone(),
        ChangeSet::Exact(changes.clone()),
        Some(commit_id.clone()),
        Some(ChangeEventIdentity {
            id: event_id.clone(),
            watermark: *watermark,
        }),
    ))
}

fn rejected(resolution: &RuntimeResolution) -> Result<CommitRejection, ProviderError> {
    let RuntimeResolution::RejectedBeforeCommit { rejection, .. } = resolution else {
        return Err(ProviderError::Transaction {
            code: "transaction_state_invalid",
            message: "expected a rejected runtime transaction".to_string(),
        });
    };
    Ok(CommitRejection::new(rejection.clone()))
}

fn durable_state(resolution: &RuntimeResolution) -> Result<DurableCommitState, ProviderError> {
    Ok(match resolution {
        RuntimeResolution::Prepared { .. } => DurableCommitState::Prepared,
        RuntimeResolution::Committing { .. } => DurableCommitState::Committing,
        RuntimeResolution::Committed { .. } => DurableCommitState::Committed {
            outcome: committed_outcome(resolution)?,
        },
        RuntimeResolution::RejectedBeforeCommit { .. } => {
            DurableCommitState::RejectedBeforeCommit {
                rejection: rejected(resolution)?,
            }
        }
        RuntimeResolution::CancelledBeforeCommit { .. } => {
            DurableCommitState::CancelledBeforeCommit
        }
        RuntimeResolution::NeedsManualRecovery { .. } => DurableCommitState::NeedsManualRecovery,
    })
}

fn cancel_outcome(resolution: &RuntimeResolution) -> Result<CancelOutcome, ProviderError> {
    Ok(match resolution {
        RuntimeResolution::Prepared { .. } => {
            return Err(ProviderError::Transaction {
                code: "transaction_state_invalid",
                message: "cancel left the transaction prepared".to_string(),
            })
        }
        RuntimeResolution::Committing { .. } => CancelOutcome::AlreadyCommitStarted,
        RuntimeResolution::Committed { .. } => {
            CancelOutcome::AlreadyCommitted(committed_outcome(resolution)?)
        }
        RuntimeResolution::RejectedBeforeCommit { .. } => {
            CancelOutcome::AlreadyRejected(rejected(resolution)?)
        }
        RuntimeResolution::CancelledBeforeCommit { .. } => CancelOutcome::CancelledBeforeCommit,
        RuntimeResolution::NeedsManualRecovery { .. } => CancelOutcome::NeedsManualRecovery,
    })
}
