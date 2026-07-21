//! Multi-type conflict detection (§6.7).
//!
//! When a file matches multiple types, their field definitions must be
//! compatible. This module detects conflicts during validation.

use crate::errors::*;
use crate::types::schema::*;
use std::collections::HashMap;

/// Check for conflicts between multiple type definitions.
/// Returns issues for any incompatible field definitions.
pub fn detect_type_conflicts(types: &[&TypeDef], path: &str) -> Vec<Issue> {
    if types.len() < 2 {
        return Vec::new();
    }

    let mut issues = Vec::new();

    // Collect all field names across all types
    let mut all_fields: HashMap<&str, Vec<(&str, &FieldDef)>> = HashMap::new();
    for td in types {
        for (fname, fdef) in &td.fields {
            all_fields
                .entry(fname.as_str())
                .or_default()
                .push((td.name.as_str(), fdef));
        }
    }

    // Check each field that appears in multiple types
    for (field_name, defs) in &all_fields {
        if defs.len() < 2 {
            continue;
        }

        check_field_conflicts(field_name, defs, path, &mut issues);
    }

    issues
}

fn check_field_conflicts(
    field_name: &str,
    defs: &[(&str, &FieldDef)],
    path: &str,
    issues: &mut Vec<Issue>,
) {
    let first = &defs[0].1;

    // 1. Check incompatible base types
    for def in &defs[1..] {
        if !types_compatible(&first.field_type, &def.1.field_type) {
            issues.push(type_conflict_issue(
                field_name,
                path,
                &format!(
                    "Incompatible types for field '{}': '{}' vs '{}'",
                    field_name, first.field_type, def.1.field_type
                ),
            ));
            return; // No need to check further if base types conflict
        }
    }

    // 2. Check enum intersection
    if first.field_type == "enum" {
        let enum_sets: Vec<Option<&Vec<String>>> =
            defs.iter().map(|(_, fd)| fd.values.as_ref()).collect();
        if enum_sets.iter().all(|s| s.is_some()) {
            let sets: Vec<&Vec<String>> = enum_sets.into_iter().flatten().collect();
            if sets.len() >= 2 {
                // Compute intersection
                let mut intersection: Vec<&String> = sets[0].iter().collect();
                for s in &sets[1..] {
                    intersection.retain(|v| s.contains(v));
                }
                if intersection.is_empty() {
                    issues.push(type_conflict_issue(
                        field_name,
                        path,
                        &format!("Empty enum intersection for field '{}'", field_name),
                    ));
                    return;
                }
            }
        }
    }

    // 3. Check numeric min/max range
    check_range_conflict(
        field_name,
        defs,
        path,
        issues,
        |fd| fd.min,
        |fd| fd.max,
        "numeric",
    );

    // 4. Check string length range
    check_range_conflict_usize(
        field_name,
        defs,
        path,
        issues,
        |fd| fd.min_length,
        |fd| fd.max_length,
        "length",
    );

    // 5. Check list items range
    check_range_conflict_usize(
        field_name,
        defs,
        path,
        issues,
        |fd| fd.min_items,
        |fd| fd.max_items,
        "items",
    );

    // 6. Check conflicting defaults
    let defaults: Vec<&serde_json::Value> = defs
        .iter()
        .filter_map(|(_, fd)| fd.default.as_ref())
        .collect();
    if defaults.len() >= 2 {
        // Check if all defaults are the same
        let first_default = defaults[0];
        if defaults[1..].iter().any(|d| *d != first_default) {
            issues.push(type_conflict_issue(
                field_name,
                path,
                &format!("Conflicting defaults for field '{}'", field_name),
            ));
        }
    }

    // 7. Check conflicting generated strategies
    let generated: Vec<&GeneratedStrategy> = defs
        .iter()
        .filter_map(|(_, fd)| fd.generated.as_ref())
        .collect();
    if generated.len() >= 2 && !all_generated_same(&generated) {
        issues.push(type_conflict_issue(
            field_name,
            path,
            &format!(
                "Conflicting generated strategies for field '{}'",
                field_name
            ),
        ));
    }

    // 8. Check conflicting link targets
    let targets: Vec<Vec<String>> = defs
        .iter()
        .map(|(_, field)| crate::links::resolver::allowed_target_types(field))
        .filter(|targets| !targets.is_empty())
        .collect();
    if targets.len() >= 2 {
        let first_target = &targets[0];
        if targets[1..].iter().any(|target| target != first_target) {
            issues.push(type_conflict_issue(
                field_name,
                path,
                &format!("Conflicting link targets for field '{}'", field_name),
            ));
        }
    }

    // 9. Check list item type conflicts (recursive)
    if first.field_type == "list" {
        let item_defs: Vec<Option<&FieldDef>> =
            defs.iter().map(|(_, fd)| fd.items.as_deref()).collect();
        if item_defs.iter().all(|d| d.is_some()) {
            let items: Vec<&FieldDef> = item_defs.into_iter().flatten().collect();
            if items.len() >= 2 {
                let first_item = items[0];
                for item in &items[1..] {
                    if !types_compatible(&first_item.field_type, &item.field_type) {
                        issues.push(type_conflict_issue(
                            field_name,
                            path,
                            &format!(
                                "Incompatible list item types for field '{}': '{}' vs '{}'",
                                field_name, first_item.field_type, item.field_type
                            ),
                        ));
                        break;
                    }
                }
            }
        }
    }

    // 10. Check object sub-field conflicts (recursive)
    if first.field_type == "object" {
        let nested_maps: Vec<Option<&HashMap<String, FieldDef>>> =
            defs.iter().map(|(_, fd)| fd.fields.as_ref()).collect();
        if nested_maps.iter().any(|m| m.is_some()) {
            // Collect sub-fields from all types
            let mut sub_fields: HashMap<&str, Vec<(&str, &FieldDef)>> = HashMap::new();
            for (type_name, fd) in defs {
                if let Some(ref fields) = fd.fields {
                    for (sf_name, sf_def) in fields {
                        sub_fields
                            .entry(sf_name.as_str())
                            .or_default()
                            .push((type_name, sf_def));
                    }
                }
            }
            for (sf_name, sf_defs) in &sub_fields {
                if sf_defs.len() >= 2 {
                    let dotted = format!("{}.{}", field_name, sf_name);
                    check_field_conflicts(&dotted, sf_defs, path, issues);
                }
            }
        }
    }
}

