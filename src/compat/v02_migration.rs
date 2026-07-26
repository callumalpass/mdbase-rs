//! Explicit, verified migration from the read-only v0.2 adapter to v0.3.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::api::{
    Diagnostic, DiagnosticCode, MdbaseError, MdbaseResult, ReadRequest, Revision, Severity,
    V02MigrationChange, V02MigrationRequest, V02MigrationResult,
};
use crate::transactions::FileBaseline;
use crate::types::schema::{FieldDef, GeneratedStrategy, MatchRules, StrictMode, TypeDef};
use crate::{Collection, SpecProfile};

const MIGRATION_DIRECTORY: &str = ".mdbase/migrations";

pub(crate) fn migrate(
    collection: &Collection,
    request: V02MigrationRequest,
) -> MdbaseResult<V02MigrationResult> {
    if collection.spec_profile != SpecProfile::V02 {
        return Err(operation_error(
            "migration_not_required",
            "The collection already uses the canonical v0.3 profile.",
            Some("mdbase.yaml"),
        ));
    }

    let mut diagnostics = Vec::new();
    let mut desired = BTreeMap::new();
    desired.insert(
        "mdbase.yaml".to_string(),
        canonical_config(collection, &mut diagnostics)?,
    );
    let mut type_names = collection.types.keys().cloned().collect::<Vec<_>>();
    type_names.sort();
    for type_name in type_names {
        let definition = &collection.types[&type_name];
        let target = format!(
            "{}/{}.md",
            collection.settings.types_folder, definition.name
        );
        desired.insert(
            target,
            canonical_type_file(
                definition,
                &collection.settings.default_strict,
                &mut diagnostics,
            )?,
        );
    }

    let verified_records = verify_equivalent_reads(collection, &desired)?;
    let lossy = diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code.as_str() == "migration_lossy");
    if lossy && !request.dry_run && !request.allow_lossy {
        return Err(MdbaseError::LossyMigration { diagnostics });
    }

    let id = migration_id(&desired);
    let manifest_path = format!("{MIGRATION_DIRECTORY}/v02-to-v03-{id}.json");
    let artifact_paths = desired.keys().cloned().collect::<Vec<_>>();
    let manifest = serde_json::to_vec_pretty(&json!({
        "kind": "mdbase.migration",
        "version": 1,
        "id": id,
        "source_spec_version": "0.2",
        "target_spec_version": crate::v03::SPEC_VERSION,
        "verified_records": verified_records,
        "allow_lossy": request.allow_lossy,
        "artifacts": artifact_paths,
        "diagnostics": diagnostics,
    }))
    .map_err(|error| {
        operation_error(
            "migration_plan_failed",
            &format!("Could not serialize the migration manifest: {error}"),
            Some(&manifest_path),
        )
    })?;
    desired.insert(manifest_path.clone(), manifest);

    let mut baseline = FileBaseline::new();
    let changed_paths = desired
        .keys()
        .cloned()
        .chain(
            collection
                .types
                .values()
                .filter_map(|definition| definition.source_path.clone()),
        )
        .collect::<BTreeSet<_>>();
    for path in changed_paths {
        match fs::read(collection.root.join(&path)) {
            Ok(bytes) => {
                baseline.insert(path, bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(operation_error(
                    "migration_plan_failed",
                    &format!("Could not read the migration baseline: {error}"),
                    Some(&path),
                ))
            }
        }
    }

    let changes = migration_changes(&baseline, &desired)?;
    if request.dry_run {
        return Ok(V02MigrationResult {
            id,
            applied: false,
            verified_records,
            manifest_path,
            changes,
            diagnostics,
        });
    }

    crate::transactions::commit_migration(collection, &baseline, &desired).map_err(|error| {
        operation_error(
            error.code(),
            &error.to_string(),
            error_path_from_transaction(&error),
        )
    })?;
    Ok(V02MigrationResult {
        id,
        applied: true,
        verified_records,
        manifest_path,
        changes,
        diagnostics,
    })
}

