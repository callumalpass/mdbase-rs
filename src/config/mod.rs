//! Configuration parsing (§4).

use std::path::Path;

/// Load and validate the mdbase configuration from a collection root.
///
/// Returns a JSON value:
/// - `{ "valid": true, "config": {...}, "warnings": [...] }` on success
/// - `{ "valid": false, "error": { "code": "...", "message": "..." } }` on failure
pub fn load_config(collection_root: &Path) -> serde_json::Value {
    load_config_internal(collection_root, false)
}

/// Load config for collection opening, allowing forward-minor versions.
pub(crate) fn load_config_for_open(collection_root: &Path) -> serde_json::Value {
    load_config_internal(collection_root, true)
}

fn load_config_internal(collection_root: &Path, allow_future_minor: bool) -> serde_json::Value {
    let config_path = collection_root.join("mdbase.yaml");

    if !config_path.exists() {
        return error_json("missing_config", "mdbase.yaml not found");
    }

    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            return error_json("invalid_config", &format!("Failed to read config: {}", e))
        }
    };

    let yaml: serde_yaml::Value = match serde_yaml::from_str(&content) {
        Ok(v) => v,
        Err(_) => return error_json("invalid_config", "Failed to parse YAML"),
    };

    let map = match yaml.as_mapping() {
        Some(m) => m,
        None => return error_json("invalid_config", "Config must be a YAML mapping"),
    };

    let mut warnings = Vec::new();

    // spec_version (required)
    let spec_version_raw = match map.get(ykey("spec_version")) {
        Some(serde_yaml::Value::String(v)) => v.clone(),
        Some(_) => return error_json("invalid_config", "spec_version must be a string"),
        None => return error_json("invalid_config", "spec_version is required"),
    };

    let spec_version = match validate_version(&spec_version_raw, &mut warnings, allow_future_minor) {
        Ok(v) => v,
        Err(e) => return e,
    };

    // Optional top-level fields
    let name = map
        .get(ykey("name"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let description = map
        .get(ykey("description"))
        .and_then(|v| v.as_str())
        .map(String::from);

    // Settings
    let settings = match parse_settings(map, &mut warnings) {
        Ok(s) => s,
        Err(e) => return e,
    };

    // Unknown top-level keys
    let known_top = ["spec_version", "name", "description", "settings"];
    for (key, _) in map {
        if let serde_yaml::Value::String(k) = key {
            if !known_top.contains(&k.as_str()) {
                warnings.push(format!("Unknown top-level key: {}", k));
            }
        }
    }

    // Build result
    let mut config = serde_json::json!({
        "spec_version": spec_version,
        "settings": settings,
    });
    if let Some(n) = name {
        config["name"] = serde_json::Value::String(n);
    }
    if let Some(d) = description {
        config["description"] = serde_json::Value::String(d);
    }

    let mut result = serde_json::json!({
        "valid": true,
        "config": config,
    });
    if !warnings.is_empty() {
        result["warnings"] = serde_json::Value::Array(
            warnings
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        );
    }

    result
}

fn error_json(code: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "valid": false,
        "error": { "code": code, "message": message }
    })
}

fn ykey(s: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(s.to_string())
}

fn get_setting<'a>(
    map: Option<&'a serde_yaml::Mapping>,
    key: &str,
) -> Option<&'a serde_yaml::Value> {
    map.and_then(|m| m.get(ykey(key)))
}

/// Look for a setting first under settings, then at the top level.
fn get_setting_or_top<'a>(
    settings_map: Option<&'a serde_yaml::Mapping>,
    top_map: &'a serde_yaml::Mapping,
    key: &str,
) -> Option<&'a serde_yaml::Value> {
    get_setting(settings_map, key).or_else(|| top_map.get(ykey(key)))
}

fn validate_version(
    version: &str,
    warnings: &mut Vec<String>,
    allow_future_minor: bool,
) -> Result<String, serde_json::Value> {
    let parts: Vec<&str> = version.split('.').collect();

    let (major, minor, patch) = match parts.len() {
        2 => {
            let major = parts[0]
                .parse::<u32>()
                .map_err(|_| error_json("invalid_config", "Invalid spec_version format"))?;
            let minor = parts[1]
                .parse::<u32>()
                .map_err(|_| error_json("invalid_config", "Invalid spec_version format"))?;
            warnings.push(format!(
                "spec_version '{}' interpreted as '{}.0'",
                version, version
            ));
            (major, minor, 0u32)
        }
        3 => {
            let major = parts[0]
                .parse::<u32>()
                .map_err(|_| error_json("invalid_config", "Invalid spec_version format"))?;
            let minor = parts[1]
                .parse::<u32>()
                .map_err(|_| error_json("invalid_config", "Invalid spec_version format"))?;
            let patch = parts[2]
                .parse::<u32>()
                .map_err(|_| error_json("invalid_config", "Invalid spec_version format"))?;
            (major, minor, patch)
        }
        _ => return Err(error_json("invalid_config", "Invalid spec_version format")),
    };

    let supported_major = 0u32;
    let supported_minor = 2u32;

    if major != supported_major {
        return Err(error_json(
            "unsupported_version",
            &format!(
                "Unsupported spec version: {}. This implementation supports 0.2.x",
                version
            ),
        ));
    }
    if minor != supported_minor {
        if allow_future_minor && minor > supported_minor {
            // Allow forward minor versions when opening a collection
        } else {
            return Err(error_json(
                "unsupported_version",
                &format!(
                    "Unsupported spec version: {}. This implementation supports 0.2.x",
                    version
                ),
            ));
        }
    }

    Ok(format!("{}.{}.{}", major, minor, patch))
}

