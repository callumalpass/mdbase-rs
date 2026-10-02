use super::linked_files::{LinkedFiles, StoredLinkTargets};
use super::resolver::{LinkResolutionIndex, LinkResolutionOptions, ResolutionKeys};
use crate::expressions::evaluator::ResolvedFileData;
use serde_json::json;

fn index(records: usize) -> (Vec<ResolvedFileData>, LinkResolutionIndex) {
    let files = (0..records)
        .map(|i| ResolvedFileData {
            path: format!("sources/book-{i}.md"),
            frontmatter: json!({"type":"source"}),
            body: String::new(),
        })
        .collect::<Vec<_>>();
    let mut index = LinkResolutionIndex::untyped(&files, &ResolutionKeys::default());
    index.known_types.insert("source".into());
    for file in &files {
        index
            .types_by_path
            .insert(file.path.clone(), vec!["source".into()]);
    }
    (files, index)
}

#[test]
fn unique_policy_keeps_the_shared_candidate_budget() {
    let (_, mut index) = index(crate::runtime::MAX_RESOLUTION_CANDIDATES + 1);
    index.basename_lower_to_paths.insert(
        "duplicate".into(),
        index.known_paths.iter().cloned().collect(),
    );
    let result = index.resolve_with_options(
        "duplicate",
        "annotations/a.md",
        &[],
        &LinkResolutionOptions {
            unique: true,
            types: vec!["source".into()],
        },
    );
    assert!(
        result.is_err(),
        "uniqueness must not turn budget exhaustion into null"
    );
}

/// Non-gating indexed lookup observations, excluding snapshot/index construction.
/// Run: CARGO_BUILD_JOBS=2 cargo test -p mdbase --lib indexed_policy_lookup_50k -- --ignored --nocapture
#[test]
#[ignore = "manual indexed 50k-record benchmark"]
fn indexed_policy_lookup_50k() {
    use std::hint::black_box;
    use std::time::Instant;
    for records in [500, 50_000] {
        let started = Instant::now();
        let (files, index) = index(records);
        let stored = StoredLinkTargets::from([(
            "annotations/a.md".into(),
            std::collections::HashMap::from([("book-42".into(), "sources/book-42.md".into())]),
        )]);
        let links = LinkedFiles::new(files, stored, ResolutionKeys::default(), Some(index));
        let build_us = started.elapsed().as_micros();
        let native = LinkResolutionOptions::default();
        let unique = LinkResolutionOptions {
            unique: true,
            types: vec!["source".into()],
        };
        for (mode, options) in [
            ("default_stored", None),
            ("explicit_native", Some(&native)),
            ("unique_source", Some(&unique)),
        ] {
            let started = Instant::now();
            for _ in 0..10_000 {
                let result = black_box(&links)
                    .resolve_with_options(
                        black_box("[[book-42]]"),
                        Some("annotations/a.md"),
                        options,
                    )
                    .unwrap()
                    .unwrap();
                assert_eq!(result.path, "sources/book-42.md");
                black_box(result);
            }
            eprintln!(
                "{}",
                json!({"records":records,"index_build_us":build_us,"mode":mode,"lookups":10000,"lookup_total_us":started.elapsed().as_micros()})
            );
        }
    }
}
