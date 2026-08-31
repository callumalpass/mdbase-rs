//! Query execution (§10).

pub mod cache_source;
pub(crate) mod canonical {
    //! Version-neutral parsed query model, semantic preflight, execution, and results.
    pub(crate) mod context;
    pub(crate) mod diagnostics;
    pub(crate) mod execute;
    pub(crate) mod model;
    pub(crate) mod preflight;
    pub(crate) mod result;

    pub use execute::QueryPerformance;
    pub(crate) use execute::{execute_model_profiled_cancellable, execute_typed, QueryEvaluation};
    #[cfg(test)]
    pub(crate) use execute::{
        record_typed_request_json_encode, record_wire_query_decode, record_wire_schema_validation,
    };
    pub(crate) use model::Query;
}
pub mod engine;
pub mod planner;
pub mod results;
