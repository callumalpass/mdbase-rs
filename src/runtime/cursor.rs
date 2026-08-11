use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{
    ChangeSet, CollectionGeneration, ExecutionOutcome, OperationContext, ProviderError, ReadCursor,
    ReadPage,
};
use crate::v03::OperationResult;

const MAX_ACTIVE_CURSORS: usize = 32;
const MAX_CURSOR_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_PAGE_ITEMS: usize = 100;
const MAX_PAGE_ITEMS: usize = 256;
const IDLE_LEASE: Duration = Duration::from_secs(30);
const HARD_LIFETIME: Duration = Duration::from_secs(5 * 60);

pub(crate) struct CursorStore {
    entries: HashMap<String, PinnedRead>,
    retained_bytes: usize,
}

struct PinnedRead {
    generation: CollectionGeneration,
    template: OperationResult,
    results: Vec<Value>,
    page_items: usize,
    retained_bytes: usize,
    created: Instant,
    last_access: Instant,
}

impl CursorStore {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            retained_bytes: 0,
        }
    }

    pub(crate) fn open(
        &mut self,
        mut outcome: ExecutionOutcome,
        page_items: Option<usize>,
        context: &OperationContext,
    ) -> Result<ReadPage, ProviderError> {
        context.check()?;
        self.remove_expired();
        let page_items = page_items
            .unwrap_or(DEFAULT_PAGE_ITEMS)
            .clamp(1, MAX_PAGE_ITEMS);
        let Some(results) = outcome
            .result
            .result
            .get_mut("results")
            .and_then(Value::as_array_mut)
            .map(std::mem::take)
        else {
            return Ok(ReadPage {
                outcome,
                next: None,
            });
        };
        if results.len() <= page_items {
            outcome.result.result["results"] = Value::Array(results);
            set_has_more(&mut outcome.result, false);
            return Ok(ReadPage {
                outcome,
                next: None,
            });
        }
        let retained_bytes = serde_json::to_vec(&results)
            .map_err(|error| ProviderError::Transaction {
                code: "cursor_serialization_failed",
                message: error.to_string(),
            })?
            .len();
        if self.entries.len() >= MAX_ACTIVE_CURSORS
            || retained_bytes > MAX_CURSOR_BYTES
            || self.retained_bytes.saturating_add(retained_bytes) > MAX_CURSOR_BYTES
        {
            return Err(ProviderError::CursorCapacityExhausted);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let generation = outcome.generation.clone();
        let now = Instant::now();
        let template = outcome.result.clone();
        self.retained_bytes += retained_bytes;
        self.entries.insert(
            id.clone(),
            PinnedRead {
                generation: generation.clone(),
                template,
                results,
                page_items,
                retained_bytes,
                created: now,
                last_access: now,
            },
        );
        self.page(
            &ReadCursor {
                id,
                generation,
                next_index: 0,
            },
            context,
        )
    }

    pub(crate) fn page(
        &mut self,
        cursor: &ReadCursor,
        context: &OperationContext,
    ) -> Result<ReadPage, ProviderError> {
        context.check()?;
        self.remove_expired();
        let Some(pinned) = self.entries.get_mut(&cursor.id) else {
            return Err(ProviderError::GenerationExpired);
        };
        if pinned.generation != cursor.generation || cursor.next_index > pinned.results.len() {
            return Err(ProviderError::InvalidReadCursor);
        }
        let end = cursor
            .next_index
            .saturating_add(pinned.page_items)
            .min(pinned.results.len());
        let mut result = pinned.template.clone();
        result.result["results"] = Value::Array(pinned.results[cursor.next_index..end].to_vec());
        set_has_more(&mut result, end < pinned.results.len());
        pinned.last_access = Instant::now();
        let next = (end < pinned.results.len()).then(|| ReadCursor {
            id: cursor.id.clone(),
            generation: cursor.generation.clone(),
            next_index: end,
        });
        context.check()?;
        Ok(ReadPage {
            outcome: ExecutionOutcome {
                result,
                generation: pinned.generation.clone(),
                changes: ChangeSet::None,
                commit_id: None,
                change_event: None,
            },
            next,
        })
    }

    pub(crate) fn release(&mut self, cursor: ReadCursor) -> Result<(), ProviderError> {
        match self.entries.get(&cursor.id) {
            Some(pinned) if pinned.generation != cursor.generation => {
                return Err(ProviderError::InvalidReadCursor)
            }
            Some(_) => {}
            None => return Ok(()),
        }
        if let Some(removed) = self.entries.remove(&cursor.id) {
            self.retained_bytes = self.retained_bytes.saturating_sub(removed.retained_bytes);
        }
        Ok(())
    }

    fn remove_expired(&mut self) {
        let now = Instant::now();
        let expired = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                now.duration_since(entry.last_access) >= IDLE_LEASE
                    || now.duration_since(entry.created) >= HARD_LIFETIME
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in expired {
            if let Some(removed) = self.entries.remove(&id) {
                self.retained_bytes = self.retained_bytes.saturating_sub(removed.retained_bytes);
            }
        }
    }
}

fn set_has_more(result: &mut OperationResult, has_more: bool) {
    if let Some(meta) = result.result.get_mut("meta").and_then(Value::as_object_mut) {
        meta.insert("has_more".to_string(), Value::Bool(has_more));
    }
}
