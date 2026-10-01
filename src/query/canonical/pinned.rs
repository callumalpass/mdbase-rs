//! Bounded metadata pages backed by one immutable SQLite WAL snapshot.
use std::sync::Arc;

use super::{
    execute::{build_metadata_page_result, QueryPerformance},
    model::{Direction, Query},
    preflight,
};
use crate::runtime::{CanonicalOperationOutcome, OperationContext, ProviderError};
use crate::{
    api::{OperationOutcome, QueryMetadata, QueryRequest, QueryResult},
    Collection,
};
use rusqlite::Connection;

pub(crate) struct PinnedMetadataQuery {
    collection: Arc<Collection>,
    connection: Connection,
    compiled: preflight::CompiledQuery,
    clock: Result<crate::expressions::evaluator::EvaluationClock, crate::cel::CelFailure>,
    total: Option<usize>,
    pub(crate) offset: u64,
}

impl PinnedMetadataQuery {
    /// General CEL/filter/summary queries retain the existing materialized path.
    /// Only queries with exact metadata ordering and deterministic type matching
    /// can paginate before record payloads are decoded.
    pub(crate) fn open(collection: Arc<Collection>, input: serde_json::Value) -> Option<Self> {
        if collection.spec_profile() != crate::SpecProfile::V03 {
            return None;
        }
        let request = QueryRequest::decode_wire(input).ok()?;
        if !super::model::validate_typed(&request).is_empty() {
            return None;
        }
        let compiled = preflight::compile(Query::from_typed(&request)).ok()?;
        if !compiled.supports_metadata_page_plan()
            || compiled.query.timezone.is_some()
            || i64::try_from(compiled.query.offset).is_err()
            || collection.types.values().any(|definition| {
                definition
                    .match_rules
                    .as_ref()
                    .is_some_and(|rules| rules.match_expr.is_some())
            })
            || collection
                .type_plans
                .values()
                .any(|plan| !plan.computed.is_empty())
        {
            return None;
        }
        let connection = crate::cache::sqlite::open_cache_db_read_only_existing(
            collection.held_root().cache_storage_path(),
            &collection.settings.cache_folder,
        )
        .ok()?;
        connection.execute_batch("BEGIN DEFERRED").ok()?;
        // Establish the snapshot while the provider's collection read gate is held.
        connection
            .query_row("SELECT COUNT(*) FROM files", [], |row| {
                row.get::<_, usize>(0)
            })
            .ok()?;
        let clock = crate::expressions::evaluator::EvaluationClock::from_utc(
            chrono::Utc::now(),
            collection.settings.timezone.as_deref(),
        )
        .ok()?;
        let offset = compiled.query.offset;
        Some(Self {
            collection,
            connection,
            compiled,
            clock: Ok(clock),
            total: None,
            offset,
        })
    }

    pub(crate) fn page(
        &mut self,
        index: usize,
        limit: usize,
        context: &OperationContext,
    ) -> Result<CanonicalOperationOutcome, ProviderError> {
        context.check()?;
        let order = self
            .compiled
            .query
            .order_by
            .iter()
            .map(|order| {
                (
                    order.field.as_str(),
                    matches!(order.direction, Direction::Desc),
                )
            })
            .collect::<Vec<_>>();
        let offset = self.offset.saturating_add(index as u64);
        let loaded = self.collection.load_query_metadata_page_connection(
            &self.connection,
            &self.compiled.query.types,
            &order,
            crate::query::cache_source::MetadataPageWindow {
                offset,
                limit: Some(limit as u64),
                known_total: self.total,
            },
            context.cancellation(),
        );
        context.check()?;
        let page = loaded.ok_or_else(|| ProviderError::Transaction {
            code: "cursor_state_invalid",
            message: "Could not read the pinned metadata page.".into(),
        })?;
        self.total = Some(page.total);
        let has_more = offset.saturating_add(page.records.len() as u64) < page.total as u64;
        let records = page.records.iter().collect::<Vec<_>>();
        let evaluated = build_metadata_page_result(
            &self.collection,
            &self.clock,
            &self.compiled,
            &records,
            page.total,
            has_more,
            &mut QueryPerformance::default(),
        )
        .map_err(|_| ProviderError::Transaction {
            code: "cursor_state_invalid",
            message: "Pinned metadata projection failed.".into(),
        })?;
        context.check()?;
        Ok(CanonicalOperationOutcome::query(OperationOutcome {
            value: QueryResult {
                records: evaluated.records.into_iter().map(Into::into).collect(),
                total_count: evaluated.total_count,
                has_more: evaluated.has_more,
                meta: QueryMetadata::new(evaluated.meta),
            },
            diagnostics: evaluated.diagnostics.into_iter().map(Into::into).collect(),
        }))
    }

    pub(crate) fn remaining(&self) -> usize {
        self.total
            .unwrap_or(0)
            .saturating_sub(self.offset.min(usize::MAX as u64) as usize)
    }
}
