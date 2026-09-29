//! Resolution keys a record contributes and the lookups a link target makes.
//! Both the complete in-memory index and the cache's candidate index use
//! these, so they cannot disagree about which records a link can name.

use std::path::Path;

use super::resolver::ResolutionKeys;

/// Which resolution key class a record key belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResolutionKeyKind {
    Basename,
    Id,
    Title,
}

impl ResolutionKeyKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Basename => "basename",
            Self::Id => "id",
            Self::Title => "title",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "basename" => Some(Self::Basename),
            "id" => Some(Self::Id),
            "title" => Some(Self::Title),
            _ => None,
        }
    }
}

/// The lowercased resolution keys a record contributes, or `None` when its path
/// cannot be a link target.
pub(crate) fn record_resolution_keys(
    path: &str,
    frontmatter: &serde_json::Value,
    keys: &ResolutionKeys,
) -> Option<Vec<(ResolutionKeyKind, String)>> {
    if crate::api::CollectionPath::new(path).is_err() {
        return None;
    }
    let mut record_keys = Vec::new();
    if let Some(basename) = Path::new(path).file_stem().and_then(|s| s.to_str()) {
        record_keys.push((ResolutionKeyKind::Basename, basename.to_lowercase()));
    }
    let text = |field: &str| frontmatter.get(field).and_then(|v| v.as_str());
    if let Some(id) = keys.id_field.as_deref().and_then(text) {
        record_keys.push((ResolutionKeyKind::Id, id.to_lowercase()));
    }
    if let Some(title) = keys.titles.then(|| text("title")).flatten() {
        record_keys.push((ResolutionKeyKind::Title, title.to_lowercase()));
    }
    Some(record_keys)
}

/// What resolving one link target reads from the index.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ResolutionLookup {
    /// A simple name: lowercased key tried as an ID, basename, then title.
    Simple(String),
    /// Explicit collection paths, tried in order.
    Path(Vec<String>),
}

impl ResolutionLookup {
    /// Spec Chapter 08: markdown links and bare paths resolve from the
    /// containing folder, as do wikilinks beginning with ./ or ../; other
    /// wikilinks containing / resolve from the collection root; a leading
    /// / is always root-relative.
    pub(crate) fn of(target: &str, source_path: &str) -> Option<Self> {
        let (target, from_source) = link_path(target);
        let target = target.as_str();
        if target.is_empty() {
            return None;
        }
        let resolved_target = if let Some(rooted) = target.strip_prefix('/') {
            crate::links::parser::normalize_segments(rooted)
        } else if from_source {
            let source_dir = source_path
                .rsplit_once('/')
                .map_or("", |(parent, _)| parent);
            let joined = if source_dir.is_empty() {
                target.to_string()
            } else {
                format!("{source_dir}/{target}")
            };
            crate::links::parser::normalize_segments(&joined)
        } else {
            // A simple wikilink may name the file with its record extension.
            target
                .strip_suffix(".md")
                .filter(|name| !name.contains('/'))
                .unwrap_or(target)
                .to_string()
        };
        if resolved_target == ".." || resolved_target.starts_with("../") {
            return None;
        }
        if !from_source && !target.starts_with('/') && !resolved_target.contains('/') {
            return Some(Self::Simple(resolved_target.to_lowercase()));
        }
        let mut candidates = vec![resolved_target.clone()];
        if !resolved_target.ends_with(".md") && !resolved_target.ends_with(".mdx") {
            candidates.push(format!("{resolved_target}.md"));
        }
        Some(Self::Path(candidates))
    }
}

/// The path a link names, and whether it resolves from the containing folder.
///
/// Input is either a raw link value or a target produced by link extraction,
/// which marks markdown and bare-path targets with `./` (spec Chapter 08).
fn link_path(link: &str) -> (String, bool) {
    let link = link.trim();
    let target = if let Some(inner) = link
        .strip_prefix("[[")
        .and_then(|rest| rest.strip_suffix("]]"))
    {
        let target = inner.split('|').next().unwrap_or(inner);
        target
            .split('#')
            .next()
            .unwrap_or(target)
            .trim()
            .to_string()
    } else if let Some(destination) = link
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(')'))
        .and_then(|inner| inner.split_once("]("))
        .map(|(_, destination)| destination.split('#').next().unwrap_or(destination).trim())
    {
        if destination.starts_with('/')
            || destination.starts_with("./")
            || destination.starts_with("../")
        {
            destination.to_string()
        } else {
            format!("./{destination}")
        }
    } else {
        link.split('#').next().unwrap_or(link).trim().to_string()
    };
    let from_source = target.starts_with("./") || target.starts_with("../");
    (target, from_source)
}
