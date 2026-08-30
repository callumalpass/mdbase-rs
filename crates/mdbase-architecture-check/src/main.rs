use regex::Regex;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use syn::visit::{self, Visit};
use syn::{Item, ItemExternCrate, ItemUse, UseTree};
use walkdir::WalkDir;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArchitectureBudgets {
    rust_source_file_count_max: usize,
    rust_source_line_count_max: usize,
    rust_source_file_max_lines: usize,
    legacy_file_line_budgets: BTreeMap<String, usize>,
    dead_code_allowances_by_file: BTreeMap<String, usize>,
    ambient_io_allowlist: BTreeMap<String, BTreeMap<String, usize>>,
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
        || relative.ends_with("/tests.rs")
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

/// Conservatively identify display-path expressions that are turned back into
/// collection filesystem authority. This is intentionally lexical: macro and
/// helper indirection must not provide a bypass around the held-root boundary.
fn without_cfg_test_items(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0;
    while let Some(relative_start) = source[cursor..].find("#[cfg(") {
        let start = cursor + relative_start;
        let Some(relative_end) = source[start..].find(']') else {
            break;
        };
        let attribute_end = start + relative_end + 1;
        if !source[start..attribute_end].contains("test") {
            output.push_str(&source[cursor..attribute_end]);
            cursor = attribute_end;
            continue;
        }
        output.push_str(&source[cursor..start]);
        let mut item_start = attribute_end
            + source[attribute_end..]
                .find(|character: char| !character.is_whitespace())
                .unwrap_or(0);
        while source[item_start..].starts_with("#[") {
            let Some(end) = source[item_start..].find(']') else {
                break;
            };
            item_start += end + 1;
            item_start += source[item_start..]
                .find(|character: char| !character.is_whitespace())
                .unwrap_or(0);
        }
        let remainder = &source[item_start..];
        let open = remainder.find('{');
        let semicolon = remainder.find(';');
        cursor = if open.is_some_and(|open| semicolon.is_none_or(|semicolon| open < semicolon)) {
            let open = item_start + open.unwrap();
            let mut depth = 0usize;
            let mut close = source.len();
            for (offset, character) in source[open..].char_indices() {
                match character {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            close = open + offset + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            close
        } else {
            item_start + semicolon.map_or(remainder.len(), |offset| offset + 1)
        };
    }
    output.push_str(&source[cursor..]);
    output
}

#[derive(Clone, Debug)]
struct UseBinding {
    local: String,
    source: Vec<String>,
}

fn cfg_test_only(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        if !attribute.path().is_ident("cfg") {
            return false;
        }
        let Ok(list) = attribute.meta.require_list() else {
            return false;
        };
        let predicate = list.tokens.to_string().replace(' ', "");
        predicate == "test" || predicate.starts_with("all(test,")
    })
}

fn item_is_test_only(item: &Item) -> bool {
    let attributes = match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        _ => return false,
    };
    cfg_test_only(attributes)
}

fn production_syntax(source: &str) -> syn::Result<syn::File> {
    let mut file = syn::parse_file(source)?;
    file.items.retain(|item| !item_is_test_only(item));
    Ok(file)
}

fn flatten_use(tree: &UseTree, prefix: &mut Vec<String>, bindings: &mut Vec<UseBinding>) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            flatten_use(&path.tree, prefix, bindings);
            prefix.pop();
        }
        UseTree::Name(name) => {
            if name.ident == "self" {
                if let Some(local) = prefix.last() {
                    bindings.push(UseBinding {
                        local: local.clone(),
                        source: prefix.clone(),
                    });
                }
            } else {
                let local = name.ident.to_string();
                let mut source = prefix.clone();
                source.push(local.clone());
                bindings.push(UseBinding { local, source });
            }
        }
        UseTree::Rename(rename) => {
            let local = rename.rename.to_string();
            let mut source = prefix.clone();
            if rename.ident != "self" {
                source.push(rename.ident.to_string());
            }
            bindings.push(UseBinding { local, source });
        }
        UseTree::Group(group) => {
            for tree in &group.items {
                flatten_use(tree, prefix, bindings);
            }
        }
        UseTree::Glob(_) => {
            let mut source = prefix.clone();
            source.push("*".to_string());
            bindings.push(UseBinding {
                local: "*".to_string(),
                source,
            });
        }
    }
}

fn resolve_path(path: &[String], aliases: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut resolved = path.to_vec();
    let mut seen = BTreeSet::new();
    while let Some(first) = resolved.first().cloned() {
        if !seen.insert(first.clone()) {
            break;
        }
        let Some(prefix) = aliases.get(&first) else {
            break;
        };
        resolved.splice(0..1, prefix.clone());
    }
    resolved
}

