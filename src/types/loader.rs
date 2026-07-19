//! Load type definitions from _types/ (§5).

use std::collections::HashMap;
use std::path::Path;

use super::schema::*;
use crate::frontmatter::parser::{is_parse_error, parse_document, yaml_to_json};

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
    if !name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return Err(format!("Type name '{}' must start with a letter", name));
    }

    // Must not start with underscore (reserved)
    if name.starts_with('_') {
        return Err(format!(
            "Type name '{}' starting with underscore is reserved",
            name
        ));
    }

    // Must not exceed 64 characters (strict: >= 64 is rejected)
    if name.len() >= 64 {
        return Err(format!("Type name '{}' exceeds maximum length", name));
    }

    // Must only contain alphanumeric, hyphens, underscores
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
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
    migrations_folder: &str,
) -> Result<HashMap<String, TypeDef>, String> {
    let result = load_types_with_warnings(collection_root, types_folder, migrations_folder)?;
    Ok(result.types)
}

/// Load all type definitions, returning warnings as well.
pub fn load_types_with_warnings(
    collection_root: &Path,
    types_folder: &str,
    migrations_folder: &str,
) -> Result<LoadTypesResult, String> {
    let types_dir = collection_root.join(types_folder);
    let migrations_dir = collection_root.join(migrations_folder);
    let mut types = HashMap::new();
    let mut warnings = Vec::new();

    if !types_dir.exists() {
        return Ok(LoadTypesResult { types, warnings });
    }

    // Recursively collect all type files from subdirectories (§2.3)
    let type_files = collect_type_files(&types_dir, Some(&migrations_dir))?;

    let placeholder_re = regex::Regex::new(r"\{(\w+)\}").expect("valid path placeholder regex");

    for path in type_files {
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read type file {:?}: {}", path, e))?;

        let type_def = parse_type_file(&content, &path, collection_root)?;

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

        // Validate field definitions
        for (field_name, field_def) in &type_def.fields {
            // Empty enum values list
            if field_def.field_type == "enum" {
                if let Some(ref values) = field_def.values {
                    if values.is_empty() {
                        return Err(format!(
                            "Type '{}' field '{}': enum must have at least one value",
                            type_def.name, field_name
                        ));
                    }
                }
            }
            // Random generation with invalid length
            if let Some(GeneratedStrategy::Derived { from, .. }) = field_def.generated.as_ref() {
                // Check if "from" is "file.name" etc (circular with path_pattern)
                if from.starts_with("file.")
                    && (type_def.path_pattern.is_some() || type_def.filename_pattern.is_some())
                {
                    let pattern = type_def
                        .path_pattern
                        .as_deref()
                        .or(type_def.filename_pattern.as_deref())
                        .unwrap_or("");
                    if pattern.contains(&format!("{{{}}}", field_name)) {
                        return Err(format!(
                                "Type '{}': circular dependency between path_pattern and generated field '{}'",
                                type_def.name, field_name
                            ));
                    }
                }
            }
            // Validate generated strategies
            if let Some(ref gen) = field_def.generated {
                match gen {
                    GeneratedStrategy::Random(len) => {
                        if *len == 0 {
                            return Err(format!(
                                "Type '{}' field '{}': random generation length must be > 0",
                                type_def.name, field_name
                            ));
                        }
                    }
                    GeneratedStrategy::Sequence(_) if field_def.field_type != "integer" => {
                        return Err(format!(
                            "Type '{}' field '{}': sequence generation requires integer type",
                            type_def.name, field_name
                        ));
                    }
                    _ => {}
                }
            }
        }

        // Validate path_pattern field references
        let pattern = type_def
            .path_pattern
            .as_deref()
            .or(type_def.filename_pattern.as_deref());
        if let Some(pattern) = pattern {
            for cap in placeholder_re.captures_iter(pattern) {
                let field = &cap[1];
                if !type_def.fields.contains_key(field) {
                    warnings.push(format!(
                        "Type '{}': path_pattern references unknown field '{}'",
                        type_def.name, field
                    ));
                } else {
                    // Check if referenced field is a computed field
                    if let Some(fd) = type_def.fields.get(field) {
                        if fd.computed.is_some() {
                            return Err(format!(
                                "Type '{}': path_pattern cannot reference computed field '{}'",
                                type_def.name, field
                            ));
                        }
                    }
                }
            }
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
fn collect_type_files(
    dir: &Path,
    migrations_dir: Option<&Path>,
) -> Result<Vec<std::path::PathBuf>, String> {
    let mut files = Vec::new();
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("Failed to read types directory: {}", e))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read dir entry: {}", e))?;
        let path = entry.path();

        if path.is_dir() {
            if let Some(skip) = migrations_dir {
                if path == skip || path.starts_with(skip) {
                    continue;
                }
            }
            // Recurse into subdirectories
            files.extend(collect_type_files(&path, migrations_dir)?);
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
fn parse_type_file(content: &str, path: &Path, collection_root: &Path) -> Result<TypeDef, String> {
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
        None => {
            return Err(format!(
                "Type file {:?} frontmatter must be a mapping",
                path
            ))
        }
    };

    let ykey = |s: &str| serde_yaml::Value::String(s.to_string());

    if mapping.get(ykey("kind")).and_then(|value| value.as_str()) == Some("mdbase.type") {
        let relative_path = path
            .strip_prefix(collection_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let type_file = crate::v03::parse_type_file(content, path, collection_root, &relative_path)
            .map_err(|diagnostics| {
                diagnostics
                    .into_iter()
                    .map(|diagnostic| diagnostic.message)
                    .collect::<Vec<_>>()
                    .join("; ")
            })?;
        return v03_type_definition(type_file);
    }

    let name = mapping
        .get(ykey("name"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("Type file {:?} missing 'name'", path))?
        .to_string();

    let description = mapping
        .get(ykey("description"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let extends_val = mapping.get(ykey("extends"));
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

    let strict = mapping.get(ykey("strict")).map(|v| match v {
        serde_yaml::Value::Bool(true) => StrictMode::Error,
        serde_yaml::Value::Bool(false) => StrictMode::Off,
        serde_yaml::Value::String(s) if s == "warn" => StrictMode::Warn,
        _ => StrictMode::Off,
    });

    let filename_pattern = mapping
        .get(ykey("filename_pattern"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let path_pattern = mapping
        .get(ykey("path_pattern"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let display_name_key = mapping
        .get(ykey("display_name_key"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let fields = match mapping.get(ykey("fields")) {
        Some(serde_yaml::Value::Mapping(fields_map)) => parse_fields(fields_map)?,
        _ => HashMap::new(),
    };

    let match_rules = mapping.get(ykey("match")).map(parse_match_rules);

    Ok(TypeDef {
        name,
        kind: None,
        version: None,
        description,
        extends,
        strict,
        filename_pattern,
        path_pattern,
        display_name_key,
        fields,
        match_rules,
        json_schema: None,
        read_defaults: HashMap::new(),
        source_path: path
            .strip_prefix(collection_root)
            .ok()
            .map(|value| value.to_string_lossy().replace('\\', "/")),
    })
}

fn v03_type_definition(type_file: crate::v03::TypeFile) -> Result<TypeDef, String> {
    let frontmatter = type_file
        .frontmatter
        .as_object()
        .ok_or_else(|| "v0.3 type file frontmatter must be an object".to_string())?;
    let schema_object = type_file.schema.as_object().ok_or_else(|| {
        format!(
            "Type '{}' embedded schema must be an object",
            type_file.name
        )
    })?;
    let required: std::collections::HashSet<String> = schema_object
        .get("required")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(String::from)
        .collect();
    let mut fields = HashMap::new();
    if let Some(properties) = schema_object
        .get("properties")
        .and_then(serde_json::Value::as_object)
    {
        for (name, schema) in properties {
            let mut field = field_from_json_schema(schema)?;
            field.required = required.contains(name);
            fields.insert(name.clone(), field);
        }
    }

    let mut match_rules = frontmatter.get("match").map(parse_v03_match_rules);
    if match_rules.as_ref().is_some_and(|rules| {
        rules.path_glob.is_none()
            && rules.path_globs.is_none()
            && rules.fields_present.is_none()
            && rules.where_clause.is_none()
    }) {
        match_rules = None;
    }
    let collection = frontmatter
        .get("collection")
        .and_then(serde_json::Value::as_object);
    let read_defaults = collection
        .and_then(|value| value.get("read_defaults"))
        .and_then(serde_json::Value::as_object)
        .map(|value| {
            value
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    let display_name_key = collection
        .and_then(|value| value.get("display"))
        .and_then(serde_json::Value::as_object)
        .and_then(|value| value.get("name_field"))
        .and_then(serde_json::Value::as_str)
        .map(String::from);
    let path_pattern = collection
        .and_then(|value| value.get("path"))
        .and_then(serde_json::Value::as_object)
        .and_then(|value| value.get("pattern"))
        .and_then(serde_json::Value::as_str)
        .map(String::from);

    if let Some(unique_rules) = collection
        .and_then(|value| value.get("unique"))
        .and_then(serde_json::Value::as_array)
    {
        for rule in unique_rules {
            if let Some(field_name) = rule.get("field").and_then(serde_json::Value::as_str) {
                if let Some(field) = fields.get_mut(field_name) {
                    field.unique = true;
                }
            }
        }
    }
    if let Some(link_rules) = collection
        .and_then(|value| value.get("links"))
        .and_then(serde_json::Value::as_object)
    {
        for (field_name, rule) in link_rules {
            let Some(field) = fields.get_mut(field_name) else {
                continue;
            };
            field.validate_exists = rule
                .get("validate_exists")
                .and_then(serde_json::Value::as_bool);
            field.target = rule
                .get("target_type")
                .and_then(serde_json::Value::as_str)
                .filter(|target| *target != "any")
                .map(String::from);
        }
    }

    Ok(TypeDef {
        name: type_file.name,
        kind: Some("mdbase.type".to_string()),
        version: type_file.version,
        description: frontmatter
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
        extends: None,
        strict: match schema_object.get("additionalProperties") {
            Some(serde_json::Value::Bool(false)) => Some(StrictMode::Error),
            _ => Some(StrictMode::Off),
        },
        filename_pattern: None,
        path_pattern,
        display_name_key,
        fields,
        match_rules,
        json_schema: Some(type_file.schema),
        read_defaults,
        source_path: Some(type_file.path),
    })
}

fn field_from_json_schema(schema: &serde_json::Value) -> Result<FieldDef, String> {
    let Some(object) = schema.as_object() else {
        return Ok(FieldDef::default());
    };
    let field_type = match object.get("type") {
        Some(serde_json::Value::String(value)) => match value.as_str() {
            "array" => "list".to_string(),
            value => value.to_string(),
        },
        _ if object.contains_key("enum") || object.contains_key("const") => "enum".to_string(),
        _ => "any".to_string(),
    };
    let mut field = FieldDef {
        field_type,
        description: object
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
        deprecated: object
            .get("deprecated")
            .and_then(serde_json::Value::as_bool)
            .filter(|value| *value)
            .map(|_| "deprecated".to_string()),
        min: object
            .get("minimum")
            .or_else(|| object.get("exclusiveMinimum"))
            .and_then(serde_json::Value::as_f64),
        max: object
            .get("maximum")
            .or_else(|| object.get("exclusiveMaximum"))
            .and_then(serde_json::Value::as_f64),
        min_length: object
            .get("minLength")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize),
        max_length: object
            .get("maxLength")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize),
        pattern: object
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
        min_items: object
            .get("minItems")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize),
        max_items: object
            .get("maxItems")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize),
        list_unique: object
            .get("uniqueItems")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        values: object
            .get("enum")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(String::from)
                    .collect()
            }),
        ..FieldDef::default()
    };
    if field.values.is_none() {
        field.values = object
            .get("const")
            .and_then(serde_json::Value::as_str)
            .map(|value| vec![value.to_string()]);
    }
    field.items = object
        .get("items")
        .map(field_from_json_schema)
        .transpose()?
        .map(Box::new);
    field.fields = object
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .map(|properties| {
            properties
                .iter()
                .map(|(name, schema)| Ok((name.clone(), field_from_json_schema(schema)?)))
                .collect::<Result<HashMap<_, _>, String>>()
        })
        .transpose()?;
    Ok(field)
}

fn parse_v03_match_rules(value: &serde_json::Value) -> MatchRules {
    let path_glob_value = value.get("path_glob");
    MatchRules {
        path_glob: path_glob_value
            .and_then(serde_json::Value::as_str)
            .map(String::from),
        path_globs: path_glob_value
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(String::from)
                    .collect()
            }),
        fields_present: value
            .get("fields_present")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(String::from)
                    .collect()
            }),
        where_clause: value.get("where").cloned(),
    }
}

/// Parse a fields mapping from YAML.
fn parse_fields(fields_map: &serde_yaml::Mapping) -> Result<HashMap<String, FieldDef>, String> {
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
        .get(ykey("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("any")
        .to_string();

    let required = mapping
        .get(ykey("required"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let default = mapping.get(ykey("default")).map(yaml_to_json);

    let generated = mapping.get(ykey("generated")).map(parse_generated);

    let description = mapping
        .get(ykey("description"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let deprecated = mapping.get(ykey("deprecated")).map(|v| match v {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Bool(true) => "deprecated".to_string(),
        _ => "deprecated".to_string(),
    });

    let unique = mapping
        .get(ykey("unique"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let min = mapping.get(ykey("min")).and_then(|v| v.as_f64());
    let max = mapping.get(ykey("max")).and_then(|v| v.as_f64());
    let min_length = mapping
        .get(ykey("min_length"))
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let max_length = mapping
        .get(ykey("max_length"))
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);

    let pattern = mapping
        .get(ykey("pattern"))
        .and_then(|v| v.as_str())
        .map(String::from);

    // Validate regex pattern if present
    if let Some(ref pat) = pattern {
        if fancy_regex::Regex::new(pat).is_err() {
            return Err(format!("Invalid regex pattern: '{}'", pat));
        }
    }

    let values = match mapping.get(ykey("values")) {
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
        .get(ykey("items"))
        .map(parse_field_def)
        .transpose()?
        .map(Box::new);

    let nested_fields = match mapping.get(ykey("fields")) {
        Some(serde_yaml::Value::Mapping(m)) => Some(parse_fields(m)?),
        _ => None,
    };

    let min_items = mapping
        .get(ykey("min_items"))
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let max_items = mapping
        .get(ykey("max_items"))
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);

    let list_unique = mapping
        .get(ykey("unique"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let target = mapping
        .get(ykey("target"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let validate_exists = mapping
        .get(ykey("validate_exists"))
        .and_then(|v| v.as_bool());

    let computed = mapping
        .get(ykey("computed"))
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
            "sequence" => GeneratedStrategy::Sequence(1),
            _ => GeneratedStrategy::Now, // fallback
        },
        serde_yaml::Value::Mapping(m) => {
            let ykey = |s: &str| serde_yaml::Value::String(s.to_string());
            // Check for random key (e.g., {random: 8})
            if let Some(random_val) = m.get(ykey("random")) {
                let len = random_val.as_u64().unwrap_or(0);
                return GeneratedStrategy::Random(len);
            }
            // Check for sequence key (e.g., {sequence: {start: 100}})
            if let Some(seq_val) = m.get(ykey("sequence")) {
                let start = if let Some(seq_map) = seq_val.as_mapping() {
                    seq_map
                        .get(ykey("start"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(1)
                } else {
                    1
                };
                return GeneratedStrategy::Sequence(start);
            }
            // Check for strategy key first (e.g., {strategy: uuid})
            if let Some(strategy) = m.get(ykey("strategy")).and_then(|v| v.as_str()) {
                match strategy {
                    "ulid" => GeneratedStrategy::Ulid,
                    "uuid" => GeneratedStrategy::Uuid,
                    "now" => GeneratedStrategy::Now,
                    "now_on_write" => GeneratedStrategy::NowOnWrite,
                    "timestamp" => GeneratedStrategy::Now, // timestamp maps to Now
                    "sequence" => GeneratedStrategy::Sequence(1),
                    _ => GeneratedStrategy::Now,
                }
            } else {
                let from = m
                    .get(ykey("from"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let transform = m
                    .get(ykey("transform"))
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
        None => {
            return MatchRules {
                path_glob: None,
                path_globs: None,
                fields_present: None,
                where_clause: None,
            }
        }
    };

    let ykey = |s: &str| serde_yaml::Value::String(s.to_string());

    let path_glob = mapping
        .get(ykey("path_glob"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let fields_present = mapping.get(ykey("fields_present")).and_then(|v| {
        v.as_sequence().map(|seq| {
            seq.iter()
                .filter_map(|item| item.as_str().map(String::from))
                .collect()
        })
    });

    let where_clause = mapping.get(ykey("where")).map(yaml_to_json);

    MatchRules {
        path_glob,
        path_globs: None,
        fields_present,
        where_clause,
    }
}
