use std::num::NonZeroUsize;

use super::{
    CancelOutcome, ChangeBatch, ChangeFeed, ChangeFeedBaseline, ChangeFeedOwnerId,
    ChangeFeedTransfer, ChangeFeedTransferId, ChangeFeedTransferIntent, ChangePage,
    ChangePageCursor, ChangeWatermark, CommitAttempt, CommitId, CursorReleaseOutcome,
    DurableCommitState, ExecutionOutcome, HostClaimId, OperationContext, OperationRequest,
    PreparationOutcome, PreparedMutation, ProviderError, ReadCursor, ReadPage,
    RuntimeChangeEventPage,
};

/// Provider-neutral authority over one coordinated collection runtime.
pub trait CollectionRuntime: Send + Sync {
    fn read(
        &self,
        request: &OperationRequest,
        context: &OperationContext,
    ) -> Result<ExecutionOutcome, ProviderError>;

    fn open_read(
        &self,
        request: &OperationRequest,
        context: &OperationContext,
    ) -> Result<ReadPage, ProviderError>;

    fn read_page(
        &self,
        cursor: &ReadCursor,
        context: &OperationContext,
    ) -> Result<ReadPage, ProviderError>;

    fn release_read(
        &self,
        cursor: ReadCursor,
        context: &OperationContext,
    ) -> Result<CursorReleaseOutcome, ProviderError>;

    fn prepare(
        &self,
        request: &OperationRequest,
        claim: &HostClaimId,
        context: &OperationContext,
    ) -> Result<PreparationOutcome, ProviderError>;

    fn attach_prepared(
        &self,
        claim: &HostClaimId,
        context: &OperationContext,
    ) -> Result<Option<PreparedMutation>, ProviderError>;

    fn commit(
        &self,
        mutation: &PreparedMutation,
        context: &OperationContext,
    ) -> Result<CommitAttempt, ProviderError>;

    fn cancel(
        &self,
        mutation: &PreparedMutation,
        context: &OperationContext,
    ) -> Result<CancelOutcome, ProviderError>;

    fn resolve_commit(
        &self,
        commit_id: &CommitId,
        context: &OperationContext,
    ) -> Result<Option<DurableCommitState>, ProviderError>;

    fn resolve_claim(
        &self,
        claim: &HostClaimId,
        context: &OperationContext,
    ) -> Result<Option<(CommitId, DurableCommitState)>, ProviderError>;

    fn change_page(
        &self,
        batch: &ChangeBatch,
        after: Option<&ChangePageCursor>,
        limit: NonZeroUsize,
        context: &OperationContext,
    ) -> Result<ChangePage, ProviderError>;

    fn open_change_feed(
        &self,
        owner: &ChangeFeedOwnerId,
        context: &OperationContext,
    ) -> Result<ChangeFeed, ProviderError>;

    fn transfer_change_feed(
        &self,
        intent: &ChangeFeedTransferIntent,
        context: &OperationContext,
    ) -> Result<ChangeFeedTransfer, ProviderError>;

    fn ack_change_feed_transfer(
        &self,
        transfer: &ChangeFeedTransferId,
        context: &OperationContext,
    ) -> Result<(), ProviderError>;

    fn establish_change_feed_baseline(
        &self,
        feed: &ChangeFeed,
        context: &OperationContext,
    ) -> Result<ChangeFeedBaseline, ProviderError>;

    fn read_change_events(
        &self,
        feed: &ChangeFeed,
        after: Option<ChangeWatermark>,
        limit: NonZeroUsize,
        context: &OperationContext,
    ) -> Result<RuntimeChangeEventPage, ProviderError>;

    fn ack_change_events(
        &self,
        feed: &ChangeFeed,
        through: ChangeWatermark,
        context: &OperationContext,
    ) -> Result<(), ProviderError>;

    fn ack_commit_resolution(
        &self,
        commit_id: &CommitId,
        context: &OperationContext,
    ) -> Result<(), ProviderError>;
}

impl CollectionRuntime for super::FilesystemRuntime {
    fn read(
        &self,
        request: &OperationRequest,
        context: &OperationContext,
    ) -> Result<ExecutionOutcome, ProviderError> {
        super::FilesystemRuntime::read(self, request, context)
    }