fn ambient_api(path: &[String]) -> Option<&'static str> {
    let suffix = |expected: &[&str]| {
        path.len() >= expected.len()
            && path[path.len() - expected.len()..]
                .iter()
                .map(String::as_str)
                .eq(expected.iter().copied())
    };
    if path == ["std", "fs", "*"] {
        Some("std::fs::*")
    } else if path == ["tokio", "fs", "*"] {
        Some("tokio::fs::*")
    } else if path.starts_with(&["std".into(), "fs".into(), "File".into()]) {
        Some("std::fs::File")
    } else if path.starts_with(&["std".into(), "fs".into(), "OpenOptions".into()]) {
        Some("std::fs::OpenOptions")
    } else if path.starts_with(&["std".into(), "fs".into()]) {
        Some("std::fs")
    } else if path.starts_with(&["tokio".into(), "fs".into()]) {
        Some("tokio::fs")
    } else if path.first().is_some_and(|part| part == "walkdir") {
        Some("walkdir")
    } else if path.first().is_some_and(|part| part == "tempfile") {
        Some("tempfile")
    } else if path.first().is_some_and(|part| part == "cap_std")
        && (suffix(&["ambient_authority"])
            || suffix(&["Dir", "open_ambient_dir"])
            || suffix(&["open_ambient_dir"]))
    {
        Some("cap_std::ambient-acquisition")
    } else {
        None
    }
}

#[derive(Default)]
struct BindingCollector {
    bindings: Vec<UseBinding>,
}

impl<'ast> Visit<'ast> for BindingCollector {
    fn visit_item(&mut self, item: &'ast Item) {
        if !item_is_test_only(item) {
            visit::visit_item(self, item);
        }
    }

    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        flatten_use(&item.tree, &mut Vec::new(), &mut self.bindings);
    }

    fn visit_item_extern_crate(&mut self, item: &'ast ItemExternCrate) {
        self.bindings.push(UseBinding {
            local: item
                .rename
                .as_ref()
                .map_or_else(|| item.ident.to_string(), |(_, rename)| rename.to_string()),
            source: vec![item.ident.to_string()],
        });
    }
}

struct AmbientVisitor<'a> {
    aliases: &'a BTreeMap<String, Vec<String>>,
    inventory: BTreeMap<String, usize>,
}

impl AmbientVisitor<'_> {
    fn record(&mut self, path: Vec<String>) {
        let path = resolve_path(&path, self.aliases);
        if let Some(api) = ambient_api(&path) {
            *self.inventory.entry(api.to_string()).or_default() += 1;
        }
    }

    fn visit_macro_tokens(&mut self, stream: proc_macro2::TokenStream) {
        let tokens = stream.into_iter().collect::<Vec<_>>();
        let mut index = 0;
        while index < tokens.len() {
            if let proc_macro2::TokenTree::Group(group) = &tokens[index] {
                self.visit_macro_tokens(group.stream());
            }
            let proc_macro2::TokenTree::Ident(first) = &tokens[index] else {
                index += 1;
                continue;
            };
            let mut path = vec![first.to_string()];
            let mut end = index + 1;
            while end + 1 < tokens.len()
                && matches!(&tokens[end], proc_macro2::TokenTree::Punct(p) if p.as_char() == ':')
                && matches!(&tokens[end + 1], proc_macro2::TokenTree::Punct(p) if p.as_char() == ':')
            {
                let Some(proc_macro2::TokenTree::Ident(next)) = tokens.get(end + 2) else {
                    break;
                };
                path.push(next.to_string());
                end += 3;
            }
            self.record(path);
            index = end;
        }
    }
}

impl<'ast> Visit<'ast> for AmbientVisitor<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        if !item_is_test_only(item) {
            visit::visit_item(self, item);
        }
    }

    fn visit_item_use(&mut self, _item: &'ast ItemUse) {}

    fn visit_item_extern_crate(&mut self, _item: &'ast ItemExternCrate) {}

    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.record(
            path.segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect(),
        );
        visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, item: &'ast syn::Macro) {
        self.visit_macro_tokens(item.tokens.clone());
    }
}

