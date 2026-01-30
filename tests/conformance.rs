//! Conformance test runner for mdbase.
//!
//! Reads YAML test files from ~/projects/mdbase-spec/tests/ and executes
//! them against the Rust implementation.

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

#[derive(Debug, Deserialize)]
struct TestSetup {
    config: Option<String>,
    types: Option<HashMap<String, String>>,
    files: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct TestExpectation {
    valid: Option<bool>,
    issues: Option<Vec<ExpectedIssue>>,
    error: Option<ExpectedError>,
    result: Option<serde_yaml::Value>,
    results: Option<Vec<serde_yaml::Value>>,
    count: Option<usize>,
    paths: Option<Vec<String>>,
    frontmatter: Option<serde_yaml::Value>,
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
struct TestCase {
    name: String,
    spec_ref: Option<String>,
    operation: String,
    input: serde_yaml::Value,
    expect: TestExpectation,
}

#[derive(Debug, Deserialize)]
struct TestGroup {
    name: String,
    level: u32,
    category: String,
    spec_ref: String,
    setup: TestSetup,
    tests: Vec<TestCase>,
}

/// Materializes a test setup into a temporary directory.
fn materialize_setup(setup: &TestSetup) -> TempDir {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let root = tmp.path();

    // Write config
    let config_content = setup
        .config
        .as_deref()
        .unwrap_or("spec_version: \"0.1.0\"\n");
    fs::write(root.join("mdbase.yaml"), config_content).unwrap();

    // Write type files
    if let Some(types) = &setup.types {
        let types_dir = root.join("_types");
        fs::create_dir_all(&types_dir).unwrap();
        for (filename, content) in types {
            fs::write(types_dir.join(filename), content).unwrap();
        }
    }

    // Write content files
    if let Some(files) = &setup.files {
        for (file_path, content) in files {
            let full_path = root.join(file_path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(full_path, content).unwrap();
        }
    }

    tmp
}

/// Discover all test YAML files organized by level.
fn discover_tests() -> Vec<(PathBuf, TestGroup)> {
    let tests_dir = spec_tests_dir();
    let mut groups = Vec::new();

    if !tests_dir.exists() {
        return groups;
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
            match serde_yaml::from_str::<TestGroup>(&content) {
                Ok(group) => groups.push((file.path(), group)),
                Err(err) => {
                    eprintln!("Failed to parse {:?}: {}", file.path(), err);
                }
            }
        }
    }

    groups
}

#[test]
fn conformance_tests() {
    let groups = discover_tests();

    if groups.is_empty() {
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

    for (path, group) in &groups {
        let filename = path.file_name().unwrap().to_string_lossy();
        println!(
            "\n=== Level {} | {} ({}) ===",
            group.level, group.name, filename
        );

        let tmp = materialize_setup(&group.setup);

        for test_case in &group.tests {
            total += 1;

            match execute_operation(tmp.path(), &test_case.operation, &test_case.input) {
                Ok(result) => {
                    match check_expectation(&result, &test_case.expect) {
                        Ok(()) => {
                            passed += 1;
                            println!("  ✓ {}", test_case.name);
                        }
                        Err(msg) => {
                            failed += 1;
                            let err = format!(
                                "[{}] {}: {}",
                                filename, test_case.name, msg
                            );
                            println!("  ✗ {}: {}", test_case.name, msg);
                            errors.push(err);
                        }
                    }
                }
                Err(err) => {
                    // If the test expects an error, check if codes match
                    if let Some(expected_error) = &test_case.expect.error {
                        if let Some(expected_code) = &expected_error.code {
                            if err.contains(expected_code) {
                                passed += 1;
                                println!("  ✓ {} (expected error)", test_case.name);
                                continue;
                            }
                        }
                    }
                    failed += 1;
                    let msg = format!(
                        "[{}] {}: operation error: {}",
                        filename, test_case.name, err
                    );
                    println!("  ✗ {}: {}", test_case.name, err);
                    errors.push(msg);
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

/// Execute a single test operation against the mdbase implementation.
fn execute_operation(
    _collection_root: &Path,
    operation: &str,
    input: &serde_yaml::Value,
) -> Result<serde_json::Value, String> {
    // TODO: Import and call the actual mdbase API
    Err(format!(
        "Operation '{}' not yet implemented. Input: {:?}",
        operation, input
    ))
}

/// Compare actual result against expected.
fn check_expectation(
    actual: &serde_json::Value,
    expected: &TestExpectation,
) -> Result<(), String> {
    if let Some(valid) = expected.valid {
        let actual_valid = actual.get("valid").and_then(|v| v.as_bool());
        if actual_valid != Some(valid) {
            return Err(format!(
                "expected valid={}, got {:?}",
                valid, actual_valid
            ));
        }
    }

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

    Ok(())
}
