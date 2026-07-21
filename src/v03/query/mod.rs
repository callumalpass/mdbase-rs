//! Canonical v0.3 query validation, semantic preflight, and execution.

mod execute;
mod model;
mod preflight;

pub(crate) use execute::execute;
