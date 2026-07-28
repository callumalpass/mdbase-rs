use clap::{Args as ClapArgs, ValueEnum};
use mdbase::frontmatter::parser::json_to_yaml_mapping;
use mdbase::frontmatter::serializer::serialize_document;
use mdbase::runtime::{FilesystemRuntime, OperationKind, OperationRequest};
use mdbase::v03::QueryPerformance;
use mdbase::Collection;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Workload to run; queries is the fastest feedback loop
    #[arg(long, value_enum, default_value_t = Scenario::Queries)]
    scenario: Scenario,

    /// Number of task files in the synthetic fixture
    #[arg(long, default_value_t = 5000)]
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
    #[arg(long = "read-iters", default_value_t = 200)]
    read_iterations: usize,

    /// Iterations for query operations
    #[arg(long = "query-iters", default_value_t = 5)]
    query_iterations: usize,

    /// Iterations for saved-view discovery and execution
    #[arg(long = "view-iters", default_value_t = 5)]
    view_iterations: usize,

    /// Iterations for the two-pass paginated editor index workload
    #[arg(long = "editor-iters", default_value_t = 1)]
    editor_iterations: usize,

    /// Iterations for update operations
    #[arg(long = "update-iters", default_value_t = 20)]
    update_iterations: usize,

    /// Iterations for rename operations
    #[arg(long = "rename-iters", default_value_t = 5)]
    rename_iterations: usize,

    /// Iterations for create operations
    #[arg(long = "create-iters", default_value_t = 20)]
    create_iterations: usize,

    /// Iterations for delete operations
    #[arg(long = "delete-iters", default_value_t = 20)]
    delete_iterations: usize,

    /// Iterations for cache rebuild operations
    #[arg(long = "cache-rebuild-iters", default_value_t = 1)]
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

    /// JSON file containing release performance budgets
    #[arg(long)]
    thresholds: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Scenario {
    Queries,
    Views,
    Core,
    All,
}

#[derive(Debug, Serialize)]
struct ProfileConfig {
    scenario: &'static str,
    files: usize,
    projects: usize,
    rename_refs: usize,
    open_iterations: usize,
    read_iterations: usize,
    query_iterations: usize,
    view_iterations: usize,
    editor_iterations: usize,
    update_iterations: usize,
    rename_iterations: usize,
    create_iterations: usize,
    delete_iterations: usize,
    cache_rebuild_iterations: usize,
    seed: u64,
}

#[derive(Debug, Serialize)]
struct FixtureSummary {
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    phases: BTreeMap<String, PhaseSummary>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    counters: BTreeMap<String, f64>,
}

