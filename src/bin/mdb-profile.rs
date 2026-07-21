use clap::Parser;
use mdbase::frontmatter::parser::json_to_yaml_mapping;
use mdbase::frontmatter::serializer::serialize_document;
use mdbase::Collection;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::Serialize;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

const TASK_TYPE_DEF: &str = r#"---
kind: mdbase.type
name: task
version: 1
description: Synthetic task type for profiling
match:
  path_glob: ["tasks/*.md", "scratch/*.md"]
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
      project: { type: string }
      tags:
        type: array
        items: { type: string }
collection:
  read_defaults:
    status: open
    priority: 3
    points: 1
  links:
    project:
      target_type: project
      validate_exists: true
---

# Task
"#;

const PROJECT_TYPE_DEF: &str = r#"---
kind: mdbase.type
name: project
version: 1
description: Synthetic project type for profiling
match:
  path_glob: "projects/*.md"
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    required: [type, id, title]
    additionalProperties: false
    properties:
      type: { const: project }
      id: { type: string }
      title: { type: string, minLength: 1 }
---

# Project
"#;

const STATUS_CYCLE: [&str; 3] = ["open", "in-progress", "done"];

#[derive(Debug, Parser)]
#[command(
    name = "mdb-profile",
    version,
    about = "Run repeatable performance profiling for core mdbase operations"
)]
struct Args {
    /// Number of task files in the synthetic fixture
    #[arg(long, default_value_t = 2000)]
    files: usize,

    /// Number of project files in the synthetic fixture
    #[arg(long, default_value_t = 80)]
    projects: usize,

    /// Number of files containing references for rename/update_refs profiling
    #[arg(long, default_value_t = 100)]
    rename_refs: usize,

    /// Iterations for Collection::open
    #[arg(long = "open-iters", default_value_t = 20)]
    open_iterations: usize,

    /// Iterations for read operations
    #[arg(long = "read-iters", default_value_t = 1000)]
    read_iterations: usize,

    /// Iterations for query operations
    #[arg(long = "query-iters", default_value_t = 250)]
    query_iterations: usize,

    /// Iterations for update operations
    #[arg(long = "update-iters", default_value_t = 500)]
    update_iterations: usize,

    /// Iterations for rename operations
    #[arg(long = "rename-iters", default_value_t = 50)]
    rename_iterations: usize,

    /// Iterations for create operations
    #[arg(long = "create-iters", default_value_t = 300)]
    create_iterations: usize,

    /// Iterations for delete operations
    #[arg(long = "delete-iters", default_value_t = 300)]
    delete_iterations: usize,

    /// Iterations for cache rebuild operations
    #[arg(long = "cache-rebuild-iters", default_value_t = 5)]
    cache_rebuild_iterations: usize,

    /// Deterministic RNG seed for path selection
    #[arg(long, default_value_t = 42)]
    seed: u64,

    /// Optional fixture root path; defaults to `/tmp/mdbase-profile-<timestamp>-<pid>`.
    #[arg(long)]
    fixture_root: Option<PathBuf>,

    /// Keep fixture data after profiling
    #[arg(long)]
    keep_fixture: bool,

    /// Optional output path for JSON report (stdout if omitted)
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct ProfileConfig {
    files: usize,
    projects: usize,
    rename_refs: usize,
    open_iterations: usize,
    read_iterations: usize,
    query_iterations: usize,
    update_iterations: usize,
    rename_iterations: usize,
    create_iterations: usize,
    delete_iterations: usize,
    cache_rebuild_iterations: usize,
    seed: u64,
}

#[derive(Debug, Serialize)]
struct FixtureSummary {
    root: String,
    kept: bool,
    task_files: usize,
    project_files: usize,
    rename_reference_files: usize,
}

#[derive(Debug, Serialize)]
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
    stddev_ms: f64,
    ops_per_sec: f64,
}

#[derive(Debug, Serialize)]
struct ProfileReport {
    tool: &'static str,
    version: &'static str,
    generated_at: String,
    total_runtime_ms: f64,
    config: ProfileConfig,
    fixture: FixtureSummary,
    operations: Vec<OperationSummary>,
}

struct FixtureData {
    root: PathBuf,
    task_paths: Vec<String>,
    rename_source_a: String,
    rename_source_b: String,
}

struct FixtureCleanup {
    root: PathBuf,
    keep: bool,
}

impl Drop for FixtureCleanup {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse();
    validate_args(&args)?;

    let fixture_root = determine_fixture_root(&args);
    if fixture_root.exists() {
        return Err(format!(
            "Fixture root already exists: {}",
            fixture_root.display()
        ));
    }

    let fixture = build_fixture(&fixture_root, &args)?;
    let _cleanup = FixtureCleanup {
        root: fixture.root.clone(),
        keep: args.keep_fixture,
    };

