//! Repeatable, non-gating observations of link traversal cost as a collection grows.
//!
//! Generates a synthetic collection shaped like a reading library: `source` records and
//! `annotation` records whose declared `source` link field holds an ID wikilink. Each case runs
//! through `FilesystemRuntime`, as a Connect daemon runs it, and is timed once the runtime is
//! warm. Usage:
//!
//! ```bash
//! cargo run --release --bin link-traversal-benchmark -- --annotations 2000 6000 --sources 1000
//! ```

use mdbase::runtime::{FilesystemRuntime, OperationKind, OperationRequest};
use mdbase::v03::OperationResult;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

const CONFIG: &str = r#"spec_version: "0.3.0"
name: "Link traversal benchmark"
settings:
  types_folder: "_types"
  validation: "warn"
  timezone: "UTC"
  exclude: ["_types", ".mdbase"]
"#;

const SOURCE_TYPE: &str = r#"---
kind: mdbase.type
name: source
version: 1
match:
  path_glob: "sources/**/*.md"
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    required: [id, title]
    properties:
      id: { type: string }
      title: { type: string }
      kind: { type: string }
      tags: { type: array, items: { type: string } }
---
"#;

const ANNOTATION_TYPE: &str = r#"---
kind: mdbase.type
name: annotation
version: 1
match:
  path_glob: "annotations/**/*.md"
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    required: [id, source]
    properties:
      id: { type: string }
      source: { type: string }
      created_at: { type: string }
      tags: { type: array, items: { type: string } }
collection:
  links:
    source:
      target_type: source
      validate_exists: true
---
"#;

const REPEATS: usize = 3;

fn main() {
    if let Err(error) = run() {
        eprintln!("link traversal benchmark failed: {error}");
        std::process::exit(1);
    }
}

struct Options {
    annotations: Vec<usize>,
    sources: usize,
    output: Option<String>,
}

fn parse_options() -> Result<Options, String> {
    let mut options = Options {
        annotations: vec![2_000, 6_000],
        sources: 1_000,
        output: None,
    };
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--annotations" => {
                options.annotations.clear();
                while let Some(value) = args.peek().and_then(|value| value.parse().ok()) {
                    options.annotations.push(value);
                    args.next();
                }
            }
            "--sources" => {
                options.sources = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .ok_or("--sources needs a number")?;
            }
            "--output" => options.output = args.next(),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(options)
}

