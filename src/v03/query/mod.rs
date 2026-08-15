//! Canonical v0.3 query validation, semantic preflight, and execution.

pub(crate) mod context;
mod diagnostics;
mod execute;
pub(crate) mod model;
pub(crate) mod preflight;
mod result;

pub use execute::QueryPerformance;
pub(crate) use execute::{
    execute, execute_cancellable, execute_profiled, execute_runtime_cancellable,
};
