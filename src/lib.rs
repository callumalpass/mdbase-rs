//! mdbase - Rust implementation of the mdbase specification.
//!
//! Uses SQLite as a backing store for queries, compiling mdbase expressions
//! to SQL WHERE clauses via json_extract().

pub mod errors;

pub mod api;
pub mod cache;
pub(crate) mod compat;
pub mod config;
pub mod expressions;
pub mod frontmatter;
pub mod generated;
pub mod init;
pub mod links;
pub mod matching;
pub mod operations;
pub mod query;
pub mod runtime;
pub mod runtime_contracts;
pub(crate) mod snapshot;
pub(crate) mod transactions;
pub mod types;
pub mod v03;
pub mod validation;
pub mod views;
pub mod watch;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::types::inheritance;
use crate::types::loader;
use crate::types::schema::*;

/// Parsed config settings used at runtime.
#[derive(Debug, Clone)]
pub struct Settings {
    pub extensions: Vec<String>,
    pub exclude: Vec<String>,
    pub include_subfolders: bool,
    pub types_folder: String,
    pub migrations_folder: String,
    pub explicit_type_keys: Vec<String>,
    pub write_defaults: bool,
    pub default_validation: String,
    pub default_strict: serde_json::Value,
    pub timezone: Option<String>,
    pub id_field: String,
    pub id_field_explicit: bool,
    pub write_nulls: String,
    pub write_empty_lists: bool,
    pub rename_update_refs: bool,
    pub cache_folder: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecProfile {
    V02,
    V03,
}

/// How a loaded collection relates to the canonical v0.3 data model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatibilityMode {
    /// The collection is already canonical v0.3.
    Canonical,
    /// Legacy v0.2 data is translated for read-only compatibility.
    V02ReadOnly,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            extensions: Vec::new(),
            exclude: vec![".git".into(), "node_modules".into(), ".mdbase".into()],
            include_subfolders: true,
            types_folder: "_types".into(),
            migrations_folder: "_types/_migrations".into(),
            explicit_type_keys: vec!["type".into(), "types".into()],
            write_defaults: true,
            default_validation: "warn".into(),
            default_strict: serde_json::Value::Bool(false),
            timezone: None,
            id_field: "id".into(),
            id_field_explicit: false,
            write_nulls: "omit".into(),
            write_empty_lists: true,
            rename_update_refs: true,
            cache_folder: ".mdbase".into(),
        }
    }
}

/// A loaded mdbase collection.
pub struct Collection {
    pub(crate) root: PathBuf,
    pub(crate) spec_profile: SpecProfile,
    pub(crate) settings: Settings,
    /// Namespaced collection configuration retained for optional adapters.
    pub(crate) config_extensions: serde_json::Map<String, serde_json::Value>,
    pub(crate) types: HashMap<String, TypeDef>,
    pub(crate) type_plans: HashMap<String, crate::types::compiled::CompiledTypePlan>,
    pub(crate) type_warnings: Vec<String>,
}

impl Collection {
    /// Authorized collection root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Immutable runtime settings loaded with this collection.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Specification profile detected at open time.
    pub fn spec_profile(&self) -> SpecProfile {
        self.spec_profile
    }

    /// Canonical or legacy compatibility mode.
    pub fn compatibility_mode(&self) -> CompatibilityMode {
        match self.spec_profile {
            SpecProfile::V03 => CompatibilityMode::Canonical,
            SpecProfile::V02 => CompatibilityMode::V02ReadOnly,
        }
    }

    /// Loaded type definitions keyed by canonical lowercase name.
    pub fn types(&self) -> &HashMap<String, TypeDef> {
        &self.types
    }

    /// Warnings produced while translating or loading type definitions.
    pub fn type_warnings(&self) -> &[String] {
        &self.type_warnings
    }

    /// Namespaced configuration values retained for optional adapters.
    pub fn config_extensions(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.config_extensions
    }

