//! Typed Rust implementation of the mdbase specification.
//!
//! Markdown files remain authoritative. Canonical v0.3 operations are exposed
//! through [`Collection::typed`]; the SQLite query index is a derived,
//! fallible accelerator with authoritative disk fallback. Legacy v0.2
//! collections open through an isolated read/query adapter and require an
//! explicit migration before mutation.

/// Version of the collection engine embedded by a host.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod errors;

pub mod api;
pub mod cache;
pub mod cancellation;
pub(crate) mod cel;
mod collection_root;
pub(crate) mod compat;
pub mod config;
pub mod data_contracts;
pub(crate) mod diagnostic;
pub mod expressions;
pub mod field_reference;
pub(crate) mod field_references;
pub mod file_path;
pub mod frontmatter;
pub mod generated;
pub mod init;
pub mod links;
pub mod matching;
pub(crate) mod mutation;
pub mod operations;
pub mod query;
pub(crate) mod record_load;
pub mod record_path;
pub mod runtime;
pub(crate) mod snapshot;
pub(crate) mod transactions;
pub mod types;
pub mod v03;
pub mod validation;
pub mod views;
pub mod watch;

pub use cancellation::{OperationCancellation, OperationCancelled, OperationStopReason};
pub use snapshot::{CollectionDiscoveryCause, CollectionSnapshotError};

#[cfg(all(test, unix))]
pub(crate) use snapshot::replace_descendant_on_scan_for_test;
#[cfg(test)]
pub(crate) use snapshot::{
    cancel_scan_after_entries_for_test, reset_snapshot_scan_calls_for_test,
    snapshot_scan_calls_for_test,
};

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
    pub contracts_folder: String,
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
    /// Legacy v0.2 collection loaded through read-only compatibility.
    V02,
    /// Canonical v0.3 collection.
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
            contracts_folder: "_contracts".into(),
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

/// A read-only catalog of collection configuration, types, and contracts.
///
/// Unlike [`Collection`], this type exposes no record operations and never
/// owns durable transaction recovery, indexes, watchers, or change feeds.
pub struct CollectionResources {
    collection: Collection,
}

impl CollectionResources {
    pub fn open(root: &Path) -> Result<Self, serde_json::Value> {
        Collection::open_with_recovery(root, false).map(|collection| Self { collection })
    }

    pub fn spec_profile(&self) -> SpecProfile {
        self.collection.spec_profile()
    }

    pub fn types(&self) -> &HashMap<String, TypeDef> {
        self.collection.types()
    }

    pub fn list_data_contracts(&self) -> Vec<data_contracts::DataContractDefinition> {
        self.collection.list_data_contracts()
    }

    pub fn get_data_contract_implementations(
        &self,
        contract: &str,
        version: &str,
    ) -> Vec<data_contracts::DataContractImplementationDescriptor> {
        self.collection
            .get_data_contract_implementations(contract, version)
    }
}

/// A loaded mdbase collection.
pub struct Collection {
    /// Stable display path retained for public compatibility. Never use it as authority.
    pub(crate) root: PathBuf,
    pub(crate) authority: collection_root::CollectionRoot,
    pub(crate) spec_profile: SpecProfile,
    pub(crate) settings: Settings,
    /// Namespaced collection configuration retained for optional adapters.
    pub(crate) config_extensions: serde_json::Map<String, serde_json::Value>,
    pub(crate) types: HashMap<String, TypeDef>,
    pub(crate) type_plans: HashMap<String, crate::types::compiled::CompiledTypePlan>,
    pub(crate) type_warnings: Vec<String>,
    pub(crate) data_contracts: data_contracts::DataContractRegistry,
}

