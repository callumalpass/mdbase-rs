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
        Err(e) => return error_json("invalid_config", &format!("Failed to read config: {}", e)),
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

    let spec_version = match validate_version(&spec_version_raw, &mut warnings, allow_future_minor)
    {
        Ok(v) => v,
        Err(e) => return e,
    };
    let spec_profile = if crate::v03::is_supported_spec_version(&spec_version)
        || is_future_v03_compatible(&spec_version, allow_future_minor)
    {
        "v0.3"
    } else {
        "v0.2"
    };

    if spec_profile == "v0.3" {
        let config_value = crate::frontmatter::parser::yaml_to_json(&yaml);
        if let Some(diagnostic) = crate::v03::validate_config(&config_value, "mdbase.yaml")
            .into_iter()
            .find(|diagnostic| diagnostic.severity == "error")
        {
            return error_json(&diagnostic.code, &diagnostic.message);
        }
    }

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
    let known_top = ["spec_version", "name", "description", "settings", "runtime"];
    for (key, _) in map {
        if let serde_yaml::Value::String(k) = key {
            if !known_top.contains(&k.as_str()) && !k.starts_with("x-") {
                warnings.push(format!("Unknown top-level key: {}", k));
            }
        }
    }

    // Build result
    let mut config = serde_json::json!({
        "spec_version": spec_version,
        "spec_profile": spec_profile,
        "settings": settings,
    });
    if let Some(n) = name {
        config["name"] = serde_json::Value::String(n);
    }
    if let Some(d) = description {
        config["description"] = serde_json::Value::String(d);
    }
    if let Some(runtime) = map.get(ykey("runtime")) {
        config["runtime"] = crate::frontmatter::parser::yaml_to_json(runtime);
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
    if crate::v03::is_supported_spec_version(version) {
        if version != crate::v03::SPEC_VERSION {
            warnings.push(format!(
                "spec_version '{}' is a compatible v0.3 prerelease; new collections use '{}'",
                version,
                crate::v03::SPEC_VERSION
            ));
        }
        return Ok(version.to_string());
    }
    if version == "0.2" {
        warnings.push("spec_version '0.2' interpreted as major.minor alias".to_string());
        return Ok("0.2.1".to_string());
    }

    let expression = regex::Regex::new(r"^(\d+)\.(\d+)\.(\d+)(?:-([A-Za-z0-9.-]+))?$")
        .expect("valid spec version expression");
    let captures = expression
        .captures(version)
        .ok_or_else(|| error_json("invalid_config", "Invalid spec_version format"))?;
    let major = captures[1]
        .parse::<u32>()
        .map_err(|_| error_json("invalid_config", "Invalid spec_version format"))?;
    let minor = captures[2]
        .parse::<u32>()
        .map_err(|_| error_json("invalid_config", "Invalid spec_version format"))?;
    let prerelease = captures.get(4).map(|value| value.as_str());

    if major == 0 && minor == 2 && prerelease.is_none() {
        return Ok(version.to_string());
    }
    if allow_future_minor && major == 0 && minor > 3 && prerelease.is_none() {
        warnings.push(format!(
            "spec_version '{}' is newer than supported v0.3; attempting with the v0.3 profile",
            version
        ));
        return Ok(version.to_string());
    }

    Err(error_json(
        "unsupported_version",
        &format!(
            "Unsupported spec version: {} (supported: {}; legacy adapter: 0.2.x)",
            version,
            crate::v03::SPEC_VERSION
        ),
    ))
}

fn is_future_v03_compatible(version: &str, allow_future_minor: bool) -> bool {
    if !allow_future_minor {
        return false;
    }
    let expression =
        regex::Regex::new(r"^(\d+)\.(\d+)\.(\d+)$").expect("valid spec version expression");
    let Some(captures) = expression.captures(version) else {
        return false;
    };
    captures.get(1).is_some_and(|value| value.as_str() == "0")
        && captures
            .get(2)
            .and_then(|value| value.as_str().parse::<u32>().ok())
            .is_some_and(|minor| minor > 3)
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

    // v0.2 extensions or the complete v0.3 record_extensions set.
    let record_extensions = match get_setting(settings_map, "record_extensions") {
        Some(serde_yaml::Value::Sequence(seq)) => Some(
            seq.iter()
                .map(|item| {
                    item.as_str().map(String::from).ok_or_else(|| {
                        error_json("invalid_config", "record_extensions items must be strings")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Some(serde_yaml::Value::Null) | None => None,
        Some(_) => {
            return Err(error_json(
                "invalid_config",
                "settings.record_extensions must be a list",
            ))
        }
    };
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
    for ext in record_extensions.as_ref().unwrap_or(&raw_extensions) {
        let stripped = ext.strip_prefix('.').unwrap_or(ext);
        if stripped == "md" {
            if record_extensions.is_none() {
                warnings.push(format!(
                    "'{}' in extensions is redundant (md is always included)",
                    ext
                ));
            }
            continue;
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
    let validation_value = get_setting_or_top(settings_map, top_map, "default_validation")
        .or_else(|| get_setting(settings_map, "validation"));
    let default_validation = match validation_value {
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
    let id_field_explicit = matches!(
        get_setting(settings_map, "id_field"),
        Some(serde_yaml::Value::String(_))
    );
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
            "record_extensions",
            "exclude",
            "include_subfolders",
            "types_folder",
            "migrations_folder",
            "explicit_type_keys",
            "write_defaults",
            "default_validation",
            "validation",
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
        "record_extensions": std::iter::once("md".to_string()).chain(extensions.iter().cloned()).collect::<Vec<_>>(),
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
