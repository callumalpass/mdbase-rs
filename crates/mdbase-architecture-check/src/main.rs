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
    legacy_collection_facade_definitions: Vec<String>,
    legacy_compatibility_allowlist: BTreeMap<String, Vec<String>>,
    operation_context_legacy_production: usize,
    operation_context_legacy_support: usize,
    wire_only_variants: Vec<String>,
    wire_only_constructor_allowlist: BTreeMap<String, BTreeMap<String, usize>>,
    ephemeral_result_production: usize,
    ephemeral_result_support: usize,
}

struct DebtPatterns {
    dead_code: Regex,
    unchecked_collection_scan: Regex,
    v03_operation_facade: Regex,
    operation_context_legacy: Regex,
    legacy_compatibility_reference: Regex,
    legacy_facade_definition: Regex,
    wire_only_constructor: Regex,
    ephemeral_result: Regex,
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
            operation_context_legacy: Regex::new(r"\bOperationContext::legacy\s*\(").unwrap(),
            legacy_compatibility_reference: Regex::new(
                r"\b(legacy_mutation|CreateInput|UpdateInput|DeleteInput|RenameInput|CreateOutput|UpdateOutput|DeleteOutput|create_legacy|update_legacy|delete_legacy|rename_legacy|backfill_legacy|batch_update_legacy|batch_delete_legacy)\b|\bCollection::(create|update|delete|rename|backfill|batch_update|batch_delete)\b",
            )
            .unwrap(),
            legacy_facade_definition: Regex::new(
                r"(?s)pub fn (create|update|delete|rename|backfill|batch_update|batch_delete)\s*\(\s*&self,\s*input:\s*&serde_json::Value",
            )
            .unwrap(),
            wire_only_constructor: Regex::new(
                r"\b(validation_wire|view_wire|type_wire|wire_only)\b",
            )
            .unwrap(),
            ephemeral_result: Regex::new(r"\b(outcome|rejection)\.result\b").unwrap(),
        }
    }
}

fn production_source<'a>(relative: &str, source: &'a str) -> &'a str {
    if relative.starts_with("tests/")
        || relative.ends_with("_tests.rs")
        || relative == "src/runtime/tests.rs"
    {
        return "";
    }
    ["#[cfg(test)]\nmod tests", "#[cfg(test)]\r\nmod tests"]
        .into_iter()
        .filter_map(|marker| source.find(marker))
        .min()
        .map_or(source, |offset| &source[..offset])
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

fn enum_variants(source: &str, name: &str) -> BTreeSet<String> {
    let marker = format!("enum {name}");
    let start = source.find(&marker).expect("guarded enum exists");
    let open = source[start..]
        .find('{')
        .map(|offset| start + offset)
        .unwrap();
    let mut depth = 0usize;
    let mut segment = String::new();
    let variant = Regex::new(r"(?m)^\s*([A-Z][A-Za-z0-9_]*)\s*(?:\(|\{|$)").unwrap();
    let mut variants = BTreeSet::new();
    for character in source[open..].chars() {
        match character {
            '{' => {
                depth += 1;
                if depth > 1 {
                    segment.push(character);
                }
            }
            '}' => {
                if depth == 1 {
                    if let Some(found) = variant.captures(&segment) {
                        variants.insert(found[1].to_string());
                    }
                    break;
                }
                depth -= 1;
                segment.push(character);
            }
            ',' if depth == 1 => {
                if let Some(found) = variant.captures(&segment) {
                    variants.insert(found[1].to_string());
                }
                segment.clear();
            }
            _ if depth >= 1 => segment.push(character),
            _ => {}
        }
    }
    variants
}

