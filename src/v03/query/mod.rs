//! Canonical v0.3 query validation, semantic preflight, and execution.

mod context;
mod diagnostics;
mod execute;
mod model;
mod preflight;
mod result;

pub use execute::QueryPerformance;
pub(crate) use execute::{execute, execute_profiled};
