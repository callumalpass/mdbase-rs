//! Link traversal data for one query snapshot (`asFile()`, §8).

use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::Value;

use crate::expressions::evaluator::{extract_links_from_fm_value, ResolvedFileData};
use crate::links::resolver::LinkResolutionIndex;
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
    id_field: String,
    index: OnceLock<LinkResolutionIndex>,
}

impl LinkedFiles {
    /// `index`, when the caller already built one, saves building it on first use.
    pub(crate) fn new(
        files: Vec<ResolvedFileData>,
        stored: StoredLinkTargets,
        id_field: &str,
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
            id_field: id_field.to_string(),
            index: index.map(OnceLock::from).unwrap_or_default(),
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
        let mut targets = Vec::new();
        extract_links_from_fm_value(&Value::String(link.to_string()), &mut targets);
        if let ([target], Some(source)) = (targets.as_slice(), source_path) {
            if let Some(path) = self.stored.get(source).and_then(|links| links.get(target)) {
                return Ok(self.get(path));
            }
        }
        let index = self
            .index
            .get_or_init(|| LinkResolutionIndex::untyped(&self.files, &self.id_field));
        let path = index.resolve(link, source_path.unwrap_or_default(), &[])?;
        Ok(path.and_then(|path| self.get(&path)))
    }
}