#[derive(Debug, Serialize)]
struct PhaseSummary {
    mean_ms: f64,
    p95_ms: f64,
    max_ms: f64,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PerformanceThresholds {
    schema_version: u32,
    scenario: String,
    files: usize,
    operations: BTreeMap<String, OperationThreshold>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationThreshold {
    max_p95_ms: f64,
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

/// Run the deterministic engine profile selected by the unified CLI.
///
/// When `collection_root` is present, records are never modified and only the
/// metadata-page and editor query workloads run.
pub fn run(args: Args, collection_root: Option<&Path>, output_json: bool) -> Result<(), String> {
    validate_args(&args, collection_root.is_some())?;
    if let Some(root) = collection_root {
        return run_existing_collection(&args, root, output_json);
    }

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

    if matches!(args.scenario, Scenario::Core | Scenario::All) {
        operations.push(profile_read(
            &collection,
            &fixture.task_paths,
            args.read_iterations,
            args.seed,
        )?);
    }
    if matches!(args.scenario, Scenario::Queries | Scenario::All) {
        // Query profiling represents the long-running provider path, where the
        // SQLite cache has already been established by normal collection use.
        operations.push(profile_cache_rebuild(&collection, 1)?);
        operations.push(profile_query_basic(&collection, args.query_iterations)?);
        operations.push(profile_query_formula(&collection, args.query_iterations)?);
        operations.push(profile_editor_list(&collection, args.editor_iterations)?);
    }
    if matches!(args.scenario, Scenario::Views | Scenario::All) {
        if matches!(args.scenario, Scenario::Views) {
            operations.push(profile_cache_rebuild(&collection, 1)?);
        }
        operations.push(profile_list_views(&collection, args.view_iterations)?);
        operations.push(profile_execute_view(
            &collection,
            "view_execute_canonical",
            "views/tasks.md",
            "open-tasks",
            args.view_iterations,
        )?);
        operations.push(profile_execute_view(
            &collection,
            "view_execute_obsidian",
            "views/tasks.base",
            "open-tasks",
            args.view_iterations,
        )?);
    }
    if matches!(args.scenario, Scenario::Core | Scenario::All) {
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

        let runtime_started = Instant::now();
        let runtime = FilesystemRuntime::open(&fixture.root, Duration::from_millis(120))
            .map_err(|error| error.to_string())?;
        operations.push(summarize(
            "runtime_open_with_snapshot",
            vec![runtime_started.elapsed().as_secs_f64() * 1_000.0],
        ));
        operations.push(profile_runtime_update(
            &runtime,
            &fixture.task_paths,
            args.update_iterations,
            args.seed.wrapping_add(2),
        )?);
    }
    if matches!(args.scenario, Scenario::Core | Scenario::All) && args.cache_rebuild_iterations > 0
    {
        operations.push(profile_cache_rebuild(
            &collection,
            args.cache_rebuild_iterations,
        )?);
    }

    let report = ProfileReport {
        tool: "mdbase-profile-engine",
        version: env!("CARGO_PKG_VERSION"),
        generated_at,
        total_runtime_ms: run_start.elapsed().as_secs_f64() * 1000.0,
        config: ProfileConfig {
            scenario: match args.scenario {
                Scenario::Queries => "queries",
                Scenario::Views => "views",
                Scenario::Core => "core",
                Scenario::All => "all",
            },
            files: args.files,
            projects: args.projects,
            rename_refs: args.rename_refs,
            open_iterations: args.open_iterations,
            read_iterations: args.read_iterations,
            query_iterations: args.query_iterations,
            view_iterations: args.view_iterations,
            editor_iterations: args.editor_iterations,
            update_iterations: args.update_iterations,
            rename_iterations: args.rename_iterations,
            create_iterations: args.create_iterations,
            delete_iterations: args.delete_iterations,
            cache_rebuild_iterations: args.cache_rebuild_iterations,
            seed: args.seed,
        },
        fixture: FixtureSummary {
            kept: args.keep_fixture,
            task_files: args.files + 1,
            project_files: args.projects,
            rename_reference_files: args.rename_refs,
        },
        operations,
    };

    emit_report(&args, &report, output_json)?;
    validate_thresholds(&args, &report)
}

fn run_existing_collection(args: &Args, root: &Path, output_json: bool) -> Result<(), String> {
    if !matches!(args.scenario, Scenario::Queries) {
        return Err(
            "profiling an existing --root collection is read-only and supports only --scenario queries"
                .to_string(),
        );
    }
    if args.fixture_root.is_some() || args.keep_fixture {
        return Err("--fixture-root and --keep-fixture cannot be used with --root".to_string());
    }
    let root = root
        .canonicalize()
        .map_err(|error| format!("Collection path could not be resolved: {error}"))?;
    let run_start = Instant::now();
    let mut operations = vec![profile_open(&root, args.open_iterations)?];
    let collection = Collection::open(&root).map_err(format_json_error)?;
    operations.push(profile_existing_query_page(
        &collection,
        args.query_iterations,
    )?);
    operations.push(profile_editor_list(&collection, args.editor_iterations)?);
    let records = operations
        .iter()
        .find(|operation| operation.name == "v03_query_page_200")
        .and_then(|operation| operation.counters.get("candidates"))
        .copied()
        .unwrap_or(0.0) as usize;
    let report = ProfileReport {
        tool: "mdbase-profile-engine",
        version: env!("CARGO_PKG_VERSION"),
        generated_at: chrono::Utc::now().to_rfc3339(),
        total_runtime_ms: run_start.elapsed().as_secs_f64() * 1_000.0,
        config: ProfileConfig {
            scenario: "queries",
            files: records,
            projects: 0,
            rename_refs: 0,
            open_iterations: args.open_iterations,
            read_iterations: 0,
            query_iterations: args.query_iterations,
            view_iterations: 0,
            editor_iterations: args.editor_iterations,
            update_iterations: 0,
            rename_iterations: 0,
            create_iterations: 0,
            delete_iterations: 0,
            cache_rebuild_iterations: 0,
            seed: args.seed,
        },
        fixture: FixtureSummary {
            kept: true,
            task_files: records,
            project_files: 0,
            rename_reference_files: 0,
        },
        operations,
    };
    emit_report(args, &report, output_json)?;
    validate_thresholds(args, &report)
}

fn emit_report(args: &Args, report: &ProfileReport, output_json: bool) -> Result<(), String> {
    let report_json = serde_json::to_string_pretty(report)
        .map_err(|error| format!("Failed to serialize report JSON: {error}"))?;
    if let Some(output) = &args.output {
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!("Failed to create output directory {:?}: {error}", parent)
            })?;
        }
        fs::write(output, format!("{report_json}\n"))
            .map_err(|error| format!("Failed to write report to {}: {error}", output.display()))?;
    }
    if output_json {
        println!("{report_json}");
    } else {
        print_report(report);
    }
    Ok(())
}

