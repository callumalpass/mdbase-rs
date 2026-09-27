//! Collection path globs.

/// Match a portable mdbase glob (spec Chapter 02, "Path Globs") against a
/// complete collection-relative path.
///
/// `*` and `?` match within one path component, `[...]` matches one character
/// from a set or range (`[!...]` negates), and `**` as a whole component
/// matches zero or more components. Matching is case-sensitive over Unicode
/// scalar values.
pub(crate) fn portable_glob_match(pattern: &str, path: &str) -> bool {
    let pattern_parts: Vec<&str> = pattern.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();
    match_components(&path_parts, &pattern_parts)
}

fn match_components(path: &[&str], pattern: &[&str]) -> bool {
    let Some((first, rest)) = pattern.split_first() else {
        return path.is_empty();
    };
    if *first == "**" {
        return match_components(path, rest)
            || (!path.is_empty() && match_components(&path[1..], pattern));
    }
    match path.split_first() {
        Some((component, remaining)) => {
            let component: Vec<char> = component.chars().collect();
            let segment: Vec<char> = first.chars().collect();
            match_segment(&component, &segment) && match_components(remaining, rest)
        }
        None => false,
    }
}

fn match_segment(text: &[char], pattern: &[char]) -> bool {
    let Some((&head, rest)) = pattern.split_first() else {
        return text.is_empty();
    };
    match head {
        '*' => {
            match_segment(text, rest) || (!text.is_empty() && match_segment(&text[1..], pattern))
        }
        '?' => !text.is_empty() && match_segment(&text[1..], rest),
        '[' => match parse_class(rest) {
            Some((class, after)) => text
                .split_first()
                .is_some_and(|(&c, remaining)| class.matches(c) && match_segment(remaining, after)),
            None => text
                .split_first()
                .is_some_and(|(&c, remaining)| c == '[' && match_segment(remaining, rest)),
        },
        literal => text
            .split_first()
            .is_some_and(|(&c, remaining)| c == literal && match_segment(remaining, rest)),
    }
}

struct CharClass {
    negated: bool,
    ranges: Vec<(char, char)>,
}

impl CharClass {
    fn matches(&self, c: char) -> bool {
        self.ranges.iter().any(|&(low, high)| low <= c && c <= high) != self.negated
    }
}

/// Parse a class body after `[`, returning the class and the pattern after `]`.
fn parse_class(body: &[char]) -> Option<(CharClass, &[char])> {
    let (negated, mut index) = match body.first() {
        Some('!') => (true, 1),
        _ => (false, 0),
    };
    let mut ranges = Vec::new();
    let mut first = true;
    while index < body.len() {
        let c = body[index];
        if c == ']' && !first {
            return Some((CharClass { negated, ranges }, &body[index + 1..]));
        }
        if index + 2 < body.len() && body[index + 1] == '-' && body[index + 2] != ']' {
            ranges.push((c, body[index + 2]));
            index += 3;
        } else {
            ranges.push((c, c));
            index += 1;
        }
        first = false;
    }
    None
}

/// Legacy v0.2 exclude matcher. Bare names exclude a directory prefix.
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

#[cfg(test)]
mod tests {
    use super::portable_glob_match;

    #[test]
    fn portable_globs_follow_the_spec_rules() {
        let cases = [
            ("tasks/**", "tasks/a.md", true),
            ("tasks/**", "tasks/deep/a.md", true),
            ("tasks/**/*.md", "tasks/a.md", true),
            ("tasks/**/*.md", "tasks/x/y/a.md", true),
            ("archive/*.md", "archive/old.md", true),
            ("archive/*.md", "archive/2025/older.md", false),
            ("notes/?.md", "notes/é.md", true),
            ("notes/[a-c].md", "notes/b.md", true),
            ("notes/[!a-c].md", "notes/b.md", false),
            ("Notes/*.md", "notes/a.md", false),
            ("**/*.base", "views/tasks.base", true),
            ("*.md", "tasks/a.md", false),
        ];
        for (pattern, path, expected) in cases {
            assert_eq!(
                portable_glob_match(pattern, path),
                expected,
                "{pattern} {path}"
            );
        }
    }
}
