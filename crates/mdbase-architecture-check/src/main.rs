use regex::Regex;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArchitectureBudgets {
    rust_source_file_count_max: usize,
    rust_source_line_count_max: usize,
    rust_source_file_max_lines: usize,
    legacy_file_line_budgets: BTreeMap<String, usize>,
    dead_code_allowances_by_file: BTreeMap<String, usize>,
    transitional_reference_budgets: TransitionalReferenceBudgets,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TransitionalReferenceBudgets {
    unchecked_collection_scans: usize,
    v03_operation_facade: usize,
}

struct DebtPatterns {
    dead_code: Regex,
    unchecked_collection_scan: Regex,
    v03_operation_facade: Regex,
}

impl DebtPatterns {
    fn new() -> Self {
        Self {
            // These intentionally count lexical identifier references rather than only parsed
            // calls. Macro token streams, metavariable arguments, cfg_attr, comments, strings,
            // definitions, and comment-separated calls therefore consume budget. The
            // conservative false positives make transitional surface growth visible.
            dead_code: Regex::new(r"\bdead_code\b").unwrap(),
            unchecked_collection_scan: Regex::new(r"\bscan_collection_files\b").unwrap(),
            v03_operation_facade: Regex::new(r"\bv03_operations\b").unwrap(),
        }
    }
}

fn workspace_source_roots(root: &Path) -> Vec<PathBuf> {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--locked", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .output()
        .expect("run cargo metadata for workspace source discovery");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse cargo metadata");
    let members: BTreeSet<_> = metadata["workspace_members"]
        .as_array()
        .expect("workspace_members array")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    let mut roots = Vec::new();
    for package in metadata["packages"].as_array().expect("packages array") {
        let Some(id) = package["id"].as_str() else {
            continue;
        };
        if !members.contains(id) {
            continue;
        }
        let manifest = PathBuf::from(
            package["manifest_path"]
                .as_str()
                .expect("workspace package manifest_path"),
        );
        roots.push(manifest.parent().expect("manifest parent").join("src"));
    }
    roots.sort();
    roots.dedup();
    roots
}

fn collect_rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let source_roots = workspace_source_roots(root);
    for source_root in source_roots {
        if !source_root.is_dir() {
            continue;
        }
        for entry in WalkDir::new(source_root) {
            let entry = entry.expect("walk Rust workspace sources");
            if entry.file_type().is_file()
                && entry.path().extension().and_then(|value| value.to_str()) == Some("rs")
            {
                files.push(entry.into_path());
            }
        }
    }
    files.sort();
    files
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .expect("source path is inside repository")
        .to_string_lossy()
        .replace('\\', "/")
}

fn match_count(pattern: &Regex, source: &str) -> usize {
    pattern.find_iter(source).count()
}