fn validate_args(args: &Args, existing_collection: bool) -> Result<(), String> {
    if args.files == 0 {
        return Err("--files must be greater than 0".to_string());
    }
    if args.projects == 0 {
        return Err("--projects must be greater than 0".to_string());
    }
    if existing_collection && args.thresholds.is_some() {
        return Err(
            "--thresholds is only supported for deterministic synthetic fixtures".to_string(),
        );
    }
    Ok(())
}

fn validate_thresholds(args: &Args, report: &ProfileReport) -> Result<(), String> {
    let Some(path) = &args.thresholds else {
        return Ok(());
    };
    let content = fs::read_to_string(path)
        .map_err(|error| format!("Failed to read thresholds at {}: {error}", path.display()))?;
    let thresholds: PerformanceThresholds = serde_json::from_str(&content)
        .map_err(|error| format!("Failed to parse thresholds at {}: {error}", path.display()))?;
    if thresholds.schema_version != 1 {
        return Err(format!(
            "Unsupported performance threshold schema version {}",
            thresholds.schema_version
        ));
    }
    if thresholds.scenario != report.config.scenario {
        return Err(format!(
            "Threshold scenario '{}' does not match report scenario '{}'",
            thresholds.scenario, report.config.scenario
        ));
    }
    if thresholds.files != report.config.files {
        return Err(format!(
            "Threshold fixture size {} does not match report fixture size {}",
            thresholds.files, report.config.files
        ));
    }
    if thresholds.operations.is_empty() {
        return Err("Performance threshold file contains no operations".to_string());
    }

    let summaries = report
        .operations
        .iter()
        .map(|summary| (summary.name.as_str(), summary))
        .collect::<BTreeMap<_, _>>();
    let mut failures = Vec::new();
    for (name, threshold) in &thresholds.operations {
        if !threshold.max_p95_ms.is_finite() || threshold.max_p95_ms <= 0.0 {
            return Err(format!(
                "Performance threshold '{name}' must have a finite, positive max_p95_ms"
            ));
        }
        let Some(summary) = summaries.get(name.as_str()) else {
            failures.push(format!("{name}: operation was not executed"));
            continue;
        };
        if summary.p95_ms > threshold.max_p95_ms {
            failures.push(format!(
                "{name}: p95 {:.3} ms exceeded {:.3} ms",
                summary.p95_ms, threshold.max_p95_ms
            ));
        }
    }
    if failures.is_empty() {
        println!(
            "Performance budgets passed for {} operations.",
            thresholds.operations.len()
        );
        Ok(())
    } else {
        Err(format!(
            "Performance budget failure:\n- {}",
            failures.join("\n- ")
        ))
    }
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
  timezone: "UTC"
  exclude:
    - "_types"
x-obsidian:
  bases:
    include: ["views/*.base"]
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

    write_plain_markdown_file(
        &root.join("views/tasks.md"),
        r#"---
type: view
id: profiler.tasks
version: 1
name: Profiler task views
query:
  types: [task]
views:
  - id: open-tasks
    name: Open tasks
    where: 'status != "done"'
    select: [title, status, priority, points]
    order_by:
      - { field: priority, direction: desc }
      - { field: title, direction: asc }
---
"#,
    )?;
    write_plain_markdown_file(
        &root.join("views/tasks.base"),
        r#"filters:
  and:
    - 'type == "task"'
formulas:
  score: 'priority * 10 + points'
views:
  - type: table
    name: Open tasks
    filters:
      and:
        - 'status != "done"'
    order: [title, status, priority, formula.score]
    sort:
      - property: formula.score
        direction: DESC
      - property: title
        direction: ASC
"#,
    )?;

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
            phases: BTreeMap::new(),
            counters: BTreeMap::new(),
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
        phases: BTreeMap::new(),
        counters: BTreeMap::new(),
    }
}

