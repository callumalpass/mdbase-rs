//! Match rule evaluation engine (§6).

use crate::types::schema::MatchRules;

/// Check if a file matches the given match rules.
/// `rel_path` is the file path relative to the collection root (forward slashes).
/// `frontmatter` is the parsed frontmatter JSON object.
pub fn matches_rules(
    rules: &MatchRules,
    rel_path: &str,
    frontmatter: &serde_json::Value,
    timezone: Option<&str>,
) -> bool {
    matches_rules_checked(rules, rel_path, frontmatter, timezone).unwrap_or(false)
}

/// Check match rules while preserving CEL evaluation failures for v0.3
/// callers that must surface per-record diagnostics.
pub(crate) fn matches_rules_checked(
    rules: &MatchRules,
    rel_path: &str,
    frontmatter: &serde_json::Value,
    timezone: Option<&str>,
) -> Result<bool, crate::v03::cel::CelFailure> {
    matches_rules_checked_compiled(rules, None, rel_path, frontmatter, timezone)
}

pub(crate) fn matches_rules_checked_compiled(
    rules: &MatchRules,
    match_expression: Option<&crate::expressions::ast::Expr>,
    rel_path: &str,
    frontmatter: &serde_json::Value,
    timezone: Option<&str>,
) -> Result<bool, crate::v03::cel::CelFailure> {
    // All conditions in a match rule are AND'd together
    if let Some(ref path_glob) = rules.path_glob {
        if !matches_path_glob(rel_path, path_glob) {
            return Ok(false);
        }
    }
    if let Some(ref path_globs) = rules.path_globs {
        if !path_globs
            .iter()
            .any(|path_glob| matches_path_glob(rel_path, path_glob))
        {
            return Ok(false);
        }
    }

    if let Some(ref fields_present) = rules.fields_present {
        if !check_fields_present(frontmatter, fields_present) {
            return Ok(false);
        }
    }

    if let Some(ref where_clause) = rules.where_clause {
        if !evaluate_where(frontmatter, where_clause) {
            return Ok(false);
        }
    }
    if let Some(ref expression) = rules.match_expr {
        let matched = match match_expression {
            Some(expression) => crate::v03::cel::evaluate_match_expression_compiled(
                expression,
                frontmatter,
                rel_path,
                timezone,
            )?,
            None => crate::v03::cel::evaluate_match_expression(
                expression,
                frontmatter,
                rel_path,
                timezone,
            )?,
        };
        if !matched {
            return Ok(false);
        }
    }

    // At least one condition must be specified
    Ok(rules.path_glob.is_some()
        || rules.path_globs.is_some()
        || rules.fields_present.is_some()
        || rules.where_clause.is_some()
        || rules.match_expr.is_some())
}

/// Check if a relative path matches a glob pattern.
/// Supports: * (single level), ** (any depth), ? (single char)
fn matches_path_glob(rel_path: &str, pattern: &str) -> bool {
    // Normalize separators
    let path = rel_path.replace('\\', "/");
    let pattern = pattern.replace('\\', "/");

    glob_match(&path, &pattern)
}

/// Recursive glob matcher.
fn glob_match(path: &str, pattern: &str) -> bool {
    let path_parts: Vec<&str> = path.split('/').collect();
    let pattern_parts: Vec<&str> = pattern.split('/').collect();

    glob_match_parts(&path_parts, &pattern_parts)
}

fn glob_match_parts(path_parts: &[&str], pattern_parts: &[&str]) -> bool {
    if pattern_parts.is_empty() {
        return path_parts.is_empty();
    }

    if path_parts.is_empty() {
        // Remaining pattern parts must all be ** to match empty
        return pattern_parts.iter().all(|p| *p == "**");
    }

    let pat = pattern_parts[0];

    if pat == "**" {
        // ** matches zero or more path segments
        // Try matching 0 segments (skip **)
        if glob_match_parts(path_parts, &pattern_parts[1..]) {
            return true;
        }
        // Try matching 1+ segments (consume one path part, keep **)
        if glob_match_parts(&path_parts[1..], pattern_parts) {
            return true;
        }
        return false;
    }

    // Match current segment
    if segment_matches(path_parts[0], pat) {
        return glob_match_parts(&path_parts[1..], &pattern_parts[1..]);
    }

    false
}

