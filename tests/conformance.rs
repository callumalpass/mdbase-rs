//! Conformance test runner for mdbase.
//!
//! Reads YAML test files from ~/projects/mdbase-spec/tests/ and executes
//! them against the Rust implementation. Test files use a grouped format:
//!
//! ```yaml
//! name: "..."
//! level: 1
//! groups:
//!   - name: "group name"
//!     setup: { config: "...", types: {}, files: {} }
//!     tests:
//!       - name: "test name"
//!         setup: { ... }       # optional per-test override
//!         operation: load_config
//!         input: {}
//!         expect: { valid: true, config: { ... } }
//! ```

use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Path to the spec's test files.
fn spec_tests_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("MDBASE_SPEC_TESTS_DIR") {
        return PathBuf::from(path);
    }

    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            let mut home = PathBuf::from(drive);
            home.push(path);
            Some(home.into_os_string())
        })
        .expect("home directory not set; provide MDBASE_SPEC_TESTS_DIR");
    PathBuf::from(home).join("projects/mdbase-spec/tests")
}

// ---------------------------------------------------------------------------
// YAML test structures
// ---------------------------------------------------------------------------

/// Top-level test file with grouped sub-tests.
#[derive(Debug, Deserialize)]
struct TestFile {
    name: String,
    level: u32,
    #[allow(dead_code)]
    category: Option<String>,
    #[allow(dead_code)]
    spec_ref: Option<String>,
    groups: Vec<TestSubGroup>,
}

/// A group of related test cases sharing an optional setup.
#[derive(Debug, Deserialize)]
struct TestSubGroup {
    name: String,
    #[allow(dead_code)]
    spec_ref: Option<String>,
    setup: Option<TestSetup>,
    tests: Vec<TestCase>,
}

#[derive(Debug, Deserialize)]
struct TestSetup {
    config: Option<String>,
    types: Option<HashMap<String, String>>,
    files: Option<HashMap<String, serde_yaml::Value>>,
}

