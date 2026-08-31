//! Repeatable, non-gating measurements through public collection/provider/watcher APIs.

use chrono::Utc;
use mdbase::runtime::{CollectionProvider, FilesystemProvider, OperationKind, OperationRequest};
use mdbase::watch::CollectionWatcher;
use mdbase::Collection;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const TASK_TYPE: &str = r#"---
kind: mdbase.type
name: task
version: 1
description: Phase 0 synthetic task
match:
  path_glob: ["tasks/*.md", "scratch/*.md", "refs/*.md"]
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    required: [type, title]
    additionalProperties: false
    properties:
      type: { const: task }
      id: { type: string }
      title: { type: string, minLength: 1 }
      status: { enum: [open, in-progress, done] }
      priority: { type: integer, minimum: 1, maximum: 5 }
      points: { type: integer, minimum: 0, maximum: 13 }
      links:
        type: array
        items: { type: string }
---

# Task
"#;

const CONFIG: &str = r#"spec_version: "0.3.0"
name: "Phase 0 baseline"
settings:
  types_folder: "_types"
  validation: "error"
  timezone: "UTC"
  exclude: ["_types", ".mdbase"]
"#;

const STATUS: [&str; 3] = ["open", "in-progress", "done"];

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    tool: &'static str,
    generated_at: String,
    source: SourceCommits,
    host: HostInfo,
    configuration: RunConfig,
    workloads: Vec<WorkloadReport>,
    gaps: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SourceCommits {
    mdbase_rs: String,
    mdbase_connect: String,
}

#[derive(Debug, Serialize)]
struct HostInfo {
    os: &'static str,
    arch: &'static str,
    parallelism: usize,
    rss_supported: bool,
}

#[derive(Debug, Serialize)]
struct RunConfig {
    record_sizes: Vec<usize>,
    query_limit: usize,
    pagination_limit: usize,
    mixed_threads: usize,
    mixed_rounds_per_thread: usize,
    rss_requests: usize,
    seed: u64,
    fixtures_kept: bool,
}

#[derive(Debug, Serialize)]
struct WorkloadReport {
    records: usize,
    fixture: FixtureSummary,
    setup_ms: f64,
    operations: Vec<OperationSummary>,
    concurrent: Vec<ConcurrentSummary>,
    rss_soak: Option<RssSummary>,
    correctness: CorrectnessSummary,
}

#[derive(Debug, Serialize)]
struct FixtureSummary {
    root: String,
    task_records: usize,
    reference_records: usize,
    total_markdown_records: usize,
}

#[derive(Debug, Serialize, Clone)]
struct OperationSummary {
    name: String,
    iterations: usize,
    total_ms: f64,
    min_ms: f64,
    mean_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    ops_per_sec: f64,
    phases: BTreeMap<String, PhaseSummary>,
    counters: BTreeMap<String, f64>,
}

#[derive(Debug, Serialize, Clone)]
struct PhaseSummary {
    mean_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

#[derive(Debug, Serialize)]
struct ConcurrentSummary {
    name: String,
    collections: usize,
    threads: usize,
    requests: usize,
    elapsed_ms: f64,
    requests_per_sec: f64,
    errors: usize,
}

#[derive(Debug, Serialize)]
struct RssSummary {
    requests: usize,
    elapsed_ms: f64,
    requests_per_sec: f64,
    baseline_rss_kb: Option<u64>,
    sampled_peak_rss_kb: Option<u64>,
    after_rss_kb: Option<u64>,
    baseline_pss_kb: Option<u64>,
    sampled_peak_pss_kb: Option<u64>,
    after_pss_kb: Option<u64>,
    checkpoints: Vec<RssCheckpoint>,
    errors: usize,
}

#[derive(Debug, Serialize)]
struct RssCheckpoint {
    request: usize,
    rss_kb: Option<u64>,
    pss_kb: Option<u64>,
}

#[derive(Debug, Serialize)]
struct CorrectnessSummary {
    query_rows_observed: usize,
    pagination_rows_observed: usize,
    pagination_pages_observed: usize,
    mutation_successes: usize,
    reference_rename_success: bool,
    watcher_events_observed: usize,
    errors: Vec<String>,
}

#[derive(Debug)]
struct Fixture {
    root: PathBuf,
    task_paths: Vec<String>,
    rename_from: String,
    rename_to: String,
    reference_paths: Vec<String>,
    total_records: usize,
}

#[derive(Debug)]
struct Options {
    output_dir: PathBuf,
    record_sizes: Vec<usize>,
    mixed_threads: usize,
    mixed_rounds: usize,
    rss_requests: usize,
    seed: u64,
    keep_fixtures: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("phase0 baseline failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let options = Options::parse(std::env::args().skip(1))?;
    fs::create_dir_all(&options.output_dir)
        .map_err(|error| format!("create output directory: {error}"))?;

    let report_path = options.output_dir.join("phase0-baseline.json");
    let markdown_path = options.output_dir.join("phase0-baseline.md");
    let mdbase_commit = std::env::var("MDBASE_RS_COMMIT").unwrap_or_else(|_| "unknown".into());
    let connect_commit =
        std::env::var("MDBASE_CONNECT_COMMIT").unwrap_or_else(|_| "unknown".into());
    let parallelism = thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);