/// Check if a single path segment matches a glob pattern segment.
/// Supports * (any chars) and ? (single char).
fn segment_matches(segment: &str, pattern: &str) -> bool {
    segment_match_chars(segment.as_bytes(), pattern.as_bytes())
}

fn segment_match_chars(seg: &[u8], pat: &[u8]) -> bool {
    if pat.is_empty() {
        return seg.is_empty();
    }
    if seg.is_empty() {
        // Remaining pattern must all be * to match empty
        return pat.iter().all(|&c| c == b'*');
    }

    match pat[0] {
        b'*' => {
            // * matches zero or more characters in this segment
            // Try zero chars
            if segment_match_chars(seg, &pat[1..]) {
                return true;
            }
            // Try one+ chars
            segment_match_chars(&seg[1..], pat)
        }
        b'?' => {
            // ? matches exactly one character
            segment_match_chars(&seg[1..], &pat[1..])
        }
        c => {
            if seg[0] == c {
                segment_match_chars(&seg[1..], &pat[1..])
            } else {
                false
            }
        }
    }
}

/// Check that all listed fields are present and non-null in frontmatter.
fn check_fields_present(frontmatter: &serde_json::Value, fields: &[String]) -> bool {
    let obj = match frontmatter.as_object() {
        Some(o) => o,
        None => return false,
    };

    for field in fields {
        match obj.get(field) {
            None => return false,
            Some(v) if v.is_null() => return false,
            _ => {} // present and non-null
        }
    }
    true
}

/// Evaluate a where clause against frontmatter.
/// The where clause is a JSON object where keys are field names
/// and values are either literal values (exact match) or operator objects.
fn evaluate_where(frontmatter: &serde_json::Value, where_clause: &serde_json::Value) -> bool {
    let conditions = match where_clause.as_object() {
        Some(obj) => obj,
        None => return false,
    };

    let fm_obj = match frontmatter.as_object() {
        Some(obj) => obj,
        None => return false,
    };

    for (field, condition) in conditions {
        let field_value = fm_obj.get(field);

        if let Some(cond_obj) = condition.as_object() {
            // Operator-based condition
            if !evaluate_field_condition(field_value, cond_obj) {
                return false;
            }
        } else {
            // Literal value - exact equality
            match field_value {
                Some(actual) => {
                    if !values_equal(actual, condition) {
                        return false;
                    }
                }
                None => return false,
            }
        }
    }

    true
}