fn check_architecture(root: &Path) -> Result<String, Vec<String>> {
    let budget_path = root.join("config/architecture-budgets.json");
    let budgets: ArchitectureBudgets = serde_json::from_slice(
        &fs::read(&budget_path).expect("read config/architecture-budgets.json"),
    )
    .expect("parse config/architecture-budgets.json");
    let files = collect_rust_sources(root);
    let patterns = DebtPatterns::new();
    let mut failures = Vec::new();

    if budgets.rust_source_file_max_lines == 0 {
        failures.push("rustSourceFileMaxLines must be positive".to_string());
    }
    if files.len() > budgets.rust_source_file_count_max {
        failures.push(format!(
            "workspace Rust file count is {}; budget is {}",
            files.len(),
            budgets.rust_source_file_count_max
        ));
    }

    let mut measured_files = BTreeSet::new();
    let mut total_lines = 0;
    let mut unchecked_scan_references = 0;
    let mut v03_facade_references = 0;
    let mut measured_dead_code = BTreeMap::new();

    for file in &files {
        let relative = relative_path(root, file);
        let source = fs::read_to_string(file).expect("read workspace Rust source");
        let lines = source.lines().count();
        total_lines += lines;
        measured_files.insert(relative.clone());

        let maximum = budgets
            .legacy_file_line_budgets
            .get(&relative)
            .copied()
            .unwrap_or(budgets.rust_source_file_max_lines);
        if lines > maximum {
            failures.push(format!("{relative} has {lines} lines; budget is {maximum}"));
        }

        // The checker names the debt it detects; do not count those names as product debt.
        if !relative.starts_with("crates/mdbase-architecture-check/") {
            let allowances = match_count(&patterns.dead_code, &source);
            if allowances > 0 {
                measured_dead_code.insert(relative.clone(), allowances);
            }
            unchecked_scan_references += match_count(&patterns.unchecked_collection_scan, &source);
            v03_facade_references += match_count(&patterns.v03_operation_facade, &source);
        }
    }

    if total_lines > budgets.rust_source_line_count_max {
        failures.push(format!(
            "workspace Rust line count is {total_lines}; budget is {}",
            budgets.rust_source_line_count_max
        ));
    }

    for (file, maximum) in &budgets.legacy_file_line_budgets {
        if *maximum <= budgets.rust_source_file_max_lines {
            failures.push(format!(
                "{file} has an invalid legacy budget {maximum}; exceptions must exceed {}",
                budgets.rust_source_file_max_lines
            ));
        }
        if !measured_files.contains(file) {
            failures.push(format!(
                "{file} has a legacy budget but is not a workspace Rust source"
            ));
        }
    }

    for (file, actual) in &measured_dead_code {
        match budgets.dead_code_allowances_by_file.get(file) {
            Some(maximum) if actual <= maximum => {}
            Some(maximum) => failures.push(format!(
                "{file} has {actual} dead-code references; budget is {maximum}"
            )),
            None => failures.push(format!(
                "{file} introduces {actual} unregistered dead-code reference(s)"
            )),
        }
    }
    for file in budgets.dead_code_allowances_by_file.keys() {
        if !measured_files.contains(file) {
            failures.push(format!(
                "{file} has a dead-code budget but is not a workspace Rust source"
            ));
        }
    }

    if unchecked_scan_references
        > budgets
            .transitional_reference_budgets
            .unchecked_collection_scans
    {
        failures.push(format!(
            "unchecked collection scan references are {unchecked_scan_references}; budget is {}",
            budgets
                .transitional_reference_budgets
                .unchecked_collection_scans
        ));
    }
    if v03_facade_references > budgets.transitional_reference_budgets.v03_operation_facade {
        failures.push(format!(
            "v0.3 operation facade references are {v03_facade_references}; budget is {}",
            budgets.transitional_reference_budgets.v03_operation_facade
        ));
    }

    if failures.is_empty() {
        Ok(format!(
            "architecture budgets passed: {} Rust files, {} lines, {} unchecked-scan references, {} v0.3-facade references",
            files.len(), total_lines, unchecked_scan_references, v03_facade_references
        ))
    } else {
        Err(failures)
    }
}

fn main() {
    let default_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("architecture checker is nested under crates/<name>");
    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| default_root.to_path_buf())
        .canonicalize()
        .expect("resolve repository root");
    match check_architecture(&root) {
        Ok(summary) => println!("{summary}"),
        Err(failures) => {
            for failure in failures {
                eprintln!("- {failure}");
            }
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{match_count, DebtPatterns};

    #[test]
    fn lexical_debt_patterns_cover_attributes_ufcs_comments_and_macro_tokens() {
        let patterns = DebtPatterns::new();
        let source = r#"
        #![allow(dead_code)]
        #[allow(unused, dead_code)]
        #[cfg_attr(test, allow(dead_code, unused))]
        collection.v03_operations(/* retained compatibility */);
        Collection::v03_operations(&collection);
        macro_rules! operation { () => { collection.v03_operations() } }
        invoke!(collection, v03_operations);
        collection.v03_operations/**/();
        collection.scan_collection_files();
        Collection::scan_collection_files(&collection);
        collection.scan_collection_files /* gap */ ();
    "#;

        assert_eq!(match_count(&patterns.dead_code, source), 3);
        assert_eq!(match_count(&patterns.v03_operation_facade, source), 5);
        assert_eq!(match_count(&patterns.unchecked_collection_scan, source), 3);
    }
}
