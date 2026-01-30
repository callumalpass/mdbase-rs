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
    let home = std::env::var("HOME").expect("HOME not set");
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
    operation: String,
    #[serde(default)]
    input: serde_yaml::Value,
    #[serde(default)]
    expect: TestExpectation,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TestExpectation {
    valid: Option<bool>,
    issues: Option<Vec<ExpectedIssue>>,
    error: Option<ExpectedError>,
    config: Option<serde_yaml::Value>,
    warnings: Option<Vec<WarningExpectation>>,
    #[allow(dead_code)]
    result: Option<serde_yaml::Value>,
    #[allow(dead_code)]
    results: Option<Vec<serde_yaml::Value>>,
    #[allow(dead_code)]
    count: Option<usize>,
    #[allow(dead_code)]
    paths: Option<Vec<String>>,
    #[allow(dead_code)]
    frontmatter: Option<serde_yaml::Value>,
    #[allow(dead_code)]
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExpectedIssue {
    code: Option<String>,
    field: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExpectedError {
    code: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WarningExpectation {
    contains: Option<String>,
    code: Option<String>,
}

// ---------------------------------------------------------------------------
// Test infrastructure
// ---------------------------------------------------------------------------

/// Materializes a test setup into a temporary directory.
fn materialize_setup(setup: &TestSetup) -> TempDir {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let root = tmp.path();

    // Write config only if explicitly provided (null / absent = don't create)
    if let Some(config_content) = &setup.config {
        fs::write(root.join("mdbase.yaml"), config_content).unwrap();
    }

    // Write type files
    if let Some(types) = &setup.types {
        let types_dir = root.join("_types");
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
            let content = match value {
                serde_yaml::Value::String(s) => s.clone(),
                serde_yaml::Value::Mapping(map) => {
                    let key = serde_yaml::Value::String("content".to_string());
                    map.get(&key)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                }
                _ => String::new(),
            };
            fs::write(full_path, content).unwrap();
        }
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

            // Materialize group-level setup once (shared by tests without own setup)
            let group_tmp = group.setup.as_ref().map(|s| materialize_setup(s));

            for test_case in &group.tests {
                total += 1;

                // Per-test setup overrides group setup
                let test_tmp = test_case.setup.as_ref().map(|s| materialize_setup(s));
                let tmp_ref = test_tmp.as_ref().or(group_tmp.as_ref());

                let root = match tmp_ref {
                    Some(tmp) => tmp.path(),
                    None => {
                        failed += 1;
                        let msg =
                            format!("[{}] {}: no setup provided", filename, test_case.name);
                        println!("    ✗ {}: no setup", test_case.name);
                        errors.push(msg);
                        continue;
                    }
                };

                match execute_operation(root, &test_case.operation, &test_case.input) {
                    Ok(result) => match check_expectation(&result, &test_case.expect) {
                        Ok(()) => {
                            passed += 1;
                            println!("    ✓ {}", test_case.name);
                        }
                        Err(msg) => {
                            failed += 1;
                            let err =
                                format!("[{}] {}: {}", filename, test_case.name, msg);
                            println!("    ✗ {}: {}", test_case.name, msg);
                            errors.push(err);
                        }
                    },
                    Err(err) => {
                        // If the test expects an error, check if codes match
                        if let Some(expected_error) = &test_case.expect.error {
                            if let Some(expected_code) = &expected_error.code {
                                if err.contains(expected_code) {
                                    passed += 1;
                                    println!(
                                        "    ✓ {} (expected error)",
                                        test_case.name
                                    );
                                    continue;
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
    _input: &serde_yaml::Value,
) -> Result<serde_json::Value, String> {
    match operation {
        "load_config" => Ok(mdbase::config::load_config(collection_root)),
        _ => Err(format!(
            "Operation '{}' not yet implemented",
            operation
        )),
    }
}

// ---------------------------------------------------------------------------
// Expectation checking
// ---------------------------------------------------------------------------

/// Compare actual result against expected.
fn check_expectation(
    actual: &serde_json::Value,
    expected: &TestExpectation,
) -> Result<(), String> {
    // Check valid
    if let Some(valid) = expected.valid {
        let actual_valid = actual.get("valid").and_then(|v| v.as_bool());
        if actual_valid != Some(valid) {
            return Err(format!(
                "expected valid={}, got {:?}",
                valid, actual_valid
            ));
        }
    }

    // Check error
    if let Some(expected_error) = &expected.error {
        let actual_error = actual.get("error");
        if actual_error.is_none() {
            return Err("expected error in result".to_string());
        }
        if let Some(code) = &expected_error.code {
            let actual_code = actual_error
                .unwrap()
                .get("code")
                .and_then(|v| v.as_str());
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
                // Check "contains" against string warnings or warning message fields
                if let Some(needle) = &exp.contains {
                    if let Some(s) = w.as_str() {
                        return s.contains(needle.as_str());
                    }
                    if let Some(msg) = w.get("message").and_then(|m| m.as_str()) {
                        return msg.contains(needle.as_str());
                    }
                }
                // Check "code" against warning code fields
                if let Some(code) = &exp.code {
                    if let Some(actual_code) = w.get("code").and_then(|c| c.as_str()) {
                        return actual_code == code.as_str();
                    }
                }
                false
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
            .and_then(|v| v.as_array());

        if expected_issues.is_empty() {
            if let Some(issues) = actual_issues {
                if !issues.is_empty() {
                    return Err(format!("expected no issues, got {}", issues.len()));
                }
            }
        } else {
            let issues = actual_issues
                .ok_or("expected issues array in result")?;
            for exp in expected_issues {
                let found = issues.iter().any(|a| {
                    if let Some(code) = &exp.code {
                        if a.get("code").and_then(|v| v.as_str()) != Some(code) {
                            return false;
                        }
                    }
                    if let Some(field) = &exp.field {
                        if a.get("field").and_then(|v| v.as_str()) != Some(field) {
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

    Ok(())
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
fn check_partial_match(
    actual: &serde_json::Value,
    expected: &serde_json::Value,
    path: &str,
) -> Result<(), String> {
    match (actual, expected) {
        (serde_json::Value::Object(actual_map), serde_json::Value::Object(expected_map)) => {
            for (key, expected_val) in expected_map {
                let actual_val = actual_map.get(key).ok_or_else(|| {
                    format!("{}.{}: expected field missing from result", path, key)
                })?;
                check_partial_match(
                    actual_val,
                    expected_val,
                    &format!("{}.{}", path, key),
                )?;
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