fn ambient_io_tokens(source: &str) -> syn::Result<BTreeMap<String, usize>> {
    let file = production_syntax(source)?;
    let mut collector = BindingCollector::default();
    collector.visit_file(&file);
    let bindings = collector.bindings;
    let mut aliases = BTreeMap::from([
        ("std".to_string(), vec!["std".to_string()]),
        ("tokio".to_string(), vec!["tokio".to_string()]),
        ("walkdir".to_string(), vec!["walkdir".to_string()]),
        ("tempfile".to_string(), vec!["tempfile".to_string()]),
        ("cap_std".to_string(), vec!["cap_std".to_string()]),
    ]);
    for _ in 0..=bindings.len() {
        let mut changed = false;
        for binding in &bindings {
            if binding.local == "*" {
                continue;
            }
            let resolved = resolve_path(&binding.source, &aliases);
            if resolved.first().is_some_and(|root| {
                matches!(
                    root.as_str(),
                    "std" | "tokio" | "walkdir" | "tempfile" | "cap_std"
                )
            }) && aliases.get(&binding.local) != Some(&resolved)
            {
                aliases.insert(binding.local.clone(), resolved);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut visitor = AmbientVisitor {
        aliases: &aliases,
        inventory: BTreeMap::new(),
    };
    for binding in &bindings {
        visitor.record(binding.source.clone());
    }
    visitor.visit_file(&file);
    Ok(visitor.inventory)
}

fn held_authority_bypasses(source: &str) -> BTreeSet<String> {
    let source = without_cfg_test_items(source);
    let source = source.as_str();
    let mut failures = BTreeSet::new();
    let display = if source.contains("impl Collection") {
        r"(?:self|collection|shadow\.collection)\.root(?:\s*\(\s*\))?"
    } else {
        r"(?:collection|shadow\.collection)\.root(?:\s*\(\s*\))?"
    };
    let direct_patterns = [
        (
            "std-fs",
            format!(
                r"(?s)(?:std::)?fs::(?:read|read_to_string|write|metadata|symlink_metadata|canonicalize|read_dir|create_dir|create_dir_all|remove_file|remove_dir_all|rename|hard_link)\s*\([^;}}]*{display}"
            ),
        ),
        (
            "path-method",
            format!(
                r"(?s){display}[^;}}]*(?:\.exists|\.is_file|\.is_dir|\.metadata|\.symlink_metadata)\s*\("
            ),
        ),
        ("walkdir", format!(r"(?s)WalkDir::new\s*\([^;}}]*{display}")),
        (
            "tempfile",
            format!(r"(?s)(?:NamedTempFile::new_in|tempfile_in|tempdir_in)\s*\([^;}}]*{display}"),
        ),
        (
            "reopen",
            format!(r"(?s)(?:Collection::open|open_collection)\s*\([^;}}]*{display}"),
        ),
        ("under", format!(r"(?s)\.under\s*\([^;}}]*{display}")),
    ];
    for (label, expression) in direct_patterns {
        if Regex::new(&expression)
            .expect("held-authority guard regex")
            .is_match(source)
        {
            failures.insert(label.to_string());
        }
    }

    let alias = Regex::new(&format!(
        r"(?m)let\s+(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*&?\s*{display}[^;]*;"
    ))
    .expect("display-root alias regex");
    for capture in alias.captures_iter(source) {
        let name = regex::escape(&capture[1]);
        let risky = Regex::new(&format!(
            r"(?s)(?:std::)?fs::[A-Za-z_]+\s*\([^;}}]*\b{name}\b|WalkDir::new\s*\(\s*&?\s*\b{name}\b|\b{name}\b[^;}}]*(?:\.exists|\.is_file|\.is_dir|\.metadata|\.symlink_metadata)\s*\(|\.under\s*\([^;}}]*\b{name}\b"
        ))
        .expect("display-root alias use regex");
        if risky.is_match(&source[capture.get(0).unwrap().end()..]) {
            failures.insert("alias".to_string());
        }
    }

    // A helper receiving the display root and performing path I/O is equally a
    // bypass, even when the call site itself contains no std::fs token.
    let helper_call = Regex::new(&format!(r"([A-Za-z_][A-Za-z0-9_]*)\s*\(\s*&?\s*{display}"))
        .expect("display-root helper call regex");
    for capture in helper_call.captures_iter(source) {
        let helper = regex::escape(&capture[1]);
        let definition = Regex::new(&format!(
            r"(?s)fn\s+{helper}\s*\([^)]*\b([A-Za-z_][A-Za-z0-9_]*)\s*:[^)]*\)\s*[^{{]*\{{([^}}]*)\}}"
        ))
        .expect("display-root helper definition regex");
        if let Some(body) = definition.captures(source) {
            let parameter = regex::escape(&body[1]);
            let io = Regex::new(&format!(
                r"(?:std::)?fs::[A-Za-z_]+\s*\([^;}}]*\b{parameter}\b|\b{parameter}\b[^;}}]*(?:\.exists|\.is_file|\.is_dir|\.metadata)\s*\("
            ))
            .expect("display-root helper body regex");
            if io.is_match(&body[2]) {
                failures.insert("helper".to_string());
            }
        }
    }
    failures
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
    let mut ambient_io_inventory = BTreeMap::new();

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
        if !relative.starts_with("crates/mdbase-architecture-check/") && !production.is_empty() {
            match ambient_io_tokens(&source) {
                Ok(ambient) if !ambient.is_empty() => {
                    ambient_io_inventory.insert(relative.clone(), ambient);
                }
                Ok(_) => {}
                Err(error) => failures.push(format!(
                    "{relative} could not be parsed for ambient I/O ownership: {error}"
                )),
            }
        }
        if relative != "src/collection_root.rs"
            && !relative.starts_with("crates/mdbase-architecture-check/")
        {
            for bypass in held_authority_bypasses(production) {
                failures.push(format!(
                    "{relative} turns the private collection display root into {bypass} filesystem authority"
                ));
            }
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

    if ambient_io_inventory != budgets.ambient_io_allowlist {
        failures.push(format!(
            "ambient I/O ownership changed: actual {ambient_io_inventory:?}, expected {:?}",
            budgets.ambient_io_allowlist
        ));
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
    use super::{
        ambient_io_tokens, enum_variants, held_authority_bypasses, match_count, DebtPatterns,
    };
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
    fn ambient_io_guard_resolves_nested_imports_alias_chains_macros_and_qualified_paths() {
        for (label, source) in [
            (
                "nested-module",
                "use std::{fs as io}; fn f(p: &Path) { io::read(p); }",
            ),
            (
                "nested-type",
                "use std::{fs::File as F}; fn f(p: &Path) { F::open(p); }",
            ),
            (
                "renamed-extern-alias-chain",
                "extern crate std as rust_std; use rust_std::fs as io; use io::OpenOptions as O; fn f() { O::new(); }",
            ),
            (
                "macro-qualified-call",
                "use std::fs as io; fn f(p: &Path) { invoke!(io::read(p)); }",
            ),
            (
                "qualified-type-call",
                "use std::fs::File as F; fn f(p: &Path) { <F>::open(p); }",
            ),
            (
                "renamed-walkdir",
                "use walkdir::WalkDir as ArbitraryWalker; fn f(p: &Path) { ArbitraryWalker::new(p); }",
            ),
            (
                "nested-module-import",
                "mod nested { use std::{fs as hidden}; fn f() { hidden::read(\"x\"); } }",
            ),
            (
                "renamed-tempfile-extern",
                "extern crate tempfile as workspace; fn f() { workspace::tempdir(); }",
            ),
            (
                "ambient-cap-std",
                "use cap_std::fs::Dir as D; fn f(p: &Path) { D::open_ambient_dir(p, cap_std::ambient_authority()); }",
            ),
        ] {
            assert!(
                !ambient_io_tokens(source).unwrap().is_empty(),
                "synthetic {label} ambient surface was not rejected"
            );
        }

        // A helper's call site need not reveal I/O: closed ownership rejects
        // the separate unowned helper module where ambient authority appears.
        assert!(
            ambient_io_tokens("pub fn call_helper(p: &Path) { helper::read(p); }")
                .unwrap()
                .is_empty()
        );
        assert!(
            !ambient_io_tokens("pub fn read(p: &Path) { std::fs::read(p); }")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn ambient_io_guard_accepts_capability_only_files_and_ignores_test_items() {
        let source = r#"
            use cap_std::fs::{Dir, OpenOptions};
            fn held(dir: &Dir) {
                let mut options = OpenOptions::new();
                options.read(true);
                let _ = dir.open_with("record.md", &options);
            }
            #[cfg(test)]
            mod tests { fn ambient_fixture() { std::fs::read("fixture"); } }
        "#;
        assert!(ambient_io_tokens(source).unwrap().is_empty());
    }

    #[test]
    fn held_authority_guard_rejects_direct_alias_under_walk_tempfile_and_helper_bypasses() {
        for (label, source) in [
            ("collection-root", "fn f(collection: &Collection) { std::fs::read(collection.root.join(\"x\")); }"),
            ("public-root", "fn f(collection: &Collection) { std::fs::metadata(collection.root().join(\"x\")); }"),
            ("alias", "fn f(collection: &Collection) { let base = collection.root(); std::fs::write(base.join(\"x\"), b\"x\"); }"),
            ("under", "fn f(collection: &Collection, path: CollectionPath) { path.under(&collection.root).exists(); }"),
            ("walk", "fn f(collection: &Collection) { WalkDir::new(&collection.root); }"),
            ("tempfile", "fn f(collection: &Collection) { NamedTempFile::new_in(collection.root()); }"),
            ("helper", "fn read_it(base: &Path) { std::fs::read(base.join(\"x\")); } fn f(collection: &Collection) { read_it(collection.root()); }"),
        ] {
            assert!(
                !held_authority_bypasses(source).is_empty(),
                "synthetic {label} bypass was not rejected"
            );
        }
        assert!(held_authority_bypasses(
            "fn display(collection: &Collection) { format!(\"{}\", collection.root().display()); }"
        )
        .is_empty());
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