    let run_start = Instant::now();
    let generated_at = chrono::Utc::now().to_rfc3339();

    let mut operations = Vec::new();
    operations.push(profile_open(&fixture.root, args.open_iterations)?);

    let collection = Collection::open(&fixture.root).map_err(format_json_error)?;

    operations.push(profile_read(
        &collection,
        &fixture.task_paths,
        args.read_iterations,
        args.seed,
    )?);
    operations.push(profile_query_basic(&collection, args.query_iterations)?);
    operations.push(profile_query_formula(&collection, args.query_iterations)?);
    operations.push(profile_update(
        &collection,
        &fixture.task_paths,
        args.update_iterations,
        args.seed.wrapping_add(1),
    )?);
    operations.push(profile_rename(
        &collection,
        &fixture.rename_source_a,
        &fixture.rename_source_b,
        args.rename_iterations,
    )?);
    operations.push(profile_create(&collection, args.create_iterations)?);
    operations.push(profile_delete(&collection, args.delete_iterations)?);
    operations.push(profile_cache_rebuild(
        &collection,
        args.cache_rebuild_iterations,
    )?);

    let report = ProfileReport {
        tool: "mdbase-profiler",
        version: env!("CARGO_PKG_VERSION"),
        generated_at,
        total_runtime_ms: run_start.elapsed().as_secs_f64() * 1000.0,
        config: ProfileConfig {
            files: args.files,
            projects: args.projects,
            rename_refs: args.rename_refs,
            open_iterations: args.open_iterations,
            read_iterations: args.read_iterations,
            query_iterations: args.query_iterations,
            update_iterations: args.update_iterations,
            rename_iterations: args.rename_iterations,
            create_iterations: args.create_iterations,
            delete_iterations: args.delete_iterations,
            cache_rebuild_iterations: args.cache_rebuild_iterations,
            seed: args.seed,
        },
        fixture: FixtureSummary {
            root: fixture.root.to_string_lossy().to_string(),
            kept: args.keep_fixture,
            task_files: args.files + 1,
            project_files: args.projects,
            rename_reference_files: args.rename_refs,
        },
        operations,
    };

    let report_json = serde_json::to_string_pretty(&report)
        .map_err(|e| format!("Failed to serialize report JSON: {e}"))?;

    if let Some(output) = &args.output {
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create output directory {:?}: {e}", parent))?;
        }
        fs::write(output, format!("{report_json}\n"))
            .map_err(|e| format!("Failed to write report to {}: {e}", output.display()))?;
    } else {
        println!("{report_json}");
    }

    Ok(())
}

fn validate_args(args: &Args) -> Result<(), String> {
    if args.files == 0 {
        return Err("--files must be greater than 0".to_string());
    }
    if args.projects == 0 {
        return Err("--projects must be greater than 0".to_string());
    }
    Ok(())
}

fn determine_fixture_root(args: &Args) -> PathBuf {
    if let Some(root) = &args.fixture_root {
        return root.clone();
    }
    std::env::temp_dir().join(format!(
        "mdbase-profile-{}-{}",
        chrono::Utc::now().timestamp_millis(),
        std::process::id()
    ))
}