fn canonical_config(
    collection: &Collection,
    diagnostics: &mut Vec<Diagnostic>,
) -> MdbaseResult<Vec<u8>> {
    let original = read_yaml_json(&collection.root.join("mdbase.yaml"), "mdbase.yaml")?;
    let mut record_extensions = vec![Value::String("md".to_string())];
    record_extensions.extend(
        collection
            .settings
            .extensions
            .iter()
            .filter(|extension| extension.as_str() != "md")
            .cloned()
            .map(Value::String),
    );
    record_extensions.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
    record_extensions.dedup();

    let mut settings = Map::new();
    settings.insert(
        "types_folder".to_string(),
        Value::String(collection.settings.types_folder.clone()),
    );
    settings.insert(
        "record_extensions".to_string(),
        Value::Array(record_extensions),
    );
    settings.insert(
        "validation".to_string(),
        Value::String(collection.settings.default_validation.clone()),
    );
    settings.insert(
        "explicit_type_keys".to_string(),
        json!(collection.settings.explicit_type_keys),
    );
    settings.insert(
        "id_field".to_string(),
        Value::String(collection.settings.id_field.clone()),
    );
    settings.insert(
        "include_subfolders".to_string(),
        Value::Bool(collection.settings.include_subfolders),
    );
    settings.insert("exclude".to_string(), json!(collection.settings.exclude));
    settings.insert(
        "default_strict".to_string(),
        collection.settings.default_strict.clone(),
    );
    if let Some(timezone) = &collection.settings.timezone {
        settings.insert("timezone".to_string(), Value::String(timezone.clone()));
    }

    let mut config = Map::new();
    config.insert(
        "spec_version".to_string(),
        Value::String(crate::v03::SPEC_VERSION.to_string()),
    );
    config.insert("settings".to_string(), Value::Object(settings));
    for key in ["name", "description", "runtime"] {
        if let Some(value) = original.get(key) {
            config.insert(key.to_string(), value.clone());
        }
    }
    if let Some(object) = original.as_object() {
        for (key, value) in object {
            if key.starts_with("x-") {
                config.insert(key.clone(), value.clone());
            }
        }
    }
    config.insert(
        "x-mdbase-v02-migration".to_string(),
        json!({"source_spec_version": original.get("spec_version")}),
    );
    let value = Value::Object(config);
    let schema_diagnostics = crate::v03::validate_config(&value, "mdbase.yaml");
    if let Some(error) = schema_diagnostics
        .into_iter()
        .find(|diagnostic| diagnostic.severity == "error")
    {
        return Err(operation_error(
            "migration_plan_failed",
            &format!("Translated config is not canonical: {}", error.message),
            Some("mdbase.yaml"),
        ));
    }
    if collection.settings.write_defaults
        || collection.settings.write_nulls != "explicit"
        || !collection.settings.write_empty_lists
    {
        diagnostics.push(behavior_change_diagnostic(
            "v0.2 write serialization settings are not carried into canonical v0.3 writes",
            Some("mdbase.yaml"),
            None,
        ));
    }
    yaml_bytes(&value, false)
}