    let mut workloads = Vec::new();
    for records in &options.record_sizes {
        workloads.push(run_workload(*records, &options)?);
    }

    let report = Report {
        schema_version: 1,
        tool: "mdbase-phase0-baseline",
        generated_at: Utc::now().to_rfc3339(),
        source: SourceCommits {
            mdbase_rs: mdbase_commit,
            mdbase_connect: connect_commit,
        },
        host: HostInfo {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            parallelism,
            rss_supported: read_rss_pss().is_some(),
        },
        configuration: RunConfig {
            record_sizes: options.record_sizes.clone(),
            query_limit: 200,
            pagination_limit: 200,
            mixed_threads: options.mixed_threads,
            mixed_rounds_per_thread: options.mixed_rounds,
            rss_requests: options.rss_requests,
            seed: options.seed,
            fixtures_kept: options.keep_fixtures,
        },
        workloads,
        gaps: vec![
            "This local runner does not exercise Connect relay/admission or NATS; those remain integration evidence.".into(),
            "RSS/PSS observations use Linux /proc between requests, miss transient in-operation peaks, and are informational rather than thresholds.".into(),
            "The existing profile engine's saved-view workload is not duplicated here; query phase timings and cache refresh are retained.".into(),
        ],
    };

    let json = serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?;
    fs::write(&report_path, format!("{json}\n"))
        .map_err(|error| format!("write {}: {error}", report_path.display()))?;
    fs::write(&markdown_path, render_markdown(&report))
        .map_err(|error| format!("write {}: {error}", markdown_path.display()))?;
    println!("wrote {}", report_path.display());
    println!("wrote {}", markdown_path.display());
    Ok(())
}

impl Options {
    fn parse<I>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = String>,
    {
        let args = args.into_iter().collect::<Vec<_>>();
        let value = |name: &str| -> Option<String> {
            args.windows(2)
                .find(|pair| pair[0] == name)
                .map(|pair| pair[1].clone())
        };
        let output_dir = value("--output-dir")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target/benchmarks/phase0-baseline"));
        let record_sizes = value("--records")
            .unwrap_or_else(|| "2000,10000".into())
            .split(',')
            .map(|raw| {
                raw.parse::<usize>()
                    .map_err(|error| format!("invalid --records value '{raw}': {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if record_sizes.contains(&0) {
            return Err("--records values must be positive".into());
        }
        let mixed_threads = parse_usize(&value, "--mixed-threads", 4)?;
        let mixed_rounds = parse_usize(&value, "--mixed-rounds", 40)?;
        let rss_requests = parse_usize(&value, "--rss-requests", 1_600)?;
        let seed = value("--seed")
            .unwrap_or_else(|| "42".into())
            .parse::<u64>()
            .map_err(|error| format!("invalid --seed: {error}"))?;
        Ok(Self {
            output_dir,
            record_sizes,
            mixed_threads,
            mixed_rounds,
            rss_requests,
            seed,
            keep_fixtures: args.iter().any(|arg| arg == "--keep-fixtures"),
        })
    }
}

fn parse_usize(
    value: &impl Fn(&str) -> Option<String>,
    name: &str,
    default: usize,
) -> Result<usize, String> {
    value(name)
        .unwrap_or_else(|| default.to_string())
        .parse()
        .map_err(|error| format!("invalid {name}: {error}"))
}

fn run_workload(records: usize, options: &Options) -> Result<WorkloadReport, String> {
    let root = options
        .output_dir
        .join(format!("fixture-{records}-{}", std::process::id()));
    if root.exists() {
        fs::remove_dir_all(&root).map_err(|error| format!("remove stale fixture: {error}"))?;
    }
    let setup_started = Instant::now();
    let fixture = build_fixture(&root, records, options.seed)?;
    let setup_ms = setup_started.elapsed().as_secs_f64() * 1_000.0;
    let collection = Collection::open(&fixture.root).map_err(json_error)?;
    let mut correctness = CorrectnessSummary {
        query_rows_observed: 0,
        pagination_rows_observed: 0,
        pagination_pages_observed: 0,
        mutation_successes: 0,
        reference_rename_success: false,
        watcher_events_observed: 0,
        errors: Vec::new(),
    };
    let mut operations = vec![
        profile_open(&fixture.root, 3)?,
        profile_query_200(&collection, &mut correctness)?,
        profile_pagination(&collection, &mut correctness)?,
        profile_cache(&collection)?,
        profile_read(&collection, &fixture.task_paths, 20, options.seed)?,
        profile_update(&collection, &fixture.task_paths, 5, &mut correctness)?,
        profile_create(&collection, 5, &mut correctness)?,
        profile_delete(&collection, 5, &mut correctness)?,
        profile_rename(&collection, &fixture, &mut correctness)?,
    ];

    let (runtime_operation, runtime_sync, events) = profile_provider_and_watcher(&fixture, 5)?;
    correctness.watcher_events_observed = events;
    operations.push(runtime_operation);
    operations.push(runtime_sync);

    let provider =
        Arc::new(FilesystemProvider::open(&fixture.root).map_err(|error| error.to_string())?);
    let concurrent = vec![
        profile_mixed_concurrent(
            provider.clone(),
            &fixture.task_paths,
            options.mixed_threads,
            options.mixed_rounds,
            options.seed,
        )?,
        profile_two_collections(&fixture, records.min(2_000), options)?,
    ];
    let rss_soak = Some(profile_rss_soak(
        provider,
        &fixture.task_paths,
        options.rss_requests,
        options.seed,
    )?);

    if !options.keep_fixtures {
        fs::remove_dir_all(&fixture.root)
            .map_err(|error| format!("remove fixture {}: {error}", fixture.root.display()))?;
    }

    Ok(WorkloadReport {
        records,
        fixture: FixtureSummary {
            root: fixture.root.display().to_string(),
            task_records: fixture.task_paths.len(),
            reference_records: fixture.reference_paths.len(),
            total_markdown_records: fixture.total_records,
        },
        setup_ms,
        operations,
        concurrent,
        rss_soak,
        correctness,
    })
}

fn build_fixture(root: &Path, records: usize, seed: u64) -> Result<Fixture, String> {
    fs::create_dir_all(root.join("_types")).map_err(|error| error.to_string())?;
    fs::write(root.join("mdbase.yaml"), CONFIG).map_err(|error| error.to_string())?;
    fs::write(root.join("_types/task.md"), TASK_TYPE).map_err(|error| error.to_string())?;
    let mut task_paths = Vec::with_capacity(records);
    let rename_from = "tasks/rename-target.md".to_string();
    let rename_to = "tasks/renamed-target.md".to_string();
    for index in 0..records {
        let path = if index == 0 {
            rename_from.clone()
        } else {
            format!("tasks/task-{index:06}.md")
        };
        let fields = json!({
            "type": "task",
            "id": format!("task-{index:06}"),
            "title": format!("Synthetic task {index:06}"),
            "status": STATUS[index % STATUS.len()],
            "priority": (index % 5) + 1,
            "points": index % 13,
            "links": Vec::<String>::new(),
        });
        write_markdown(&root.join(&path), &fields)?;
        task_paths.push(path);
    }

    let reference_count = 100.min(records.max(1));
    let mut reference_paths = Vec::with_capacity(reference_count);
    for index in 0..reference_count {
        let path = format!("refs/ref-{index:04}.md");
        let target = if index % 2 == 0 {
            "tasks/rename-target.md"
        } else {
            "tasks/task-000001.md"
        };
        let fields = json!({
            "type": "task",
            "id": format!("ref-{index:04}"),
            "title": format!("Reference {index:04}"),
            "status": "open",
            "priority": 1,
            "points": 1,
            "links": [target],
        });
        write_markdown(&root.join(&path), &fields)?;
        reference_paths.push(path);
    }

    // Make the fixture's byte/layout shape deterministic without introducing a
    // random dependency into the generated Markdown.
    let _ = StdRng::seed_from_u64(seed);
    Ok(Fixture {
        root: root.to_path_buf(),
        task_paths,
        rename_from,
        rename_to,
        reference_paths,
        total_records: records + reference_count,
    })
}

fn write_markdown(path: &Path, fields: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let map = mdbase::frontmatter::parser::json_to_yaml_mapping(fields);
    let content = mdbase::frontmatter::serializer::serialize_document(&map, "Synthetic body.\n")
        .map_err(|error| format!("serialize {}: {error}", path.display()))?;
    fs::write(path, content).map_err(|error| format!("write {}: {error}", path.display()))
}

fn profile_open(root: &Path, iterations: usize) -> Result<OperationSummary, String> {
    timed("open", iterations, |_| {
        Collection::open(root).map(|_| ()).map_err(json_error)
    })
}

fn profile_read(
    collection: &Collection,
    task_paths: &[String],
    iterations: usize,
    seed: u64,
) -> Result<OperationSummary, String> {
    let mut rng = StdRng::seed_from_u64(seed);
    timed("read", iterations, |_| {
        let path = &task_paths[rng.gen_range(0..task_paths.len())];
        collection
            .typed()
            .and_then(|typed| typed.read(mdbase::api::ReadRequest::new(path)?))
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
}

fn profile_query_200(
    collection: &Collection,
    correctness: &mut CorrectnessSummary,
) -> Result<OperationSummary, String> {
    let operations = collection.v03_operations().map_err(|error| error.message)?;
    let query = json!({
        "order_by": [{"field": "file.path", "direction": "asc"}],
        "limit": 200,
        "include_body": false,
    });
    let mut profiles = Vec::new();
    let mut samples = Vec::new();
    for _ in 0..3 {
        let started = Instant::now();
        let (result, profile) = operations.query_profiled(&query);
        ensure_v03_success(&result)?;
        correctness.query_rows_observed = result.result["results"].as_array().map_or(0, Vec::len);
        samples.push(started.elapsed().as_secs_f64() * 1_000.0);
        profiles.push(profile);
    }
    Ok(with_query_phases("query_200", samples, profiles))
}

fn profile_pagination(
    collection: &Collection,
    correctness: &mut CorrectnessSummary,
) -> Result<OperationSummary, String> {
    let operations = collection.v03_operations().map_err(|error| error.message)?;
    let mut samples = Vec::new();
    let mut page_counts = Vec::new();
    let mut row_counts = Vec::new();
    for _ in 0..2 {
        let started = Instant::now();
        let mut offset = 0_u64;
        let mut pages = 0;
        let mut rows = 0;
        loop {
            let (result, _) = operations.query_profiled(&json!({
                "order_by": [{"field": "file.path", "direction": "asc"}],
                "limit": 200,
                "offset": offset,
                "include_body": false,
            }));
            ensure_v03_success(&result)?;
            let page = result.result["results"]
                .as_array()
                .ok_or("pagination result missing results")?;
            pages += 1;
            rows += page.len();
            offset += page.len() as u64;
            if !result.result["meta"]["has_more"].as_bool().unwrap_or(false) || page.is_empty() {
                break;
            }
        }
        samples.push(started.elapsed().as_secs_f64() * 1_000.0);
        page_counts.push(pages);
        row_counts.push(rows);
    }
    correctness.pagination_pages_observed = *page_counts.last().unwrap_or(&0);
    correctness.pagination_rows_observed = *row_counts.last().unwrap_or(&0);
    let mut summary = summarize("pagination_200_sequential", samples);
    summary
        .counters
        .insert("pages".into(), mean_usize(&page_counts));
    summary
        .counters
        .insert("rows".into(), mean_usize(&row_counts));
    Ok(summary)
}

fn profile_cache(collection: &Collection) -> Result<OperationSummary, String> {
    let mut samples = Vec::new();
    for _ in 0..2 {
        let started = Instant::now();
        let result = collection.cache_rebuild();
        ensure_json_success(&result)?;
        samples.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    let mut summary = summarize("cache_rebuild", samples);
    let operations = collection.v03_operations().map_err(|error| error.message)?;
    let (result, profile) = operations.query_profiled(&json!({
        "order_by": [{"field": "file.path", "direction": "asc"}],
        "limit": 200,
        "include_body": false,
    }));
    ensure_v03_success(&result)?;
    summary.phases.insert(
        "cache_refresh_probe".into(),
        PhaseSummary {
            mean_ms: profile.cache_refresh_us as f64 / 1_000.0,
            p95_ms: profile.cache_refresh_us as f64 / 1_000.0,
            max_ms: profile.cache_refresh_us as f64 / 1_000.0,
        },
    );
    Ok(summary)
}

fn profile_update(
    collection: &Collection,
    task_paths: &[String],
    iterations: usize,
    correctness: &mut CorrectnessSummary,
) -> Result<OperationSummary, String> {
    timed("update", iterations, |index| {
        let path = &task_paths[(index + 1) % task_paths.len()];
        let result = collection.update(&json!({
            "path": path,
            "fields": {"status": STATUS[index % STATUS.len()], "points": (index % 13) as i64},
        }));
        ensure_json_success(&result)?;
        correctness.mutation_successes += 1;
        Ok(())
    })
}

fn profile_create(
    collection: &Collection,
    iterations: usize,
    correctness: &mut CorrectnessSummary,
) -> Result<OperationSummary, String> {
    timed("create", iterations, |index| {
        let result = collection.create(&json!({
            "path": format!("scratch/create-{index:04}.md"),
            "type": "task",
            "fields": {"title": format!("Created {index}"), "status": "open", "priority": 2, "points": 1},
        }));
        ensure_json_success(&result)?;
        correctness.mutation_successes += 1;
        Ok(())
    })
}

fn profile_delete(
    collection: &Collection,
    iterations: usize,
    correctness: &mut CorrectnessSummary,
) -> Result<OperationSummary, String> {
    let paths = (0..iterations)
        .map(|index| format!("scratch/delete-{index:04}.md"))
        .collect::<Vec<_>>();
    for (index, path) in paths.iter().enumerate() {
        let result = collection.create(&json!({
            "path": path,
            "type": "task",
            "fields": {"title": format!("Delete {index}"), "status": "open", "priority": 2, "points": 1},
        }));
        ensure_json_success(&result)?;
    }
    timed("delete", iterations, |index| {
        let result = collection.delete(&json!({"path": paths[index]}));
        ensure_json_success(&result)?;
        correctness.mutation_successes += 1;
        Ok(())
    })
}

fn profile_rename(
    collection: &Collection,
    fixture: &Fixture,
    correctness: &mut CorrectnessSummary,
) -> Result<OperationSummary, String> {
    let result = collection.rename(&json!({
        "from": fixture.rename_from,
        "to": fixture.rename_to,
        "update_refs": true,
    }));
    ensure_json_success(&result)?;
    correctness.mutation_successes += 1;
    let reference_contents = fixture
        .reference_paths
        .iter()
        .filter_map(|path| fs::read_to_string(fixture.root.join(path)).ok())
        .collect::<Vec<_>>();
    let rewritten = reference_contents
        .iter()
        .filter(|content| content.contains("renamed-target.md"))
        .count();
    let stale = reference_contents
        .iter()
        .filter(|content| content.contains("../tasks/rename-target.md"))
        .count();
    correctness.reference_rename_success = rewritten > 0 && stale == 0;

    // The first rename is timed separately so its setup and semantic check do not
    // hide the measured operation. Toggle back and forth for additional samples.
    let mut samples = Vec::new();
    for (from, to) in [
        (&fixture.rename_to, &fixture.rename_from),
        (&fixture.rename_from, &fixture.rename_to),
    ] {
        let started = Instant::now();
        let result = collection.rename(&json!({"from": from, "to": to, "update_refs": true}));
        ensure_json_success(&result)?;
        samples.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    Ok(summarize("rename_update_refs", samples))
}

fn profile_provider_and_watcher(
    fixture: &Fixture,
    iterations: usize,
) -> Result<(OperationSummary, OperationSummary, usize), String> {
    let provider = FilesystemProvider::open(&fixture.root).map_err(|error| error.to_string())?;
    let watcher = CollectionWatcher::open(&fixture.root, Duration::from_millis(20))
        .map_err(|error| error.to_string())?;
    let mut operation_samples = Vec::new();
    let mut sync_samples = Vec::new();
    let mut events = 0;
    for index in 0..iterations {
        let path = fixture.task_paths[(index + 2) % fixture.task_paths.len()].clone();
        let request = OperationRequest::new(
            OperationKind::Update,
            json!({"path": path, "fields": {"points": ((index + 3) % 13) as i64}}),
        );
        let started = Instant::now();
        let result = provider
            .execute(&request)
            .map_err(|error| error.to_string())?;
        ensure_v03_success(&result)?;
        operation_samples.push(started.elapsed().as_secs_f64() * 1_000.0);
        let started = Instant::now();
        watcher
            .rescan_paths([request.input["path"].as_str().unwrap_or_default()])
            .map_err(|error| error.to_string())?;
        while watcher
            .recv_timeout(Duration::ZERO)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            events += 1;
        }
        sync_samples.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    Ok((
        summarize("provider_mutation", operation_samples),
        summarize("watcher_synchronization", sync_samples),
        events,
    ))
}

fn profile_mixed_concurrent(
    provider: Arc<FilesystemProvider>,
    task_paths: &[String],
    threads: usize,
    rounds: usize,
    seed: u64,
) -> Result<ConcurrentSummary, String> {
    let started = Instant::now();
    let mut handles = Vec::new();
    for thread_id in 0..threads.max(1) {
        let provider = provider.clone();
        let paths = task_paths.to_vec();
        handles.push(thread::spawn(move || {
            let mut rng = StdRng::seed_from_u64(seed.wrapping_add(thread_id as u64));
            let mut errors = 0;
            for round in 0..rounds {
                let path = paths[rng.gen_range(0..paths.len())].clone();
                let (kind, input) = match round % 5 {
                    0 => (OperationKind::Read, json!({"path": path})),
                    1 => (
                        OperationKind::Query,
                        json!({"order_by": [{"field": "file.path", "direction": "asc"}], "limit": 20}),
                    ),
                    _ => (
                        OperationKind::Update,
                        json!({"path": path, "fields": {"points": (round % 13) as i64}}),
                    ),
                };
                match provider.execute(&OperationRequest::new(kind, input)) {
                    Ok(result) if result.valid => {}
                    Ok(_) | Err(_) => errors += 1,
                }
            }
            errors
        }));
    }
    let errors = handles
        .into_iter()
        .map(|handle| handle.join().unwrap_or(1))
        .sum::<usize>();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let requests = threads.max(1) * rounds;
    Ok(ConcurrentSummary {
        name: "mixed_concurrent_provider_work".into(),
        collections: 1,
        threads: threads.max(1),
        requests,
        elapsed_ms,
        requests_per_sec: requests as f64 / (elapsed_ms / 1_000.0).max(f64::MIN_POSITIVE),
        errors,
    })
}

fn profile_two_collections(
    primary: &Fixture,
    secondary_records: usize,
    options: &Options,
) -> Result<ConcurrentSummary, String> {
    let secondary_root = options.output_dir.join(format!(
        "fixture-two-{}-{}",
        primary.total_records,
        std::process::id()
    ));
    let secondary = build_fixture(
        &secondary_root,
        secondary_records,
        options.seed.wrapping_add(20_000),
    )?;
    let primary_provider =
        Arc::new(FilesystemProvider::open(&primary.root).map_err(|error| error.to_string())?);
    let secondary_provider =
        Arc::new(FilesystemProvider::open(&secondary.root).map_err(|error| error.to_string())?);
    let started = Instant::now();
    let mut handles = Vec::new();
    for thread_id in 0..options.mixed_threads.max(1) {
        let provider = if thread_id % 2 == 0 {
            primary_provider.clone()
        } else {
            secondary_provider.clone()
        };
        let paths = if thread_id % 2 == 0 {
            primary.task_paths.clone()
        } else {
            secondary.task_paths.clone()
        };
        let rounds = options.mixed_rounds;
        let seed = options.seed.wrapping_add(30_000 + thread_id as u64);
        handles.push(thread::spawn(move || {
            let mut rng = StdRng::seed_from_u64(seed);
            let mut errors = 0;
            for round in 0..rounds {
                let path = paths[rng.gen_range(0..paths.len())].clone();
                let (kind, input) = if round % 3 == 0 {
                    (
                        OperationKind::Query,
                        json!({"order_by": [{"field": "file.path", "direction": "asc"}], "limit": 20}),
                    )
                } else {
                    (OperationKind::Read, json!({"path": path}))
                };
                match provider.execute(&OperationRequest::new(kind, input)) {
                    Ok(result) if result.valid => {}
                    Ok(_) | Err(_) => errors += 1,
                }
            }
            errors
        }));
    }
    let errors = handles
        .into_iter()
        .map(|handle| handle.join().unwrap_or(1))
        .sum::<usize>();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    if !options.keep_fixtures {
        fs::remove_dir_all(&secondary.root)
            .map_err(|error| format!("remove secondary fixture: {error}"))?;
    }
    let requests = options.mixed_threads.max(1) * options.mixed_rounds;
    Ok(ConcurrentSummary {
        name: "two_active_collections_mixed_work".into(),
        collections: 2,
        threads: options.mixed_threads.max(1),
        requests,
        elapsed_ms,
        requests_per_sec: requests as f64 / (elapsed_ms / 1_000.0).max(f64::MIN_POSITIVE),
        errors,
    })
}

fn profile_rss_soak(
    provider: Arc<FilesystemProvider>,
    task_paths: &[String],
    requests: usize,
    seed: u64,
) -> Result<RssSummary, String> {
    let baseline = read_rss_pss();
    let mut peak = baseline;
    let mut checkpoints = Vec::new();
    let started = Instant::now();
    let mut rng = StdRng::seed_from_u64(seed.wrapping_add(10_000));
    let mut errors = 0;
    for index in 0..requests {
        let path = task_paths[rng.gen_range(0..task_paths.len())].clone();
        let (kind, input) = if index % 5 == 0 {
            (
                OperationKind::Query,
                json!({"order_by": [{"field": "file.path", "direction": "asc"}], "limit": 200}),
            )
        } else {
            (OperationKind::Read, json!({"path": path}))
        };
        match provider.execute(&OperationRequest::new(kind, input)) {
            Ok(result) if result.valid => {}
            Ok(_) | Err(_) => errors += 1,
        }
        let current = read_rss_pss();
        peak = max_rss(peak, current);
        if index == 0 || (index + 1) % 200 == 0 || index + 1 == requests {
            checkpoints.push(RssCheckpoint {
                request: index + 1,
                rss_kb: current.map(|value| value.0),
                pss_kb: current.map(|value| value.1),
            });
        }
    }
    let after = read_rss_pss();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    Ok(RssSummary {
        requests,
        elapsed_ms,
        requests_per_sec: requests as f64 / (elapsed_ms / 1_000.0).max(f64::MIN_POSITIVE),
        baseline_rss_kb: baseline.map(|value| value.0),
        sampled_peak_rss_kb: peak.map(|value| value.0),
        after_rss_kb: after.map(|value| value.0),
        baseline_pss_kb: baseline.map(|value| value.1),
        sampled_peak_pss_kb: peak.map(|value| value.1),
        after_pss_kb: after.map(|value| value.1),
        checkpoints,
        errors,
    })
}

fn timed<F>(name: &str, iterations: usize, mut operation: F) -> Result<OperationSummary, String>
where
    F: FnMut(usize) -> Result<(), String>,
{
    let mut samples = Vec::with_capacity(iterations);
    for index in 0..iterations {
        let started = Instant::now();
        operation(index).map_err(|error| format!("{name} iteration {index}: {error}"))?;
        samples.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    Ok(summarize(name, samples))
}

fn summarize(name: &str, mut samples: Vec<f64>) -> OperationSummary {
    samples.sort_by(|left, right| left.total_cmp(right));
    let total_ms = samples.iter().sum::<f64>();
    let mean_ms = if samples.is_empty() {
        0.0
    } else {
        total_ms / samples.len() as f64
    };
    OperationSummary {
        name: name.into(),
        iterations: samples.len(),
        total_ms,
        min_ms: samples.first().copied().unwrap_or_default(),
        mean_ms,
        p50_ms: percentile(&samples, 0.50),
        p95_ms: percentile(&samples, 0.95),
        p99_ms: percentile(&samples, 0.99),
        max_ms: samples.last().copied().unwrap_or_default(),
        ops_per_sec: if mean_ms > 0.0 {
            1_000.0 / mean_ms
        } else {
            0.0
        },
        phases: BTreeMap::new(),
        counters: BTreeMap::new(),
    }
}

fn with_query_phases(
    name: &str,
    samples: Vec<f64>,
    profiles: Vec<mdbase::v03::QueryPerformance>,
) -> OperationSummary {
    let mut summary = summarize(name, samples);
    for (phase, values) in query_phases(&profiles) {
        summary.phases.insert(phase, summarize_phase(values));
    }
    summary
}

type QueryPhaseField = fn(&mdbase::v03::QueryPerformance) -> u64;

fn query_phases(profiles: &[mdbase::v03::QueryPerformance]) -> BTreeMap<String, Vec<f64>> {
    let mut phases = BTreeMap::new();
    let fields: [(&str, QueryPhaseField); 8] = [
        ("schema", |profile| profile.schema_us),
        ("cache_refresh", |profile| profile.cache_refresh_us),
        ("records_load", |profile| profile.records_load_us),
        ("all_files", |profile| profile.all_files_us),
        ("link_graph", |profile| profile.link_graph_us),
        ("evaluate", |profile| profile.evaluate_us),
        ("sort", |profile| profile.sort_us),
        ("serialize", |profile| profile.serialize_us),
    ];
    for (name, field) in fields {
        phases.insert(
            name.into(),
            profiles
                .iter()
                .map(|profile| field(profile) as f64 / 1_000.0)
                .collect(),
        );
    }
    phases
}

fn summarize_phase(mut samples: Vec<f64>) -> PhaseSummary {
    samples.sort_by(|left, right| left.total_cmp(right));
    PhaseSummary {
        mean_ms: samples.iter().sum::<f64>() / samples.len().max(1) as f64,
        p95_ms: percentile(&samples, 0.95),
        max_ms: samples.last().copied().unwrap_or_default(),
    }
}

fn percentile(samples: &[f64], percentile: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let position = percentile.clamp(0.0, 1.0) * (samples.len() - 1) as f64;
    let low = position.floor() as usize;
    let high = position.ceil() as usize;
    if low == high {
        samples[low]
    } else {
        samples[low] + (samples[high] - samples[low]) * (position - low as f64)
    }
}

fn mean_usize(values: &[usize]) -> f64 {
    values.iter().sum::<usize>() as f64 / values.len().max(1) as f64
}

fn ensure_json_success(result: &Value) -> Result<(), String> {
    if result.get("error").is_some() {
        Err(result.to_string())
    } else {
        Ok(())
    }
}

fn ensure_v03_success(result: &mdbase::v03::OperationResult) -> Result<(), String> {
    if result.valid {
        Ok(())
    } else {
        Err(result
            .diagnostics
            .iter()
            .map(|diagnostic| format!("{}: {}", diagnostic.code, diagnostic.message))
            .collect::<Vec<_>>()
            .join("; "))
    }
}

fn json_error(value: Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| value.to_string())
}

fn read_rss_pss() -> Option<(u64, u64)> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let rss = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .and_then(parse_kb_line)?;
    let pss = fs::read_to_string("/proc/self/smaps_rollup")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|line| line.starts_with("Pss:"))
                .and_then(parse_kb_line)
        })
        .unwrap_or(rss);
    Some((rss, pss))
}

fn parse_kb_line(line: &str) -> Option<u64> {
    line.split_whitespace().nth(1)?.parse().ok()
}

fn max_rss(left: Option<(u64, u64)>, right: Option<(u64, u64)>) -> Option<(u64, u64)> {
    match (left, right) {
        (Some(left), Some(right)) => Some((left.0.max(right.0), left.1.max(right.1))),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn render_markdown(report: &Report) -> String {
    let mut output = String::new();
    output.push_str("# Phase 0 collection runtime baseline observations\n\n");
    output.push_str(
        "Informational release-mode observations; no latency or RSS thresholds are enforced.\n\n",
    );
    output.push_str(&format!(
        "- Generated: `{}`\n- mdbase-rs: `{}`\n- mdbase-connect: `{}`\n- Host: `{}/{}`, {} logical CPUs\n\n",
        report.generated_at,
        report.source.mdbase_rs,
        report.source.mdbase_connect,
        report.host.os,
        report.host.arch,
        report.host.parallelism
    ));
    for workload in &report.workloads {
        output.push_str(&format!(
            "## {} synthetic records\n\nSetup: {:.2} ms; total Markdown records including references: {}.\n\n",
            workload.records, workload.setup_ms, workload.fixture.total_markdown_records
        ));
        output.push_str("| Operation | Runs | Mean ms | P95 ms | Max ms | Ops/s |\n|---|---:|---:|---:|---:|---:|\n");
        for operation in &workload.operations {
            output.push_str(&format!(
                "| `{}` | {} | {:.2} | {:.2} | {:.2} | {:.1} |\n",
                operation.name,
                operation.iterations,
                operation.mean_ms,
                operation.p95_ms,
                operation.max_ms,
                operation.ops_per_sec
            ));
        }
        output.push('\n');
        for concurrent in &workload.concurrent {
            output.push_str(&format!(
                "- Concurrent `{}`: {} collections, {} threads, {} requests, {:.2} ms, {:.1} req/s, {} errors.\n",
                concurrent.name,
                concurrent.collections,
                concurrent.threads,
                concurrent.requests,
                concurrent.elapsed_ms,
                concurrent.requests_per_sec,
                concurrent.errors
            ));
        }
        if let Some(rss) = &workload.rss_soak {
            output.push_str(&format!(
                "- RSS soak: {} requests, {:.2} ms, baseline/sampled-between-request peak/after RSS {:?}/{:?}/{:?} KiB; baseline/sampled-between-request peak/after PSS {:?}/{:?}/{:?} KiB; {} errors.\n",
                rss.requests,
                rss.elapsed_ms,
                rss.baseline_rss_kb,
                rss.sampled_peak_rss_kb,
                rss.after_rss_kb,
                rss.baseline_pss_kb,
                rss.sampled_peak_pss_kb,
                rss.after_pss_kb,
                rss.errors
            ));
        }
        output.push_str(&format!(
            "- Correctness: query rows {}, paginated rows/pages {}/{}, mutation successes {}, reference rename {}, watcher events {}.\n\n",
            workload.correctness.query_rows_observed,
            workload.correctness.pagination_rows_observed,
            workload.correctness.pagination_pages_observed,
            workload.correctness.mutation_successes,
            workload.correctness.reference_rename_success,
            workload.correctness.watcher_events_observed
        ));
        for operation in &workload.operations {
            for (phase, timing) in &operation.phases {
                output.push_str(&format!(
                    "  - `{}` `{}` phase: mean {:.3} ms, p95 {:.3} ms, max {:.3} ms.\n",
                    operation.name, phase, timing.mean_ms, timing.p95_ms, timing.max_ms
                ));
            }
        }
        output.push('\n');
    }
    output.push_str("## Gaps and interpretation\n\n");
    for gap in &report.gaps {
        output.push_str(&format!("- {}\n", gap));
    }
    output
}