fn build_fixture(root: &Path, args: &Args) -> Result<FixtureData, String> {
    fs::create_dir_all(root)
        .map_err(|e| format!("Failed to create fixture root {}: {e}", root.display()))?;
    fs::create_dir_all(root.join("_types"))
        .map_err(|e| format!("Failed to create _types folder in {}: {e}", root.display()))?;

    let config = r#"spec_version: "0.3.0"
name: "Profiler"
settings:
  types_folder: "_types"
  validation: "error"
  exclude:
    - "_types"
"#;
    fs::write(root.join("mdbase.yaml"), config)
        .map_err(|e| format!("Failed to write mdbase.yaml: {e}"))?;
    fs::write(root.join("_types/task.md"), TASK_TYPE_DEF)
        .map_err(|e| format!("Failed to write _types/task.md: {e}"))?;
    fs::write(root.join("_types/project.md"), PROJECT_TYPE_DEF)
        .map_err(|e| format!("Failed to write _types/project.md: {e}"))?;

    let mut project_ids = Vec::with_capacity(args.projects);
    for i in 0..args.projects {
        let project_id = format!("project-{i:04}");
        let project_path = root.join(format!("projects/{project_id}.md"));
        let fm = json!({
            "type": "project",
            "id": project_id,
            "title": format!("Project {i:04}"),
        });
        write_markdown_file(&project_path, &fm, "Synthetic project for profiler.\n")?;
        project_ids.push(format!("project-{i:04}"));
    }

    let mut task_paths = Vec::with_capacity(args.files);
    for i in 0..args.files {
        let rel_path = format!("tasks/task-{i:06}.md");
        let project = &project_ids[i % project_ids.len()];
        let status = STATUS_CYCLE[i % STATUS_CYCLE.len()];
        let priority = ((i % 5) + 1) as i64;
        let points = (i % 13) as i64;
        let fm = json!({
            "type": "task",
            "id": format!("task-{i:06}"),
            "title": format!("Task {i:06}"),
            "status": status,
            "priority": priority,
            "points": points,
            "project": format!("[[{project}]]"),
            "tags": [format!("team-{}", i % 10), "profile"],
        });
        let body = format!(
            "# Task {i:06}\n\nRelated project: [[{project}]]\n\nSynthetic profiling content.\n"
        );
        write_markdown_file(&root.join(&rel_path), &fm, &body)?;
        task_paths.push(rel_path);
    }

    let rename_source_a = "tasks/rename-target-a.md".to_string();
    let rename_source_b = "tasks/rename-target-b.md".to_string();
    write_markdown_file(
        &root.join(&rename_source_a),
        &json!({
            "type": "task",
            "id": "rename-target",
            "title": "Rename Target",
            "status": "open",
            "priority": 3,
        }),
        "This file is renamed repeatedly during profiling.\n",
    )?;
    task_paths.push(rename_source_a.clone());

    for i in 0..args.rename_refs {
        let ref_path = root.join(format!("refs/ref-{i:04}.md"));
        let body = format!(
            "Reference {i:04}: [[rename-target-a]] and [markdown](../tasks/rename-target-a.md)\n"
        );
        write_plain_markdown_file(&ref_path, &body)?;
    }

    Ok(FixtureData {
        root: root.to_path_buf(),
        task_paths,
        rename_source_a,
        rename_source_b,
    })
}

fn write_markdown_file(
    path: &Path,
    frontmatter: &serde_json::Value,
    body: &str,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create parent folder {}: {e}", parent.display()))?;
    }
    let yaml_mapping = json_to_yaml_mapping(frontmatter);
    let content = serialize_document(&yaml_mapping, body);
    fs::write(path, content).map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

fn write_plain_markdown_file(path: &Path, body: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create parent folder {}: {e}", parent.display()))?;
    }
    fs::write(path, body).map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

fn run_timed<F>(name: &str, iterations: usize, mut operation: F) -> Result<OperationSummary, String>
where
    F: FnMut(usize) -> Result<(), String>,
{
    let mut samples = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let start = Instant::now();
        operation(i).map_err(|e| format!("{name} iteration {i} failed: {e}"))?;
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(summarize(name, samples))
}

fn summarize(name: &str, mut samples: Vec<f64>) -> OperationSummary {
    if samples.is_empty() {
        return OperationSummary {
            name: name.to_string(),
            iterations: 0,
            total_ms: 0.0,
            min_ms: 0.0,
            mean_ms: 0.0,
            p50_ms: 0.0,
            p95_ms: 0.0,
            p99_ms: 0.0,
            max_ms: 0.0,
            stddev_ms: 0.0,
            ops_per_sec: 0.0,
        };
    }

    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let total_ms: f64 = samples.iter().sum();
    let mean_ms = total_ms / samples.len() as f64;
    let variance = samples
        .iter()
        .map(|sample| {
            let delta = sample - mean_ms;
            delta * delta
        })
        .sum::<f64>()
        / samples.len() as f64;
    let stddev_ms = variance.sqrt();

    let min_ms = samples[0];
    let max_ms = samples[samples.len() - 1];
    let p50_ms = percentile(&samples, 0.50);
    let p95_ms = percentile(&samples, 0.95);
    let p99_ms = percentile(&samples, 0.99);
    let ops_per_sec = if mean_ms > 0.0 { 1000.0 / mean_ms } else { 0.0 };

    OperationSummary {
        name: name.to_string(),
        iterations: samples.len(),
        total_ms,
        min_ms,
        mean_ms,
        p50_ms,
        p95_ms,
        p99_ms,
        max_ms,
        stddev_ms,
        ops_per_sec,
    }
}

fn percentile(sorted_samples: &[f64], percentile: f64) -> f64 {
    if sorted_samples.len() == 1 {
        return sorted_samples[0];
    }
    let clamped = percentile.clamp(0.0, 1.0);
    let pos = clamped * (sorted_samples.len() as f64 - 1.0);
    let lower = pos.floor() as usize;
    let upper = pos.ceil() as usize;
    if lower == upper {
        return sorted_samples[lower];
    }
    let weight = pos - lower as f64;
    sorted_samples[lower] * (1.0 - weight) + sorted_samples[upper] * weight
}

fn ensure_success(result: &serde_json::Value) -> Result<(), String> {
    if let Some(err) = result.get("error") {
        return Err(err.to_string());
    }
    Ok(())
}

