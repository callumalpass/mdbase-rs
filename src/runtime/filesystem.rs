use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::diff::canonical_changes;
use super::observer::NoopObserver;
use super::{
    CancelOutcome, ChangeBatch, ChangeEventIdentity, ChangeFeed, ChangeFeedBaseline,
    ChangeFeedOwnerId, ChangeFeedTransfer, ChangeFeedTransferId, ChangeFeedTransferIntent,
    ChangePage, ChangePageCursor, ChangeSet, ChangeWatermark, CollectionGeneration, CommitAttempt,
    CommitId, CommitRejection, DurableCommitState, ExecutionOutcome, FilesystemProvider,
    HostClaimId, ObserverOptions, OperationContext, OperationRequest, PreparationOutcome,
    PreparedMutation, ProviderError, ReadCursor, ReadPage, RuntimeChangeEvent,
    RuntimeChangeEventPage, RuntimeObserver,
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
            &OperationContext::legacy(),
            |collection| {
                super::feed::reconcile(
                    collection,
                    initial_generation.clone(),
                    &OperationContext::legacy(),
                )
            },
        )?;
        provider.initialize_runtime_cache(&reconciled.generation)?;
        let watcher = CollectionWatcher::open(root, debounce)?;
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
            cursors: Mutex::new(super::cursor::CursorStore::new()),
        })
    }

    pub fn provider(&self) -> Arc<FilesystemProvider> {
        self.provider.clone()
    }

    pub fn execute(&self, request: &OperationRequest) -> Result<OperationResult, ProviderError> {
        self.execute_with_context(request, &OperationContext::legacy())
    }

    /// Execute while honoring the caller's cancellation and deadline before
    /// the mutation commit boundary.
    pub fn execute_with_context(
        &self,
        request: &OperationRequest,
        context: &OperationContext,
    ) -> Result<OperationResult, ProviderError> {
        if !request.operation.is_mutation() {
            return self
                .provider
                .execute_with_post_context(request, context, |_| Ok(()));
        }
        let claim = HostClaimId::generate();
        match self.prepare(request, &claim, context)? {
            PreparationOutcome::NoMutation(outcome) => Ok(outcome.result),
            PreparationOutcome::Prepared(prepared) => match self.commit(&prepared, context)? {
                CommitAttempt::Committed(outcome) => Ok(outcome.result),
                CommitAttempt::RejectedBeforeCommit { rejection } => Ok(rejection.result),
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
        if request.operation.is_mutation() {
            return Err(ProviderError::UnsupportedOperation(
                "read requires a non-mutation operation".to_string(),
            ));
        }
        self.wait_for_settlement(context)?;
        let expected = self.current_generation()?;
        self.provider.ensure_runtime_cache(&expected, context)?;
        let mut generation = None;
        let result = self
            .provider
            .execute_with_post_context(request, context, |_| {
                generation = Some(self.current_generation()?);
                Ok(())
            })?;
        let generation = generation.ok_or(ProviderError::LockPoisoned)?;
        Ok(ExecutionOutcome {
            result,
            generation,
            changes: ChangeSet::None,
            commit_id: None,
            change_event: None,
        })
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
        let outcome = self.read(&expanded, context)?;
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
    ) -> Result<(), ProviderError> {
        context.check()?;
        self.cursor_lock(context)?.release(cursor)?;
        context.check()
    }

    /// Validate and durably stage one exact mutation under an opaque host claim.
    pub fn prepare(
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
                let preparation = crate::v03::batch::prepare_single_runtime(
                    collection,
                    request.operation.as_str(),
                    &request.input,
                    context,
                )?;
                match preparation {
                    crate::v03::batch::RuntimeSinglePreparation::NoMutation(result) => {
                        Ok(PreparationOutcome::NoMutation(ExecutionOutcome {
                            result,
                            generation: self.current_generation()?,
                            changes: ChangeSet::None,
                            commit_id: None,
                            change_event: None,
                        }))
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
                                result: &plan.result,
                                changes: &changes,
                                event_id: &event_id,
                            },
                            context,
                        )
                        .map_err(transaction_error)?
                        {
                            RuntimePrepareOutcome::NoMutation(result) => {
                                Ok(PreparationOutcome::NoMutation(ExecutionOutcome {
                                    result,
                                    generation: self.current_generation()?,
                                    changes: ChangeSet::None,
                                    commit_id: None,
                                    change_event: None,
                                }))
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
                let settlement = OperationContext::legacy();
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
                let settlement = OperationContext::legacy();
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
        self.synchronize_with_context(&OperationContext::legacy())
    }

    /// Complete a full comparison within an explicit operation boundary.
    pub fn synchronize_with_context(
        &self,
        context: &OperationContext,
    ) -> Result<(), ProviderError> {
        context.check()?;
        self.watcher_lock(context)?.rescan()?;
        context.check()
    }

    fn current_generation(&self) -> Result<CollectionGeneration, ProviderError> {
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
            let mut active = self.settlement_lock(context)?;
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
                let settlement_context = OperationContext::legacy();
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
            std::thread::sleep(context.next_wait()?);
        }
    }

    fn finalize_resolution(
        &self,
        resolution: &RuntimeResolution,
    ) -> Result<CommitAttempt, ProviderError> {
        self.provider
            .with_collection_boundary_context(&OperationContext::legacy(), |collection| {
                finish_resolution_inside(
                    collection,
                    self.provider.as_ref(),
                    self.watcher.as_ref(),
                    self.pending_watch.as_ref(),
                    self.order.as_ref(),
                    resolution,
                )
            })
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
    Ok(ExecutionOutcome {
        result: result.clone(),
        generation: generation.clone(),
        changes: ChangeSet::Exact(changes.clone()),
        commit_id: Some(commit_id.clone()),
        change_event: Some(ChangeEventIdentity {
            id: event_id.clone(),
            watermark: *watermark,
        }),
    })
}

fn rejected(resolution: &RuntimeResolution) -> Result<CommitRejection, ProviderError> {
    let RuntimeResolution::RejectedBeforeCommit { rejection, .. } = resolution else {
        return Err(ProviderError::Transaction {
            code: "transaction_state_invalid",
            message: "expected a rejected runtime transaction".to_string(),
        });
    };
    Ok(CommitRejection {
        result: rejection.clone(),
    })
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