fn canonical_type_file(
    definition: &TypeDef,
    default_strict: &Value,
    diagnostics: &mut Vec<Diagnostic>,
) -> MdbaseResult<Vec<u8>> {
    let mut properties = Map::new();
    let mut required = Vec::new();
    let mut defaults = Map::new();
    let mut unique = Vec::new();
    let mut links = Map::new();
    let mut lifecycle_create = Map::new();
    let mut lifecycle_update = Map::new();
    let mut field_names = definition.fields.keys().cloned().collect::<Vec<_>>();
    field_names.sort();
    for field_name in field_names {
        let field = &definition.fields[&field_name];
        properties.insert(field_name.clone(), field_schema(field)?);
        if field.required {
            required.push(Value::String(field_name.clone()));
        }
        if let Some(default) = &field.default {
            defaults.insert(field_name.clone(), default.clone());
        }
        if field.unique {
            unique.push(json!({"field": field_name}));
        }
        if let Some(link) = link_definition(field) {
            let target_type = if link.target_types.len() > 1 {
                json!(link.target_types)
            } else {
                link.target
                    .as_ref()
                    .map_or_else(|| json!("any"), |target| json!(target))
            };
            links.insert(
                field_name.clone(),
                json!({
                    "target_type": target_type,
                    "validate_exists": link.validate_exists.unwrap_or(false),
                }),
            );
        }
        if let Some(generated) = &field.generated {
            match generated_provider(generated) {
                Some((create, update)) => {
                    lifecycle_create.insert(field_name.clone(), create);
                    if let Some(update) = update {
                        lifecycle_update.insert(field_name.clone(), update);
                    }
                }
                None => diagnostics.push(lossy_diagnostic(
                    &format!(
                        "Generated strategy for '{}.{}' has no canonical lifecycle equivalent",
                        definition.name, field_name
                    ),
                    definition.source_path.as_deref(),
                    Some(&field_name),
                )),
            }
        }
    }

    let additional_properties = match definition.strict {
        Some(StrictMode::Error) => false,
        Some(StrictMode::Off) => true,
        Some(StrictMode::Warn) => {
            diagnostics.push(lossy_diagnostic(
                &format!(
                    "Type '{}' uses strict: warn; canonical JSON Schema cannot preserve warning-only unknown fields",
                    definition.name
                ),
                definition.source_path.as_deref(),
                None,
            ));
            true
        }
        None => match default_strict {
            Value::Bool(true) => false,
            Value::String(value) if value == "warn" => {
                diagnostics.push(lossy_diagnostic(
                    &format!(
                        "Type '{}' inherits default_strict: warn; canonical JSON Schema cannot preserve warning-only unknown fields",
                        definition.name
                    ),
                    definition.source_path.as_deref(),
                    None,
                ));
                true
            }
            _ => true,
        },
    };
    let mut schema = Map::from_iter([
        ("type".to_string(), Value::String("object".to_string())),
        ("properties".to_string(), Value::Object(properties)),
        (
            "additionalProperties".to_string(),
            Value::Bool(additional_properties),
        ),
    ]);
    if !required.is_empty() {
        schema.insert("required".to_string(), Value::Array(required));
    }

    let mut collection = Map::new();
    if !defaults.is_empty() {
        collection.insert("read_defaults".to_string(), Value::Object(defaults));
    }
    if !unique.is_empty() {
        collection.insert("unique".to_string(), Value::Array(unique));
    }
    if !links.is_empty() {
        collection.insert("links".to_string(), Value::Object(links));
    }
    if let Some(display) = &definition.display_name_key {
        collection.insert("display".to_string(), json!({"name_field": display}));
    }
    if let Some(pattern) = definition
        .path_pattern
        .as_ref()
        .or(definition.filename_pattern.as_ref())
    {
        collection.insert("path".to_string(), json!({"pattern": pattern}));
    }

    let mut frontmatter = Map::from_iter([
        ("kind".to_string(), Value::String("mdbase.type".to_string())),
        ("name".to_string(), Value::String(definition.name.clone())),
        (
            "version".to_string(),
            Value::Number(definition.version.unwrap_or(1).max(1).into()),
        ),
        (
            "schema".to_string(),
            json!({
                "dialect": "json-schema-2020-12",
                "value": Value::Object(schema),
            }),
        ),
        (
            "x-mdbase-v02-migration".to_string(),
            json!({"source_path": definition.source_path}),
        ),
    ]);
    if let Some(description) = &definition.description {
        frontmatter.insert(
            "description".to_string(),
            Value::String(description.clone()),
        );
    }
    if let Some(rules) = &definition.match_rules {
        if let Some(value) = match_value(rules) {
            frontmatter.insert("match".to_string(), value);
        }
    }
    if !collection.is_empty() {
        frontmatter.insert("collection".to_string(), Value::Object(collection));
    }
    let mut lifecycle = Map::new();
    if !lifecycle_create.is_empty() {
        lifecycle.insert("on_create".to_string(), json!({"set": lifecycle_create}));
    }
    if !lifecycle_update.is_empty() {
        lifecycle.insert("on_update".to_string(), json!({"set": lifecycle_update}));
    }
    if !lifecycle.is_empty() {
        frontmatter.insert("lifecycle".to_string(), Value::Object(lifecycle));
    }

    let value = Value::Object(frontmatter);
    let path = definition
        .source_path
        .as_deref()
        .unwrap_or("<translated type>");
    if let Some(error) = crate::v03::validate_type_file(&value, path)
        .into_iter()
        .find(|diagnostic| diagnostic.severity == "error")
    {
        return Err(operation_error(
            "migration_plan_failed",
            &format!(
                "Translated type '{}' is not canonical: {}",
                definition.name, error.message
            ),
            definition.source_path.as_deref(),
        ));
    }
    yaml_bytes(&value, true)
}

