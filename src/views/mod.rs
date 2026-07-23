//! Saved-view discovery and execution.

mod execute;
mod expression;
mod model;
mod source;

pub use model::{
    NamedViewDescriptor, ViewDocumentDescriptor, ViewPresentation, ViewPropertyDescriptor,
};

use serde_json::Value;

use crate::v03::OperationResult;
use crate::Collection;

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