/// Evaluate a single field condition with operators.
fn evaluate_field_condition(
    field_value: Option<&serde_json::Value>,
    operators: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    for (op, expected) in operators {
        match op.as_str() {
            "eq" => match field_value {
                Some(actual) => {
                    if !values_equal(actual, expected) {
                        return false;
                    }
                }
                None => return false,
            },
            "neq" => {
                if let Some(actual) = field_value {
                    if values_equal(actual, expected) {
                        return false;
                    }
                }
            }
            "gt" => match field_value {
                Some(actual) => {
                    if compare_values(actual, expected) != Some(std::cmp::Ordering::Greater) {
                        return false;
                    }
                }
                None => return false,
            },
            "gte" => match field_value {
                Some(actual) => {
                    if compare_values(actual, expected)
                        .is_none_or(|ord| ord == std::cmp::Ordering::Less)
                    {
                        return false;
                    }
                }
                None => return false,
            },
            "lt" => match field_value {
                Some(actual) => {
                    if compare_values(actual, expected) != Some(std::cmp::Ordering::Less) {
                        return false;
                    }
                }
                None => return false,
            },
            "lte" => match field_value {
                Some(actual) => {
                    if compare_values(actual, expected)
                        .is_none_or(|ord| ord == std::cmp::Ordering::Greater)
                    {
                        return false;
                    }
                }
                None => return false,
            },
            "exists" => {
                let should_exist = expected.as_bool().unwrap_or(true);
                let does_exist = field_value.is_some_and(|v| !v.is_null());
                if should_exist != does_exist {
                    return false;
                }
            }
            "contains" => match field_value {
                Some(serde_json::Value::Array(arr)) => {
                    if !arr.iter().any(|item| values_equal(item, expected)) {
                        return false;
                    }
                }
                _ => return false,
            },
            "containsAll" => match field_value {
                Some(serde_json::Value::Array(arr)) => {
                    if let Some(expected_arr) = expected.as_array() {
                        for exp in expected_arr {
                            if !arr.iter().any(|item| values_equal(item, exp)) {
                                return false;
                            }
                        }
                    } else {
                        return false;
                    }
                }
                _ => return false,
            },
            "containsAny" => match field_value {
                Some(serde_json::Value::Array(arr)) => {
                    if let Some(expected_arr) = expected.as_array() {
                        if !expected_arr
                            .iter()
                            .any(|exp| arr.iter().any(|item| values_equal(item, exp)))
                        {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                _ => return false,
            },
            "startsWith" => match field_value {
                Some(serde_json::Value::String(s)) => {
                    if let Some(prefix) = expected.as_str() {
                        if !s.starts_with(prefix) {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                _ => return false,
            },
            "endsWith" => match field_value {
                Some(serde_json::Value::String(s)) => {
                    if let Some(suffix) = expected.as_str() {
                        if !s.ends_with(suffix) {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                _ => return false,
            },
            "matches" => match field_value {
                Some(serde_json::Value::String(s)) => {
                    if let Some(pattern) = expected.as_str() {
                        match fancy_regex::Regex::new(pattern) {
                            Ok(re) => {
                                if !re.is_match(s).unwrap_or(false) {
                                    return false;
                                }
                            }
                            Err(_) => return false,
                        }
                    } else {
                        return false;
                    }
                }
                _ => return false,
            },
            _ => {} // Unknown operator - ignore
        }
    }
    true
}

/// Compare two JSON values for equality (with type coercion).
fn values_equal(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    if a == b {
        return true;
    }
    // Try numeric comparison
    if let (Some(an), Some(bn)) = (to_f64(a), to_f64(b)) {
        return (an - bn).abs() < f64::EPSILON;
    }
    // Try string comparison
    if let (Some(as_str), Some(bs_str)) = (a.as_str(), b.as_str()) {
        return as_str == bs_str;
    }
    false
}

/// Compare two JSON values, returning ordering if comparable.
fn compare_values(a: &serde_json::Value, b: &serde_json::Value) -> Option<std::cmp::Ordering> {
    // Try numeric comparison
    if let (Some(an), Some(bn)) = (to_f64(a), to_f64(b)) {
        return an.partial_cmp(&bn);
    }
    // Try string comparison
    if let (Some(as_str), Some(bs_str)) = (a.as_str(), b.as_str()) {
        return Some(as_str.cmp(bs_str));
    }
    None
}

fn to_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

// --- impl Collection methods for matching ---

use crate::matching::glob::match_glob_pattern;
use crate::Collection;

impl Collection {
    /// Check if a path is excluded from the collection.
    pub(crate) fn is_excluded(&self, rel_path: &str) -> bool {
        // Check types folder
        if rel_path.starts_with(&format!("{}/", self.settings.types_folder))
            || rel_path == self.settings.types_folder
        {
            return true;
        }

        // Check cache folder
        if rel_path.starts_with(&format!("{}/", self.settings.cache_folder))
            || rel_path == self.settings.cache_folder
        {
            return true;
        }

        // Check migrations folder
        if rel_path.starts_with(&format!("{}/", self.settings.migrations_folder))
            || rel_path == self.settings.migrations_folder
        {
            return true;
        }

        // Check default .mdbase even if custom cache_folder
        if self.settings.cache_folder != ".mdbase"
            && (rel_path.starts_with(".mdbase/") || rel_path == ".mdbase")
        {
            return true;
        }

        // Check mdbase.yaml
        if rel_path == "mdbase.yaml" {
            return true;
        }

        // Check exclude patterns
        for pattern in &self.settings.exclude {
            if match_glob_pattern(pattern, rel_path) {
                return true;
            }
        }

        // Check include_subfolders
        if !self.settings.include_subfolders && rel_path.contains('/') {
            return true;
        }

        // Check nested collection boundary (§2.8)
        // If any parent directory of this path contains mdbase.yaml,
        // the file is inside a nested collection and not part of this one.
        if self.is_in_nested_collection(rel_path) {
            return true;
        }

        false
    }

    /// Check if a path is excluded by user-configured exclude patterns only.
    /// Unlike is_excluded, this does NOT exclude system folders (types, cache).
    /// Used by read operations which should be able to access type definition files.
    #[allow(dead_code)]
    pub(crate) fn is_user_excluded(&self, rel_path: &str) -> bool {
        for pattern in &self.settings.exclude {
            if match_glob_pattern(pattern, rel_path) {
                return true;
            }
        }
        if self.is_in_nested_collection(rel_path) {
            return true;
        }
        false
    }

    /// Check if a relative path is inside a nested collection.
    /// Returns true if any parent directory along the path contains a mdbase.yaml file.
    pub(crate) fn is_in_nested_collection(&self, rel_path: &str) -> bool {
        let path = std::path::Path::new(rel_path);
        let mut current = std::path::PathBuf::new();
        // Check each parent directory component (not the file itself)
        for component in path.parent().into_iter().flat_map(|p| p.components()) {
            current.push(component);
            let config_path = self.root.join(&current).join("mdbase.yaml");
            if config_path.exists() {
                return true;
            }
        }
        false
    }

    /// Check if a file extension is valid for this collection.
    pub(crate) fn is_valid_extension(&self, path: &str) -> bool {
        if path.ends_with(".md") {
            return true;
        }
        for ext in &self.settings.extensions {
            if path.ends_with(&format!(".{}", ext)) {
                return true;
            }
        }
        false
    }

    /// Determine the type(s) of a file from its frontmatter.
    /// Type names are canonicalized to lowercase for lookup.
    pub(crate) fn determine_types(&self, frontmatter: &serde_json::Value) -> Vec<String> {
        self.determine_types_for_path(frontmatter, None)
    }

    /// Determine types for a file at the given path.
    /// If explicit type keys are found in frontmatter, uses those (and stops match rule evaluation).
    /// Otherwise evaluates match rules from all types.
    pub fn determine_types_for_path(
        &self,
        frontmatter: &serde_json::Value,
        rel_path: Option<&str>,
    ) -> Vec<String> {
        self.determine_types_for_path_checked(frontmatter, rel_path)
            .0
    }

    pub(crate) fn determine_types_for_path_checked(
        &self,
        frontmatter: &serde_json::Value,
        rel_path: Option<&str>,
    ) -> (Vec<String>, Vec<(String, crate::v03::cel::CelFailure)>) {
        let mut types = Vec::new();
        let mut has_explicit = false;

        if let Some(obj) = frontmatter.as_object() {
            for key in &self.settings.explicit_type_keys {
                if let Some(val) = obj.get(key) {
                    match val {
                        serde_json::Value::String(s) => {
                            if !s.is_empty() {
                                types.push(s.to_lowercase());
                                has_explicit = true;
                            }
                        }
                        serde_json::Value::Array(arr) => {
                            for item in arr {
                                if let Some(s) = item.as_str() {
                                    types.push(s.to_lowercase());
                                    has_explicit = true;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // If explicit types found, stop here (§6.6)
        if has_explicit {
            return (types, Vec::new());
        }

        // Evaluate match rules from all types
        let mut failures = Vec::new();
        if let Some(path) = rel_path {
            let mut definitions = self.types.iter().collect::<Vec<_>>();
            definitions.sort_by_key(|(type_name, _)| type_name.as_str());
            for (type_name, type_def) in definitions {
                if let Some(ref rules) = type_def.match_rules {
                    let compiled = self
                        .type_plans
                        .get(type_name)
                        .and_then(|plan| plan.match_expression.as_deref());
                    match matches_rules_checked_compiled(
                        rules,
                        compiled,
                        path,
                        frontmatter,
                        self.settings.timezone.as_deref(),
                    ) {
                        Ok(true) if !types.contains(type_name) => types.push(type_name.clone()),
                        Ok(_) => {}
                        Err(failure) => failures.push((type_name.clone(), failure)),
                    }
                }
            }
        }

        (types, failures)
    }
}
