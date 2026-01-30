//! Load type definitions from _types/ (§5).

use std::collections::HashMap;
use std::path::Path;

use crate::frontmatter::parser::{parse_document, is_parse_error, yaml_to_json};
use super::schema::*;

/// Result of loading types, including warnings.
pub struct LoadTypesResult {
    pub types: HashMap<String, TypeDef>,
    pub warnings: Vec<String>,
}

/// Reserved type names that cannot be used.
const RESERVED_TYPE_NAMES: &[&str] = &["file", "formula", "this"];

/// Validate a type name according to §5 rules.
pub fn validate_type_name(name: &str) -> Result<(), String> {
    // Must not be empty
    if name.is_empty() {
        return Err("Type name must not be empty".to_string());
    }

    // Must start with a letter
    if !name.chars().next().map_or(false, |c| c.is_ascii_alphabetic()) {
        return Err(format!("Type name '{}' must start with a letter", name));
    }

    // Must not start with underscore (reserved)
    if name.starts_with('_') {
        return Err(format!("Type name '{}' starting with underscore is reserved", name));
    }

    // Must not exceed 64 characters (strict: >= 64 is rejected)
    if name.len() >= 64 {
        return Err(format!("Type name '{}' exceeds maximum length", name));
    }

    // Must only contain alphanumeric, hyphens, underscores
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!("Type name '{}' contains invalid characters", name));
    }

    // Must not be a reserved keyword
    if RESERVED_TYPE_NAMES.contains(&name.to_lowercase().as_str()) {
        return Err(format!("Type name '{}' is a reserved keyword", name));
    }

    Ok(())
}

/// Load all type definitions from the types folder.
pub fn load_types(
    collection_root: &Path,
    types_folder: &str,
) -> Result<HashMap<String, TypeDef>, String> {
    let result = load_types_with_warnings(collection_root, types_folder)?;
    Ok(result.types)
}

/// Load all type definitions, returning warnings as well.
pub fn load_types_with_warnings(
    collection_root: &Path,
    types_folder: &str,
) -> Result<LoadTypesResult, String> {
    let types_dir = collection_root.join(types_folder);
    let mut types = HashMap::new();
    let mut warnings = Vec::new();

    if !types_dir.exists() {
        return Ok(LoadTypesResult { types, warnings });
    }

    // Recursively collect all type files from subdirectories (§2.3)
    let type_files = collect_type_files(&types_dir)?;

    for path in type_files {
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read type file {:?}: {}", path, e))?;

        let type_def = parse_type_file(&content, &path)?;

        // Validate type name
        validate_type_name(&type_def.name)?;

        // Check filename matches name (warning if mismatch)
        let file_stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if file_stem.to_lowercase() != type_def.name.to_lowercase() {
            warnings.push(format!(
                "Type name '{}' does not match filename '{}'",
                type_def.name, file_stem
            ));
        }

        // Canonicalize name to lowercase
        let canonical_name = type_def.name.to_lowercase();
        let mut type_def = type_def;
        type_def.name = canonical_name.clone();

        types.insert(canonical_name, type_def);
    }

    Ok(LoadTypesResult { types, warnings })
}

/// Recursively collect all type files (.md, .yaml, .yml) from a directory.
fn collect_type_files(dir: &Path) -> Result<Vec<std::path::PathBuf>, String> {
    let mut files = Vec::new();
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("Failed to read types directory: {}", e))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read dir entry: {}", e))?;
        let path = entry.path();

        if path.is_dir() {
            // Recurse into subdirectories
            files.extend(collect_type_files(&path)?);
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext == "md" || ext == "yaml" || ext == "yml" {
                files.push(path);
            }
        }
    }

    Ok(files)
}