fn check_range_conflict(
    field_name: &str,
    defs: &[(&str, &FieldDef)],
    path: &str,
    issues: &mut Vec<Issue>,
    get_min: fn(&FieldDef) -> Option<f64>,
    get_max: fn(&FieldDef) -> Option<f64>,
    _kind: &str,
) {
    let mins: Vec<f64> = defs.iter().filter_map(|(_, fd)| get_min(fd)).collect();
    let maxs: Vec<f64> = defs.iter().filter_map(|(_, fd)| get_max(fd)).collect();

    if !mins.is_empty() && !maxs.is_empty() {
        let merged_min = mins.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let merged_max = maxs.iter().cloned().fold(f64::INFINITY, f64::min);
        if merged_min > merged_max {
            issues.push(type_conflict_issue(
                field_name,
                path,
                &format!(
                    "Impossible range for field '{}': min {} > max {}",
                    field_name, merged_min, merged_max
                ),
            ));
        }
    }
}

fn check_range_conflict_usize(
    field_name: &str,
    defs: &[(&str, &FieldDef)],
    path: &str,
    issues: &mut Vec<Issue>,
    get_min: fn(&FieldDef) -> Option<usize>,
    get_max: fn(&FieldDef) -> Option<usize>,
    _kind: &str,
) {
    let mins: Vec<usize> = defs.iter().filter_map(|(_, fd)| get_min(fd)).collect();
    let maxs: Vec<usize> = defs.iter().filter_map(|(_, fd)| get_max(fd)).collect();

    if !mins.is_empty() && !maxs.is_empty() {
        let merged_min = *mins.iter().max().unwrap();
        let merged_max = *maxs.iter().min().unwrap();
        if merged_min > merged_max {
            issues.push(type_conflict_issue(
                field_name,
                path,
                &format!(
                    "Impossible range for field '{}': min {} > max {}",
                    field_name, merged_min, merged_max
                ),
            ));
        }
    }
}

fn types_compatible(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    // "any" is compatible with everything
    if a == "any" || b == "any" {
        return true;
    }
    // integer is compatible with number
    if (a == "integer" && b == "number") || (a == "number" && b == "integer") {
        return true;
    }
    false
}

fn all_generated_same(strategies: &[&GeneratedStrategy]) -> bool {
    if strategies.len() < 2 {
        return true;
    }
    let first = strategies[0];
    strategies[1..].iter().all(|s| generated_eq(first, s))
}

fn generated_eq(a: &GeneratedStrategy, b: &GeneratedStrategy) -> bool {
    match (a, b) {
        (GeneratedStrategy::Ulid, GeneratedStrategy::Ulid) => true,
        (GeneratedStrategy::Uuid, GeneratedStrategy::Uuid) => true,
        (GeneratedStrategy::Now, GeneratedStrategy::Now) => true,
        (GeneratedStrategy::NowOnWrite, GeneratedStrategy::NowOnWrite) => true,
        (
            GeneratedStrategy::Derived {
                from: f1,
                transform: t1,
            },
            GeneratedStrategy::Derived {
                from: f2,
                transform: t2,
            },
        ) => f1 == f2 && t1 == t2,
        _ => false,
    }
}

fn type_conflict_issue(field: &str, path: &str, message: &str) -> Issue {
    Issue {
        code: TYPE_CONFLICT.to_string(),
        message: message.to_string(),
        path: Some(path.to_string()),
        field: Some(field.to_string()),
        severity: Severity::Error,
        expected: None,
        actual: None,
        type_name: None,
        line: None,
        column: None,
    }
}
