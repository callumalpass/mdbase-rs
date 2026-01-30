//! mdbase - Rust implementation of the mdbase specification.
//!
//! Uses SQLite as a backing store for queries, compiling mdbase expressions
//! to SQL WHERE clauses via json_extract().

pub mod errors;

pub mod cache;
pub mod config;
pub mod expressions;
pub mod frontmatter;
pub mod generated;
pub mod links;
pub mod matching;
pub mod operations;
pub mod query;
pub mod types;
pub mod validation;
pub mod watch;