fn field_schema(field: &FieldDef) -> MdbaseResult<Value> {
    let mut schema = Map::new();
    match field.field_type.as_str() {
        "integer" | "number" | "boolean" | "object" | "string" => {
            schema.insert("type".to_string(), Value::String(field.field_type.clone()));
        }
        "list" => {
            schema.insert("type".to_string(), Value::String("array".to_string()));
        }
        "date" | "datetime" | "time" | "duration" | "link" | "path" | "enum" => {
            schema.insert("type".to_string(), Value::String("string".to_string()));
        }
        "any" => {}
        other => {
            schema.insert("type".to_string(), Value::String("string".to_string()));
            schema.insert(
                "x-mdbase-v02-unknown-type".to_string(),
                Value::String(other.to_string()),
            );
        }
    }
    if let Some(description) = &field.description {
        schema.insert(
            "description".to_string(),
            Value::String(description.clone()),
        );
    }
    if field.deprecated.is_some() {
        schema.insert("deprecated".to_string(), Value::Bool(true));
    }
    insert_number(&mut schema, "minimum", field.min);
    insert_number(&mut schema, "maximum", field.max);
    insert_usize(&mut schema, "minLength", field.min_length);
    insert_usize(&mut schema, "maxLength", field.max_length);
    insert_usize(&mut schema, "minItems", field.min_items);
    insert_usize(&mut schema, "maxItems", field.max_items);
    if let Some(pattern) = &field.pattern {
        schema.insert("pattern".to_string(), Value::String(pattern.clone()));
    }
    if let Some(values) = &field.values {
        schema.insert("enum".to_string(), json!(values));
    }
    if field.list_unique {
        schema.insert("uniqueItems".to_string(), Value::Bool(true));
    }
    if let Some(items) = &field.items {
        schema.insert("items".to_string(), field_schema(items)?);
    }
    if let Some(fields) = &field.fields {
        let mut nested = Map::new();
        let mut required = Vec::new();
        let mut names = fields.keys().cloned().collect::<Vec<_>>();
        names.sort();
        for name in names {
            let nested_field = &fields[&name];
            nested.insert(name.clone(), field_schema(nested_field)?);
            if nested_field.required {
                required.push(Value::String(name));
            }
        }
        schema.insert("properties".to_string(), Value::Object(nested));
        if !required.is_empty() {
            schema.insert("required".to_string(), Value::Array(required));
        }
    }
    schema.insert(
        "x-mdbase-v02-field".to_string(),
        json!({
            "type": field.field_type,
            "computed": field.computed,
            "deprecated_message": field.deprecated,
            "target": field.target,
            "target_types": field.target_types,
            "validate_exists": field.validate_exists,
        }),
    );
    Ok(Value::Object(schema))
}

fn generated_provider(generated: &GeneratedStrategy) -> Option<(Value, Option<Value>)> {
    match generated {
        GeneratedStrategy::Ulid => Some((json!({"ulid": true}), None)),
        GeneratedStrategy::Uuid => Some((json!({"uuid": true}), None)),
        GeneratedStrategy::Now => Some((json!({"now": true}), None)),
        GeneratedStrategy::NowOnWrite => Some((json!({"now": true}), Some(json!({"now": true})))),
        GeneratedStrategy::Derived { from, transform }
            if !from.starts_with("file.") && transform == "slugify" =>
        {
            Some((json!({"slugify": from}), None))
        }
        GeneratedStrategy::Derived { from, transform }
            if !from.starts_with("file.") && transform.is_empty() =>
        {
            Some((json!({"copy": from}), None))
        }
        GeneratedStrategy::Sequence(_)
        | GeneratedStrategy::Random(_)
        | GeneratedStrategy::Derived { .. } => None,
    }
}

