//! Link traversal data for one query snapshot (`asFile()`, §8).
//!
//! Default traversal reuses stored graph winners. Explicit policies re-resolve
//! through the typed snapshot index; they never mutate the default link graph.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::Value;

use crate::expressions::evaluator::{extract_links_from_fm_value, ResolvedFileData};
use crate::links::resolver::{LinkResolutionIndex, LinkResolutionOptions, ResolutionKeys};
use crate::runtime::CatalogError;

/// Stored links resolved while building the link graph: source path → stored target → target path.
pub(crate) type StoredLinkTargets = HashMap<String, HashMap<String, String>>;

/// The records a query can traverse to, with the links they already resolve to.
///
/// A record's own stored links (frontmatter link fields and body links) were resolved once,
/// with their declared target types, when the link graph was built or indexed; traversal
/// reuses those targets so it agrees with backlinks and validation. Any other link value, such
/// as one built in an expression, is resolved by the same rules without target types, through
/// an index built on first use. Both are hash lookups, so traversal costs the same however large
/// the collection is.
#[derive(Debug, Default)]
pub struct LinkedFiles {
    files: Vec<ResolvedFileData>,
    by_path: HashMap<String, usize>,
    stored: StoredLinkTargets,
    keys: ResolutionKeys,
    index: OnceLock<LinkResolutionIndex>,
    policy_complete: bool,
}

impl LinkedFiles {
    /// `index`, when the caller already built one, saves building it on first use.
    pub(crate) fn new(
        files: Vec<ResolvedFileData>,
        stored: StoredLinkTargets,
        keys: ResolutionKeys,
        index: Option<LinkResolutionIndex>,
    ) -> Self {
        let by_path = files
            .iter()
            .enumerate()
            .map(|(position, file)| (file.path.clone(), position))
            .collect();
        Self {
            files,
            by_path,
            stored,
            keys,
            policy_complete: index.is_some(),
            index: index.map(OnceLock::from).unwrap_or_default(),
        }
    }

    /// Prepare policy evidence only for a plan that can request it. Cached
    /// default traversals retain their stored-winner/untyped-index fast paths.
    pub(crate) fn prepare_policy_index(&mut self, collection: &crate::Collection) {
        if !self.policy_complete {
            self.index = OnceLock::from(collection.build_link_resolution_index(&self.files));
            self.policy_complete = true;
        }
    }

    /// Every record, in snapshot order.
    pub fn files(&self) -> &[ResolvedFileData] {
        &self.files
    }

    pub fn get(&self, path: &str) -> Option<&ResolvedFileData> {
        self.by_path
            .get(path)
            .map(|&position| &self.files[position])
    }

    /// The record a link value points to, read from the record at `source_path`.
    pub(crate) fn resolve(
        &self,
        link: &str,
        source_path: Option<&str>,
    ) -> Result<Option<&ResolvedFileData>, CatalogError> {
        self.resolve_with_options(link, source_path, None)
    }

    pub(crate) fn resolve_with_options(
        &self,
        link: &str,
        source_path: Option<&str>,
        options: Option<&LinkResolutionOptions>,
    ) -> Result<Option<&ResolvedFileData>, CatalogError> {
        self.resolve_with_options_from(link, source_path, source_path, options)
    }

