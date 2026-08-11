use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

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
    signing_key: [u8; 32],
    runtime_epoch: String,
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
    pub(crate) fn new(runtime_epoch: impl Into<String>) -> Self {
        Self {
            entries: HashMap::new(),
            retained_bytes: 0,
            signing_key: cursor_signing_key(),
            runtime_epoch: runtime_epoch.into(),
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
        self.page(&self.issue(&id, 0), context)
    }

    pub(crate) fn page(
        &mut self,
        cursor: &ReadCursor,
        context: &OperationContext,
    ) -> Result<ReadPage, ProviderError> {
        context.check()?;
        self.remove_expired();
        let (id, next_index) = self.authenticate(cursor)?;
        let Some(pinned) = self.entries.get_mut(&id) else {
            return Err(ProviderError::GenerationExpired);
        };
        if next_index > pinned.results.len() {
            return Err(ProviderError::InvalidReadCursor);
        }
        let end = next_index
            .saturating_add(pinned.page_items)
            .min(pinned.results.len());
        let mut result = pinned.template.clone();
        result.result["results"] = Value::Array(pinned.results[next_index..end].to_vec());
        set_has_more(&mut result, end < pinned.results.len());
        pinned.last_access = Instant::now();
        let generation = pinned.generation.clone();
        let next = (end < pinned.results.len()).then(|| self.issue(&id, end));
        context.check()?;
        Ok(ReadPage {
            outcome: ExecutionOutcome {
                result,
                generation,
                changes: ChangeSet::None,
                commit_id: None,
                change_event: None,
            },
            next,
        })
    }

    pub(crate) fn release(&mut self, cursor: ReadCursor) -> Result<(), ProviderError> {
        let (id, _) = self.authenticate(&cursor)?;
        if let Some(removed) = self.entries.remove(&id) {
            self.retained_bytes = self.retained_bytes.saturating_sub(removed.retained_bytes);
        }
        Ok(())
    }

    pub(crate) fn measurements(&mut self) -> (usize, usize) {
        self.remove_expired();
        (self.entries.len(), self.retained_bytes)
    }

    fn issue(&self, id: &str, next_index: usize) -> ReadCursor {
        let message = format!("v1:{}:{id}:{next_index}", self.runtime_epoch);
        ReadCursor::issued(format!(
            "{message}:{}",
            cursor_mac(&self.signing_key, message.as_bytes())
        ))
    }

    fn authenticate(&self, cursor: &ReadCursor) -> Result<(String, usize), ProviderError> {
        let parts = cursor.as_token().split(':').collect::<Vec<_>>();
        if parts.len() != 5 || parts[0] != "v1" || parts[4].len() != 64 {
            return Err(ProviderError::InvalidReadCursor);
        }
        if parts[1] != self.runtime_epoch {
            return Err(ProviderError::GenerationExpired);
        }
        let id = uuid::Uuid::parse_str(parts[2])
            .map_err(|_| ProviderError::InvalidReadCursor)?
            .to_string();
        if id != parts[2] {
            return Err(ProviderError::InvalidReadCursor);
        }
        let next_index = parts[3]
            .parse::<usize>()
            .map_err(|_| ProviderError::InvalidReadCursor)?;
        let message = format!("v1:{}:{id}:{next_index}", self.runtime_epoch);
        let expected = cursor_mac(&self.signing_key, message.as_bytes());
        if !constant_time_equal(expected.as_bytes(), parts[4].as_bytes()) {
            return Err(ProviderError::InvalidReadCursor);
        }
        Ok((id, next_index))
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

fn cursor_signing_key() -> [u8; 32] {
    let mut key = [0_u8; 32];
    key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    key
}

fn cursor_mac(key: &[u8; 32], message: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut inner_pad = [0x36_u8; BLOCK];
    let mut outer_pad = [0x5c_u8; BLOCK];
    for (index, byte) in key.iter().enumerate() {
        inner_pad[index] ^= byte;
        outer_pad[index] ^= byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    format!("{:x}", outer.finalize())
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn set_has_more(result: &mut OperationResult, has_more: bool) {
    if let Some(meta) = result.result.get_mut("meta").and_then(Value::as_object_mut) {
        meta.insert("has_more".to_string(), Value::Bool(has_more));
    }
}