fn match_value(rules: &MatchRules) -> Option<Value> {
    let mut value = Map::new();
    match (&rules.path_glob, &rules.path_globs) {
        (Some(path), _) => {
            value.insert("path_glob".to_string(), Value::String(path.clone()));
        }
        (None, Some(paths)) if !paths.is_empty() => {
            value.insert("path_glob".to_string(), json!(paths));
        }
        _ => {}
    }
    if let Some(fields) = &rules.fields_present {
        if !fields.is_empty() {
            value.insert("fields_present".to_string(), json!(fields));
        }
    }
    if let Some(where_clause) = &rules.where_clause {
        value.insert("where".to_string(), where_clause.clone());
    }
    if let Some(expression) = &rules.match_expr {
        value.insert("expr".to_string(), json!({"$expr": expression}));
    }
    (!value.is_empty()).then_some(Value::Object(value))
}

fn verify_equivalent_reads(collection: &Collection, desired: &FileBaseline) -> MdbaseResult<usize> {
    let temporary = tempfile::tempdir().map_err(|error| {
        operation_error(
            "migration_verification_failed",
            &format!("Could not create an isolated verification directory: {error}"),
            None,
        )
    })?;
    copy_for_verification(
        &collection.root,
        temporary.path(),
        &collection.settings.types_folder,
    )?;
    for (path, bytes) in desired {
        let target = temporary.path().join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                operation_error(
                    "migration_verification_failed",
                    &format!("Could not prepare translated definitions: {error}"),
                    Some(path),
                )
            })?;
        }
        fs::write(&target, bytes).map_err(|error| {
            operation_error(
                "migration_verification_failed",
                &format!("Could not write translated definitions: {error}"),
                Some(path),
            )
        })?;
    }
    let canonical = Collection::open(temporary.path()).map_err(|error| {
        operation_error(
            "migration_verification_failed",
            &format!("Translated collection could not be opened: {error}"),
            Some("mdbase.yaml"),
        )
    })?;
    let legacy_api = collection.typed()?;
    let canonical_api = canonical.typed()?;
    let paths = collection
        .scan_collection_files_checked()
        .map_err(|error| {
            operation_error(
                "migration_verification_failed",
                &format!("Could not enumerate records for verification: {error}"),
                None,
            )
        })?;
    for absolute in &paths {
        let path = absolute
            .strip_prefix(&collection.root)
            .expect("scanner only returns paths below the collection root")
            .to_string_lossy()
            .replace('\\', "/");
        let request = ReadRequest::new(&path)?;
        let legacy = legacy_api.read(request.clone())?.value;
        let canonical = canonical_api.read(request)?.value;
        if legacy.frontmatter != canonical.frontmatter
            || legacy.raw_frontmatter != canonical.raw_frontmatter
            || legacy.types != canonical.types
            || legacy.body != canonical.body
        {
            return Err(operation_error(
                "migration_verification_failed",
                "Canonical read does not match the v0.2 compatibility read.",
                Some(&path),
            ));
        }
    }
    Ok(paths.len())
}

fn copy_for_verification(source: &Path, target: &Path, types_folder: &str) -> MdbaseResult<()> {
    for entry in WalkDir::new(source).sort_by_file_name() {
        let entry = entry.map_err(|error| {
            operation_error(
                "migration_verification_failed",
                &format!("Could not inspect the source collection: {error}"),
                None,
            )
        })?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .expect("walkdir entry is below its root");
        if relative.as_os_str().is_empty() {
            continue;
        }
        let logical = relative.to_string_lossy().replace('\\', "/");
        if logical == ".mdbase"
            || logical.starts_with(".mdbase/")
            || logical == types_folder
            || logical.starts_with(&format!("{types_folder}/"))
            || logical == "mdbase.yaml"
        {
            continue;
        }
        if entry.file_type().is_symlink() {
            return Err(operation_error(
                "migration_verification_failed",
                "Symbolic links cannot be copied into the isolated verifier.",
                Some(&logical),
            ));
        }
        let destination = target.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&destination).map_err(|error| {
                operation_error(
                    "migration_verification_failed",
                    &format!("Could not prepare the verifier: {error}"),
                    Some(&logical),
                )
            })?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    operation_error(
                        "migration_verification_failed",
                        &format!("Could not prepare the verifier: {error}"),
                        Some(&logical),
                    )
                })?;
            }
            fs::copy(entry.path(), &destination).map_err(|error| {
                operation_error(
                    "migration_verification_failed",
                    &format!("Could not copy a record for verification: {error}"),
                    Some(&logical),
                )
            })?;
        }
    }
    Ok(())
}

