use std::time::{Duration, Instant};

use mdbase::runtime_contracts::{
    ComposeOptions, ContractDocument, ContractSource, RuntimeContracts,
};
use serde_json::json;

#[test]
fn large_virtual_registry_composes_deterministically_with_bounded_time() {
    const CONTRACTS: usize = 2_000;
    let runtime = RuntimeContracts::new().unwrap();
    let documents = (0..CONTRACTS)
        .map(|index| {
            ContractDocument::virtual_contract(json!({
                "type": "event",
                "id": format!("benchmark.event.{index:04}"),
                "version": 1,
                "provider": "benchmark",
                "name": format!("Benchmark event {index}"),
                "schemas": {
                    "dialect": "json-schema-2020-12",
                    "payload": {
                        "type": "object",
                        "properties": {"value": {"type": "integer"}}
                    }
                }
            }))
        })
        .collect::<Vec<_>>();

    let started = Instant::now();
    let registry = runtime.compose(
        vec![ContractSource::built_in(documents)],
        &ComposeOptions::default(),
    );
    let elapsed = started.elapsed();

    assert!(registry.valid(), "{:#?}", registry.diagnostics);
    assert_eq!(registry.events.len(), CONTRACTS);
    assert_eq!(
        registry.events.keys().next().map(String::as_str),
        Some("benchmark.event.0000")
    );
    assert_eq!(
        registry.events.keys().next_back().map(String::as_str),
        Some("benchmark.event.1999")
    );
    // This deliberately generous guard catches accidental superlinear work or
    // per-contract network I/O while remaining stable on debug CI runners.
    assert!(
        elapsed < Duration::from_secs(15),
        "composition took {elapsed:?}"
    );
    eprintln!(
        "runtime_registry_performance contracts={CONTRACTS} elapsed_us={}",
        elapsed.as_micros()
    );
}