fn parse_settings(
    top_map: &serde_yaml::Mapping,
    warnings: &mut Vec<String>,
) -> Result<serde_json::Value, serde_json::Value> {
    let settings_map = match top_map.get(ykey("settings")) {
        Some(serde_yaml::Value::Mapping(m)) => Some(m),
        Some(serde_yaml::Value::Null) | None => None,
        Some(_) => return Err(error_json("invalid_config", "settings must be a mapping")),
    };

    // extensions
    let raw_extensions = match get_setting(settings_map, "extensions") {
        Some(serde_yaml::Value::Sequence(seq)) => {
            let mut exts = Vec::new();
            for item in seq {
                match item {
                    serde_yaml::Value::String(s) => exts.push(s.clone()),
                    _ => {
                        return Err(error_json(
                            "invalid_config",
                            "extensions items must be strings",
                        ))
                    }
                }
            }
            exts
        }
        Some(serde_yaml::Value::Null) | None => Vec::new(),
        Some(_) => {
            return Err(error_json(
                "invalid_config",
                "settings.extensions must be a list",
            ))
        }
    };

    // Normalize extensions: strip leading dots, warn about "md"
    let mut extensions = Vec::new();
    for ext in &raw_extensions {
        let stripped = ext.strip_prefix('.').unwrap_or(ext);
        if stripped == "md" {
            warnings.push(format!(
                "'{}' in extensions is redundant (md is always included)",
                ext
            ));
        }
        extensions.push(stripped.to_string());
    }

    // exclude
    let exclude = match get_setting(settings_map, "exclude") {
        Some(serde_yaml::Value::Sequence(seq)) => seq
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect::<Vec<_>>(),
        Some(serde_yaml::Value::Null) | None => {
            vec![
                ".git".to_string(),
                "node_modules".to_string(),
                ".mdbase".to_string(),
            ]
        }
        Some(_) => {
            return Err(error_json(
                "invalid_config",
                "settings.exclude must be a list",
            ))
        }
    };

    // include_subfolders
    let include_subfolders = match get_setting(settings_map, "include_subfolders") {
        Some(serde_yaml::Value::Bool(b)) => *b,
        Some(serde_yaml::Value::Null) | None => true,
        Some(_) => {
            return Err(error_json(
                "invalid_config",
                "settings.include_subfolders must be a boolean",
            ))
        }
    };

    // types_folder
    let types_folder = match get_setting(settings_map, "types_folder") {
        Some(serde_yaml::Value::String(s)) => s.clone(),
        Some(serde_yaml::Value::Null) | None => "_types".to_string(),
        Some(_) => {
            return Err(error_json(
                "invalid_config",
                "settings.types_folder must be a string",
            ))
        }
    };

    // migrations_folder
    let migrations_folder = match get_setting(settings_map, "migrations_folder") {
        Some(serde_yaml::Value::String(s)) => s.clone(),
        Some(serde_yaml::Value::Null) | None => format!("{}/_migrations", types_folder),
        Some(_) => {
            return Err(error_json(
                "invalid_config",
                "settings.migrations_folder must be a string",
            ))
        }
    };

    // explicit_type_keys
    let explicit_type_keys = match get_setting(settings_map, "explicit_type_keys") {
        Some(serde_yaml::Value::Sequence(seq)) => seq
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect::<Vec<_>>(),
        Some(serde_yaml::Value::Null) | None => {
            vec!["type".to_string(), "types".to_string()]
        }
        Some(_) => {
            return Err(error_json(
                "invalid_config",
                "settings.explicit_type_keys must be a list",
            ))
        }
    };

    // write_defaults
    let write_defaults = match get_setting(settings_map, "write_defaults") {
        Some(serde_yaml::Value::Bool(b)) => *b,
        Some(serde_yaml::Value::Null) | None => true,
        Some(_) => {
            return Err(error_json(
                "invalid_config",
                "settings.write_defaults must be a boolean",
            ))
        }
    };

    // default_validation (check both settings and top level)
    let default_validation = match get_setting_or_top(settings_map, top_map, "default_validation") {
        Some(serde_yaml::Value::String(s)) => match s.as_str() {
            "off" | "warn" | "error" => s.clone(),
            _ => {
                return Err(error_json(
                    "invalid_config",
                    &format!(
                        "settings.default_validation must be 'off', 'warn', or 'error', got '{}'",
                        s
                    ),
                ))
            }
        },
        Some(serde_yaml::Value::Null) | None => "warn".to_string(),
        Some(_) => {
            return Err(error_json(
                "invalid_config",
                "settings.default_validation must be a string",
            ))
        }
    };

    // default_strict (bool or "warn")
    let default_strict: serde_json::Value = match get_setting(settings_map, "default_strict") {
        Some(serde_yaml::Value::Bool(b)) => serde_json::Value::Bool(*b),
        Some(serde_yaml::Value::String(s)) if s == "warn" => {
            serde_json::Value::String("warn".to_string())
        }
        Some(serde_yaml::Value::Null) | None => serde_json::Value::Bool(false),
        Some(_) => {
            return Err(error_json(
                "invalid_config",
                "settings.default_strict must be a boolean or 'warn'",
            ))
        }
    };

    // timezone
    let timezone = match get_setting(settings_map, "timezone") {
        Some(serde_yaml::Value::String(s)) => Some(s.clone()),
        Some(serde_yaml::Value::Null) | None => None,
        Some(_) => {
            return Err(error_json(
                "invalid_config",
                "settings.timezone must be a string",
            ))
        }
    };

    // id_field
    let id_field_explicit = matches!(get_setting(settings_map, "id_field"), Some(serde_yaml::Value::String(_)));
    let id_field = match get_setting(settings_map, "id_field") {
        Some(serde_yaml::Value::String(s)) => s.clone(),
        Some(serde_yaml::Value::Null) | None => "id".to_string(),
        Some(_) => {
            return Err(error_json(
                "invalid_config",
                "settings.id_field must be a string",
            ))
        }
    };

    // write_nulls
    let write_nulls = match get_setting(settings_map, "write_nulls") {
        Some(serde_yaml::Value::String(s)) => match s.as_str() {
            "omit" | "explicit" => s.clone(),
            _ => {
                return Err(error_json(
                    "invalid_config",
                    &format!(
                        "settings.write_nulls must be 'omit' or 'explicit', got '{}'",
                        s
                    ),
                ))
            }
        },
        Some(serde_yaml::Value::Null) | None => "omit".to_string(),
        Some(_) => {
            return Err(error_json(
                "invalid_config",
                "settings.write_nulls must be a string",
            ))
        }
    };

    // write_empty_lists
    let write_empty_lists = match get_setting(settings_map, "write_empty_lists") {
        Some(serde_yaml::Value::Bool(b)) => *b,
        Some(serde_yaml::Value::Null) | None => true,
        Some(_) => {
            return Err(error_json(
                "invalid_config",
                "settings.write_empty_lists must be a boolean",
            ))
        }
    };

    // rename_update_refs
    let rename_update_refs = match get_setting(settings_map, "rename_update_refs") {
        Some(serde_yaml::Value::Bool(b)) => *b,
        Some(serde_yaml::Value::Null) | None => true,
        Some(_) => {
            return Err(error_json(
                "invalid_config",
                "settings.rename_update_refs must be a boolean",
            ))
        }
    };

    // cache_folder
    let cache_folder = match get_setting(settings_map, "cache_folder") {
        Some(serde_yaml::Value::String(s)) => s.clone(),
        Some(serde_yaml::Value::Null) | None => ".mdbase".to_string(),
        Some(_) => {
            return Err(error_json(
                "invalid_config",
                "settings.cache_folder must be a string",
            ))
        }
    };

    // Unknown settings keys
    if let Some(smap) = settings_map {
        let known = [
            "extensions",
            "exclude",
            "include_subfolders",
            "types_folder",
            "migrations_folder",
            "explicit_type_keys",
            "write_defaults",
            "default_validation",
            "default_strict",
            "timezone",
            "id_field",
            "write_nulls",
            "write_empty_lists",
            "rename_update_refs",
            "cache_folder",
        ];
        for (key, _) in smap {
            if let serde_yaml::Value::String(k) = key {
                if !known.contains(&k.as_str()) {
                    warnings.push(format!("Unknown settings key: {}", k));
                }
            }
        }
    }

    Ok(serde_json::json!({
        "extensions": extensions,
        "exclude": exclude,
        "include_subfolders": include_subfolders,
        "types_folder": types_folder,
        "explicit_type_keys": explicit_type_keys,
        "migrations_folder": migrations_folder,
        "write_defaults": write_defaults,
        "default_validation": default_validation,
        "default_strict": default_strict,
        "timezone": timezone,
        "id_field": id_field,
        "id_field_explicit": id_field_explicit,
        "write_nulls": write_nulls,
        "write_empty_lists": write_empty_lists,
        "rename_update_refs": rename_update_refs,
        "cache_folder": cache_folder,
    }))
}
