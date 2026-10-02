//! Cursor read orchestration; filesystem mutation ownership stays in the parent.
use super::FilesystemRuntime;
use crate::runtime::{
    CursorReleaseOutcome, OperationContext, OperationKind, OperationRequest, ProviderError,
    ReadCursor, ReadPage,
};

impl FilesystemRuntime {
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
        if request.operation == OperationKind::Query {
            self.wait_for_settlement(context)?;
            let expected = self.current_generation()?;
            self.provider.ensure_runtime_cache(&expected, context)?;
            let pinned = self
                .provider
                .with_collection_arc_read_context(context, |collection| {
                    let source = crate::query::canonical::pinned::PinnedMetadataQuery::open(
                        collection,
                        request.input.clone(),
                    );
                    match source {
                        Some(source) => {
                            Ok::<_, ProviderError>(Some((source, self.current_generation()?)))
                        }
                        None => Ok(None),
                    }
                })?;
            if let Some((source, generation)) = pinned {
                let retained_bytes = serde_json::to_vec(&request.input)
                    .map_err(|_| ProviderError::InvalidReadCursor)?
                    .len()
                    .saturating_add(4096);
                return self.cursor_lock(context)?.open_metadata(
                    source,
                    generation,
                    page_items,
                    retained_bytes,
                    context,
                );
            }
        }
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

    /// Validate an explicitly repeated output mode before consuming or releasing
    /// a continuation. Omitting the mode always retains the pinned shape.
    pub fn validate_read_output(
        &self,
        cursor: &ReadCursor,
        output: crate::api::QueryOutput,
        context: &OperationContext,
    ) -> Result<(), ProviderError> {
        context.check()?;
        self.cursor_lock(context)?.validate_output(cursor, output)
    }

    /// Choose a bounded continuation size without changing the pinned data.
    /// Replaying an issued cursor with a different explicit size is rejected.
    pub fn read_page_with_limit(
        &self,
        cursor: &ReadCursor,
        limit: Option<usize>,
        context: &OperationContext,
    ) -> Result<ReadPage, ProviderError> {
        self.cursor_lock(context)?
            .page_with_limit(cursor, limit, context)
    }

    /// Explicitly release a pinned read and its bounded retained state.
    pub fn release_read(
        &self,
        cursor: ReadCursor,
        context: &OperationContext,
    ) -> Result<CursorReleaseOutcome, ProviderError> {
        context.check()?;
        let released = self.cursor_lock(context)?.release(cursor)?;
        context.check()?;
        Ok(CursorReleaseOutcome { released })
    }
}