fn compatibility_references(pattern: &Regex, source: &str) -> BTreeSet<String> {
    pattern
        .captures_iter(source)
        .filter_map(|capture| {
            capture
                .get(1)
                .map(|value| value.as_str().to_string())
                .or_else(|| {
                    capture
                        .get(2)
                        .map(|value| format!("Collection::{}", value.as_str()))
                })
        })
        .collect()
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
    let mut compatibility_inventory = BTreeMap::new();
    let mut wire_only_constructor_inventory: BTreeMap<String, BTreeMap<String, usize>> =
        BTreeMap::new();
    let mut operation_context_legacy_production = 0;
    let mut operation_context_legacy_support = 0;
    let mut ephemeral_result_production = 0;
    let mut ephemeral_result_support = 0;
    let mut measured_dead_code = BTreeMap::new();

    for file in &files {
        let relative = relative_path(root, file);
        let source = fs::read_to_string(file).expect("read workspace Rust source");
        let production = production_source(&relative, &source);
        let support = source.len() - production.len();
        let support_source = &source[source.len() - support..];
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

        let typed_runtime_caller = matches!(
            relative.as_str(),
            "src/runtime/batch.rs"
                | "src/runtime/cursor.rs"
                | "src/runtime/filesystem.rs"
                | "src/runtime/outcome.rs"
                | "src/runtime/tests.rs"
                | "src/transactions/runtime.rs"
        );
        if typed_runtime_caller && source.contains(".result.result") {
            failures.push(format!(
                "{relative} accesses nested OperationResult JSON; semantic runtime callers must match typed outcomes"
            ));
        }
        if matches!(
            relative.as_str(),
            "src/runtime/batch.rs" | "src/runtime/cursor.rs" | "src/runtime/filesystem.rs"
        ) && (source.contains("recover_v03") || source.contains("from_v03"))
        {
            failures.push(format!(
                "{relative} converts v0.3 results in a typed execution path; only journal recovery may decode old outcomes"
            ));
        }
        let typed_hosted_caller = matches!(
            relative.as_str(),
            "src/runtime/catalog.rs"
                | "src/runtime/hosted_mutation.rs"
                | "src/runtime/hosted_query.rs"
                | "src/runtime/hosted_resource.rs"
                | "src/runtime/hosted_validation.rs"
                | "src/runtime/hosted_view.rs"
                | "src/runtime/projection.rs"
        );
        if typed_hosted_caller && (source.contains("recover_v03") || source.contains("from_v03")) {
            failures.push(format!(
                "{relative} decodes v0.3 in a typed hosted path; conversion belongs at the canonical adapter edge"
            ));
        }
        if typed_hosted_caller && source.contains(".result.result") {
            failures.push(format!(
                "{relative} infers hosted semantics from OperationResult JSON instead of typed outcomes or canonical changes"
            ));
        }
        if relative != "src/compat/legacy_mutation.rs"
            && patterns.legacy_facade_definition.is_match(&source)
        {
            failures.push(format!(
                "{relative} defines a public context-free JSON Collection facade outside src/compat/legacy_mutation.rs"
            ));
        }
        if !relative.starts_with("crates/mdbase-architecture-check/") && !production.is_empty() {
            let references =
                compatibility_references(&patterns.legacy_compatibility_reference, &source);
            if !references.is_empty() {
                compatibility_inventory.insert(relative.clone(), references);
            }
            let mut constructors = BTreeMap::new();
            for capture in patterns.wire_only_constructor.captures_iter(production) {
                *constructors.entry(capture[1].to_string()).or_insert(0) += 1;
            }
            if !constructors.is_empty() {
                wire_only_constructor_inventory.insert(relative.clone(), constructors);
            }
        }
        // Collection authority is acquired exactly once. Display paths remain
        // public API/diagnostic data and must not become a second authority.
        if relative != "src/collection_root.rs"
            && !relative.starts_with("crates/mdbase-architecture-check/")
            && (source.contains("open_ambient_dir") || source.contains("ambient_authority()"))
        {
            failures.push(format!(
                "{relative} acquires ambient filesystem authority outside CollectionRoot"
            ));
        }
        if matches!(
            relative.as_str(),
            "src/runtime/provider.rs" | "src/runtime/snapshot.rs" | "src/mutation/shadow.rs"
        ) && source.contains("self.root.join")
        {
            failures.push(format!(
                "{relative} turns the collection display path back into authority"
            ));
        }
        if matches!(
            relative.as_str(),
            "src/runtime/provider.rs" | "src/runtime/snapshot.rs" | "src/mutation/shadow.rs"
        ) && (source.contains("Collection::open(&self.root)")
            || source.contains("open_collection(&self.root)"))
        {
            failures.push(format!(
                "{relative} reopens collection authority through the ambient root name"
            ));
        }

        if relative == "src/runtime/canonical_operation.rs"
            && [
                "pub records: Vec<Value>",
                "pub meta: Value",
                "pub references_affected: Vec<Value>",
            ]
            .iter()
            .any(|forbidden| source.contains(forbidden))
        {
            failures.push(
                "canonical migrated outcomes expose an unclassified JSON Value domain".to_string(),
            );
        }

        // The checker names the debt it detects; do not count those names as product debt.
        if !relative.starts_with("crates/mdbase-architecture-check/") {
            let allowances = match_count(&patterns.dead_code, &source);
            if allowances > 0 {
                measured_dead_code.insert(relative.clone(), allowances);
            }
            unchecked_scan_references += match_count(&patterns.unchecked_collection_scan, &source);
            v03_facade_references += match_count(&patterns.v03_operation_facade, &source);
            operation_context_legacy_production +=
                match_count(&patterns.operation_context_legacy, production);
            operation_context_legacy_support +=
                match_count(&patterns.operation_context_legacy, support_source);
            ephemeral_result_production += match_count(&patterns.ephemeral_result, production);
            ephemeral_result_support += match_count(&patterns.ephemeral_result, support_source);
        }
    }

    // Integration fixtures are not Cargo source roots, so inventory them as
    // support without charging production file/line budgets.
    let integration_tests = root.join("tests");
    if integration_tests.is_dir() {
        for entry in WalkDir::new(integration_tests) {
            let entry = entry.expect("walk integration test sources");
            if !entry.file_type().is_file()
                || entry.path().extension().and_then(|value| value.to_str()) != Some("rs")
            {
                continue;
            }
            let source = fs::read_to_string(entry.path()).expect("read integration test source");
            operation_context_legacy_support +=
                match_count(&patterns.operation_context_legacy, &source);
            ephemeral_result_support += match_count(&patterns.ephemeral_result, &source);
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

    let facade_path = root.join("src/compat/legacy_mutation.rs");
    let facade_source = fs::read_to_string(&facade_path).expect("read legacy facade owner");
    let actual_facade: BTreeSet<_> = patterns
        .legacy_facade_definition
        .captures_iter(&facade_source)
        .map(|capture| capture[1].to_string())
        .collect();
    let expected_facade: BTreeSet<_> = budgets
        .transitional_reference_budgets
        .legacy_collection_facade_definitions
        .iter()
        .cloned()
        .collect();
    if actual_facade != expected_facade {
        failures.push(format!(
            "legacy Collection facade definitions must be owned exactly by src/compat/legacy_mutation.rs: actual {actual_facade:?}, expected {expected_facade:?}"
        ));
    }
    let compat_module = fs::read_to_string(root.join("src/compat/mod.rs"))
        .expect("read compatibility module owner");
    if !compat_module
        .contains("#[cfg(feature = \"legacy-collection-mutation\")]\nmod legacy_mutation;")
    {
        failures.push(
            "legacy mutation facade must remain gated by legacy-collection-mutation".to_string(),
        );
    }

    let expected_compatibility: BTreeMap<_, BTreeSet<_>> = budgets
        .transitional_reference_budgets
        .legacy_compatibility_allowlist
        .iter()
        .map(|(file, names)| (file.clone(), names.iter().cloned().collect()))
        .collect();
    if compatibility_inventory != expected_compatibility {
        failures.push(format!(
            "legacy compatibility ownership changed: actual {compatibility_inventory:?}, expected {expected_compatibility:?}"
        ));
    }

    let canonical_source = fs::read_to_string(root.join("src/runtime/canonical_operation.rs"))
        .expect("read canonical operation model");
    let actual_wire_variants = enum_variants(&canonical_source, "WireOnlyOperationValue");
    let expected_wire_variants: BTreeSet<_> = budgets
        .transitional_reference_budgets
        .wire_only_variants
        .iter()
        .cloned()
        .collect();
    if actual_wire_variants != expected_wire_variants {
        failures.push(format!(
            "WireOnlyOperationValue variants changed: actual {actual_wire_variants:?}, expected {expected_wire_variants:?}"
        ));
    }
    if wire_only_constructor_inventory
        != budgets
            .transitional_reference_budgets
            .wire_only_constructor_allowlist
    {
        failures.push(format!(
            "wire-only constructor ownership changed: actual {wire_only_constructor_inventory:?}, expected {:?}",
            budgets
                .transitional_reference_budgets
                .wire_only_constructor_allowlist
        ));
    }

    for (label, actual, maximum) in [
        (
            "OperationContext::legacy production callers",
            operation_context_legacy_production,
            budgets
                .transitional_reference_budgets
                .operation_context_legacy_production,
        ),
        (
            "OperationContext::legacy test/support callers",
            operation_context_legacy_support,
            budgets
                .transitional_reference_budgets
                .operation_context_legacy_support,
        ),
        (
            "ExecutionOutcome/CommitRejection.result production callers",
            ephemeral_result_production,
            budgets
                .transitional_reference_budgets
                .ephemeral_result_production,
        ),
        (
            "ExecutionOutcome/CommitRejection.result test/support callers",
            ephemeral_result_support,
            budgets
                .transitional_reference_budgets
                .ephemeral_result_support,
        ),
    ] {
        if actual > maximum {
            failures.push(format!("{label} are {actual}; budget is {maximum}"));
        }
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
    use super::{enum_variants, match_count, DebtPatterns};
    use std::collections::BTreeSet;

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

    #[test]
    fn enum_inventory_parses_every_top_level_variant_generically() {
        let source = r#"
            enum WireOnlyOperationValue {
                Validation(Value),
                Future { nested: Option<(u8, u8)> },
                Unit,
            }
        "#;
        assert_eq!(
            enum_variants(source, "WireOnlyOperationValue"),
            ["Future", "Unit", "Validation"]
                .into_iter()
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
        );
    }
}