/// Parse a single type definition file.
fn parse_type_file(content: &str, path: &Path) -> Result<TypeDef, String> {
    let doc = parse_document(content);

    let yaml = match &doc.frontmatter {
        Some(v) if is_parse_error(v) => {
            return Err(format!("Invalid YAML in type file {:?}", path));
        }
        Some(v) => v,
        None => return Err(format!("Type file {:?} has no frontmatter", path)),
    };

    let mapping = match yaml.as_mapping() {
        Some(m) => m,
        None => return Err(format!("Type file {:?} frontmatter must be a mapping", path)),
    };

    let ykey = |s: &str| serde_yaml::Value::String(s.to_string());

    let name = mapping
        .get(&ykey("name"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("Type file {:?} missing 'name'", path))?
        .to_string();

    let description = mapping
        .get(&ykey("description"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let extends_val = mapping.get(&ykey("extends"));
    let extends = match extends_val {
        Some(serde_yaml::Value::String(s)) => Some(s.to_lowercase()),
        Some(serde_yaml::Value::Null) | None => None,
        Some(_) => {
            return Err(format!(
                "Type '{}' has invalid 'extends' value (must be a string)",
                name
            ));
        }
    };

    let strict = mapping
        .get(&ykey("strict"))
        .map(|v| match v {
            serde_yaml::Value::Bool(true) => StrictMode::Error,
            serde_yaml::Value::Bool(false) => StrictMode::Off,
            serde_yaml::Value::String(s) if s == "warn" => StrictMode::Warn,
            _ => StrictMode::Off,
        });

    let filename_pattern = mapping
        .get(&ykey("filename_pattern"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let fields = match mapping.get(&ykey("fields")) {
        Some(serde_yaml::Value::Mapping(fields_map)) => parse_fields(fields_map)?,
        _ => HashMap::new(),
    };

    let match_rules = mapping.get(&ykey("match")).map(|v| parse_match_rules(v));

    Ok(TypeDef {
        name,
        description,
        extends,
        strict,
        filename_pattern,
        fields,
        match_rules,
    })
}

/// Parse a fields mapping from YAML.
fn parse_fields(
    fields_map: &serde_yaml::Mapping,
) -> Result<HashMap<String, FieldDef>, String> {
    let mut fields = HashMap::new();

    for (key, value) in fields_map {
        let field_name = match key {
            serde_yaml::Value::String(s) => s.clone(),
            _ => continue,
        };

        let field_def = parse_field_def(value)?;
        fields.insert(field_name, field_def);
    }

    Ok(fields)
}

/// Parse a single field definition from YAML.
fn parse_field_def(value: &serde_yaml::Value) -> Result<FieldDef, String> {
    let mapping = match value.as_mapping() {
        Some(m) => m,
        None => {
            // Short form: just a type string
            if let Some(s) = value.as_str() {
                return Ok(FieldDef {
                    field_type: s.to_string(),
                    ..Default::default()
                });
            }
            return Ok(FieldDef::default());
        }
    };

    let ykey = |s: &str| serde_yaml::Value::String(s.to_string());

    let field_type = mapping
        .get(&ykey("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("any")
        .to_string();

    let required = mapping
        .get(&ykey("required"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let default = mapping
        .get(&ykey("default"))
        .map(|v| yaml_to_json(v));

    let generated = mapping
        .get(&ykey("generated"))
        .map(|v| parse_generated(v));

    let description = mapping
        .get(&ykey("description"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let deprecated = mapping
        .get(&ykey("deprecated"))
        .map(|v| match v {
            serde_yaml::Value::String(s) => s.clone(),
            serde_yaml::Value::Bool(true) => "deprecated".to_string(),
            _ => "deprecated".to_string(),
        });

    let unique = mapping
        .get(&ykey("unique"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let min = mapping.get(&ykey("min")).and_then(|v| v.as_f64());
    let max = mapping.get(&ykey("max")).and_then(|v| v.as_f64());
    let min_length = mapping.get(&ykey("min_length")).and_then(|v| v.as_u64()).map(|v| v as usize);
    let max_length = mapping.get(&ykey("max_length")).and_then(|v| v.as_u64()).map(|v| v as usize);

    let pattern = mapping
        .get(&ykey("pattern"))
        .and_then(|v| v.as_str())
        .map(String::from);

    // Validate regex pattern if present
    if let Some(ref pat) = pattern {
        if fancy_regex::Regex::new(pat).is_err() {
            return Err(format!("Invalid regex pattern: '{}'", pat));
        }
    }

    let values = match mapping.get(&ykey("values")) {
        Some(v) => {
            if let Some(seq) = v.as_sequence() {
                let mut vals = Vec::new();
                for item in seq {
                    match item.as_str() {
                        Some(s) => vals.push(s.to_string()),
                        None => return Err(format!("Enum values must be strings, got {:?}", item)),
                    }
                }
                Some(vals)
            } else {
                None
            }
        }
        None => None,
    };

    let items = mapping
        .get(&ykey("items"))
        .map(|v| parse_field_def(v))
        .transpose()?
        .map(Box::new);

    let nested_fields = match mapping.get(&ykey("fields")) {
        Some(serde_yaml::Value::Mapping(m)) => Some(parse_fields(m)?),
        _ => None,
    };

    let min_items = mapping.get(&ykey("min_items")).and_then(|v| v.as_u64()).map(|v| v as usize);
    let max_items = mapping.get(&ykey("max_items")).and_then(|v| v.as_u64()).map(|v| v as usize);

    let list_unique = mapping
        .get(&ykey("unique"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let target = mapping
        .get(&ykey("target"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let validate_exists = mapping
        .get(&ykey("validate_exists"))
        .and_then(|v| v.as_bool());

    let computed = mapping
        .get(&ykey("computed"))
        .and_then(|v| v.as_str())
        .map(String::from);

    Ok(FieldDef {
        field_type,
        required,
        default,
        generated,
        description,
        deprecated,
        unique,
        min,
        max,
        min_length,
        max_length,
        pattern,
        values,
        items,
        fields: nested_fields,
        min_items,
        max_items,
        list_unique,
        target,
        validate_exists,
        computed,
    })
}

/// Parse a generated field strategy.
fn parse_generated(value: &serde_yaml::Value) -> GeneratedStrategy {
    match value {
        serde_yaml::Value::String(s) => match s.as_str() {
            "ulid" => GeneratedStrategy::Ulid,
            "uuid" => GeneratedStrategy::Uuid,
            "now" => GeneratedStrategy::Now,
            "now_on_write" => GeneratedStrategy::NowOnWrite,
            _ => GeneratedStrategy::Now, // fallback
        },
        serde_yaml::Value::Mapping(m) => {
            let ykey = |s: &str| serde_yaml::Value::String(s.to_string());
            // Check for strategy key first (e.g., {strategy: uuid})
            if let Some(strategy) = m.get(&ykey("strategy")).and_then(|v| v.as_str()) {
                match strategy {
                    "ulid" => GeneratedStrategy::Ulid,
                    "uuid" => GeneratedStrategy::Uuid,
                    "now" => GeneratedStrategy::Now,
                    "now_on_write" => GeneratedStrategy::NowOnWrite,
                    "timestamp" => GeneratedStrategy::Now, // timestamp maps to Now
                    _ => GeneratedStrategy::Now,
                }
            } else {
                let from = m
                    .get(&ykey("from"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let transform = m
                    .get(&ykey("transform"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                GeneratedStrategy::Derived { from, transform }
            }
        }
        _ => GeneratedStrategy::Now,
    }
}

/// Parse match rules from YAML.
fn parse_match_rules(value: &serde_yaml::Value) -> MatchRules {
    let mapping = match value.as_mapping() {
        Some(m) => m,
        None => return MatchRules { path_glob: None, fields_present: None, where_clause: None },
    };

    let ykey = |s: &str| serde_yaml::Value::String(s.to_string());

    let path_glob = mapping
        .get(&ykey("path_glob"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let fields_present = mapping.get(&ykey("fields_present")).and_then(|v| {
        v.as_sequence().map(|seq| {
            seq.iter()
                .filter_map(|item| item.as_str().map(String::from))
                .collect()
        })
    });

    let where_clause = mapping.get(&ykey("where")).map(|v| yaml_to_json(v));

    MatchRules {
        path_glob,
        fields_present,
        where_clause,
    }
}
