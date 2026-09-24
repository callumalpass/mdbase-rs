//! The files an Obsidian Bases evaluation can reach, with lookups built once per evaluation.

use std::collections::HashMap;
use std::sync::OnceLock;

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::expression::{
    ensure_markdown_extension, normalize_path, strip_markdown_extension, strip_subpath, wikilink,
    BasesFile, BasesLink,
};

/// Links written as whole property values, in property and list order, as Obsidian lists them
/// before a note's body links: wikilinks, and Markdown links such as `[Label](Note.md)`.
pub(crate) fn frontmatter_links(properties: &Map<String, Value>) -> Vec<BasesLink> {
    static MARKDOWN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\[([^\]]*)\]\(([^)]+)\)$").expect("markdown link expression")
    });
    let link = |value: &Value| {
        let text = value.as_str()?.trim();
        wikilink(text).or_else(|| {
            let captures = MARKDOWN.captures(text)?;
            Some(BasesLink {
                path: captures[2].to_string(),
                display: Some(captures[1].to_string()),
                ..Default::default()
            })
        })
    };
    properties
        .values()
        .flat_map(|value| match value {
            Value::Array(items) => items.iter().filter_map(link).collect::<Vec<_>>(),
            value => link(value).into_iter().collect(),
        })
        .collect()
}

/// Every file a view evaluation may open through `file()`, `asFile()` or `this`.
///
/// Link targets that the resolution map does not name are found with Obsidian's fallback
/// rules: exact path, path with `.md`, path suffix, basename or name, then the same
/// case-insensitively. Each rule is a map from its key to the first file, in list order, that
/// satisfies it, so a lookup returns the file a scan in that order would, without the scan.
#[derive(Debug, Default)]
pub(crate) struct BasesFiles {
    files: Vec<BasesFile>,
    lookup: OnceLock<FileLookup>,
}

#[derive(Debug, Default)]
struct FileLookup {
    path: HashMap<String, usize>,
    normalized: HashMap<String, usize>,
    suffix: HashMap<String, usize>,
    basename: HashMap<String, usize>,
    stem: HashMap<String, usize>,
    lower_normalized: HashMap<String, usize>,
    lower_name: HashMap<String, usize>,
    lower_basename: HashMap<String, usize>,
}

impl FromIterator<BasesFile> for BasesFiles {
    fn from_iter<T: IntoIterator<Item = BasesFile>>(files: T) -> Self {
        Self::new(files.into_iter().collect())
    }
}

impl BasesFiles {
    pub(crate) fn new(files: Vec<BasesFile>) -> Self {
        Self {
            files,
            lookup: OnceLock::new(),
        }
    }

    pub(crate) fn iter(&self) -> std::slice::Iter<'_, BasesFile> {
        self.files.iter()
    }

    /// The file at exactly this collection path.
    pub(crate) fn by_path(&self, path: &str) -> Option<&BasesFile> {
        self.at(self.lookup().path.get(path).copied())
    }

    /// The file a link target names under Obsidian's fallback rules, in their priority order.
    pub(crate) fn find(&self, target: &str) -> Option<&BasesFile> {
        let lookup = self.lookup();
        let target = strip_subpath(target);
        let markdown = ensure_markdown_extension(&target);
        let lower_target = target.to_lowercase();
        let lower_markdown = markdown.to_lowercase();
        let lower_basename = strip_markdown_extension(&target).to_lowercase();
        let first =
            |candidates: &[Option<&usize>]| candidates.iter().flatten().map(|at| **at).min();
        let found = first(&[lookup.normalized.get(&normalize_path(&target))])
            .or_else(|| first(&[lookup.normalized.get(&normalize_path(&markdown))]))
            .or_else(|| first(&[lookup.suffix.get(&markdown)]))
            .or_else(|| first(&[lookup.basename.get(&target), lookup.stem.get(&target)]))
            .or_else(|| first(&[lookup.lower_normalized.get(&lower_target)]))
            .or_else(|| first(&[lookup.lower_normalized.get(&lower_markdown)]))
            .or_else(|| {
                first(&[
                    lookup.lower_name.get(&lower_target),
                    lookup.lower_name.get(&lower_markdown),
                ])
            })
            .or_else(|| first(&[lookup.lower_basename.get(&lower_basename)]));
        self.at(found)
    }

    fn at(&self, position: Option<usize>) -> Option<&BasesFile> {
        position.and_then(|position| self.files.get(position))
    }

    fn lookup(&self) -> &FileLookup {
        self.lookup.get_or_init(|| {
            let mut lookup = FileLookup::default();
            for (position, file) in self.files.iter().enumerate() {
                let keep = |map: &mut HashMap<String, usize>, key: String| {
                    map.entry(key).or_insert(position);
                };
                let normalized = normalize_path(&file.path);
                keep(&mut lookup.path, file.path.clone());
                // `path.ends_with("/{target}")` holds exactly when the target is the path after
                // one of its slashes.
                for (slash, _) in normalized.match_indices('/') {
                    keep(&mut lookup.suffix, normalized[slash + 1..].to_string());
                }
                keep(&mut lookup.lower_normalized, normalized.to_lowercase());
                keep(&mut lookup.normalized, normalized);
                keep(&mut lookup.basename, file.basename.clone());
                keep(&mut lookup.stem, strip_markdown_extension(&file.name));
                keep(&mut lookup.lower_name, file.name.to_lowercase());
                keep(&mut lookup.lower_basename, file.basename.to_lowercase());
            }
            lookup
        })
    }
}