fn run_profiled_query<F>(
    name: &str,
    iterations: usize,
    mut operation: F,
) -> Result<OperationSummary, String>
where
    F: FnMut(usize) -> Result<QueryPerformance, String>,
{
    let mut samples = Vec::with_capacity(iterations);
    let mut profiles = Vec::with_capacity(iterations);
    for index in 0..iterations {
        let started = Instant::now();
        let profile = operation(index)
            .map_err(|error| format!("{name} iteration {index} failed: {error}"))?;
        samples.push(started.elapsed().as_secs_f64() * 1_000.0);
        profiles.push(profile);
    }
    let mut summary = summarize(name, samples);
    summary.phases = summarize_query_phases(&profiles);
    summary.counters = summarize_query_counters(&profiles);
    Ok(summary)
}

fn summarize_query_phases(profiles: &[QueryPerformance]) -> BTreeMap<String, PhaseSummary> {
    type QueryPhase = (&'static str, fn(&QueryPerformance) -> u64);
    let fields: [QueryPhase; 13] = [
        ("schema", |value| value.schema_us),
        ("preflight", |value| value.preflight_us),
        ("cache_open", |value| value.cache_open_us),
        ("cache_refresh", |value| value.cache_refresh_us),
        ("records_load", |value| value.records_load_us),
        ("all_files", |value| value.all_files_us),
        ("link_graph", |value| value.link_graph_us),
        ("context", |value| value.context_us),
        ("evaluate", |value| value.evaluate_us),
        ("sort", |value| value.sort_us),
        ("groups", |value| value.groups_us),
        ("serialize", |value| value.serialize_us),
        ("total", |value| value.total_us),
    ];
    fields
        .into_iter()
        .map(|(name, field)| {
            let mut samples = profiles
                .iter()
                .map(|profile| field(profile) as f64 / 1_000.0)
                .collect::<Vec<_>>();
            samples.sort_by(|left, right| {
                left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal)
            });
            let mean_ms = samples.iter().sum::<f64>() / samples.len().max(1) as f64;
            let p95_ms = percentile(&samples, 0.95);
            let max_ms = samples.last().copied().unwrap_or_default();
            (
                name.to_string(),
                PhaseSummary {
                    mean_ms,
                    p95_ms,
                    max_ms,
                },
            )
        })
        .collect()
}

fn summarize_query_counters(profiles: &[QueryPerformance]) -> BTreeMap<String, f64> {
    let count = profiles.len().max(1) as f64;
    [
        (
            "records_loaded",
            profiles
                .iter()
                .map(|profile| profile.records_loaded as f64)
                .sum::<f64>()
                / count,
        ),
        (
            "candidates",
            profiles
                .iter()
                .map(|profile| profile.candidates as f64)
                .sum::<f64>()
                / count,
        ),
        (
            "results",
            profiles
                .iter()
                .map(|profile| profile.results as f64)
                .sum::<f64>()
                / count,
        ),
        (
            "link_graph_built",
            profiles
                .iter()
                .filter(|profile| profile.link_graph_built)
                .count() as f64
                / count,
        ),
        (
            "cache_fallback",
            profiles
                .iter()
                .filter(|profile| profile.cache_fallback)
                .count() as f64
                / count,
        ),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value))
    .collect()
}

