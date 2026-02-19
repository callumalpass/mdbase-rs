//! Init operation (§12.11) — initialize a new collection.

use std::fs;
use std::path::Path;

/// Initialize a new collection at the given root.
pub fn init_collection(root: &Path, input: &serde_json::Value) -> serde_json::Value {
    // Determine types folder
    let mut types_folder = "_types".to_string();
    let mut config_yaml = "spec_version: \"0.2.1\"\n".to_string();

    if let Some(config_input) = input.get("config") {
        if let Some(obj) = config_input.as_object() {
            // Build config YAML from input
            let mut lines = Vec::new();
            if let Some(sv) = obj.get("spec_version").and_then(|v| v.as_str()) {
                lines.push(format!("spec_version: \"{}\"", sv));
            } else {
                lines.push("spec_version: \"0.2.1\"".to_string());
            }
            if let Some(settings) = obj.get("settings").and_then(|v| v.as_object()) {
                lines.push("settings:".to_string());
                if let Some(tf) = settings.get("types_folder").and_then(|v| v.as_str()) {
                    types_folder = tf.to_string();
                    lines.push(format!("  types_folder: \"{}\"", tf));
                }
            }
            config_yaml = lines.join("\n") + "\n";
        } else if let Some(s) = config_input.as_str() {
            config_yaml = s.to_string();
            // Parse to extract types_folder
            if let Ok(parsed) = serde_yaml::from_str::<serde_yaml::Value>(&config_yaml) {
                if let Some(settings) = parsed.get("settings") {
                    if let Some(tf) = settings.get("types_folder").and_then(|v| v.as_str()) {
                        types_folder = tf.to_string();
                    }
                }
            }
        }
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

    // Create meta type file (§5.8)
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
