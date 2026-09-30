//! Validation (§9).

pub mod coercion;
pub(crate) mod cross_record;
pub mod fields;
pub mod merge;
#[cfg(all(test, feature = "legacy-collection-mutation"))]
mod strict_write_tests;
pub mod validator;