fn merge_query_performance(target: &mut QueryPerformance, value: QueryPerformance) {
    target.total_us += value.total_us;
    target.schema_us += value.schema_us;
    target.preflight_us += value.preflight_us;
    target.clock_us += value.clock_us;
    target.load_us += value.load_us;
    target.cache_open_us += value.cache_open_us;
    target.cache_refresh_us += value.cache_refresh_us;
    target.records_load_us += value.records_load_us;
    target.all_files_us += value.all_files_us;
    target.link_graph_us += value.link_graph_us;
    target.context_us += value.context_us;
    target.evaluate_us += value.evaluate_us;
    target.sort_us += value.sort_us;
    target.groups_us += value.groups_us;
    target.serialize_us += value.serialize_us;
    target.records_loaded += value.records_loaded;
    target.candidates += value.candidates;
    target.results += value.results;
    target.cache_used |= value.cache_used;
    target.cache_fallback |= value.cache_fallback;
    target.link_graph_built |= value.link_graph_built;
}

fn print_report(report: &ProfileReport) {
    println!(
        "mdbase profile: {} task records, {} project records ({:.2}s total)",
        report.fixture.task_files,
        report.fixture.project_files,
        report.total_runtime_ms / 1_000.0
    );
    println!(
        "{:<24} {:>6} {:>11} {:>11} {:>11}",
        "operation", "runs", "mean", "p95", "max"
    );
    for operation in &report.operations {
        println!(
            "{:<24} {:>6} {:>8.2} ms {:>8.2} ms {:>8.2} ms",
            operation.name,
            operation.iterations,
            operation.mean_ms,
            operation.p95_ms,
            operation.max_ms
        );
        let dominant = operation
            .phases
            .iter()
            .filter(|(name, _)| name.as_str() != "total")
            .filter(|(_, phase)| phase.mean_ms >= 0.05)
            .map(|(name, phase)| format!("{name}={:.2}ms", phase.mean_ms))
            .collect::<Vec<_>>();
        if !dominant.is_empty() {
            println!("  {}", dominant.join(" "));
        }
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
        "types": ["task"],
        "where": "priority >= 3 && status != \"done\"",
        "order_by": [
            {"field": "priority", "direction": "desc"},
            {"field": "points", "direction": "asc"}
        ],
        "limit": 120
    });
    let operations = collection.v03_operations().map_err(|error| error.message)?;

    run_profiled_query("v03_query_basic", iterations, |_| {
        let (result, profile) = operations.query_profiled(&query);
        ensure_v03_success(&result)?;
        Ok(profile)
    })
}

fn profile_existing_query_page(
    collection: &Collection,
    iterations: usize,
) -> Result<OperationSummary, String> {
    let operations = collection.v03_operations().map_err(|error| error.message)?;
    run_profiled_query("v03_query_page_200", iterations, |_| {
        let (result, profile) = operations.query_profiled(&json!({
            "order_by": [{"field": "file.mtime", "direction": "desc"}],
            "limit": 200,
            "include_body": false,
        }));
        ensure_v03_success(&result)?;
        Ok(profile)
    })
}

fn profile_query_formula(
    collection: &Collection,
    iterations: usize,
) -> Result<OperationSummary, String> {
    let query = json!({
        "types": ["task"],
        "projections": {
            "weighted": {"expr": "priority * points"},
            "is_open": {"expr": "status == \"open\""}
        },
        "where": "projection.weighted >= 8 && projection.is_open",
        "limit": 80
    });
    let operations = collection.v03_operations().map_err(|error| error.message)?;

    run_profiled_query("v03_query_projection", iterations, |_| {
        let (result, profile) = operations.query_profiled(&query);
        ensure_v03_success(&result)?;
        Ok(profile)
    })
}

fn profile_list_views(
    collection: &Collection,
    iterations: usize,
) -> Result<OperationSummary, String> {
    let operations = collection
        .v03_operations()
        .map_err(|diagnostic| diagnostic.message.clone())?;
    let mut summary = run_timed("view_list", iterations, |_| {
        ensure_view_success(operations.list_views(&json!({})))
    })?;
    let listed = operations.list_views(&json!({}));
    ensure_view_success(listed.clone())?;
    summary.counters.insert(
        "view_documents".to_string(),
        listed.result["meta"]["total_count"]
            .as_u64()
            .unwrap_or_default() as f64,
    );
    Ok(summary)
}

