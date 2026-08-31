use std::collections::HashMap;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::{
    CanonicalOperationOutcome, ChangeSet, CollectionGeneration, ExecutionOutcome, OperationContext,
    ProviderError, ReadCursor, ReadPage,
};

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
    template: CanonicalOperationOutcome,
    results: Vec<crate::api::ProjectedValue>,
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
        let Some(query) = outcome.operation.query_value_mut() else {
            return Ok(ReadPage {
                outcome,
                next: None,
            });
        };
        let results = std::mem::take(&mut query.records);
        let retained_u64 = if results.is_empty() {
            0
        } else {
            measured_json_bytes(&results, context.capture_limits().max_retained_bytes)?
        };
        if results.len() <= page_items {
            context.charge_retained(retained_u64)?;
            query.records = results;
            query.has_more = false;
            set_has_more(&mut query.meta, false);
            return Ok(ReadPage {
                outcome,
                next: None,
            });
        }
        let retained_bytes =
            usize::try_from(retained_u64).map_err(|_| crate::runtime::CaptureLimitExceeded {
                kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
                limit: usize::MAX as u64,
                attempted: retained_u64,
            })?;
        let store_bytes = self
            .retained_bytes
            .checked_add(retained_bytes)
            .ok_or(ProviderError::CursorCapacityExhausted)?;
        if self.entries.len() >= MAX_ACTIVE_CURSORS
            || retained_bytes > MAX_CURSOR_BYTES
            || store_bytes > MAX_CURSOR_BYTES
        {
            return Err(ProviderError::CursorCapacityExhausted);
        }
        // Reserve while holding the store lock, before charging the operation.
        // A failed reservation or capacity check therefore consumes no meter.
        self.entries
            .try_reserve(1)
            .map_err(|_| ProviderError::CursorCapacityExhausted)?;
        context.charge_retained(retained_u64)?;
        let id = uuid::Uuid::new_v4().to_string();
        let generation = outcome.generation.clone();
        let now = Instant::now();
        let template = outcome.operation.clone();
        self.retained_bytes = store_bytes;
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
        let mut operation = pinned.template.clone();
        let Some(query) = operation.query_value_mut() else {
            return Err(ProviderError::Transaction {
                code: "cursor_state_invalid",
                message: "pinned read no longer contains a typed query".to_string(),
            });
        };
        query.records = pinned.results[next_index..end].to_vec();
        query.has_more = end < pinned.results.len();
        set_has_more(&mut query.meta, query.has_more);
        pinned.last_access = Instant::now();
        let generation = pinned.generation.clone();
        let next = (end < pinned.results.len()).then(|| self.issue(&id, end));
        context.check()?;
        Ok(ReadPage {
            outcome: ExecutionOutcome::new(operation, generation, ChangeSet::None, None, None),
            next,
        })
    }

    pub(crate) fn release(&mut self, cursor: ReadCursor) -> Result<bool, ProviderError> {
        let (id, _) = self.authenticate(&cursor)?;
        let released = if let Some(removed) = self.entries.remove(&id) {
            self.retained_bytes = self.retained_bytes.saturating_sub(removed.retained_bytes);
            true
        } else {
            false
        };
        Ok(released)
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

pub(crate) fn measured_json_bytes<T: serde::Serialize>(
    value: &T,
    limit: u64,
) -> Result<u64, ProviderError> {
    struct Counter {
        bytes: u64,
        limit: u64,
        exceeded: Option<crate::runtime::CaptureLimitExceeded>,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let attempted = self
                .bytes
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| std::io::Error::other("capture arithmetic overflow"))?;
            if attempted > self.limit {
                self.exceeded = Some(crate::runtime::CaptureLimitExceeded {
                    kind: crate::runtime::CaptureLimitKind::RetainedBytes,
                    limit: self.limit,
                    attempted,
                });
                return Err(std::io::Error::other(
                    "capture retained-byte limit exceeded",
                ));
            }
            self.bytes = attempted;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter {
        bytes: 0,
        limit,
        exceeded: None,
    };
    if let Err(error) = serde_json::to_writer(&mut counter, value) {
        if let Some(exceeded) = counter.exceeded {
            return Err(exceeded.into());
        }
        return Err(ProviderError::Transaction {
            code: "cursor_serialization_failed",
            message: error.to_string(),
        });
    }
    Ok(counter.bytes)
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

fn set_has_more(meta: &mut serde_json::Value, has_more: bool) {
    if let Some(meta) = meta.as_object_mut() {
        meta.insert("has_more".to_string(), serde_json::Value::Bool(has_more));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ProjectedValue, QueryMetadata};
    use crate::runtime::{CanonicalOperationValue, CanonicalQueryValue, OperationDeadline};
    use serde_json::json;

    #[test]
    fn failed_store_capacity_does_not_consume_operation_meter() {
        let generation = CollectionGeneration::initial();
        let operation = CanonicalOperationOutcome {
            valid: true,
            value: CanonicalOperationValue::Query(Some(CanonicalQueryValue {
                records: vec![
                    ProjectedValue::new(json!({"id": 1})),
                    ProjectedValue::new(json!({"id": 2})),
                ],
                total_count: Some(2),
                has_more: false,
                meta: QueryMetadata::new(json!({})),
                embedded_diagnostics: Vec::new(),
            })),
            diagnostics: Vec::new(),
        };
        let outcome =
            ExecutionOutcome::new(operation, generation.clone(), ChangeSet::None, None, None);
        let records = match outcome.operation.value() {
            CanonicalOperationValue::Query(Some(query)) => &query.records,
            _ => unreachable!(),
        };
        let retained = measured_json_bytes(records, u64::MAX).unwrap();
        let context = OperationContext::with_capture_limits(
            &crate::OperationCancellation::new(),
            OperationDeadline::after(Duration::from_secs(1)),
            crate::runtime::CaptureLimits::builder()
                .max_retained_bytes(retained)
                .build(),
        );
        let mut store = CursorStore::new(generation.runtime_epoch());
        store.retained_bytes = MAX_CURSOR_BYTES;

        assert!(matches!(
            store.open(outcome, Some(1), &context),
            Err(ProviderError::CursorCapacityExhausted)
        ));
        context.charge_retained(retained).unwrap();
    }
}
