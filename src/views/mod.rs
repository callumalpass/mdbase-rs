//! Saved-view discovery and execution.

mod execute;
mod expression;
mod model;

pub use model::{NamedViewDescriptor, ViewDocumentDescriptor, ViewPresentation};

use serde_json::Value;

use crate::v03::OperationResult;
use crate::Collection;

pub(crate) fn list(collection: &Collection, input: &Value) -> OperationResult {
    execute::list_views(collection, input)
}

pub(crate) fn execute(collection: &Collection, input: &Value) -> OperationResult {
    execute::execute_view(collection, input)
}