    /// An explicit lexical source override must not discard the originating
    /// record's declared target constraints.
    pub(crate) fn resolve_with_options_from(
        &self,
        link: &str,
        source_path: Option<&str>,
        declaration_source: Option<&str>,
        options: Option<&LinkResolutionOptions>,
    ) -> Result<Option<&ResolvedFileData>, CatalogError> {
        if let Some(options) = options {
            // Hosted neighborhoods contain winners, not the complete candidate
            // universe. This remains false even after a default traversal lazily
            // builds the neighborhood's untyped index.
            if !self.policy_complete {
                return Err(CatalogError {
                    code: "link_resolution_options_context_required".into(),
                    message: "asFile options require a complete typed resolution index".into(),
                });
            }
            let index = self.index.get().expect("complete policy index");
            for name in &options.types {
                if !index.known_types.contains(&name.to_lowercase()) {
                    return Err(CatalogError {
                        code: "invalid_link_resolution_options".into(),
                        message: format!("Unknown asFile target type '{name}'"),
                    });
                }
            }
        }
        let target = if options.is_some() {
            let Some(target) = traversal_target(link)? else {
                return Ok(None);
            };
            target
        } else {
            // Preserve no-option Reader/TaskNotes/Connect linksTo recipes.
            // Remove this parser distinction only with an approved default-semantics
            // revision and its consumer migration, not options capability adoption.
            let mut targets = Vec::new();
            extract_links_from_fm_value(&Value::String(link.to_string()), &mut targets);
            match targets.as_slice() {
                [target] => target.clone(),
                _ => link.to_string(),
            }
        };
        let target = target.as_str();
        if options.is_some() {
            match crate::links::resolver::ResolutionLookup::of(
                target,
                source_path.unwrap_or_default(),
            ) {
                None => {
                    return Err(CatalogError {
                        code: "path_traversal".into(),
                        message: "Link target crosses the collection root".into(),
                    })
                }
                Some(crate::links::resolver::ResolutionLookup::Path(paths)) => {
                    if paths
                        .iter()
                        .any(|path| crate::api::CollectionPath::new(path).is_err())
                    {
                        return Err(CatalogError {
                            code: "path_traversal".into(),
                            message: "Unsafe link target".into(),
                        });
                    }
                }
                Some(crate::links::resolver::ResolutionLookup::Simple(_)) => {}
            }
        }
        if options.is_none() {
            if let Some(source) = source_path {
                if let Some(path) = self.stored.get(source).and_then(|links| links.get(target)) {
                    return Ok(self.get(path));
                }
            }
        }
        let index = self
            .index
            .get_or_init(|| LinkResolutionIndex::untyped(&self.files, &self.keys));
        let source = source_path.unwrap_or_default();
        let path = if let Some(options) = options {
            let origin = declaration_source.unwrap_or(source);
            if index
                .declared_type_conflicts
                .get(origin)
                .is_some_and(|targets| targets.contains(target))
            {
                return Err(CatalogError {
                    code: "link_resolution_field_context_required".into(),
                    message:
                        "Conflicting stored-target declarations require field-specific provenance"
                            .into(),
                });
            }
            let declared = index
                .declared_types
                .get(origin)
                .and_then(|targets| targets.get(target))
                .map_or(&[][..], Vec::as_slice);
            index.resolve_with_options(target, source, declared, options)?
        } else {
            index.resolve(target, source, &[])?
        };
        Ok(path.and_then(|path| self.get(&path)))
    }
}

/// A scalar traversal must name one target, not silently choose from prose or
/// multiple links. Extraction retains Markdown/bare-path source relativity.
pub(crate) fn traversal_target(link: &str) -> Result<Option<String>, CatalogError> {
    let normalized = link.replace('\\', "/");
    let link = normalized.trim();
    let link = link
        .strip_prefix('!')
        .filter(|_| link.starts_with("!["))
        .unwrap_or(link);
    if link.is_empty() || link.starts_with('#') {
        return Ok(None);
    }
    if link.starts_with("http://") || link.starts_with("https://") || link.starts_with("mailto:") {
        return Ok(None);
    }
    let malformed = || CatalogError {
        code: "malformed_link".into(),
        message: "asFile requires one well-formed link target".into(),
    };
    if link.starts_with("[[")
        && (!link.ends_with("]]") || link[2..link.len() - 2].contains(['[', ']']))
    {
        return Err(malformed());
    }
    if link.starts_with('[')
        && !link.starts_with("[[")
        && (link.matches("](").count() != 1 || !link.ends_with(')'))
    {
        return Err(malformed());
    }
    if link.starts_with("[[#") {
        return Ok(None);
    }
    let mut targets = Vec::new();
    if link.starts_with('[') && !link.starts_with("[[") {
        let destination =
            crate::expressions::evaluator::markdown_link_destination(link).ok_or_else(malformed)?;
        let destination = destination.split('#').next().unwrap_or_default();
        if destination.is_empty() {
            return Ok(None);
        }
        targets.push(crate::expressions::evaluator::source_relative(
            destination.to_string(),
        ));
    } else {
        extract_links_from_fm_value(&Value::String(link.to_string()), &mut targets);
    }
    let target = match targets.as_slice() {
        [target] => target.clone(),
        [] if !link.starts_with('[') => link.to_string(),
        _ => return Err(malformed()),
    };
    let unmarked = target.strip_prefix("./").unwrap_or(&target);
    if unmarked.starts_with("http://")
        || unmarked.starts_with("https://")
        || unmarked.starts_with("mailto:")
    {
        return Ok(None);
    }
    if target.contains(['\n', '\r']) {
        return Err(malformed());
    }
    if target.contains('\0') || target.contains(':') || target.starts_with("//") {
        return Err(CatalogError {
            code: "path_traversal".into(),
            message: "Unsafe link target".into(),
        });
    }
    let target = if target.contains('/') && !target.starts_with("./") && !target.starts_with("../")
    {
        let rooted = target.starts_with('/');
        let normalized = crate::links::parser::normalize_segments(target.trim_start_matches('/'));
        if normalized == ".." || normalized.starts_with("../") {
            return Err(CatalogError {
                code: "path_traversal".into(),
                message: "Link target crosses the collection root".into(),
            });
        }
        if rooted {
            format!("/{normalized}")
        } else {
            normalized
        }
    } else {
        target
    };
    Ok(Some(target))
}