    /// Borrow the typed canonical operation service.
    pub fn typed(&self) -> api::MdbaseResult<api::TypedCollection<'_>> {
        api::TypedCollection::new(self)
    }

    /// Open a collection from a root directory.
    pub fn open(root: &Path) -> Result<Self, serde_json::Value> {
        let config_result = config::load_config_for_open(root);
        if config_result.get("valid") != Some(&serde_json::Value::Bool(true)) {
            return Err(config_result);
        }

        let config = &config_result["config"];
        let settings_json = &config["settings"];
        let spec_profile = match config["spec_profile"].as_str() {
            Some("v0.3") => SpecProfile::V03,
            _ => SpecProfile::V02,
        };

        let settings = Settings {
            extensions: settings_json["extensions"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            exclude: settings_json["exclude"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_else(|| vec![".git".into(), "node_modules".into(), ".mdbase".into()]),
            include_subfolders: settings_json["include_subfolders"]
                .as_bool()
                .unwrap_or(true),
            types_folder: settings_json["types_folder"]
                .as_str()
                .unwrap_or("_types")
                .to_string(),
            migrations_folder: settings_json["migrations_folder"]
                .as_str()
                .unwrap_or("_types/_migrations")
                .to_string(),
            explicit_type_keys: settings_json["explicit_type_keys"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_else(|| vec!["type".into(), "types".into()]),
            write_defaults: if spec_profile == SpecProfile::V03 {
                false
            } else {
                settings_json["write_defaults"].as_bool().unwrap_or(true)
            },
            default_validation: settings_json["default_validation"]
                .as_str()
                .unwrap_or("warn")
                .to_string(),
            default_strict: settings_json["default_strict"].clone(),
            timezone: settings_json["timezone"].as_str().map(|s| s.to_string()),
            id_field: settings_json["id_field"]
                .as_str()
                .unwrap_or("id")
                .to_string(),
            id_field_explicit: settings_json["id_field_explicit"]
                .as_bool()
                .unwrap_or(false),
            write_nulls: if spec_profile == SpecProfile::V03 {
                "explicit".to_string()
            } else {
                settings_json["write_nulls"]
                    .as_str()
                    .unwrap_or("omit")
                    .to_string()
            },
            write_empty_lists: settings_json["write_empty_lists"].as_bool().unwrap_or(true),
            rename_update_refs: settings_json["rename_update_refs"]
                .as_bool()
                .unwrap_or(true),
            cache_folder: settings_json["cache_folder"]
                .as_str()
                .unwrap_or(".mdbase")
                .to_string(),
        };
        let config_extensions: serde_json::Map<String, serde_json::Value> = config
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, _)| key.starts_with("x-"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();

        for (label, path) in [
            ("types_folder", settings.types_folder.as_str()),
            ("migrations_folder", settings.migrations_folder.as_str()),
            ("cache_folder", settings.cache_folder.as_str()),
        ] {
            crate::operations::ensure_safe_relative_path(path, spec_profile).map_err(|_| {
                crate::errors::op_error(
                    crate::errors::INVALID_CONFIG,
                    &format!("settings.{label} must be relative to the collection"),
                )
            })?;
            crate::operations::ensure_no_symlink_components(root, path, spec_profile)?;
        }

        // Recovery must precede type loading. A system migration may have
        // replaced or removed legacy type definitions before the config flip;
        // loading types first could otherwise make the recovery journal
        // unreachable. Re-open from scratch if recovery changed any files so
        // config and settings are parsed from the completed transaction.
        let recovery_collection = Collection {
            root: root.to_path_buf(),
            spec_profile,
            settings: settings.clone(),
            config_extensions: config_extensions.clone(),
            types: HashMap::new(),
            type_plans: HashMap::new(),
            type_warnings: Vec::new(),
        };
        let recovered =
            crate::transactions::recover_pending(&recovery_collection).map_err(|error| {
                serde_json::json!({
                    "valid": false,
                    "error": {
                        "code": error.code(),
                        "message": error.to_string(),
                    }
                })
            })?;
        if recovered {
            return Self::open(root);
        }

        // Load types
        let load_result = loader::load_types_with_warnings(
            root,
            &settings.types_folder,
            &settings.migrations_folder,
        )
        .map_err(|e| {
            serde_json::json!({
                "valid": false,
                "error": { "code": "invalid_type_definition", "message": e }
            })
        })?;

        let mut types = load_result.types;
        let type_warnings = load_result.warnings;

        // Resolve inheritance
        inheritance::resolve_inheritance(&mut types).map_err(|e| {
            // Determine error code based on the message
            let code = if e.contains("Circular") {
                "circular_inheritance"
            } else if e.contains("Unknown type") || e.contains("extends unknown") {
                "missing_parent_type"
            } else {
                "invalid_type_definition"
            };
            serde_json::json!({
                "valid": false,
                "error": { "code": code, "message": e }
            })
        })?;

        let type_plans = crate::types::compiled::compile_registry(&types).map_err(|error| {
            serde_json::json!({
                "valid": false,
                "error": {
                    "code": error.code,
                    "message": error.message,
                }
            })
        })?;

        let collection = Collection {
            root: root.to_path_buf(),
            spec_profile,
            settings,
            config_extensions,
            types,
            type_plans,
            type_warnings,
        };
        Ok(collection)
    }

    pub(crate) fn config_strict_mode(&self) -> StrictMode {
        match &self.settings.default_strict {
            serde_json::Value::Bool(true) => StrictMode::Error,
            serde_json::Value::Bool(false) => StrictMode::Off,
            serde_json::Value::String(s) if s == "warn" => StrictMode::Warn,
            _ => StrictMode::Off,
        }
    }

    /// Scan all Markdown files in the collection.
    ///
    /// Discovery failures are explicit so callers cannot confuse an
    /// incomplete collection with an empty or smaller one.
    pub(crate) fn scan_collection_files_checked(
        &self,
    ) -> Result<Vec<PathBuf>, crate::snapshot::CollectionScanError> {
        let mut files = Vec::new();
        self.scan_dir_recursive_checked(&self.root, &mut files)?;
        files.sort();
        Ok(files)
    }

    /// Transitional wrapper for legacy operations that have not yet adopted
    /// the typed snapshot boundary.
    pub(crate) fn scan_collection_files(&self) -> Vec<PathBuf> {
        self.scan_collection_files_checked().unwrap_or_default()
    }

    fn scan_dir_recursive_checked(
        &self,
        dir: &Path,
        files: &mut Vec<PathBuf>,
    ) -> Result<(), crate::snapshot::CollectionScanError> {
        use crate::snapshot::CollectionScanError;

        let entries =
            std::fs::read_dir(dir).map_err(|source| CollectionScanError::ReadDirectory {
                path: dir.to_path_buf(),
                source,
            })?;
        for entry in entries {
            let entry = entry.map_err(|source| CollectionScanError::ReadEntry {
                directory: dir.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            let file_type =
                entry
                    .file_type()
                    .map_err(|source| CollectionScanError::InspectEntry {
                        path: path.clone(),
                        source,
                    })?;

            // Never follow links while discovering collection resources. A
            // symlink below the authorized root can otherwise expose an
            // unrelated directory to query, cache, links, runtime loading, or
            // watch even when direct CRUD paths are contained.
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if self.settings.include_subfolders {
                    let rel = path
                        .strip_prefix(&self.root)
                        .map_err(|_| CollectionScanError::OutsideRoot { path: path.clone() })?
                        .to_string_lossy()
                        .to_string();
                    if !self.is_excluded(&rel) {
                        self.scan_dir_recursive_checked(&path, files)?;
                    }
                }
            } else if file_type.is_file() {
                let rel = path
                    .strip_prefix(&self.root)
                    .map_err(|_| CollectionScanError::OutsideRoot { path: path.clone() })?
                    .to_string_lossy()
                    .to_string();
                if !self.is_excluded(&rel) && self.is_valid_extension(&rel) {
                    files.push(path);
                }
            }
        }
        Ok(())
    }
}

// Standalone helper functions have been extracted to their respective modules:
// - op_error, issue_to_json -> crate::errors
// - apply_transform, slugify, derive_path -> crate::generated
// - coerce_value -> crate::validation::coercion
// - match_glob_pattern -> crate::matching::glob
// - count_leading_dotdot, normalize_link_path, normalize_segments -> crate::links::parser
// - compute_relative_path -> crate::links::resolver
// - expression_references_field, expr_contains_ident, is_truthy_value -> crate::expressions
// - QueryEvalContext -> crate::query::engine
