//! Import and migration adapters for older collection formats.

#[cfg(feature = "legacy-collection-mutation")]
mod legacy_mutation;
pub(crate) mod v02;
pub(crate) mod v02_migration;