fn profile_execute_view(
    collection: &Collection,
    name: &str,
    path: &str,
    view: &str,
    iterations: usize,
) -> Result<OperationSummary, String> {
    let operations = collection
        .v03_operations()
        .map_err(|diagnostic| diagnostic.message.clone())?;
    let input = json!({"path": path, "view": view, "limit": 200});
    let mut summary = run_timed(name, iterations, |_| {
        ensure_view_success(operations.execute_view(&input))
    })?;
    let executed = operations.execute_view(&input);
    ensure_view_success(executed.clone())?;
    summary.counters.insert(
        "matching_records".to_string(),
        executed.result["meta"]["total_count"]
            .as_u64()
            .unwrap_or_default() as f64,
    );
    summary.counters.insert(
        "page_results".to_string(),
        executed.result["results"].as_array().map_or(0, Vec::len) as f64,
    );
    Ok(summary)
}

fn ensure_view_success(result: mdbase::v03::OperationResult) -> Result<(), String> {
    if result.valid {
        return Ok(());
    }
    Err(result
        .diagnostics
        .iter()
        .map(|diagnostic| format!("{}: {}", diagnostic.code, diagnostic.message))
        .collect::<Vec<_>>()
        .join("; "))
}

fn profile_editor_list(
    collection: &Collection,
    iterations: usize,
) -> Result<OperationSummary, String> {
    let operations = collection.v03_operations().map_err(|error| error.message)?;
    run_profiled_query("editor_two_pass_index", iterations, |_| {
        let mut combined = QueryPerformance::default();
        let mut snapshot: Option<String> = None;
        for include_body in [false, true] {
            let mut offset = 0_u64;
            loop {
                let limit = if offset == 0 { 200 } else { 1_000 };
                let mut input = json!({
                    "order_by": [{"field": "file.mtime", "direction": "desc"}],
                    "limit": limit,
                    "offset": offset,
                    "include_body": include_body,
                });
                if let Some(snapshot) = &snapshot {
                    input["snapshot"] = Value::String(snapshot.clone());
                }
                let (result, profile) = operations.query_profiled(&input);
                ensure_v03_success(&result)?;
                if snapshot.is_none() {
                    snapshot = result.result["meta"]["snapshot"]
                        .as_str()
                        .map(str::to_string);
                }
                let page = result.result["results"]
                    .as_array()
                    .ok_or("query result did not contain a results array")?;
                let has_more = result.result["meta"]["has_more"].as_bool().unwrap_or(false);
                offset += page.len() as u64;
                merge_query_performance(&mut combined, profile);
                if !has_more || page.is_empty() {
                    break;
                }
            }
        }
        Ok(combined)
    })
}

fn ensure_v03_success(result: &mdbase::v03::OperationResult) -> Result<(), String> {
    if result.valid {
        Ok(())
    } else {
        Err(result
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>()
            .join("; "))
    }
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

fn profile_runtime_update(
    runtime: &FilesystemRuntime,
    task_paths: &[String],
    iterations: usize,
    seed: u64,
) -> Result<OperationSummary, String> {
    let mut rng = StdRng::seed_from_u64(seed);
    let picks: Vec<usize> = (0..iterations)
        .map(|_| rng.gen_range(0..task_paths.len()))
        .collect();
    run_timed("runtime_update_and_watch", iterations, |i| {
        let result = runtime
            .execute(&OperationRequest::new(
                OperationKind::Update,
                json!({
                    "path": task_paths[picks[i]],
                    "fields": {
                        "status": STATUS_CYCLE[i % STATUS_CYCLE.len()],
                        "points": ((i + 1) % 13) as i64,
                    },
                }),
            ))
            .map_err(|error| error.to_string())?;
        ensure_v03_success(&result)?;
        while runtime
            .recv_timeout(Duration::ZERO)
            .map_err(|error| error.to_string())?
            .is_some()
        {}
        Ok(())
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