fn format_json_error(value: serde_json::Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| value.to_string())
}

fn profile_open(root: &Path, iterations: usize) -> Result<OperationSummary, String> {
    run_timed("open", iterations, |_| {
        Collection::open(root)
            .map(|_| ())
            .map_err(format_json_error)
    })
}

fn profile_read(
    collection: &Collection,
    task_paths: &[String],
    iterations: usize,
    seed: u64,
) -> Result<OperationSummary, String> {
    let mut rng = StdRng::seed_from_u64(seed);
    let picks: Vec<usize> = (0..iterations)
        .map(|_| rng.gen_range(0..task_paths.len()))
        .collect();

    run_timed("read", iterations, |i| {
        let path = &task_paths[picks[i]];
        let result = collection.read(&json!({ "path": path }));
        ensure_success(&result)
    })
}

fn profile_query_basic(
    collection: &Collection,
    iterations: usize,
) -> Result<OperationSummary, String> {
    let query = json!({
        "query": {
            "types": ["task"],
            "where": "priority >= 3 && status != \"done\"",
            "order_by": [
                {"field": "priority", "direction": "desc"},
                {"field": "points", "direction": "asc"}
            ],
            "limit": 120
        }
    });

    run_timed("query_basic", iterations, |_| {
        let result = collection.query(&query);
        ensure_success(&result)
    })
}

fn profile_query_formula(
    collection: &Collection,
    iterations: usize,
) -> Result<OperationSummary, String> {
    let query = json!({
        "query": {
            "types": ["task"],
            "formulas": {
                "weighted": "priority * points",
                "is_open": "status == \"open\""
            },
            "where": "formula.weighted >= 8 && formula.is_open",
            "limit": 80
        }
    });

    run_timed("query_formula", iterations, |_| {
        let result = collection.query(&query);
        ensure_success(&result)
    })
}

fn profile_update(
    collection: &Collection,
    task_paths: &[String],
    iterations: usize,
    seed: u64,
) -> Result<OperationSummary, String> {
    let mut rng = StdRng::seed_from_u64(seed);
    let picks: Vec<usize> = (0..iterations)
        .map(|_| rng.gen_range(0..task_paths.len()))
        .collect();

    run_timed("update", iterations, |i| {
        let path = &task_paths[picks[i]];
        let status = STATUS_CYCLE[i % STATUS_CYCLE.len()];
        let fields = json!({
            "status": status,
            "points": (i % 13) as i64,
        });
        let result = collection.update(&json!({
            "path": path,
            "fields": fields,
        }));
        ensure_success(&result)
    })
}

fn profile_rename(
    collection: &Collection,
    source_a: &str,
    source_b: &str,
    iterations: usize,
) -> Result<OperationSummary, String> {
    let mut using_a = true;
    run_timed("rename_update_refs", iterations, |_| {
        let (from, to) = if using_a {
            (source_a, source_b)
        } else {
            (source_b, source_a)
        };
        let result = collection.rename(&json!({
            "from": from,
            "to": to,
            "update_refs": true,
        }));
        ensure_success(&result)?;
        using_a = !using_a;
        Ok(())
    })
}

fn profile_create(collection: &Collection, iterations: usize) -> Result<OperationSummary, String> {
    let create_paths: Vec<String> = (0..iterations)
        .map(|i| format!("scratch/create-{i:06}.md"))
        .collect();
    run_timed("create", iterations, |i| {
        let result = collection.create(&json!({
            "path": create_paths[i],
            "type": "task",
            "fields": {
                "title": format!("Created Task {i:06}"),
                "status": STATUS_CYCLE[i % STATUS_CYCLE.len()],
                "priority": ((i % 5) + 1) as i64,
                "points": (i % 13) as i64,
            }
        }));
        ensure_success(&result)
    })
}

fn profile_delete(collection: &Collection, iterations: usize) -> Result<OperationSummary, String> {
    let delete_paths: Vec<String> = (0..iterations)
        .map(|i| format!("scratch/delete-{i:06}.md"))
        .collect();

    for (i, path) in delete_paths.iter().enumerate() {
        let created = collection.create(&json!({
            "path": path,
            "type": "task",
            "fields": {
                "title": format!("Delete Task {i:06}"),
                "status": "open",
                "priority": 3,
                "points": 1,
            }
        }));
        ensure_success(&created)?;
    }

    run_timed("delete", iterations, |i| {
        let result = collection.delete(&json!({ "path": delete_paths[i] }));
        ensure_success(&result)
    })
}

fn profile_cache_rebuild(
    collection: &Collection,
    iterations: usize,
) -> Result<OperationSummary, String> {
    run_timed("cache_rebuild", iterations, |_| {
        let result = collection.cache_rebuild();
        ensure_success(&result)
    })
}
