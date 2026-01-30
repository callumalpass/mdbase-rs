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
    result: Option<serde_yaml::Value>,
    results: Option<Vec<serde_yaml::Value>>,
    count: Option<usize>,
    paths: Option<Vec<String>>,
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
                    let content_str = map.get(&key("content"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let encoding = map.get(&key("encoding"))
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

    // Files: merge (group first, test overrides)
    let mut all_files: HashMap<String, serde_yaml::Value> = HashMap::new();
    if let Some(files) = &group.files {
        for (k, v) in files {
            all_files.insert(k.clone(), v.clone());
        }
    }
    if let Some(files) = &test.files {
        for (k, v) in files {
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
                let content_str = map.get(&key("content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let encoding = map.get(&key("encoding"))
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

            for test_case in &group.tests {
                total += 1;

                // Materialize setup for each test (fresh copy every time)
                let test_tmp = match (&test_case.setup, &group.setup) {
                    (Some(test_setup), Some(group_setup)) => {
                        Some(materialize_merged_setup(group_setup, test_setup))
                    }
                    (Some(test_setup), None) => Some(materialize_setup(test_setup)),
                    (None, Some(group_setup)) => Some(materialize_setup(group_setup)),
                    (None, None) => None,
                };
                let tmp_ref = test_tmp.as_ref();

                // If no setup, create minimal collection
                let fallback_tmp;
                let root = match tmp_ref {
                    Some(tmp) => tmp.path(),
                    None => {
                        let minimal_setup = TestSetup {
                            config: Some("spec_version: \"0.1.0\"\n".to_string()),
                            types: None,
                            files: None,
                        };
                        fallback_tmp = materialize_setup(&minimal_setup);
                        fallback_tmp.path()
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
    input: &serde_yaml::Value,
) -> Result<serde_json::Value, String> {
    let input_json = yaml_to_json(input);

    match operation {
        "load_config" => Ok(mdbase::config::load_config(collection_root)),
        "read" | "create" | "update" | "delete" | "rename" | "validate"
        | "load_types" | "get_types" | "get_type" | "create_type" => {
            let collection_result = mdbase::Collection::open(collection_root);
            let collection = match collection_result {
                Ok(c) => c,
                Err(e) => {
                    // If it's a config/type error, extract the error
                    if let Some(err) = e.get("error") {
                        let code = err.get("code").and_then(|c| c.as_str()).unwrap_or("unknown");
                        let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown");
                        // For load_types/get_type, return as a structured result with valid: false
                        if operation == "load_types" || operation == "get_types" || operation == "get_type" {
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
                "load_types" | "get_types" => {
                    // If a path is provided, determine types for that specific file
                    if let Some(path) = input_json.get("path").and_then(|v| v.as_str()) {
                        let full_path = collection.root.join(path);
                        let frontmatter = if full_path.exists() {
                            let content = fs::read_to_string(&full_path).unwrap_or_default();
                            let doc = mdbase::frontmatter::parser::parse_document(&content);
                            match &doc.frontmatter {
                                Some(serde_yaml::Value::Mapping(m)) =>
                                    mdbase::frontmatter::parser::yaml_mapping_to_json(m),
                                _ => serde_json::json!({}),
                            }
                        } else {
                            serde_json::json!({})
                        };
                        let type_names = collection.determine_types_for_path(&frontmatter, Some(path));
                        serde_json::json!({
                            "valid": true,
                            "types": type_names,
                        })
                    } else {
                        // Return list of all loaded type names
                        let type_names: Vec<String> = collection.types.keys().cloned().collect();
                        let types_detail: Vec<serde_json::Value> = collection.types.values().map(|t| {
                            let mut obj = serde_json::json!({
                                "name": t.name,
                            });
                            if let Some(ref desc) = t.description {
                                obj["description"] = serde_json::Value::String(desc.clone());
                            }
                            if let Some(ref extends) = t.extends {
                                obj["extends"] = serde_json::Value::String(extends.clone());
                            }
                            let fields: serde_json::Map<String, serde_json::Value> = t.fields.iter().map(|(k, v)| {
                                (k.clone(), serde_json::json!({"type": v.field_type}))
                            }).collect();
                            obj["fields"] = serde_json::Value::Object(fields);
                            obj
                        }).collect();
                        let mut result = serde_json::json!({
                            "valid": true,
                            "types": types_detail,
                            "names": type_names,
                            "count": type_names.len(),
                        });
                        if !collection.type_warnings.is_empty() {
                            let warnings: Vec<serde_json::Value> = collection.type_warnings.iter().map(|w| {
                                serde_json::json!({
                                    "message": w,
                                })
                            }).collect();
                            result["warnings"] = serde_json::Value::Array(warnings);
                        }
                        result
                    }
                }
                "get_type" => {
                    let raw_name = input_json.get("type").or(input_json.get("name")).and_then(|v| v.as_str()).unwrap_or("");
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
                                        mdbase::types::schema::GeneratedStrategy::NowOnWrite => "now_on_write",
                                        mdbase::types::schema::GeneratedStrategy::Derived { .. } => "derived",
                                    };
                                    fd["generated"] = serde_json::Value::String(gen_str.to_string());
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
                    let name = input_json.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let fields_input = input_json.get("fields");
                    let parent_input = input_json.get("parent").and_then(|v| v.as_str());
                    let strict_input = input_json.get("strict").and_then(|v| v.as_bool());
                    let types_dir = collection.root.join(&collection.settings.types_folder);
                    let _ = std::fs::create_dir_all(&types_dir);

                    // Validate type name
                    if let Err(_) = mdbase::types::loader::validate_type_name(name) {
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
                    let valid_types = ["string", "integer", "number", "boolean", "date", "datetime", "time",
                                       "enum", "list", "object", "link", "image", "file", "url", "email", "slug"];
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
                        fm_map.insert(
                            serde_yaml::Value::String("fields".to_string()),
                            yaml_fields,
                        );
                    }
                    let yaml_str = serde_yaml::to_string(&serde_yaml::Value::Mapping(fm_map)).unwrap_or_default();
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
                                        let code = err.get("code").and_then(|c| c.as_str()).unwrap_or("invalid_type_definition");
                                        let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("Failed to load type");
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
                _ => unreachable!(),
            };

            // If result contains an error, return it as Err for the test runner
            if let Some(error) = result.get("error") {
                let code = error.get("code").and_then(|c| c.as_str()).unwrap_or("unknown");
                let msg = error.get("message").and_then(|m| m.as_str()).unwrap_or("");
                return Err(format!("{}: {}", code, msg));
            }

            Ok(result)
        }
        "evaluate" => {
            let expression = input_json.get("expression")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "evaluate requires 'expression' field".to_string())?;

            // Parse the expression
            let parsed = match mdbase::expressions::parser::Parser::parse(expression) {
                Ok(expr) => expr,
                Err(e) => {
                    return Ok(serde_json::json!({
                        "error": { "code": "invalid_expression", "message": e }
                    }));
                }
            };

            // Build evaluation context
            let ctx = if let Some(path_val) = input_json.get("path").and_then(|v| v.as_str()) {
                // Read the file to get frontmatter context
                let collection = mdbase::Collection::open(collection_root)
                    .map_err(|e| format!("Failed to open collection: {}", e))?;
                let read_result = collection.read(&serde_json::json!({"path": path_val}));
                let frontmatter = read_result
                    .get("frontmatter")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                let body = read_result
                    .get("body")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                mdbase::expressions::evaluator::EvalContext {
                    frontmatter,
                    file_path: Some(path_val.to_string()),
                    body,
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
            .or_else(|| actual.get("validation").and_then(|v| v.get("issues")))
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
                    return Err(format!(
                        "expected type '{}' in {:?}",
                        et, actual_strs
                    ));
                }
            }
        } else {
            return Err(format!(
                "expected types {:?}, got {:?}",
                expected_types, actual.get("types")
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
                            format!("{}.{}: expected field '{}' to be present", path, key, base_key)
                        })?;
                        if val.is_null() || val.as_str().map_or(false, |s| s.is_empty()) {
                            return Err(format!(
                                "{}.{}: expected field '{}' to be present and non-empty, got {:?}",
                                path, key, base_key, val
                            ));
                        }
                        continue;
                    }
                    // Handle _positive assertions: check field is a positive number
                    if key.ends_with("_positive") && expected_val == &serde_json::Value::Bool(true) {
                        let base_key = &key[..key.len() - 9]; // strip "_positive"
                        let val = actual_map.get(base_key).ok_or_else(|| {
                            format!("{}.{}: expected field '{}' to be present", path, key, base_key)
                        })?;
                        let is_positive = val.as_i64().map_or(false, |n| n > 0)
                            || val.as_f64().map_or(false, |n| n > 0.0);
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
                    check_partial_match(
                        actual_val,
                        expected_val,
                        &format!("{}.{}", path, key),
                    )?;
                }
                Ok(())
            } else {
                Err(format!(
                    "{}: expected object, got {:?}",
                    path, actual
                ))
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
