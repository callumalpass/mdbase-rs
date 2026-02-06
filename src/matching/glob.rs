//! path_glob matching (§6.4).

/// Simple glob pattern matcher for exclude patterns.
pub(crate) fn match_glob_pattern(pattern: &str, path: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path.starts_with(&format!("{}/", prefix)) || path == prefix;
    }

    if pattern.starts_with("*.") {
        let ext = &pattern[1..]; // e.g., ".draft.md"
        return path.ends_with(ext);
    }

    if pattern.contains('*') {
        // Simple wildcard matching
        let parts: Vec<&str> = pattern.split('*').collect();
        if parts.len() == 2 {
            return path.starts_with(parts[0]) && path.ends_with(parts[1]);
        }
    }

    // Exact match (directory name)
    path == pattern || path.starts_with(&format!("{}/", pattern))
}
