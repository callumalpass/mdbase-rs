//! Link parsing, resolution, and traversal (§8).

pub mod extractor;
pub mod linked_files;
pub mod parser;
mod resolution_keys;
pub mod resolver;
pub mod traversal;

#[cfg(test)]
mod policy_tests;