fn run() -> Result<(), String> {
    let options = parse_options()?;
    let mut report = Vec::new();
    for &annotations in &options.annotations {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let started = Instant::now();
        build_fixture(dir.path(), options.sources, annotations)?;
        let fixture_ms = ms(started);
        let started = Instant::now();
        let runtime = FilesystemRuntime::open(dir.path(), Duration::from_millis(120))
            .map_err(|error| error.to_string())?;
        let open_ms = ms(started);
        let mut cases = Vec::new();
        for (name, query) in queries() {
            cases.push(profile_query(&runtime, name, &query)?);
        }
        cases.push(profile_delete_check(&runtime)?);
        let entry = json!({
            "annotations": annotations,
            "sources": options.sources,
            "fixture_ms": fixture_ms,
            "open_ms": open_ms,
            "debug_assertions": cfg!(debug_assertions),
            "cases": cases,
        });
        print_summary(&entry);
        report.push(entry);
    }
    if let Some(path) = options.output {
        fs::write(
            &path,
            serde_json::to_string_pretty(&report).unwrap_or_default(),
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn queries() -> Vec<(&'static str, Value)> {
    vec![
        (
            "plain_page",
            json!({"types": ["annotation"], "order_by": [{"field": "created_at", "direction": "desc"}], "limit": 100}),
        ),
        (
            "asfile_where",
            json!({"types": ["annotation"], "where": "source.asFile() != null && source.asFile().kind == \"book\"", "limit": 100}),
        ),
        (
            "asfile_sort",
            json!({
                "types": ["annotation"],
                "projections": {"source_title": {"expr": "source.asFile().title"}},
                "order_by": [
                    {"field": "projection.source_title", "direction": "asc"},
                    {"field": "created_at", "direction": "asc"}
                ],
                "limit": 100
            }),
        ),
        (
            "backlinks_where",
            json!({"types": ["source"], "where": "file.backlinks.size() > 5", "limit": 100}),
        ),
    ]
}

fn execute(
    runtime: &FilesystemRuntime,
    kind: OperationKind,
    input: &Value,
) -> Result<OperationResult, String> {
    runtime
        .execute(&OperationRequest::new(kind, input.clone()))
        .map_err(|error| error.to_string())
}

fn profile_query(runtime: &FilesystemRuntime, name: &str, query: &Value) -> Result<Value, String> {
    // The first run warms the runtime and its cache; the measured runs are warm.
    let first = execute(runtime, OperationKind::Query, query)?;
    ensure_success(name, &first)?;
    let total = first.result["meta"]["total_count"].clone();
    let mut samples = Vec::new();
    for _ in 0..REPEATS {
        let started = Instant::now();
        let result = execute(runtime, OperationKind::Query, query)?;
        ensure_success(name, &result)?;
        if result.result["meta"]["total_count"] != total {
            return Err(format!("{name}: total_count changed between runs"));
        }
        samples.push(ms(started));
    }
    Ok(json!({
        "case": name,
        "median_ms": median(&mut samples),
        "samples_ms": samples,
        "total_count": total,
    }))
}

/// A dry-run delete that reports the records that would be left with broken links.
fn profile_delete_check(runtime: &FilesystemRuntime) -> Result<Value, String> {
    let input = json!({"path": "sources/src-00000.md", "check_backlinks": true, "dry_run": true});
    let mut samples = Vec::new();
    let mut broken = 0;
    for _ in 0..REPEATS {
        let started = Instant::now();
        let result = execute(runtime, OperationKind::Delete, &input)?;
        ensure_success("delete_check_backlinks", &result)?;
        samples.push(ms(started));
        broken = result.result["broken_links"].as_array().map_or(0, Vec::len);
    }
    Ok(json!({
        "case": "delete_check_backlinks",
        "median_ms": median(&mut samples),
        "samples_ms": samples,
        "broken_links": broken,
    }))
}

fn build_fixture(root: &Path, sources: usize, annotations: usize) -> Result<(), String> {
    let write = |path: &Path, content: &str| -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(path, content).map_err(|error| error.to_string())
    };
    write(&root.join("mdbase.yaml"), CONFIG)?;
    write(&root.join("_types/source.md"), SOURCE_TYPE)?;
    write(&root.join("_types/annotation.md"), ANNOTATION_TYPE)?;
    let kinds = ["book", "article", "chapter", "thesis"];
    for index in 0..sources {
        let body = "Notes about this source. ".repeat(8);
        write(
            &root.join(format!("sources/src-{index:05}.md")),
            &format!(
                "---\nid: src_{index:05}\ntitle: Source {index:05}\nkind: {}\ntags: [reading]\n---\n\n{body}\n",
                kinds[index % kinds.len()]
            ),
        )?;
    }
    for index in 0..annotations {
        // Deterministic spread: every source is linked, some far more often than others.
        let source = (index * 7919 + index / 3) % sources;
        write(
            &root.join(format!("annotations/{:02}/ann-{index:06}.md", index % 50)),
            &format!(
                "---\nid: ann_{index:06}\nsource: '[[src_{source:05}]]'\ncreated_at: 2026-0{}-{:02}T00:00:00Z\ntags: []\n---\n\n> A highlighted passage number {index}.\n\nA short note.\n",
                1 + index % 9,
                10 + index % 18
            ),
        )?;
    }
    Ok(())
}

fn print_summary(entry: &Value) {
    println!(
        "annotations={} sources={} (open {:.0} ms)",
        entry["annotations"],
        entry["sources"],
        entry["open_ms"].as_f64().unwrap_or(0.0)
    );
    for case in entry["cases"].as_array().into_iter().flatten() {
        println!(
            "  {:<24} {:>10.1} ms  ({} results)",
            case["case"].as_str().unwrap_or(""),
            case["median_ms"].as_f64().unwrap_or(0.0),
            case.get("total_count")
                .or(case.get("broken_links"))
                .unwrap_or(&Value::Null)
        );
    }
}

fn ensure_success(name: &str, result: &OperationResult) -> Result<(), String> {
    if result.valid {
        return Ok(());
    }
    Err(format!(
        "{name}: {}",
        result
            .diagnostics
            .iter()
            .map(|diagnostic| format!("{}: {}", diagnostic.code, diagnostic.message))
            .collect::<Vec<_>>()
            .join("; ")
    ))
}

fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1_000.0
}

fn median(samples: &mut [f64]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted.get(sorted.len() / 2).copied().unwrap_or(0.0)
}