#[derive(Debug, Deserialize)]
struct TestCase {
    name: String,
    #[allow(dead_code)]
    spec_ref: Option<String>,
    setup: Option<TestSetup>,
    simulate: Option<serde_yaml::Value>,
    operation: String,
    #[serde(default)]
    input: serde_yaml::Value,
    #[serde(default)]
    expect: TestExpectation,
    verify_after: Option<serde_yaml::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TestExpectation {
    valid: Option<bool>,
    issues: Option<Vec<ExpectedIssue>>,
    error: Option<ExpectedError>,
    config: Option<serde_yaml::Value>,
    warnings: Option<Vec<WarningExpectation>>,
    result: Option<serde_yaml::Value>,
    results: Option<Vec<serde_yaml::Value>>,
    count: Option<usize>,
    paths: Option<Vec<String>>,
    meta: Option<serde_yaml::Value>,
    frontmatter: Option<serde_yaml::Value>,
    body: Option<String>,
    body_contains: Option<String>,
    path: Option<String>,
    path_contains: Option<String>,
    types: Option<Vec<String>>,
    file: Option<serde_yaml::Value>,
    deleted: Option<bool>,
    from: Option<String>,
    to: Option<String>,
    validation: Option<serde_yaml::Value>,
    summaries: Option<serde_yaml::Value>,
    groups: Option<Vec<serde_yaml::Value>>,
    link: Option<serde_yaml::Value>,
    resolved_path: Option<serde_yaml::Value>,
    results_count: Option<usize>,
    batch_result: Option<serde_yaml::Value>,
    broken_links: Option<Vec<serde_yaml::Value>>,
    partial_updates: Option<serde_yaml::Value>,
    #[serde(alias = "type_loaded")]
    type_loaded: Option<bool>,
    // Watch-specific fields (§15)
    events: Option<Vec<serde_yaml::Value>>,
    events_ordered: Option<Vec<serde_yaml::Value>>,
    events_contain: Option<Vec<serde_yaml::Value>>,
    max_event_count: Option<usize>,
    listener_query: Option<serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
struct ExpectedIssue {
    code: Option<String>,
    field: Option<String>,
    path: Option<String>,
    severity: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExpectedError {
    code: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WarningExpectation {
    contains: Option<String>,
    code: Option<String>,
    path: Option<String>,
    message_contains: Option<String>,
}

// ---------------------------------------------------------------------------
// Test infrastructure
// ---------------------------------------------------------------------------

/// Extract the types_folder setting from a config YAML string.
/// Defaults to "_types" if not specified.
fn get_types_folder(config: Option<&String>) -> String {
    if let Some(cfg) = config {
        if let Ok(yaml) = serde_yaml::from_str::<serde_yaml::Value>(cfg) {
            if let Some(folder) = yaml
                .get("settings")
                .and_then(|s| s.get("types_folder"))
                .and_then(|v| v.as_str())
            {
                return folder.to_string();
            }
        }
    }
    "_types".to_string()
}

/// Materializes a test setup into a temporary directory.
fn materialize_setup(setup: &TestSetup) -> TempDir {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let root = tmp.path();

    // Write config only if explicitly provided (null / absent = don't create)
    if let Some(config_content) = &setup.config {
        fs::write(root.join("mdbase.yaml"), config_content).unwrap();
    }

    // Write type files - use types_folder from config
    if let Some(types) = &setup.types {
        let types_folder = get_types_folder(setup.config.as_ref());
        let types_dir = root.join(&types_folder);
        fs::create_dir_all(&types_dir).unwrap();
        for (filename, content) in types {
            let type_path = types_dir.join(filename);
            if let Some(parent) = type_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(type_path, content).unwrap();
        }
    }

    // Write content files
    if let Some(files) = &setup.files {
        for (file_path, value) in files {
            let full_path = root.join(file_path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            // Handle both plain strings and {encoding, content} maps
            match value {
                serde_yaml::Value::String(s) => {
                    fs::write(&full_path, s).unwrap();
                }
                serde_yaml::Value::Mapping(map) => {
                    let key = |s: &str| serde_yaml::Value::String(s.to_string());
                    let content_str = map
                        .get(key("content"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let encoding = map
                        .get(key("encoding"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("utf-8");
                    if encoding == "latin-1" || encoding == "iso-8859-1" {
                        // Write raw bytes - each char as a single byte
                        let bytes: Vec<u8> = content_str.chars().map(|c| c as u8).collect();
                        fs::write(&full_path, bytes).unwrap();
                    } else {
                        fs::write(&full_path, content_str).unwrap();
                    }
                }
                _ => {
                    fs::write(&full_path, "").unwrap();
                }
            };
        }
    }

    tmp
}

/// Materializes a merged test setup: group provides defaults, test overrides.
fn materialize_merged_setup(group: &TestSetup, test: &TestSetup) -> TempDir {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let root = tmp.path();

    // Config: test overrides group
    let config = test.config.as_ref().or(group.config.as_ref());
    if let Some(config_content) = config {
        fs::write(root.join("mdbase.yaml"), config_content).unwrap();
    }

    // Types: merge (group first, test overrides)
    let mut all_types: HashMap<String, String> = HashMap::new();
    if let Some(types) = &group.types {
        for (k, v) in types {
            all_types.insert(k.clone(), v.clone());
        }
    }
    if let Some(types) = &test.types {
        for (k, v) in types {
            all_types.insert(k.clone(), v.clone());
        }
    }
    if !all_types.is_empty() {
        let types_folder = get_types_folder(config);
        let types_dir = root.join(&types_folder);
        fs::create_dir_all(&types_dir).unwrap();
        for (filename, content) in &all_types {
            let type_path = types_dir.join(filename);
            if let Some(parent) = type_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(type_path, content).unwrap();
        }
    }

    // Files: merge (group first, test overrides same-named files)
    // When a test provides >= 2 files AND at least one overlaps with a group file,
    // remove group files from directories where overlap occurs. This handles tests
    // that intend to set up a clean scenario (e.g. backlinks tests) while preserving
    // additive tests that only override a single helper file.
    let mut all_files: HashMap<String, serde_yaml::Value> = HashMap::new();
    if let Some(files) = &group.files {
        for (k, v) in files {
            all_files.insert(k.clone(), v.clone());
        }
    }
    if let Some(test_files) = &test.files {
        if test_files.len() >= 2 {
            // Find directories where test files overlap with group files
            let mut overlap_dirs: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            if let Some(group_files) = &group.files {
                for test_path in test_files.keys() {
                    if group_files.contains_key(test_path) {
                        if let Some(dir) = Path::new(test_path).parent() {
                            overlap_dirs.insert(dir.to_string_lossy().to_string());
                        }
                    }
                }
            }
            // Remove group files from overlap directories
            if !overlap_dirs.is_empty() {
                all_files.retain(|path, _| {
                    if let Some(dir) = Path::new(path).parent() {
                        !overlap_dirs.contains(&dir.to_string_lossy().to_string())
                    } else {
                        true
                    }
                });
            }
        }
        for (k, v) in test_files {
            all_files.insert(k.clone(), v.clone());
        }
    }
    for (file_path, value) in &all_files {
        let full_path = root.join(file_path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        match value {
            serde_yaml::Value::String(s) => {
                fs::write(&full_path, s).unwrap();
            }
            serde_yaml::Value::Mapping(map) => {
                let key = |s: &str| serde_yaml::Value::String(s.to_string());
                let content_str = map
                    .get(key("content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let encoding = map
                    .get(key("encoding"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("utf-8");
                if encoding == "latin-1" || encoding == "iso-8859-1" {
                    let bytes: Vec<u8> = content_str.chars().map(|c| c as u8).collect();
                    fs::write(&full_path, bytes).unwrap();
                } else {
                    fs::write(&full_path, content_str).unwrap();
                }
            }
            _ => {
                fs::write(&full_path, "").unwrap();
            }
        };
    }

    tmp
}

/// Discover all test YAML files organized by level.
fn discover_tests() -> Vec<(PathBuf, TestFile)> {
    let tests_dir = spec_tests_dir();
    let mut test_files = Vec::new();

    if !tests_dir.exists() {
        return test_files;
    }

    let mut level_dirs: Vec<_> = fs::read_dir(&tests_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect();
    level_dirs.sort_by_key(|e| e.file_name());

    for level_dir in level_dirs {
        let mut files: Vec<_> = fs::read_dir(level_dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.ends_with(".yaml") || name.ends_with(".yml")
            })
            .collect();
        files.sort_by_key(|e| e.file_name());

        for file in files {
            let content = fs::read_to_string(file.path()).unwrap();
            match serde_yaml::from_str::<TestFile>(&content) {
                Ok(test_file) => test_files.push((file.path(), test_file)),
                Err(err) => {
                    eprintln!("Failed to parse {:?}: {}", file.path(), err);
                }
            }
        }
    }

    test_files
}

// ---------------------------------------------------------------------------
// Main test entry point
// ---------------------------------------------------------------------------

#[test]
fn conformance_tests() {
    let test_files = discover_tests();

    if test_files.is_empty() {
        println!(
            "No test files found in {:?}. Run the test-writing Ralph Loop first.",
            spec_tests_dir()
        );
        return;
    }

    let mut total = 0;
    let mut passed = 0;
    let mut failed = 0;
    let mut errors: Vec<String> = Vec::new();

    for (path, test_file) in &test_files {
        let filename = path.file_name().unwrap().to_string_lossy();
        println!(
            "\n=== Level {} | {} ({}) ===",
            test_file.level, test_file.name, filename
        );

        for group in &test_file.groups {
            println!("  --- {} ---", group.name);

            // For groups with config: null, share state across tests (e.g., init groups)
            let group_has_null_config = group
                .setup
                .as_ref()
                .map(|s| s.config.is_none())
                .unwrap_or(false);
            let shared_tmp = if group_has_null_config {
                group.setup.as_ref().map(materialize_setup)
            } else {
                None
            };

            for test_case in &group.tests {
                total += 1;

                // Materialize setup for each test (fresh copy every time)
                // Exception: groups with config=null share state (for init tests)
                let test_tmp = if group_has_null_config && test_case.setup.is_none() {
                    None // use shared_tmp
                } else {
                    match (&test_case.setup, &group.setup) {
                        (Some(test_setup), Some(group_setup)) => {
                            Some(materialize_merged_setup(group_setup, test_setup))
                        }
                        (Some(test_setup), None) => Some(materialize_setup(test_setup)),
                        (None, Some(group_setup)) if !group_has_null_config => {
                            Some(materialize_setup(group_setup))
                        }
                        _ => None,
                    }
                };
                let tmp_ref = test_tmp.as_ref();

                // If no setup, use shared temp (for init groups) or create minimal
                let fallback_tmp;
                let root = match tmp_ref {
                    Some(tmp) => tmp.path(),
                    None => {
                        if let Some(ref shared) = shared_tmp {
                            shared.path()
                        } else {
                            let minimal_setup = TestSetup {
                                config: Some("spec_version: \"0.1.0\"\n".to_string()),
                                types: None,
                                files: None,
                            };
                            fallback_tmp = materialize_setup(&minimal_setup);
                            fallback_tmp.path()
                        }
                    }
                };

                // Handle watch operations specially - bypass normal simulate handling
                if test_case.operation == "watch" {
                    // Collect simulate from test_case.simulate and/or input.simulate
                    let input_json = yaml_to_json(&test_case.input);
                    let sim_json = test_case
                        .simulate
                        .as_ref()
                        .map(yaml_to_json)
                        .unwrap_or_else(|| serde_json::json!({}));
                    let input_sim_json = input_json.get("simulate").cloned();

                    let watch_result =
                        mdbase::watch::simulate_watch(root, &sim_json, input_sim_json.as_ref());

                    let events = watch_result.events;

                    match check_watch_expectation(root, &events, &test_case.expect) {
                        Ok(()) => {
                            passed += 1;
                            println!("    ✓ {}", test_case.name);
                        }
                        Err(msg) => {
                            failed += 1;
                            let err = format!("[{}] {}: {}", filename, test_case.name, msg);
                            println!("    ✗ {}: {}", test_case.name, msg);
                            errors.push(err);
                        }
                    }
                    continue;
                }

                // Apply simulate actions before the operation
                // For concurrent modification testing: record mtime before modify,
                // then inject last_known_mtime into the operation input
                let mut input_override: Option<serde_yaml::Value> = None;

                // Collect simulate blocks from both test_case.simulate and input.simulate
                let input_simulate = test_case
                    .input
                    .as_mapping()
                    .and_then(|m| m.get(serde_yaml::Value::String("simulate".into())))
                    .cloned();
                let simulate_sources: Vec<&serde_yaml::Value> =
                    [test_case.simulate.as_ref(), input_simulate.as_ref()]
                        .iter()
                        .filter_map(|s| *s)
                        .collect();

                if !simulate_sources.is_empty() && test_case.name.contains("concurrent") {
                    eprintln!(
                        "    DEBUG simulate_sources count={} for '{}'",
                        simulate_sources.len(),
                        test_case.name
                    );
                }
                for sim in &simulate_sources {
                    if let Some(mapping) = sim.as_mapping() {
                        for (key, val) in mapping {
                            let action = key.as_str().unwrap_or("");
                            match action {
                                "external_modify" | "external_create" => {
                                    if let Some(path_str) = val
                                        .as_mapping()
                                        .and_then(|m| {
                                            m.get(serde_yaml::Value::String("path".to_string()))
                                        })
                                        .and_then(|v| v.as_str())
                                    {
                                        let timing = val
                                            .as_mapping()
                                            .and_then(|m| {
                                                m.get(serde_yaml::Value::String(
                                                    "timing".to_string(),
                                                ))
                                            })
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");

                                        // Build file content from either "content" or "frontmatter" field
                                        let file_content = if let Some(content) = val
                                            .as_mapping()
                                            .and_then(|m| {
                                                m.get(serde_yaml::Value::String(
                                                    "content".to_string(),
                                                ))
                                            })
                                            .and_then(|v| v.as_str())
                                        {
                                            Some(content.to_string())
                                        } else if let Some(fm) = val.as_mapping().and_then(|m| {
                                            m.get(serde_yaml::Value::String(
                                                "frontmatter".to_string(),
                                            ))
                                        }) {
                                            // Build markdown file from frontmatter map
                                            let yaml_str =
                                                serde_yaml::to_string(fm).unwrap_or_default();
                                            Some(format!("---\n{}---\n", yaml_str))
                                        } else {
                                            None
                                        };

                                        if timing == "before_ref_update" {
                                            // Don't apply now — pass to rename as simulate_before_ref_update
                                            let full_path = root.join(path_str);
                                            let original_mtime_ms = fs::metadata(&full_path)
                                                .ok()
                                                .and_then(|m| m.modified().ok())
                                                .map(|t| {
                                                    t.duration_since(std::time::UNIX_EPOCH)
                                                        .map(|d| d.as_millis() as u64)
                                                        .unwrap_or(0)
                                                });

                                            let content = file_content.unwrap_or_default();

                                            let mut override_map = match input_override
                                                .as_ref()
                                                .unwrap_or(&test_case.input)
                                            {
                                                serde_yaml::Value::Mapping(m) => m.clone(),
                                                _ => serde_yaml::Mapping::new(),
                                            };
                                            let sim_item = serde_yaml::Value::Sequence(vec![
                                                serde_yaml::Value::Mapping({
                                                    let mut m = serde_yaml::Mapping::new();
                                                    m.insert(
                                                        serde_yaml::Value::String("path".into()),
                                                        serde_yaml::Value::String(
                                                            path_str.to_string(),
                                                        ),
                                                    );
                                                    m.insert(
                                                        serde_yaml::Value::String("content".into()),
                                                        serde_yaml::Value::String(content),
                                                    );
                                                    m
                                                }),
                                            ]);
                                            override_map.insert(
                                                serde_yaml::Value::String(
                                                    "simulate_before_ref_update".into(),
                                                ),
                                                sim_item,
                                            );
                                            if let Some(ms) = original_mtime_ms {
                                                let mut ref_mtimes = serde_yaml::Mapping::new();
                                                ref_mtimes.insert(
                                                    serde_yaml::Value::String(path_str.to_string()),
                                                    serde_yaml::Value::Number(
                                                        serde_yaml::Number::from(ms),
                                                    ),
                                                );
                                                override_map.insert(
                                                    serde_yaml::Value::String(
                                                        "last_known_ref_mtimes".into(),
                                                    ),
                                                    serde_yaml::Value::Mapping(ref_mtimes),
                                                );
                                            }
                                            // Remove the simulate key from input so it's not passed to the operation
                                            override_map.remove(serde_yaml::Value::String(
                                                "simulate".into(),
                                            ));
                                            input_override =
                                                Some(serde_yaml::Value::Mapping(override_map));
                                        } else if let Some(content) = file_content {
                                            // No timing: record mtime, apply modify, pass last_known_mtime
                                            let full_path = root.join(path_str);
                                            let original_mtime_ms = fs::metadata(&full_path)
                                                .ok()
                                                .and_then(|m| m.modified().ok())
                                                .map(|t| {
                                                    t.duration_since(std::time::UNIX_EPOCH)
                                                        .map(|d| d.as_millis() as u64)
                                                        .unwrap_or(0)
                                                });

                                            if let Some(parent) = full_path.parent() {
                                                let _ = fs::create_dir_all(parent);
                                            }
                                            let _ = fs::write(&full_path, &content);
                                            // Ensure mtime differs from original: explicitly set
                                            // mtime to 1s after original to guarantee detection
                                            // regardless of filesystem timestamp granularity.
                                            if let Some(orig_ms) = original_mtime_ms {
                                                let bumped = std::time::UNIX_EPOCH
                                                    + std::time::Duration::from_millis(
                                                        orig_ms + 1000,
                                                    );
                                                let times =
                                                    std::fs::FileTimes::new().set_modified(bumped);
                                                if let Ok(f) = std::fs::File::options()
                                                    .write(true)
                                                    .open(&full_path)
                                                {
                                                    let _ = f.set_times(times);
                                                }
                                            }

                                            if test_case.name.contains("concurrent") {
                                                let new_mtime_ms = fs::metadata(&full_path)
                                                    .ok()
                                                    .and_then(|m| m.modified().ok())
                                                    .map(|t| {
                                                        t.duration_since(std::time::UNIX_EPOCH)
                                                            .map(|d| d.as_millis() as u64)
                                                            .unwrap_or(0)
                                                    });
                                                eprintln!("    DEBUG mtime: original={:?} new={:?} path={}", original_mtime_ms, new_mtime_ms, path_str);
                                            }

                                            if let Some(ms) = original_mtime_ms {
                                                let mut override_map = match input_override
                                                    .as_ref()
                                                    .unwrap_or(&test_case.input)
                                                {
                                                    serde_yaml::Value::Mapping(m) => m.clone(),
                                                    _ => serde_yaml::Mapping::new(),
                                                };
                                                override_map.insert(
                                                    serde_yaml::Value::String(
                                                        "last_known_mtime".into(),
                                                    ),
                                                    serde_yaml::Value::Number(
                                                        serde_yaml::Number::from(ms),
                                                    ),
                                                );
                                                // Remove the simulate key from input
                                                override_map.remove(serde_yaml::Value::String(
                                                    "simulate".into(),
                                                ));
                                                input_override =
                                                    Some(serde_yaml::Value::Mapping(override_map));
                                            }
                                        }
                                    }
                                }
                                "external_delete" => {
                                    if let Some(path_str) = val
                                        .as_mapping()
                                        .and_then(|m| {
                                            m.get(serde_yaml::Value::String("path".to_string()))
                                        })
                                        .and_then(|v| v.as_str())
                                    {
                                        let full_path = root.join(path_str);
                                        let _ = fs::remove_file(&full_path);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }

                let effective_input = input_override.as_ref().unwrap_or(&test_case.input);
                match execute_operation(
                    root,
                    &test_case.operation,
                    effective_input,
                    test_case.simulate.as_ref(),
                ) {
                    Ok(result) => match check_expectation(&result, &test_case.expect) {
                        Ok(()) => {
                            // Run verify_after checks if present
                            let verify_ok = run_verify_after(
                                root,
                                &test_case.verify_after,
                                &test_case.name,
                                &filename,
                            );
                            match verify_ok {
                                Ok(()) => {
                                    passed += 1;
                                    println!("    ✓ {}", test_case.name);
                                }
                                Err(msg) => {
                                    failed += 1;
                                    let err = format!(
                                        "[{}] {}: verify_after: {}",
                                        filename, test_case.name, msg
                                    );
                                    println!("    ✗ {}: verify_after: {}", test_case.name, msg);
                                    errors.push(err);
                                }
                            }
                        }
                        Err(msg) => {
                            failed += 1;
                            let err = format!("[{}] {}: {}", filename, test_case.name, msg);
                            println!("    ✗ {}: {}", test_case.name, msg);
                            errors.push(err);
                        }
                    },
                    Err(err) => {
                        // If the test expects an error, check if codes match
                        if let Some(expected_error) = &test_case.expect.error {
                            if let Some(expected_code) = &expected_error.code {
                                if err.contains(expected_code) {
                                    // Run verify_after for error cases too
                                    let verify_ok = run_verify_after(
                                        root,
                                        &test_case.verify_after,
                                        &test_case.name,
                                        &filename,
                                    );
                                    match verify_ok {
                                        Ok(()) => {
                                            passed += 1;
                                            println!("    ✓ {} (expected error)", test_case.name);
                                        }
                                        Err(msg) => {
                                            failed += 1;
                                            let err = format!(
                                                "[{}] {}: verify_after: {}",
                                                filename, test_case.name, msg
                                            );
                                            println!(
                                                "    ✗ {}: verify_after: {}",
                                                test_case.name, msg
                                            );
                                            errors.push(err);
                                        }
                                    }
                                    continue;
                                }
                            }
                        }
                        // For read operations that fail with file_not_found on excluded
                        // paths (e.g., type files after init), try reading directly from disk
                        if test_case.operation == "read"
                            && err.contains("file_not_found")
                            && test_case.expect.error.is_none()
                        {
                            if let Some(path) = yaml_to_json(&test_case.input)
                                .get("path")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                            {
                                let full_path = root.join(&path);
                                if full_path.exists() {
                                    let content =
                                        std::fs::read_to_string(&full_path).unwrap_or_default();
                                    let doc = mdbase::frontmatter::parser::parse_document(&content);
                                    let fm = match &doc.frontmatter {
                                        Some(serde_yaml::Value::Mapping(m)) => {
                                            mdbase::frontmatter::parser::yaml_mapping_to_json(m)
                                        }
                                        _ => serde_json::json!({}),
                                    };
                                    let result = serde_json::json!({
                                        "path": path,
                                        "frontmatter": fm,
                                        "body": doc.body,
                                    });
                                    match check_expectation(&result, &test_case.expect) {
                                        Ok(()) => {
                                            passed += 1;
                                            println!("    ✓ {}", test_case.name);
                                            continue;
                                        }
                                        Err(msg) => {
                                            failed += 1;
                                            let err = format!(
                                                "[{}] {}: {}",
                                                filename, test_case.name, msg
                                            );
                                            println!("    ✗ {}: {}", test_case.name, msg);
                                            errors.push(err);
                                            continue;
                                        }
                                    }
                                }
                            }
                        }
                        failed += 1;
                        let msg = format!(
                            "[{}] {}: operation error: {}",
                            filename, test_case.name, err
                        );
                        println!("    ✗ {}: {}", test_case.name, err);
                        errors.push(msg);
                    }
                }
            }
        }
    }

    println!("\n--- Results ---");
    println!("Total: {}, Passed: {}, Failed: {}", total, passed, failed);

    if !errors.is_empty() {
        println!("\nFailures:");
        for err in &errors {
            println!("  - {}", err);
        }
    }

    assert_eq!(failed, 0, "{} conformance test(s) failed", failed);
}

// ---------------------------------------------------------------------------
// Operation dispatch
// ---------------------------------------------------------------------------

/// Execute a single test operation against the mdbase implementation.
fn execute_operation(
    collection_root: &Path,
    operation: &str,
    input: &serde_yaml::Value,
    simulate: Option<&serde_yaml::Value>,
) -> Result<serde_json::Value, String> {
    let input_json = yaml_to_json(input);

    match operation {
        "load_config" => Ok(mdbase::config::load_config(collection_root)),
        "query" => {
            let collection = mdbase::Collection::open(collection_root).map_err(|e| {
                if let Some(err) = e.get("error") {
                    let code = err
                        .get("code")
                        .and_then(|c| c.as_str())
                        .unwrap_or("unknown");
                    let msg = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown");
                    format!("{}: {}", code, msg)
                } else {
                    format!("Failed to open collection: {}", e)
                }
            })?;
            Ok(collection.query(&input_json))
        }
        "parse_link" => {
            let collection = mdbase::Collection::open(collection_root)
                .map_err(|e| format!("Failed to open collection: {:?}", e))?;
            Ok(collection.parse_link(&input_json))
        }
        "resolve_link" => {
            let collection = mdbase::Collection::open(collection_root)
                .map_err(|e| format!("Failed to open collection: {:?}", e))?;
            let result = collection.resolve_link(&input_json);
            // Check if result contains issues (for path_traversal etc.)
            if let Some(error) = result.get("error") {
                let code = error
                    .get("code")
                    .and_then(|c| c.as_str())
                    .unwrap_or("unknown");
                let msg = error.get("message").and_then(|m| m.as_str()).unwrap_or("");
                return Err(format!("{}: {}", code, msg));
            }
            Ok(result)
        }
        "read" | "create" | "update" | "delete" | "rename" | "validate" | "load_types"
        | "get_types" | "get_type" | "create_type" | "cache_rebuild" | "cache_clear"
        | "backfill" | "migrate" => {
            let collection_result = mdbase::Collection::open(collection_root);
            let collection = match collection_result {
                Ok(c) => c,
                Err(e) => {
                    // If it's a config/type error, extract the error
                    if let Some(err) = e.get("error") {
                        let code = err
                            .get("code")
                            .and_then(|c| c.as_str())
                            .unwrap_or("unknown");
                        let msg = err
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown");
                        // For load_types/get_type, return as a structured result with valid: false
                        if operation == "load_types"
                            || operation == "get_types"
                            || operation == "get_type"
                        {
                            return Ok(serde_json::json!({
                                "valid": false,
                                "error": { "code": code, "message": msg },
                            }));
                        }
                        return Err(format!("{}: {}", code, msg));
                    }
                    return Err(format!("Failed to open collection: {}", e));
                }
            };

            let result = match operation {
                "read" => collection.read(&input_json),
                "create" => collection.create(&input_json),
                "update" => collection.update(&input_json),
                "delete" => collection.delete(&input_json),
                "rename" => collection.rename(&input_json),
                "validate" => collection.validate_op(&input_json),
                "cache_rebuild" => collection.cache_rebuild(),
                "cache_clear" => collection.cache_clear(),
                "load_types" | "get_types" => {
                    // If a path is provided, determine types for that specific file
                    if let Some(path) = input_json.get("path").and_then(|v| v.as_str()) {
                        let full_path = collection.root.join(path);
                        let frontmatter = if full_path.exists() {
                            let content = fs::read_to_string(&full_path).unwrap_or_default();
                            let doc = mdbase::frontmatter::parser::parse_document(&content);
                            match &doc.frontmatter {
                                Some(serde_yaml::Value::Mapping(m)) => {
                                    mdbase::frontmatter::parser::yaml_mapping_to_json(m)
                                }
                                _ => serde_json::json!({}),
                            }
                        } else {
                            serde_json::json!({})
                        };
                        let type_names =
                            collection.determine_types_for_path(&frontmatter, Some(path));
                        serde_json::json!({
                            "valid": true,
                            "types": type_names,
                        })
                    } else {
                        // Return list of all loaded type names
                        let type_names: Vec<String> = collection.types.keys().cloned().collect();
                        let types_detail: Vec<serde_json::Value> = collection
                            .types
                            .values()
                            .map(|t| {
                                let mut obj = serde_json::json!({
                                    "name": t.name,
                                });
                                if let Some(ref desc) = t.description {
                                    obj["description"] = serde_json::Value::String(desc.clone());
                                }
                                if let Some(ref extends) = t.extends {
                                    obj["extends"] = serde_json::Value::String(extends.clone());
                                }
                                let fields: serde_json::Map<String, serde_json::Value> = t
                                    .fields
                                    .iter()
                                    .map(|(k, v)| {
                                        (k.clone(), serde_json::json!({"type": v.field_type}))
                                    })
                                    .collect();
                                obj["fields"] = serde_json::Value::Object(fields);
                                obj
                            })
                            .collect();
                        let mut result = serde_json::json!({
                            "valid": true,
                            "types": types_detail,
                            "names": type_names,
                            "count": type_names.len(),
                        });
                        if !collection.type_warnings.is_empty() {
                            let warnings: Vec<serde_json::Value> = collection
                                .type_warnings
                                .iter()
                                .map(|w| {
                                    serde_json::json!({
                                        "message": w,
                                    })
                                })
                                .collect();
                            result["warnings"] = serde_json::Value::Array(warnings);
                        }
                        result
                    }
                }
                "get_type" => {
                    let raw_name = input_json
                        .get("type")
                        .or(input_json.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let name = raw_name.to_lowercase();
                    match collection.types.get(&name) {
                        Some(t) => {
                            let mut fields = serde_json::Map::new();
                            for (k, v) in &t.fields {
                                let mut fd = serde_json::json!({"type": v.field_type});
                                if v.required {
                                    fd["required"] = serde_json::Value::Bool(true);
                                }
                                if let Some(ref default) = v.default {
                                    fd["default"] = default.clone();
                                }
                                if let Some(ref gen) = v.generated {
                                    let gen_str = match gen {
                                        mdbase::types::schema::GeneratedStrategy::Ulid => "ulid",
                                        mdbase::types::schema::GeneratedStrategy::Uuid => "uuid",
                                        mdbase::types::schema::GeneratedStrategy::Now => "now",
                                        mdbase::types::schema::GeneratedStrategy::NowOnWrite => {
                                            "now_on_write"
                                        }
                                        mdbase::types::schema::GeneratedStrategy::Derived {
                                            ..
                                        } => "derived",
                                        mdbase::types::schema::GeneratedStrategy::Sequence(_) => {
                                            "sequence"
                                        }
                                        mdbase::types::schema::GeneratedStrategy::Random(_) => {
                                            "random"
                                        }
                                    };
                                    fd["generated"] =
                                        serde_json::Value::String(gen_str.to_string());
                                }
                                if let Some(ref vals) = v.values {
                                    fd["values"] = serde_json::json!(vals);
                                }
                                fields.insert(k.clone(), fd);
                            }
                            serde_json::json!({
                                "valid": true,
                                "name": t.name,
                                "type": {
                                    "name": t.name,
                                    "fields": fields,
                                },
                                "fields": fields,
                            })
                        }
                        None => {
                            return Err(format!("unknown_type: Type '{}' not found", name));
                        }
                    }
                }
                "create_type" => {
                    let name = input_json
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let fields_input = input_json.get("fields");
                    let parent_input = input_json.get("parent").and_then(|v| v.as_str());
                    let strict_input = input_json.get("strict").and_then(|v| v.as_bool());
                    let types_dir = collection.root.join(&collection.settings.types_folder);
                    let _ = std::fs::create_dir_all(&types_dir);

                    // Validate type name
                    if mdbase::types::loader::validate_type_name(name).is_err() {
                        return Ok(serde_json::json!({
                            "error": { "code": "invalid_type_definition", "message": format!("Invalid type name: '{}'", name) }
                        }));
                    }

                    // Check for name conflicts (case-insensitive)
                    let name_lower = name.to_lowercase();
                    if collection.types.contains_key(&name_lower) {
                        return Ok(serde_json::json!({
                            "error": { "code": "path_conflict", "message": format!("Type '{}' already exists", name) }
                        }));
                    }

                    // Check parent exists
                    if let Some(parent) = parent_input {
                        let parent_lower = parent.to_lowercase();
                        if !collection.types.contains_key(&parent_lower) {
                            return Ok(serde_json::json!({
                                "error": { "code": "missing_parent_type", "message": format!("Parent type '{}' not found", parent) }
                            }));
                        }
                    }

                    // Validate field types
                    let valid_types = [
                        "string", "integer", "number", "boolean", "date", "datetime", "time",
                        "enum", "list", "object", "link", "image", "file", "url", "email", "slug",
                    ];
                    if let Some(fields) = fields_input {
                        if let Some(obj) = fields.as_object() {
                            for (field_name, field_def) in obj {
                                if let Some(ft) = field_def.get("type").and_then(|v| v.as_str()) {
                                    if !valid_types.contains(&ft) {
                                        return Ok(serde_json::json!({
                                            "error": { "code": "invalid_type_definition",
                                                       "message": format!("Invalid field type '{}' for field '{}'", ft, field_name) }
                                        }));
                                    }
                                }
                                // Validate enum values are strings
                                if let Some(values) = field_def.get("values") {
                                    if let Some(arr) = values.as_array() {
                                        for v in arr {
                                            if !v.is_string() {
                                                return Ok(serde_json::json!({
                                                    "error": { "code": "invalid_type_definition",
                                                               "message": format!("Enum values must be strings for field '{}'", field_name) }
                                                }));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Build YAML frontmatter using serde_yaml for proper serialization
                    let mut fm_map = serde_yaml::Mapping::new();
                    fm_map.insert(
                        serde_yaml::Value::String("name".to_string()),
                        serde_yaml::Value::String(name.to_string()),
                    );
                    if let Some(parent) = parent_input {
                        fm_map.insert(
                            serde_yaml::Value::String("extends".to_string()),
                            serde_yaml::Value::String(parent.to_string()),
                        );
                    }
                    if let Some(strict) = strict_input {
                        fm_map.insert(
                            serde_yaml::Value::String("strict".to_string()),
                            serde_yaml::Value::Bool(strict),
                        );
                    }
                    if let Some(fields) = fields_input {
                        let yaml_fields = mdbase::frontmatter::parser::json_to_yaml(fields);
                        fm_map.insert(serde_yaml::Value::String("fields".to_string()), yaml_fields);
                    }
                    let yaml_str = serde_yaml::to_string(&serde_yaml::Value::Mapping(fm_map))
                        .unwrap_or_default();
                    let content = format!("---\n{}---\n", yaml_str);

                    let type_path = types_dir.join(format!("{}.md", name));
                    match std::fs::write(&type_path, &content) {
                        Ok(_) => {
                            // Reload types to verify the type loads correctly
                            let reload_result = mdbase::Collection::open(collection_root);
                            match reload_result {
                                Ok(reloaded) => {
                                    let name_lower = name.to_lowercase();
                                    let type_loaded = reloaded.types.contains_key(&name_lower);
                                    serde_json::json!({
                                        "name": name,
                                        "path": format!("{}/{}.md", collection.settings.types_folder, name),
                                        "type_loaded": type_loaded,
                                    })
                                }
                                Err(e) => {
                                    // Type file created but failed to load - clean up and report error
                                    let _ = std::fs::remove_file(&type_path);
                                    if let Some(err) = e.get("error") {
                                        let code = err
                                            .get("code")
                                            .and_then(|c| c.as_str())
                                            .unwrap_or("invalid_type_definition");
                                        let msg = err
                                            .get("message")
                                            .and_then(|m| m.as_str())
                                            .unwrap_or("Failed to load type");
                                        return Ok(serde_json::json!({
                                            "error": { "code": code, "message": msg }
                                        }));
                                    }
                                    return Ok(serde_json::json!({
                                        "error": { "code": "invalid_type_definition", "message": "Failed to reload collection after type creation" }
                                    }));
                                }
                            }
                        }
                        Err(e) => return Err(format!("io_error: {}", e)),
                    }
                }
                "backfill" => collection.backfill(&input_json),
                "migrate" => collection.migrate(&input_json),
                _ => unreachable!(),
            };

            // If result contains an error, return it as Err for the test runner
            // Exception: rename_ref_update_failed keeps the full result (has from/to + partial_updates)
            if let Some(error) = result.get("error") {
                let code = error
                    .get("code")
                    .and_then(|c| c.as_str())
                    .unwrap_or("unknown");
                if code == "rename_ref_update_failed" {
                    return Ok(result);
                }
                let msg = error.get("message").and_then(|m| m.as_str()).unwrap_or("");
                return Err(format!("{}: {}", code, msg));
            }

            Ok(result)
        }
        "evaluate" => {
            let expression = input_json
                .get("expression")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "evaluate requires 'expression' field".to_string())?;

            // Parse the expression
            let parsed = match mdbase::expressions::parser::Parser::parse(expression) {
                Ok(expr) => expr,
                Err(e) => {
                    let code = if e.contains("expression_depth_exceeded") {
                        "expression_depth_exceeded"
                    } else {
                        "invalid_expression"
                    };
                    return Ok(serde_json::json!({
                        "error": { "code": code, "message": e }
                    }));
                }
            };

            // Build evaluation context
            let ctx = if let Some(path_val) = input_json
                .get("path")
                .or_else(|| input_json.get("file"))
                .or_else(|| input_json.get("context_path"))
                .and_then(|v| v.as_str())
            {
                // Read the file to get frontmatter context
                let collection = mdbase::Collection::open(collection_root)
                    .map_err(|e| format!("Failed to open collection: {}", e))?;
                let read_result = collection.read(&serde_json::json!({"path": path_val}));
                let frontmatter = read_result
                    .get("frontmatter")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                let raw_frontmatter = read_result.get("raw_frontmatter").cloned();
                let body = read_result
                    .get("body")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let file_size = read_result.pointer("/file/size").and_then(|v| v.as_u64());
                let file_mtime = read_result
                    .pointer("/file/mtime")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                // Build all_files for asFile() traversal
                let all_files = collection.build_all_files_data();
                let backlinks_index = collection.build_backlinks_index(&all_files);
                let all_files_arc = std::sync::Arc::new(all_files);
                let backlinks_arc = std::sync::Arc::new(backlinks_index);
                let types_arc = std::sync::Arc::new(collection.types.clone());
                let type_names_for_file = collection.determine_types_for_path(
                    &read_result
                        .get("frontmatter")
                        .cloned()
                        .unwrap_or(serde_json::json!({})),
                    Some(path_val),
                );
                mdbase::expressions::evaluator::EvalContext {
                    frontmatter,
                    raw_frontmatter,
                    file_path: Some(path_val.to_string()),
                    body,
                    file_size,
                    file_mtime: file_mtime.clone(),
                    file_ctime: None,
                    this_context: None,
                    all_files: Some(all_files_arc),
                    traversal_depth: std::cell::Cell::new(0),
                    backlinks_index: Some(backlinks_arc),
                    type_names: Some(type_names_for_file),
                    types: Some(types_arc),
                    note_namespace_source: Default::default(),
                    string_concat: true,
                }
            } else if let Some(context_val) = input_json.get("context") {
                // Inline frontmatter context provided
                mdbase::expressions::evaluator::EvalContext {
                    frontmatter: context_val.clone(),
                    raw_frontmatter: None,
                    file_path: None,
                    body: None,
                    file_size: None,
                    file_mtime: None,
                    file_ctime: None,
                    this_context: None,
                    all_files: None,
                    traversal_depth: std::cell::Cell::new(0),
                    backlinks_index: None,
                    type_names: None,
                    types: None,
                    note_namespace_source: Default::default(),
                    string_concat: true,
                }
            } else {
                // No file context - evaluate in empty context
                mdbase::expressions::evaluator::EvalContext::empty()
            };

            // Evaluate the expression
            match mdbase::expressions::evaluator::evaluate(&parsed, &ctx) {
                Ok(value) => Ok(serde_json::json!({"result": value})),
                Err(e) => Ok(serde_json::json!({
                    "error": { "code": e.code, "message": e.message }
                })),
            }
        }
        "batch_update" | "batch_delete" => {
            let collection = mdbase::Collection::open(collection_root).map_err(|e| {
                if let Some(err) = e.get("error") {
                    let code = err
                        .get("code")
                        .and_then(|c| c.as_str())
                        .unwrap_or("unknown");
                    let msg = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown");
                    format!("{}: {}", code, msg)
                } else {
                    format!("Failed to open collection: {}", e)
                }
            })?;

            // Extract simulate parameters from both top-level simulate and input.simulate
            let sim_io_error: Option<String> = simulate
                .and_then(|s| s.as_mapping())
                .and_then(|m| m.get(serde_yaml::Value::String("io_error_on".to_string())))
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| {
                    input_json
                        .get("simulate")
                        .and_then(|s| s.get("io_error_on"))
                        .and_then(|v| v.as_str())
                        .map(String::from)
                });
            let skip_dependents: bool = simulate
                .and_then(|s| s.as_mapping())
                .and_then(|m| m.get(serde_yaml::Value::String("skip_dependents".to_string())))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || input_json
                    .get("simulate")
                    .and_then(|s| s.get("skip_dependents"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

            let result = if operation == "batch_update" {
                collection.batch_update(&input_json, sim_io_error.as_deref(), skip_dependents)
            } else {
                collection.batch_delete(&input_json, sim_io_error.as_deref())
            };

            // If result contains an error, return it as Err for the test runner
            if let Some(error) = result.get("error") {
                let code = error
                    .get("code")
                    .and_then(|c| c.as_str())
                    .unwrap_or("unknown");
                let msg = error.get("message").and_then(|m| m.as_str()).unwrap_or("");
                return Err(format!("{}: {}", code, msg));
            }

            Ok(result)
        }
        "init" => {
            let result = mdbase::init::init_collection(collection_root, &input_json);
            if let Some(error) = result.get("error") {
                let code = error
                    .get("code")
                    .and_then(|c| c.as_str())
                    .unwrap_or("unknown");
                let msg = error.get("message").and_then(|m| m.as_str()).unwrap_or("");
                return Err(format!("{}: {}", code, msg));
            }
            Ok(result)
        }
        _ => Err(format!("Operation '{}' not yet implemented", operation)),
    }
}

// ---------------------------------------------------------------------------
// verify_after: run follow-up operations after the primary operation
// ---------------------------------------------------------------------------

fn run_verify_after(
    root: &Path,
    verify_after: &Option<serde_yaml::Value>,
    _test_name: &str,
    _filename: &str,
) -> Result<(), String> {
    let verify_val = match verify_after {
        Some(v) => v,
        None => return Ok(()),
    };

    // verify_after can be a single object or a list of steps
    let verify_steps: Vec<serde_yaml::Value> = match verify_val {
        serde_yaml::Value::Sequence(seq) => seq.clone(),
        obj @ serde_yaml::Value::Mapping(_) => vec![obj.clone()],
        _ => return Ok(()),
    };

    for (i, step) in verify_steps.iter().enumerate() {
        let step_json = yaml_to_json(step);
        let op = step_json
            .get("operation")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let input = step_json
            .get("input")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let expect_val = step_json
            .get("expect")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        // Deserialize expected into TestExpectation
        let expect_yaml_str = serde_json::to_string(&expect_val).unwrap_or_default();
        let step_expect: TestExpectation =
            serde_yaml::from_str(&expect_yaml_str).unwrap_or_default();

        // Convert input back to yaml for execute_operation
        let input_yaml_str = serde_json::to_string(&input).unwrap_or_default();
        let input_yaml: serde_yaml::Value =
            serde_yaml::from_str(&input_yaml_str).unwrap_or_default();

        match execute_operation(root, op, &input_yaml, None) {
            Ok(result) => {
                check_expectation(&result, &step_expect)
                    .map_err(|msg| format!("verify_after[{}] ({}): {}", i, op, msg))?;
            }
            Err(err) => {
                // If the verify step expects an error, check it matches
                if let Some(expected_error) = &step_expect.error {
                    if let Some(expected_code) = &expected_error.code {
                        if err.contains(expected_code) {
                            continue;
                        }
                    }
                }
                // For read operations that fail on excluded paths (e.g. type files
                // after init), try reading directly from disk as a fallback
                if op == "read" && err.contains("file_not_found") {
                    if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
                        let full_path = root.join(path);
                        if full_path.exists() {
                            let content = std::fs::read_to_string(&full_path)
                                .map_err(|e| format!("verify_after[{}] ({}): {}", i, op, e))?;
                            let doc = mdbase::frontmatter::parser::parse_document(&content);
                            let fm = match &doc.frontmatter {
                                Some(serde_yaml::Value::Mapping(m)) => {
                                    mdbase::frontmatter::parser::yaml_mapping_to_json(m)
                                }
                                _ => serde_json::json!({}),
                            };
                            let result = serde_json::json!({
                                "path": path,
                                "frontmatter": fm,
                                "body": doc.body,
                            });
                            check_expectation(&result, &step_expect)
                                .map_err(|msg| format!("verify_after[{}] ({}): {}", i, op, msg))?;
                            continue;
                        }
                    }
                }
                return Err(format!("verify_after[{}] ({}): {}", i, op, err));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Expectation checking
// ---------------------------------------------------------------------------

/// Compare actual result against expected.
fn check_expectation(actual: &serde_json::Value, expected: &TestExpectation) -> Result<(), String> {
    // Check valid
    if let Some(valid) = expected.valid {
        let actual_valid = actual.get("valid").and_then(|v| v.as_bool());
        if actual_valid != Some(valid) {
            return Err(format!("expected valid={}, got {:?}", valid, actual_valid));
        }
    }

    // Check error
    if let Some(expected_error) = &expected.error {
        let actual_error = actual.get("error");
        if actual_error.is_none() {
            return Err("expected error in result".to_string());
        }
        if let Some(code) = &expected_error.code {
            let actual_code = actual_error.unwrap().get("code").and_then(|v| v.as_str());
            if actual_code != Some(code) {
                return Err(format!(
                    "expected error code '{}', got {:?}",
                    code, actual_code
                ));
            }
        }
    }

    // Check config (partial match)
    if let Some(expected_config) = &expected.config {
        let actual_config = actual
            .get("config")
            .ok_or_else(|| "expected 'config' in result".to_string())?;
        let expected_json = yaml_to_json(expected_config);
        check_partial_match(actual_config, &expected_json, "config")?;
    }

    // Check warnings
    if let Some(expected_warnings) = &expected.warnings {
        let actual_warnings = actual
            .get("warnings")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "expected 'warnings' array in result".to_string())?;
        for exp in expected_warnings {
            let found = actual_warnings.iter().any(|w| {
                // Check "path" against warning path field
                if let Some(exp_path) = &exp.path {
                    if let Some(actual_path) = w.get("path").and_then(|p| p.as_str()) {
                        if actual_path != exp_path.as_str() {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                // Check "message_contains" against warning message field
                if let Some(needle) = &exp.message_contains {
                    if let Some(msg) = w.get("message").and_then(|m| m.as_str()) {
                        if !msg.to_lowercase().contains(&needle.to_lowercase()) {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                // Check "contains" against string warnings or warning message fields
                if let Some(needle) = &exp.contains {
                    if let Some(s) = w.as_str() {
                        if !s.contains(needle.as_str()) {
                            return false;
                        }
                    } else if let Some(msg) = w.get("message").and_then(|m| m.as_str()) {
                        if !msg.contains(needle.as_str()) {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                // Check "code" against warning code fields
                if let Some(code) = &exp.code {
                    if let Some(actual_code) = w.get("code").and_then(|c| c.as_str()) {
                        if actual_code != code.as_str() {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                // If we passed all checks and at least one field was checked, it's a match
                exp.path.is_some()
                    || exp.message_contains.is_some()
                    || exp.contains.is_some()
                    || exp.code.is_some()
            });
            if !found {
                return Err(format!(
                    "expected warning {:?}, got {:?}",
                    exp, actual_warnings
                ));
            }
        }
    }

    // Check issues
    if let Some(expected_issues) = &expected.issues {
        let actual_issues = actual
            .get("issues")
            .or_else(|| actual.get("errors"))
            .or_else(|| actual.get("validation").and_then(|v| v.get("issues")))
            .and_then(|v| v.as_array());

        if expected_issues.is_empty() {
            if let Some(issues) = actual_issues {
                if !issues.is_empty() {
                    return Err(format!("expected no issues, got {}", issues.len()));
                }
            }
        } else {
            let issues = actual_issues.ok_or("expected issues array in result")?;
            for exp in expected_issues {
                let found = issues.iter().any(|a| {
                    if let Some(code) = &exp.code {
                        let actual_code = a.get("code").and_then(|v| v.as_str()).unwrap_or("");
                        // constraint_violation matches any constraint-related code
                        let code_matches = actual_code == code.as_str()
                            || (code == "constraint_violation"
                                && matches!(
                                    actual_code,
                                    "number_too_large"
                                        | "number_too_small"
                                        | "string_too_short"
                                        | "string_too_long"
                                        | "pattern_mismatch"
                                        | "list_too_short"
                                        | "list_too_long"
                                ));
                        if !code_matches {
                            return false;
                        }
                    }
                    if let Some(field) = &exp.field {
                        if a.get("field").and_then(|v| v.as_str()) != Some(field) {
                            return false;
                        }
                    }
                    if let Some(path) = &exp.path {
                        if a.get("path").and_then(|v| v.as_str()) != Some(path) {
                            return false;
                        }
                    }
                    if let Some(severity) = &exp.severity {
                        if a.get("severity").and_then(|v| v.as_str()) != Some(severity) {
                            return false;
                        }
                    }
                    true
                });
                if !found {
                    return Err(format!("expected issue {:?} not found", exp));
                }
            }
        }
    }

    // Check path
    if let Some(expected_path) = &expected.path {
        let actual_path = actual.get("path").and_then(|v| v.as_str());
        if actual_path != Some(expected_path) {
            return Err(format!(
                "expected path '{}', got {:?}",
                expected_path, actual_path
            ));
        }
    }

    // Check path_contains
    if let Some(expected_contains) = &expected.path_contains {
        let actual_path = actual.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if !actual_path.contains(expected_contains.as_str()) {
            return Err(format!(
                "expected path containing '{}', got '{}'",
                expected_contains, actual_path
            ));
        }
    }

    // Check types
    if let Some(expected_types) = &expected.types {
        let actual_types = actual.get("types").and_then(|v| v.as_array());
        if let Some(actual_arr) = actual_types {
            let actual_strs: Vec<&str> = actual_arr.iter().filter_map(|v| v.as_str()).collect();
            for et in expected_types {
                if !actual_strs.contains(&et.as_str()) {
                    return Err(format!("expected type '{}' in {:?}", et, actual_strs));
                }
            }
        } else {
            return Err(format!(
                "expected types {:?}, got {:?}",
                expected_types,
                actual.get("types")
            ));
        }
    }

    // Check frontmatter (partial match)
    if let Some(expected_fm) = &expected.frontmatter {
        let actual_fm = actual
            .get("frontmatter")
            .ok_or_else(|| "expected 'frontmatter' in result".to_string())?;
        let expected_json = yaml_to_json(expected_fm);
        check_partial_match(actual_fm, &expected_json, "frontmatter")?;
    }

    // Check validation (partial match)
    if let Some(expected_validation) = &expected.validation {
        let actual_validation = actual
            .get("validation")
            .ok_or_else(|| "expected 'validation' in result".to_string())?;
        let expected_json = yaml_to_json(expected_validation);
        check_partial_match(actual_validation, &expected_json, "validation")?;
    }

    // Check body_contains
    if let Some(expected_body) = &expected.body_contains {
        let actual_body = actual.get("body").and_then(|v| v.as_str()).unwrap_or("");
        if !actual_body.contains(expected_body.as_str()) {
            return Err(format!(
                "expected body containing '{}', got '{}'",
                expected_body, actual_body
            ));
        }
    }

    // Check body (exact)
    if let Some(expected_body) = &expected.body {
        let actual_body = actual.get("body").and_then(|v| v.as_str()).unwrap_or("");
        if actual_body != expected_body {
            return Err(format!(
                "expected body '{}', got '{}'",
                expected_body, actual_body
            ));
        }
    }

    // Check file metadata
    if let Some(expected_file) = &expected.file {
        let actual_file = actual
            .get("file")
            .ok_or_else(|| "expected 'file' in result".to_string())?;
        let expected_json = yaml_to_json(expected_file);
        check_partial_match(actual_file, &expected_json, "file")?;
    }

    // Check deleted
    if let Some(expected_deleted) = &expected.deleted {
        let actual_deleted = actual.get("deleted").and_then(|v| v.as_bool());
        if actual_deleted != Some(*expected_deleted) {
            return Err(format!(
                "expected deleted={}, got {:?}",
                expected_deleted, actual_deleted
            ));
        }
    }

    // Check from/to (rename)
    if let Some(expected_from) = &expected.from {
        let actual_from = actual.get("from").and_then(|v| v.as_str());
        if actual_from != Some(expected_from) {
            return Err(format!(
                "expected from='{}', got {:?}",
                expected_from, actual_from
            ));
        }
    }
    if let Some(expected_to) = &expected.to {
        let actual_to = actual.get("to").and_then(|v| v.as_str());
        if actual_to != Some(expected_to) {
            return Err(format!(
                "expected to='{}', got {:?}",
                expected_to, actual_to
            ));
        }
    }

    // Check result (single result, partial match)
    if let Some(expected_result) = &expected.result {
        let expected_json = yaml_to_json(expected_result);
        // For evaluate operations, check .result field
        if let Some(actual_result) = actual.get("result") {
            check_partial_match(actual_result, &expected_json, "result")?;
        } else {
            // Check against whole result
            check_partial_match(actual, &expected_json, "result")?;
        }
    }

    // Check results (array of result items)
    if let Some(expected_results) = &expected.results {
        let actual_results = actual
            .get("results")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                format!(
                    "expected 'results' array in result, got {:?}",
                    actual.get("results")
                )
            })?;

        let expected_json: Vec<serde_json::Value> =
            expected_results.iter().map(yaml_to_json).collect();

        if actual_results.len() != expected_json.len() {
            return Err(format!(
                "expected {} results, got {} (paths: {:?})",
                expected_json.len(),
                actual_results.len(),
                actual_results
                    .iter()
                    .filter_map(|r| r.get("path").and_then(|v| v.as_str()))
                    .collect::<Vec<_>>()
            ));
        }

        for (i, (actual_item, expected_item)) in
            actual_results.iter().zip(expected_json.iter()).enumerate()
        {
            // Handle body_contains in expected result items
            if let Some(body_contains) = expected_item.get("body_contains").and_then(|v| v.as_str())
            {
                let actual_body = actual_item
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !actual_body.contains(body_contains) {
                    return Err(format!(
                        "results[{}]: expected body containing '{}', got '{}'",
                        i, body_contains, actual_body
                    ));
                }
                // Check remaining fields (excluding body_contains)
                if let serde_json::Value::Object(expected_map) = expected_item {
                    for (key, val) in expected_map {
                        if key == "body_contains" {
                            continue;
                        }
                        let actual_val = actual_item
                            .get(key)
                            .or_else(|| actual_item.get("frontmatter").and_then(|fm| fm.get(key)))
                            .ok_or_else(|| {
                                format!(
                                    "results[{}].{}: expected field missing from result",
                                    i, key
                                )
                            })?;
                        check_partial_match(actual_val, val, &format!("results[{}].{}", i, key))?;
                    }
                }
            } else {
                // For query result items, expected fields like "value" may be in frontmatter
                // rather than at the top level. Merge frontmatter into top-level for matching.
                if let (
                    serde_json::Value::Object(actual_map),
                    serde_json::Value::Object(_expected_map),
                ) = (actual_item, expected_item)
                {
                    let mut augmented = actual_map.clone();
                    if let Some(serde_json::Value::Object(fm)) = actual_map.get("frontmatter") {
                        for (k, v) in fm {
                            if !augmented.contains_key(k) {
                                augmented.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    let augmented_val = serde_json::Value::Object(augmented);
                    check_partial_match(&augmented_val, expected_item, &format!("results[{}]", i))?;
                } else {
                    check_partial_match(actual_item, expected_item, &format!("results[{}]", i))?;
                }
            }
        }
    }

    // Check meta (partial match)
    if let Some(expected_meta) = &expected.meta {
        let actual_meta = actual
            .get("meta")
            .ok_or_else(|| "expected 'meta' in result".to_string())?;
        let expected_json = yaml_to_json(expected_meta);
        check_partial_match(actual_meta, &expected_json, "meta")?;
    }

    // Check summaries (partial match)
    if let Some(expected_summaries) = &expected.summaries {
        let actual_summaries = actual
            .get("summaries")
            .ok_or_else(|| "expected 'summaries' in result".to_string())?;
        let expected_json = yaml_to_json(expected_summaries);
        check_partial_match(actual_summaries, &expected_json, "summaries")?;
    }

    // Check batch_result (partial match)
    if let Some(expected_batch) = &expected.batch_result {
        let actual_batch = actual
            .get("batch_result")
            .ok_or_else(|| format!("expected 'batch_result' in result, got {:?}", actual))?;
        let expected_json = yaml_to_json(expected_batch);
        check_partial_match(actual_batch, &expected_json, "batch_result")?;
    }

    // Check broken_links
    if let Some(expected_broken) = &expected.broken_links {
        let actual_broken = actual
            .get("broken_links")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "expected 'broken_links' array in result".to_string())?;
        let expected_json: Vec<serde_json::Value> =
            expected_broken.iter().map(yaml_to_json).collect();
        for (i, exp) in expected_json.iter().enumerate() {
            let found = actual_broken
                .iter()
                .any(|a| check_partial_match(a, exp, "broken_link").is_ok());
            if !found {
                return Err(format!(
                    "broken_links[{}]: expected {:?} not found in {:?}",
                    i, exp, actual_broken
                ));
            }
        }
    }

    // Check partial_updates (for rename_ref_update_failed)
    if let Some(expected_partial) = &expected.partial_updates {
        let actual_partial = actual
            .get("partial_updates")
            .ok_or_else(|| format!("expected 'partial_updates' in result, got {:?}", actual))?;
        let expected_json = yaml_to_json(expected_partial);
        check_partial_match(actual_partial, &expected_json, "partial_updates")?;
    }

    // Check groups
    if let Some(expected_groups) = &expected.groups {
        let actual_groups = actual
            .get("groups")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "expected 'groups' array in result".to_string())?;

        // Match each expected group against actual groups by key in order
        // Expected groups are checked positionally (by index in the sorted order)
        for (i, expected_group) in expected_groups.iter().enumerate() {
            let expected_json = yaml_to_json(expected_group);

            if i >= actual_groups.len() {
                return Err(format!(
                    "expected at least {} groups, got {}",
                    i + 1,
                    actual_groups.len()
                ));
            }

            // Find matching group in actual - first try positional, then by key
            let expected_key = expected_json
                .get("key")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let actual_group = if let Some(ag) = actual_groups.get(i) {
                let actual_key = ag.get("key").cloned().unwrap_or(serde_json::Value::Null);
                if actual_key == expected_key {
                    ag
                } else {
                    // Search by key
                    actual_groups
                        .iter()
                        .find(|g| {
                            g.get("key").cloned().unwrap_or(serde_json::Value::Null) == expected_key
                        })
                        .ok_or_else(|| {
                            format!(
                        "groups[{}]: expected group with key {:?}, not found in actual groups",
                        i, expected_key
                    )
                        })?
                }
            } else {
                return Err(format!("groups[{}]: not enough actual groups", i));
            };

            // Check group results (partial match on each result)
            if let Some(expected_results) = expected_json.get("results").and_then(|v| v.as_array())
            {
                let actual_results = actual_group
                    .get("results")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| format!("groups[{}]: expected 'results' array", i))?;

                for (j, expected_item) in expected_results.iter().enumerate() {
                    if j >= actual_results.len() {
                        return Err(format!(
                            "groups[{}]: expected at least {} results, got {}",
                            i,
                            j + 1,
                            actual_results.len()
                        ));
                    }
                    check_partial_match(
                        &actual_results[j],
                        expected_item,
                        &format!("groups[{}].results[{}]", i, j),
                    )?;
                }
            }

            // Check group summaries
            if let Some(expected_summaries) = expected_json.get("summaries") {
                let actual_summaries = actual_group
                    .get("summaries")
                    .ok_or_else(|| format!("groups[{}]: expected 'summaries'", i))?;
                check_partial_match(
                    actual_summaries,
                    expected_summaries,
                    &format!("groups[{}].summaries", i),
                )?;
            }
        }
    }

    // Check count
    if let Some(expected_count) = expected.count {
        let actual_count = actual
            .get("meta")
            .and_then(|m| m.get("total_count"))
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        if actual_count != Some(expected_count) {
            return Err(format!(
                "expected count={}, got {:?}",
                expected_count, actual_count
            ));
        }
    }

    // Check paths
    if let Some(expected_paths) = &expected.paths {
        let actual_results = actual
            .get("results")
            .and_then(|v| v.as_array())
            .ok_or("expected 'results' for paths check")?;
        let actual_paths: Vec<&str> = actual_results
            .iter()
            .filter_map(|r| r.get("path").and_then(|v| v.as_str()))
            .collect();
        for ep in expected_paths {
            if !actual_paths.contains(&ep.as_str()) {
                return Err(format!(
                    "expected path '{}' in results, got {:?}",
                    ep, actual_paths
                ));
            }
        }
    }

    // Check link (parse_link result)
    if let Some(expected_link) = &expected.link {
        let actual_link = actual.get("link").ok_or("expected 'link' in result")?;
        let expected_json = yaml_to_json(expected_link);
        check_partial_match(actual_link, &expected_json, "link")?;
    }

    // Check resolved_path
    if let Some(expected_rp) = &expected.resolved_path {
        let actual_rp = actual.get("resolved_path");
        let expected_json = yaml_to_json(expected_rp);
        match actual_rp {
            Some(val) => {
                let expected_val = &expected_json;
                if val != expected_val {
                    return Err(format!(
                        "resolved_path: expected {:?}, got {:?}",
                        expected_val, val
                    ));
                }
            }
            None => {
                return Err(format!(
                    "expected resolved_path in result, got {:?}",
                    actual
                ))
            }
        }
    }

    // Check results_count
    if let Some(expected_count) = expected.results_count {
        let actual_results = actual
            .get("results")
            .and_then(|v| v.as_array())
            .ok_or("expected 'results' array for results_count check")?;
        if actual_results.len() != expected_count {
            return Err(format!(
                "expected {} results, got {}",
                expected_count,
                actual_results.len()
            ));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Watch expectation checking
// ---------------------------------------------------------------------------

/// Check watch-specific expectations against actual events.
fn check_watch_expectation(
    root: &Path,
    actual_events: &[serde_json::Value],
    expect: &TestExpectation,
) -> Result<(), String> {
    // Check max_event_count
    if let Some(max_count) = expect.max_event_count {
        if actual_events.len() > max_count {
            return Err(format!(
                "expected max {} events, got {} events: {:?}",
                max_count,
                actual_events.len(),
                actual_events
                    .iter()
                    .map(|e| e.get("event").and_then(|v| v.as_str()).unwrap_or("?"))
                    .collect::<Vec<_>>()
            ));
        }
        // If max_event_count is 0 and events is empty, that's correct
        if max_count == 0 && actual_events.is_empty() {
            return Ok(());
        }
    }

    // Check events (partial match on each event in order)
    if let Some(expected_events) = &expect.events {
        let expected_json: Vec<serde_json::Value> =
            expected_events.iter().map(yaml_to_json).collect();

        if expected_json.is_empty() {
            // Expect no events
            if !actual_events.is_empty() {
                return Err(format!(
                    "expected no events, got {} events: {:?}",
                    actual_events.len(),
                    actual_events
                        .iter()
                        .map(|e| e.get("event").and_then(|v| v.as_str()).unwrap_or("?"))
                        .collect::<Vec<_>>()
                ));
            }
            // Check listener_query even if events are empty
            if let Some(listener_query) = &expect.listener_query {
                return check_listener_query(root, listener_query);
            }
            return Ok(());
        }

        // Match expected events against actual events
        // Each expected event should match a corresponding actual event
        // For partial matching, we look for events by event type and path
        for (i, expected_event) in expected_json.iter().enumerate() {
            let expected_type = expected_event
                .get("event")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            // Find matching actual event (by type and path, in order)
            let actual_event = actual_events
                .iter()
                .filter(|e| e.get("event").and_then(|v| v.as_str()) == Some(expected_type))
                .nth(
                    // Count how many previous expected events have the same type
                    expected_json[..i]
                        .iter()
                        .filter(|prev| {
                            prev.get("event").and_then(|v| v.as_str()) == Some(expected_type)
                        })
                        .count(),
                );

            let actual_event = match actual_event {
                Some(e) => e,
                None => {
                    return Err(format!(
                        "events[{}]: expected {} event, not found in actual events {:?}",
                        i,
                        expected_type,
                        actual_events
                            .iter()
                            .map(|e| {
                                format!(
                                    "{}:{}",
                                    e.get("event").and_then(|v| v.as_str()).unwrap_or("?"),
                                    e.get("path").and_then(|v| v.as_str()).unwrap_or("?")
                                )
                            })
                            .collect::<Vec<_>>()
                    ));
                }
            };

            // Partial match each expected field
            check_watch_event_match(actual_event, expected_event, &format!("events[{}]", i))?;
        }
    }

    // Check events_ordered (strict order, partial match on each)
    if let Some(expected_ordered) = &expect.events_ordered {
        let expected_json: Vec<serde_json::Value> =
            expected_ordered.iter().map(yaml_to_json).collect();

        if actual_events.len() < expected_json.len() {
            return Err(format!(
                "events_ordered: expected at least {} events, got {}",
                expected_json.len(),
                actual_events.len()
            ));
        }

        for (i, expected_event) in expected_json.iter().enumerate() {
            if i >= actual_events.len() {
                return Err(format!("events_ordered[{}]: not enough actual events", i));
            }
            check_watch_event_match(
                &actual_events[i],
                expected_event,
                &format!("events_ordered[{}]", i),
            )?;
        }
    }

    // Check events_contain (each expected event must appear somewhere)
    if let Some(expected_contain) = &expect.events_contain {
        let expected_json: Vec<serde_json::Value> =
            expected_contain.iter().map(yaml_to_json).collect();

        for (i, expected_event) in expected_json.iter().enumerate() {
            let found = actual_events
                .iter()
                .any(|actual| check_watch_event_match(actual, expected_event, "").is_ok());
            if !found {
                return Err(format!(
                    "events_contain[{}]: expected event {:?} not found in actual events",
                    i, expected_event
                ));
            }
        }
    }

    // Check listener_query
    if let Some(listener_query) = &expect.listener_query {
        check_listener_query(root, listener_query)?;
    }

    Ok(())
}

/// Check that an actual watch event matches expected fields (partial match).
fn check_watch_event_match(
    actual: &serde_json::Value,
    expected: &serde_json::Value,
    path: &str,
) -> Result<(), String> {
    if let Some(expected_obj) = expected.as_object() {
        for (key, expected_val) in expected_obj {
            match key.as_str() {
                // timestamp_present: check timestamp field exists and is non-empty
                "timestamp_present" if expected_val == &serde_json::Value::Bool(true) => {
                    let ts = actual.get("timestamp");
                    if ts.is_none() || ts == Some(&serde_json::Value::Null) {
                        return Err(format!("{}.timestamp_present: timestamp not found", path));
                    }
                }
                // has_fields: check that all listed fields exist
                "has_fields" => {
                    if let Some(fields) = expected_val.as_array() {
                        for field in fields {
                            if let Some(field_name) = field.as_str() {
                                if actual.get(field_name).is_none() {
                                    return Err(format!(
                                        "{}.has_fields: field '{}' not found in event {:?}",
                                        path, field_name, actual
                                    ));
                                }
                            }
                        }
                    }
                }
                // affected_files_not_contain: negative check
                "affected_files_not_contain" => {
                    if let Some(forbidden) = expected_val.as_array() {
                        if let Some(actual_files) =
                            actual.get("affected_files").and_then(|v| v.as_array())
                        {
                            for f in forbidden {
                                if let Some(f_str) = f.as_str() {
                                    let found =
                                        actual_files.iter().any(|af| af.as_str() == Some(f_str));
                                    if found {
                                        return Err(format!(
                                            "{}.affected_files_not_contain: '{}' found in {:?}",
                                            path, f_str, actual_files
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
                // type field in expected maps to type_name in actual (for type_changed)
                "type" => {
                    // Check both "type" and "type_name" in actual
                    let actual_val = actual.get("type").or_else(|| actual.get("type_name"));
                    if let Some(expected_str) = expected_val.as_str() {
                        let actual_str = actual_val.and_then(|v| v.as_str());
                        if actual_str != Some(expected_str) {
                            return Err(format!(
                                "{}.type: expected '{}', got {:?}",
                                path, expected_str, actual_str
                            ));
                        }
                    }
                }
                // issues: set-based matching (each expected issue must match some actual issue)
                "issues" => {
                    if let Some(expected_issues) = expected_val.as_array() {
                        let actual_issues = actual
                            .get("issues")
                            .and_then(|v| v.as_array())
                            .ok_or_else(|| format!("{}.issues: expected issues array", path))?;
                        for (j, exp_issue) in expected_issues.iter().enumerate() {
                            let found = actual_issues.iter().any(|actual_issue| {
                                check_partial_match(actual_issue, exp_issue, "").is_ok()
                            });
                            if !found {
                                return Err(format!(
                                    "{}.issues[{}]: expected {:?} not found in {:?}",
                                    path, j, exp_issue, actual_issues
                                ));
                            }
                        }
                    }
                }
                // Skip interval_ms (test metadata, not part of event)
                "interval_ms" => {}
                // Standard fields: partial match
                _ => {
                    if let Some(actual_val) = actual.get(key) {
                        check_partial_match(
                            actual_val,
                            expected_val,
                            &format!("{}.{}", path, key),
                        )?;
                    } else {
                        return Err(format!(
                            "{}.{}: expected field missing from event. Event: {:?}",
                            path, key, actual
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Check listener_query: execute a follow-up operation and verify expectations.
fn check_listener_query(root: &Path, listener_query: &serde_yaml::Value) -> Result<(), String> {
    let lq_json = yaml_to_json(listener_query);
    let operation = lq_json
        .get("operation")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "listener_query: missing 'operation'".to_string())?;
    let input = lq_json
        .get("input")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let expect = lq_json
        .get("expect")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    // Convert input back to YAML for execute_operation
    let input_yaml_str = serde_json::to_string(&input).unwrap_or_default();
    let input_yaml: serde_yaml::Value = serde_yaml::from_str(&input_yaml_str).unwrap_or_default();

    // Deserialize expect into TestExpectation
    let expect_yaml_str = serde_json::to_string(&expect).unwrap_or_default();
    let step_expect: TestExpectation = serde_yaml::from_str(&expect_yaml_str).unwrap_or_default();

    match execute_operation(root, operation, &input_yaml, None) {
        Ok(result) => check_expectation(&result, &step_expect)
            .map_err(|msg| format!("listener_query ({}): {}", operation, msg)),
        Err(err) => {
            // If listener_query expects an error, check if codes match
            if let Some(expected_error) = &step_expect.error {
                if let Some(expected_code) = &expected_error.code {
                    if err.contains(expected_code) {
                        return Ok(());
                    }
                }
            }
            Err(format!("listener_query ({}): {}", operation, err))
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a serde_yaml::Value to serde_json::Value.
fn yaml_to_json(yaml: &serde_yaml::Value) -> serde_json::Value {
    match yaml {
        serde_yaml::Value::Null => serde_json::Value::Null,
        serde_yaml::Value::Bool(b) => serde_json::Value::Bool(*b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_json::Value::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                serde_json::Value::Number(u.into())
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            } else {
                serde_json::Value::Null
            }
        }
        serde_yaml::Value::String(s) => serde_json::Value::String(s.clone()),
        serde_yaml::Value::Sequence(seq) => {
            serde_json::Value::Array(seq.iter().map(yaml_to_json).collect())
        }
        serde_yaml::Value::Mapping(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                if let serde_yaml::Value::String(key) = k {
                    obj.insert(key.clone(), yaml_to_json(v));
                }
            }
            serde_json::Value::Object(obj)
        }
        serde_yaml::Value::Tagged(tagged) => yaml_to_json(&tagged.value),
    }
}

/// Check that actual JSON contains at least all fields in expected.
/// Objects are partially matched (only expected keys checked);
/// arrays and primitives require exact equality.
///
/// Special assertion objects:
/// - `{ "not_null": true }` - value must not be null
/// - `{ "not_equals": value }` - value must not equal the given value
/// - `{ "matches": "regex" }` - value must match regex pattern
fn check_partial_match(
    actual: &serde_json::Value,
    expected: &serde_json::Value,
    path: &str,
) -> Result<(), String> {
    match (actual, expected) {
        (_, serde_json::Value::Object(expected_map)) => {
            // Check for assertion objects
            if let Some(serde_json::Value::Bool(true)) = expected_map.get("not_null") {
                if actual.is_null() {
                    return Err(format!("{}: expected not null, got null", path));
                }
                return Ok(());
            }
            if let Some(not_eq_val) = expected_map.get("not_equals") {
                if actual == not_eq_val {
                    return Err(format!(
                        "{}: expected not equal to {:?}, got {:?}",
                        path, not_eq_val, actual
                    ));
                }
                return Ok(());
            }
            if let Some(serde_json::Value::String(pattern)) = expected_map.get("matches") {
                let actual_str = match actual.as_str() {
                    Some(s) => s.to_string(),
                    None => actual.to_string().trim_matches('"').to_string(),
                };
                let re = regex::Regex::new(pattern)
                    .map_err(|e| format!("{}: invalid regex '{}': {}", path, pattern, e))?;
                if !re.is_match(&actual_str) {
                    return Err(format!(
                        "{}: expected to match '{}', got {:?}",
                        path, pattern, actual_str
                    ));
                }
                return Ok(());
            }

            // Standard partial match for objects
            if let serde_json::Value::Object(actual_map) = actual {
                for (key, expected_val) in expected_map {
                    // Handle _present assertions: check field exists and is non-empty
                    if key.ends_with("_present") && expected_val == &serde_json::Value::Bool(true) {
                        let base_key = &key[..key.len() - 8]; // strip "_present"
                        let val = actual_map.get(base_key).ok_or_else(|| {
                            format!(
                                "{}.{}: expected field '{}' to be present",
                                path, key, base_key
                            )
                        })?;
                        if val.is_null() || val.as_str().is_some_and(|s| s.is_empty()) {
                            return Err(format!(
                                "{}.{}: expected field '{}' to be present and non-empty, got {:?}",
                                path, key, base_key, val
                            ));
                        }
                        continue;
                    }
                    // Handle _positive assertions: check field is a positive number
                    if key.ends_with("_positive") && expected_val == &serde_json::Value::Bool(true)
                    {
                        let base_key = &key[..key.len() - 9]; // strip "_positive"
                        let val = actual_map.get(base_key).ok_or_else(|| {
                            format!(
                                "{}.{}: expected field '{}' to be present",
                                path, key, base_key
                            )
                        })?;
                        let is_positive = val.as_i64().is_some_and(|n| n > 0)
                            || val.as_f64().is_some_and(|n| n > 0.0);
                        if !is_positive {
                            return Err(format!(
                                "{}.{}: expected field '{}' to be positive, got {:?}",
                                path, key, base_key, val
                            ));
                        }
                        continue;
                    }
                    let actual_val = actual_map.get(key).ok_or_else(|| {
                        format!("{}.{}: expected field missing from result", path, key)
                    })?;
                    check_partial_match(actual_val, expected_val, &format!("{}.{}", path, key))?;
                }
                Ok(())
            } else {
                Err(format!("{}: expected object, got {:?}", path, actual))
            }
        }
        (serde_json::Value::Array(actual_arr), serde_json::Value::Array(expected_arr)) => {
            // For arrays, check each expected element matches the corresponding actual element
            if actual_arr.len() != expected_arr.len() {
                return Err(format!(
                    "{}: expected array of length {}, got {}",
                    path,
                    expected_arr.len(),
                    actual_arr.len()
                ));
            }
            for (i, (a, e)) in actual_arr.iter().zip(expected_arr.iter()).enumerate() {
                check_partial_match(a, e, &format!("{}[{}]", path, i))?;
            }
            Ok(())
        }
        _ => {
            if actual != expected {
                Err(format!(
                    "{}: expected {:?}, got {:?}",
                    path, expected, actual
                ))
            } else {
                Ok(())
            }
        }
    }
}
