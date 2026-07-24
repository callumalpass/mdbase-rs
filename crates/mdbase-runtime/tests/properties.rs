use std::collections::{BTreeMap, BTreeSet};

use chrono::{TimeZone, Utc};
use mdbase_runtime::{InMemoryRuntimeStore, PreparedEvent, RuntimeStore};
use proptest::prelude::*;
use serde_json::json;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn event_admission_has_set_semantics(deliveries in prop::collection::vec(0u8..32, 1..256)) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        runtime.block_on(async move {
            let store = InMemoryRuntimeStore::new();
            let now = Utc.with_ymd_and_hms(2026, 7, 24, 0, 0, 0).single().unwrap();
            let mut cursors = BTreeMap::new();
            let expected = deliveries.iter().copied().collect::<BTreeSet<_>>();

            for id in deliveries {
                let event_id = format!("evt_{id}");
                let outcome = store.admit_event(PreparedEvent {
                    source_runtime: "property-runtime".to_string(),
                    event_id: event_id.clone(),
                    envelope: json!({"id": event_id}),
                    received_at: now,
                    runs: Vec::new(),
                }).await.unwrap();
                if let Some(cursor) = cursors.get(&id) {
                    prop_assert!(outcome.duplicate);
                    prop_assert_eq!(outcome.cursor, *cursor);
                } else {
                    prop_assert!(!outcome.duplicate);
                    cursors.insert(id, outcome.cursor);
                }
            }

            let snapshot = store.snapshot().await.unwrap();
            prop_assert_eq!(snapshot.events.len(), expected.len());
            prop_assert_eq!(
                snapshot.events.iter().map(|event| event.cursor).collect::<BTreeSet<_>>().len(),
                expected.len()
            );
            Ok(())
        })?;
    }
}
