//! Init operation (§12.11) — initialize a new collection.

use std::fs;
use std::path::Path;

/// Initialize a new collection at the given root.
pub fn init_collection(root: &Path, input: &serde_json::Value) -> serde_json::Value {
    let (config, config_yaml) = match prepare_config(input.get("config")) {
        Ok(config) => config,
        Err(error) => return error,
    };
    let spec_version = match config
        .get("spec_version")
        .and_then(serde_json::Value::as_str)
    {
        Some(version) => version,
        None => return init_error("invalid_config", "spec_version must be a string"),
    };
    let legacy_v02 = is_legacy_v02(spec_version);
    if !legacy_v02 && !crate::v03::is_supported_spec_version(spec_version) {
        return init_error(
            "unsupported_version",
            &format!(
                "Unsupported spec version: {spec_version} (supported: {}; legacy adapter: 0.2.x)",
                crate::v03::SPEC_VERSION
            ),
        );
    }
    if !legacy_v02 {
        if let Some(diagnostic) = crate::v03::validate_config(&config, "mdbase.yaml")
            .into_iter()
            .find(|diagnostic| diagnostic.severity == "error")
        {
            return init_error(&diagnostic.code, &diagnostic.message);
        }
    }

    let types_folder = config
        .pointer("/settings/types_folder")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("_types")
        .to_string();
    if !is_safe_relative_folder(&types_folder) {
        return init_error(
            "path_traversal",
            "settings.types_folder must be a non-empty relative path without traversal segments",
        );
    }

    // Check for existing collection
    let config_path = root.join("mdbase.yaml");
    if config_path.exists() {
        return serde_json::json!({
            "error": {
                "code": "path_conflict",
                "message": "Collection already exists at target path"
            }
        });
    }

    if let Err(e) = fs::create_dir_all(root) {
        return init_error(
            "invalid_path",
            &format!("Failed to create collection root: {e}"),
        );
    }

    // Create config file
    if let Err(e) = fs::write(&config_path, &config_yaml) {
        return serde_json::json!({
            "error": {
                "code": "invalid_path",
                "message": format!("Failed to write config: {}", e)
            }
        });
    }

    // Create types folder
    let types_dir = root.join(&types_folder);
    if let Err(e) = fs::create_dir_all(&types_dir) {
        return serde_json::json!({
            "error": {
                "code": "invalid_path",
                "message": format!("Failed to create types folder: {}", e)
            }
        });
    }

    if !legacy_v02 {
        return serde_json::json!({
            "config_path": "mdbase.yaml",
            "types_folder": types_folder,
        });
    }

    // Explicit v0.2 initialization retains the legacy generated meta type.
    let meta_type_content = format!(
        "---\nname: meta\nmatch:\n  path_glob: \"{}/**/*.md\"\nstrict: false\nfields:\n  name:\n    type: string\n    required: true\n  description:\n    type: string\n  version:\n    type: integer\n  extends:\n    type: string\n  strict:\n    type: enum\n    values: [\"true\", \"false\", \"warn\"]\n  display_name_key:\n    type: string\n  match:\n    type: object\n    fields:\n      path_glob:\n        type: string\n      fields_present:\n        type: list\n      where:\n        type: object\n  path_pattern:\n    type: string\n  filename_pattern:\n    type: string\n  fields:\n    type: any\n---\n",
        types_folder
    );
    let meta_type_path = types_dir.join("meta.md");
    if let Err(e) = fs::write(&meta_type_path, &meta_type_content) {
        return serde_json::json!({
            "error": {
                "code": "invalid_path",
                "message": format!("Failed to write meta type: {}", e)
            }
        });
    }

    let meta_type_rel = format!("{}/meta.md", types_folder);
    serde_json::json!({
        "config_path": "mdbase.yaml",
        "types_folder": types_folder,
        "meta_type_path": meta_type_rel,
    })
}

fn prepare_config(
    input: Option<&serde_json::Value>,
) -> Result<(serde_json::Value, String), serde_json::Value> {
    match input {
        None => {
            let config = serde_json::json!({ "spec_version": crate::v03::SPEC_VERSION });
            let yaml = serde_yaml::to_string(&config)
                .map_err(|error| init_error("invalid_config", &error.to_string()))?;
            Ok((config, yaml))
        }
        Some(serde_json::Value::Object(input)) => {
            let mut values = serde_json::Map::new();
            values.insert(
                "spec_version".to_string(),
                input
                    .get("spec_version")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!(crate::v03::SPEC_VERSION)),
            );
            for (key, value) in input {
                if key != "spec_version" {
                    values.insert(key.clone(), value.clone());
                }
            }
            let config = serde_json::Value::Object(values);
            let yaml = serde_yaml::to_string(&config)
                .map_err(|error| init_error("invalid_config", &error.to_string()))?;
            Ok((config, yaml))
        }
        Some(serde_json::Value::String(source)) => {
            let yaml: serde_yaml::Value = serde_yaml::from_str(source)
                .map_err(|_| init_error("invalid_config", "Failed to parse YAML"))?;
            if !yaml.is_mapping() {
                return Err(init_error(
                    "invalid_config",
                    "Config must be a YAML mapping",
                ));
            }
            Ok((
                crate::frontmatter::parser::yaml_to_json(&yaml),
                source.to_string(),
            ))
        }
        Some(_) => Err(init_error(
            "invalid_config",
            "config must be a mapping or YAML string",
        )),
    }
}

fn is_legacy_v02(version: &str) -> bool {
    version == "0.2"
        || version.strip_prefix("0.2.").is_some_and(|patch| {
            !patch.is_empty() && patch.chars().all(|value| value.is_ascii_digit())
        })
}

fn is_safe_relative_folder(folder: &str) -> bool {
    let normalized = folder.replace('\\', "/");
    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized.contains('\0')
        || normalized
            .split('/')
            .next()
            .is_some_and(|segment| segment.ends_with(':'))
    {
        return false;
    }
    normalized
        .split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

fn init_error(code: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "error": {
            "code": code,
            "message": message,
        }
    })
}
