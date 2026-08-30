//! v0.3 query wire adapter.
//!
//! This module owns only schema validation, wire decoding, and operation-result
//! envelopes. Parsed query semantics live in [`crate::query::canonical`].

use std::time::Instant;

use serde_json::{json, Value};

use crate::diagnostic::Diagnostic;
use crate::query::canonical;
pub use crate::query::canonical::QueryPerformance;
use crate::v03::{validate_query, OperationResult};
use crate::{Collection, OperationCancellation, OperationCancelled};

pub(crate) fn execute(collection: &Collection, input: &Value) -> OperationResult {
    execute_profiled(collection, input).0
}

pub(crate) fn execute_profiled(
    collection: &Collection,
    input: &Value,
) -> (OperationResult, QueryPerformance) {
    let context = crate::runtime::OperationContext::internal();
    execute_wire_profiled_cancellable(collection, input, context.cancellation(), false)
        .expect("the context-free compatibility context is active")
}

pub(crate) fn execute_cancellable(
    collection: &Collection,
    input: &Value,
    cancellation: &OperationCancellation,
) -> Result<OperationResult, OperationCancelled> {
    execute_wire_profiled_cancellable(collection, input, cancellation, false)
        .map(|(result, _)| result)
}

pub(crate) fn execute_runtime_cancellable(
    collection: &Collection,
    input: &Value,
    cancellation: &OperationCancellation,
) -> Result<OperationResult, OperationCancelled> {
    execute_wire_profiled_cancellable(collection, input, cancellation, true)
        .map(|(result, _)| result)
}

fn execute_wire_profiled_cancellable(
    collection: &Collection,
    input: &Value,
    cancellation: &OperationCancellation,
    runtime_cache: bool,
) -> Result<(OperationResult, QueryPerformance), OperationCancelled> {
    let total_started = Instant::now();
    cancellation.check()?;
    let phase = Instant::now();
    #[cfg(test)]
    canonical::record_wire_schema_validation();
    let schema_diagnostics = validate_query(input)
        .into_iter()
        .map(canonical::diagnostics::invalid_schema)
        .collect::<Vec<_>>();
    let schema_us = micros(phase.elapsed());
    cancellation.check()?;
    if !schema_diagnostics.is_empty() {
        return Ok((
            failed(schema_diagnostics),
            QueryPerformance {
                total_us: micros(total_started.elapsed()),
                schema_us,
                ..QueryPerformance::default()
            },
        ));
    }
    #[cfg(test)]
    canonical::record_wire_query_decode();
    let query = match serde_json::from_value::<canonical::Query>(input.clone()) {
        Ok(query) => query,
        Err(error) => {
            return Ok((
                failed(vec![Diagnostic::error(
                    "invalid_query",
                    format!("Query could not be decoded: {error}"),
                    None,
                )]),
                QueryPerformance {
                    total_us: micros(total_started.elapsed()),
                    schema_us,
                    ..QueryPerformance::default()
                },
            ));
        }
    };
    canonical::execute_model_profiled_cancellable(
        collection,
        query,
        cancellation,
        runtime_cache,
        total_started,
        schema_us,
    )
    .map(|(evaluation, performance)| (evaluation_into_wire(evaluation), performance))
}

fn evaluation_into_wire(evaluation: canonical::QueryEvaluation) -> OperationResult {
    match evaluation {
        Ok(execution) => {
            let inner_diagnostics =
                serde_json::to_value(&execution.diagnostics).unwrap_or_else(|_| json!([]));
            OperationResult {
                valid: true,
                result: json!({
                    "results": execution.records,
                    "meta": execution.meta,
                    "diagnostics": inner_diagnostics,
                }),
                diagnostics: execution.diagnostics,
            }
        }
        Err(diagnostics) => failed(diagnostics),
    }
}

fn failed(diagnostics: Vec<Diagnostic>) -> OperationResult {
    OperationResult {
        valid: false,
        result: json!({}),
        diagnostics,
    }
}

fn micros(duration: std::time::Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}