    fn open_read(
        &self,
        request: &OperationRequest,
        context: &OperationContext,
    ) -> Result<ReadPage, ProviderError> {
        super::FilesystemRuntime::open_read(self, request, context)
    }

    fn read_page(
        &self,
        cursor: &ReadCursor,
        context: &OperationContext,
    ) -> Result<ReadPage, ProviderError> {
        super::FilesystemRuntime::read_page(self, cursor, context)
    }

    fn release_read(
        &self,
        cursor: ReadCursor,
        context: &OperationContext,
    ) -> Result<CursorReleaseOutcome, ProviderError> {
        super::FilesystemRuntime::release_read(self, cursor, context)
    }

    fn prepare(
        &self,
        request: &OperationRequest,
        claim: &HostClaimId,
        context: &OperationContext,
    ) -> Result<PreparationOutcome, ProviderError> {
        super::FilesystemRuntime::prepare(self, request, claim, context)
    }

    fn attach_prepared(
        &self,
        claim: &HostClaimId,
        context: &OperationContext,
    ) -> Result<Option<PreparedMutation>, ProviderError> {
        super::FilesystemRuntime::attach_prepared(self, claim, context)
    }

    fn commit(
        &self,
        mutation: &PreparedMutation,
        context: &OperationContext,
    ) -> Result<CommitAttempt, ProviderError> {
        super::FilesystemRuntime::commit(self, mutation, context)
    }

    fn cancel(
        &self,
        mutation: &PreparedMutation,
        context: &OperationContext,
    ) -> Result<CancelOutcome, ProviderError> {
        super::FilesystemRuntime::cancel(self, mutation, context)
    }

    fn resolve_commit(
        &self,
        commit_id: &CommitId,
        context: &OperationContext,
    ) -> Result<Option<DurableCommitState>, ProviderError> {
        super::FilesystemRuntime::resolve_commit(self, commit_id, context)
    }

    fn resolve_claim(
        &self,
        claim: &HostClaimId,
        context: &OperationContext,
    ) -> Result<Option<(CommitId, DurableCommitState)>, ProviderError> {
        super::FilesystemRuntime::resolve_claim(self, claim, context)
    }

    fn change_page(
        &self,
        batch: &ChangeBatch,
        after: Option<&ChangePageCursor>,
        limit: NonZeroUsize,
        context: &OperationContext,
    ) -> Result<ChangePage, ProviderError> {
        super::FilesystemRuntime::change_page(self, batch, after, limit, context)
    }

    fn open_change_feed(
        &self,
        owner: &ChangeFeedOwnerId,
        context: &OperationContext,
    ) -> Result<ChangeFeed, ProviderError> {
        super::FilesystemRuntime::open_change_feed(self, owner, context)
    }

    fn transfer_change_feed(
        &self,
        intent: &ChangeFeedTransferIntent,
        context: &OperationContext,
    ) -> Result<ChangeFeedTransfer, ProviderError> {
        super::FilesystemRuntime::transfer_change_feed(self, intent, context)
    }

    fn ack_change_feed_transfer(
        &self,
        transfer: &ChangeFeedTransferId,
        context: &OperationContext,
    ) -> Result<(), ProviderError> {
        super::FilesystemRuntime::ack_change_feed_transfer(self, transfer, context)
    }

    fn establish_change_feed_baseline(
        &self,
        feed: &ChangeFeed,
        context: &OperationContext,
    ) -> Result<ChangeFeedBaseline, ProviderError> {
        super::FilesystemRuntime::establish_change_feed_baseline(self, feed, context)
    }

    fn read_change_events(
        &self,
        feed: &ChangeFeed,
        after: Option<ChangeWatermark>,
        limit: NonZeroUsize,
        context: &OperationContext,
    ) -> Result<RuntimeChangeEventPage, ProviderError> {
        super::FilesystemRuntime::read_change_events(self, feed, after, limit, context)
    }

    fn ack_change_events(
        &self,
        feed: &ChangeFeed,
        through: ChangeWatermark,
        context: &OperationContext,
    ) -> Result<(), ProviderError> {
        super::FilesystemRuntime::ack_change_events(self, feed, through, context)
    }

    fn ack_commit_resolution(
        &self,
        commit_id: &CommitId,
        context: &OperationContext,
    ) -> Result<(), ProviderError> {
        super::FilesystemRuntime::ack_commit_resolution(self, commit_id, context)
    }
}