impl Collection {
    /// Authorized collection root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn root_capability(&self) -> std::io::Result<cap_std::fs::Dir> {
        self.authority.dir()
    }

    pub(crate) fn held_root(&self) -> &collection_root::CollectionRoot {
        &self.authority
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
        Self::open_with_recovery(root, true)
    }

    /// Load collection state for an observer that must never own recovery.
    pub(crate) fn open_for_observation(root: &Path) -> Result<Self, serde_json::Value> {
        Self::open_with_recovery(root, false)
    }

    fn open_with_recovery(
        root: &Path,
        recover_pending_transactions: bool,
    ) -> Result<Self, serde_json::Value> {
        let authority = collection_root::CollectionRoot::acquire(root).map_err(|error| {
            crate::errors::op_error(
                crate::errors::INVALID_CONFIG,
                &format!("collection root could not be opened: {error}"),
            )
        })?;
        Self::open_held(authority, recover_pending_transactions)
    }

    pub(crate) fn reopen_held(
        &self,
        recover_pending_transactions: bool,
    ) -> Result<Self, serde_json::Value> {
        Self::open_held(self.authority.clone(), recover_pending_transactions)
    }

    fn open_held(
        authority: collection_root::CollectionRoot,
        recover_pending_transactions: bool,
    ) -> Result<Self, serde_json::Value> {
        let root = authority.display_path();
        let config_result = config::load_config_for_open_held(&authority);
        if config_result.get("valid") != Some(&serde_json::Value::Bool(true)) {
            return Err(config_result);
        }

        let config = &config_result["config"];
        let settings_json = &config["settings"];
        let spec_profile = match config["spec_profile"].as_str() {
            Some("v0.3") => SpecProfile::V03,
            _ => SpecProfile::V02,
        };

        let mut settings = Settings {
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
            contracts_folder: settings_json["contracts_folder"]
                .as_str()
                .unwrap_or("_contracts")
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

        for (label, folder, allow_hidden) in [
            ("types_folder", &mut settings.types_folder, false),
            ("contracts_folder", &mut settings.contracts_folder, false),
            ("migrations_folder", &mut settings.migrations_folder, false),
            ("cache_folder", &mut settings.cache_folder, true),
        ] {
            let normalized = crate::api::CollectionPath::new(folder.as_str()).map_err(|error| {
                crate::errors::op_error(
                    crate::errors::INVALID_CONFIG,
                    &format!("settings.{label} is not a portable collection path: {error}"),
                )
            })?;
            if !allow_hidden
                && normalized
                    .as_str()
                    .split('/')
                    .any(|component| component.starts_with('.'))
            {
                return Err(crate::errors::op_error(
                    crate::errors::INVALID_CONFIG,
                    &format!("settings.{label} must not use a hidden filesystem namespace"),
                ));
            }
            *folder = normalized.to_string();
            match authority.open_dir(&normalized.to_path_buf()) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(crate::errors::op_error(
                        crate::errors::PATH_TRAVERSAL,
                        &format!("settings.{label} is not a no-follow directory: {error}"),
                    ));
                }
            }
        }

        // Recovery must precede type loading. A system migration may have
        // replaced or removed legacy type definitions before the config flip;
        // loading types first could otherwise make the recovery journal
        // unreachable. Re-open from scratch if recovery changed any files so
        // config and settings are parsed from the completed transaction.
        let recovered = if recover_pending_transactions {
            let recovery_collection = Collection {
                root: root.to_path_buf(),
                authority: authority.clone(),
                spec_profile,
                settings: settings.clone(),
                config_extensions: config_extensions.clone(),
                types: HashMap::new(),
                type_plans: HashMap::new(),
                type_warnings: Vec::new(),
                data_contracts: data_contracts::DataContractRegistry::empty(),
            };
            crate::transactions::recover_pending(&recovery_collection).map_err(|error| {
                serde_json::json!({
                    "valid": false,
                    "error": {
                        "code": error.code(),
                        "message": error.to_string(),
                    }
                })
            })?
        } else {
            false
        };
        if recovered {
            return Self::open_held(authority, recover_pending_transactions);
        }

        // Load types from the held capability, never from the display path.
        let load_result = loader::load_types_with_warnings_held(
            &authority,
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

        let data_contracts =
            data_contracts::DataContractRegistry::load_held(&authority, &settings, &types)
                .map_err(|error| {
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
            authority,
            spec_profile,
            settings,
            config_extensions,
            types,
            type_plans,
            type_warnings,
            data_contracts,
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