fn migration_changes(
    baseline: &FileBaseline,
    desired: &FileBaseline,
) -> MdbaseResult<Vec<V02MigrationChange>> {
    baseline
        .keys()
        .chain(desired.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| baseline.get(path) != desired.get(path))
        .map(|path| {
            let before_revision = baseline
                .get(&path)
                .map(|bytes| Revision::parse(crate::v03::revision(bytes)))
                .transpose()?;
            let after_revision = desired
                .get(&path)
                .map(|bytes| Revision::parse(crate::v03::revision(bytes)))
                .transpose()?;
            Ok(V02MigrationChange {
                path,
                before_revision,
                after_revision,
            })
        })
        .collect()
}

fn migration_id(desired: &FileBaseline) -> String {
    let mut hash = Sha256::new();
    for (path, bytes) in desired {
        hash.update(path.as_bytes());
        hash.update([0]);
        hash.update(bytes);
        hash.update([0]);
    }
    format!("{:x}", hash.finalize())[..16].to_string()
}

fn yaml_bytes(value: &Value, frontmatter: bool) -> MdbaseResult<Vec<u8>> {
    let yaml = serde_yaml::to_string(value).map_err(|error| MdbaseError::InvalidResult {
        message: format!("could not serialize translated YAML: {error}"),
    })?;
    Ok(if frontmatter {
        format!("---\n{yaml}---\n").into_bytes()
    } else {
        yaml.into_bytes()
    })
}

fn read_yaml_json(path: &Path, label: &str) -> MdbaseResult<Value> {
    let bytes = fs::read(path).map_err(|error| {
        operation_error(
            "migration_plan_failed",
            &format!("Could not read legacy YAML: {error}"),
            Some(label),
        )
    })?;
    let yaml: serde_yaml::Value = serde_yaml::from_slice(&bytes).map_err(|error| {
        operation_error(
            "migration_plan_failed",
            &format!("Could not parse legacy YAML: {error}"),
            Some(label),
        )
    })?;
    Ok(crate::frontmatter::parser::yaml_to_json(&yaml))
}

fn insert_number(target: &mut Map<String, Value>, key: &str, value: Option<f64>) {
    if let Some(number) = value.and_then(serde_json::Number::from_f64) {
        target.insert(key.to_string(), Value::Number(number));
    }
}

fn insert_usize(target: &mut Map<String, Value>, key: &str, value: Option<usize>) {
    if let Some(value) = value {
        target.insert(key.to_string(), json!(value));
    }
}

fn link_definition(field: &FieldDef) -> Option<&FieldDef> {
    if field.field_type == "link" {
        Some(field)
    } else {
        field
            .items
            .as_deref()
            .filter(|items| items.field_type == "link")
    }
}

fn lossy_diagnostic(message: &str, path: Option<&str>, field: Option<&str>) -> Diagnostic {
    Diagnostic {
        severity: Severity::Warning,
        code: DiagnosticCode::new("migration_lossy"),
        message: message.to_string(),
        path: path.map(str::to_string),
        field: field.map(str::to_string),
        type_name: None,
        schema_location: None,
        details: None,
    }
}

fn behavior_change_diagnostic(
    message: &str,
    path: Option<&str>,
    field: Option<&str>,
) -> Diagnostic {
    Diagnostic {
        severity: Severity::Warning,
        code: DiagnosticCode::new("migration_behavior_changed"),
        message: message.to_string(),
        path: path.map(str::to_string),
        field: field.map(str::to_string),
        type_name: None,
        schema_location: None,
        details: None,
    }
}

fn operation_error(code: &str, message: &str, path: Option<&str>) -> MdbaseError {
    MdbaseError::Operation {
        diagnostics: vec![Diagnostic {
            severity: Severity::Error,
            code: DiagnosticCode::new(code),
            message: message.to_string(),
            path: path.map(str::to_string),
            field: None,
            type_name: None,
            schema_location: None,
            details: None,
        }],
    }
}

fn error_path_from_transaction(error: &crate::transactions::TransactionError) -> Option<&str> {
    match error {
        crate::transactions::TransactionError::ConcurrentModification(path)
        | crate::transactions::TransactionError::UnsafePath(path) => Some(path),
        _ => None,
    }
}
