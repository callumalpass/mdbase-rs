//! Saved-view discovery and execution.

mod execute;
mod expression;
mod files;
mod model;
mod source;

pub use model::{
    NamedViewDescriptor, ViewDocumentDescriptor, ViewPresentation, ViewPropertyDescriptor,
};

use crate::api::CollectionPath;
use crate::diagnostic::Diagnostic;
use crate::v03::{validate_view, OperationResult};
use crate::Collection;
use serde_json::{json, Value};

pub(crate) const VIEW_CONTRACT: &str = "mdbase.view";
const VIEW_CONTRACT_VERSION: &str = "1.0.0";
const BASE_CONTRACT: &str = "obsidian.base";
const BASE_CONTRACT_VERSION: &str = "1.0.0";

/// Whether a record with these matched types is an Obsidian Base stored as a
/// record: one of its types implements `obsidian.base` (spec Obsidian Bases
/// adapter, "Bases as records").
pub(crate) fn implements_base_contract(collection: &Collection, types: &[String]) -> bool {
    let implementing = collection
        .data_contracts
        .implementations(BASE_CONTRACT, BASE_CONTRACT_VERSION);
    types
        .iter()
        .any(|name| implementing.iter().any(|entry| &entry.type_name == name))
}

/// A record resolved through the `mdbase.view` record contract (spec Chapter 11).
pub(crate) enum ViewRecord {
    /// No matched type implements `mdbase.view`.
    NotView,
    /// The validated `mdbase.view` contract view.
    View(Value),
    Invalid(Vec<Diagnostic>),
}

impl ViewRecord {
    /// The contract view, or the failure an operation addressing `path` reports.
    pub(crate) fn into_view(self, path: &str) -> Result<Value, OperationResult> {
        let diagnostics = match self {
            Self::View(view) => return Ok(view),
            Self::NotView => vec![Diagnostic::error(
                "view_not_found",
                format!("Record '{path}' is not a saved view."),
                Some(path.to_string()),
            )],
            Self::Invalid(diagnostics) => diagnostics,
        };
        Err(OperationResult {
            valid: false,
            result: json!({}),
            diagnostics,
        })
    }
}

/// Resolve a record through its one type implementing `mdbase.view`.
/// `effective` runs only for records of an implementing type.
pub(crate) fn resolve_view_record(
    collection: &Collection,
    path: &str,
    types: &[String],
    effective: impl FnOnce() -> Value,
) -> ViewRecord {
    let implementing = collection
        .data_contracts
        .implementations(VIEW_CONTRACT, VIEW_CONTRACT_VERSION);
    let mut candidates = types
        .iter()
        .filter(|name| implementing.iter().any(|entry| &entry.type_name == *name));
    let Some(type_name) = candidates.next() else {
        return ViewRecord::NotView;
    };
    let invalid = |message: String| Diagnostic::error("invalid_view", message, Some(path.into()));
    if candidates.next().is_some() {
        let message = format!("Record matches several types implementing {VIEW_CONTRACT}.");
        return ViewRecord::Invalid(vec![invalid(message)]);
    }
    let projected = collection.project_contract_type(
        type_name,
        VIEW_CONTRACT,
        VIEW_CONTRACT_VERSION,
        &effective(),
    );
    let diagnostics = if projected.valid {
        // An installed contract copy may differ locally; execution depends on
        // the canonical view shape regardless.
        validate_view(&projected.view, path)
    } else {
        let messages = projected.diagnostics.into_iter();
        messages
            .map(|diagnostic| invalid(diagnostic.message))
            .collect()
    };
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == "error")
    {
        ViewRecord::Invalid(diagnostics)
    } else {
        ViewRecord::View(projected.view)
    }
}

/// Effective frontmatter for a raw record, as `FileRecord` defines it.
pub(crate) fn effective_frontmatter(
    collection: &Collection,
    types: &[String],
    raw: &Value,
) -> Value {
    collection.coerce_types(&collection.apply_defaults(raw, types), types)
}

pub(crate) fn list(collection: &Collection, input: &Value) -> OperationResult {
    execute::list_views(collection, input)
}

pub(crate) fn execute(collection: &Collection, input: &Value) -> OperationResult {
    execute::execute_view(collection, input)
}

pub(crate) fn read_source(collection: &Collection, input: &Value) -> OperationResult {
    source::read(collection, input)
}

pub(crate) fn create_source(collection: &Collection, input: &Value) -> OperationResult {
    source::create(collection, input)
}

pub(crate) fn update_source(collection: &Collection, input: &Value) -> OperationResult {
    source::update(collection, input)
}

pub(crate) fn delete_source(collection: &Collection, input: &Value) -> OperationResult {
    source::delete(collection, input)
}

pub(crate) use execute::{
    base_uses_backlinks, combined_filter_matches, evaluate_property, is_configured_obsidian_source,
    validate_base_expressions,
};
pub(crate) use execute::{prepare_hosted_canonical_view, verify_canonical_view_context};
pub(crate) use expression::{
    lower_hosted_candidate, serialize_bases_file, uses_file_ctime, uses_relationships,
    BasesEvaluationContext, BasesFile, BasesLink, BasesTimezone, BASES_OPERATION_CANCELLED,
    BASES_WORK_BUDGET_EXCEEDED,
};
pub(crate) use files::{frontmatter_links, BasesFiles};
pub(crate) use model::{
    stable_named_view_ids, BaseFilter, ObsidianBaseDocument, ObsidianBaseView, ViewReferenceInput,
};

fn normalized_source_path(path: &str) -> Option<CollectionPath> {
    let path = CollectionPath::new(path).ok()?;
    path.as_str()
        .split('/')
        .all(|component| !component.starts_with('.'))
        .then_some(path)
}

/// A minimal `mdbase.view` contract and canonical `view` type for tests.
#[cfg(test)]
pub(crate) mod view_contract_fixture {
    use std::path::Path;

    pub(crate) const CONTRACT_PATH: &str = "_contracts/mdbase.view.md";
    pub(crate) const CONTRACT: &str = "---\nkind: mdbase.contract\ncontract_type: record\nid: mdbase.view\nversion: 1.0.0\nrecord_schema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    properties:\n      id: {}\n      version: {}\n      name: {}\n      query: {}\n      views: {}\n---\n";
    pub(crate) const TYPE_PATH: &str = "_types/view.md";
    pub(crate) const TYPE: &str = "---\nkind: mdbase.type\nname: view\nversion: 1\nmatch:\n  where:\n    type: view\nschema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n    properties:\n      id: {}\n      version: {}\n      name: {}\n      query: {}\n      views: {}\nimplements:\n  - contract: mdbase.view\n    version: 1.0.0\n    fields:\n      id: id\n      version: version\n      name: name\n      query: query\n      views: views\n---\n";

    pub(crate) fn documents() -> [(&'static str, &'static str); 2] {
        [(CONTRACT_PATH, CONTRACT), (TYPE_PATH, TYPE)]
    }

    pub(crate) fn install(root: &Path) {
        for (path, document) in documents() {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, document).unwrap();
        }
    }
}
