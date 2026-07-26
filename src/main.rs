use std::io::IsTerminal;
use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand};
use mdbase::api::{
    BatchOperation, BatchRequest, CollectionPath, CreateRequest, DeleteRequest, MdbaseError,
    MdbaseResult, OperationOutcome, QueryDirection, QueryRequest, ReadRequest, RenameRequest,
    Revision, UpdateRequest, V02MigrationRequest,
};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// mdbase - a markdown-based data store
#[derive(Parser)]
#[command(name = "mdbase", version, about)]
struct Cli {
    /// Root directory of the collection (defaults to current directory)
    #[arg(short = 'C', long = "root", global = true)]
    root: Option<PathBuf>,

    /// Pretty-print JSON output
    #[arg(long, global = true)]
    pretty: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize a new collection
    Init {
        /// Config YAML string to write into mdbase.yaml
        #[arg(long, conflicts_with = "config_file")]
        config: Option<String>,

        /// Path to a config YAML file
        #[arg(long, conflicts_with = "config")]
        config_file: Option<String>,
    },

    /// Read file metadata and frontmatter
    Read {
        /// File path (relative to collection root)
        path: String,
    },

    /// Create a new file
    Create {
        /// File path
        #[arg(long)]
        path: Option<String>,

        /// File type
        #[arg(long = "type")]
        file_type: Option<String>,

        /// Fields as JSON string
        #[arg(long)]
        fields: Option<String>,

        /// Require this current revision before creating
        #[arg(long)]
        if_revision: Option<String>,

        /// Validate and report the mutation without writing
        #[arg(long)]
        dry_run: bool,
    },

    /// Update an existing file
    Update {
        /// File path
        path: String,

        /// Fields as JSON string
        #[arg(long)]
        fields: Option<String>,

        /// Require this current revision before updating
        #[arg(long)]
        if_revision: Option<String>,

        /// Validate and report the mutation without writing
        #[arg(long)]
        dry_run: bool,
    },

    /// Delete a file
    Delete {
        /// File path
        path: String,

        /// Check for backlinks before deleting
        #[arg(long)]
        check_backlinks: bool,

        /// Require this current revision before deleting
        #[arg(long)]
        if_revision: Option<String>,

        /// Validate and report the mutation without writing
        #[arg(long)]
        dry_run: bool,
    },

    /// Rename or move a file
    Rename {
        /// Source path
        from: String,

        /// Destination path
        to: String,

        /// Update references in other files (the default)
        #[arg(long, conflicts_with = "no_update_refs")]
        update_refs: bool,

        /// Do not update references in other files
        #[arg(long, conflicts_with = "update_refs")]
        no_update_refs: bool,

        /// Require this current revision before renaming
        #[arg(long)]
        if_revision: Option<String>,

        /// Validate and report the mutation without writing
        #[arg(long)]
        dry_run: bool,
    },

    /// Query the collection
    Query {
        /// Read a complete typed query request from this JSON file (`-` for stdin)
        #[arg(
            long,
            value_name = "PATH",
            conflicts_with_all = ["types", "where_clause", "folder", "order_by", "limit", "offset", "include_body"]
        )]
        request: Option<String>,

        /// Filter by type(s), comma-separated
        #[arg(long)]
        types: Option<String>,

        /// Where clause expression
        #[arg(long = "where")]
        where_clause: Option<String>,

        /// Filter by folder
        #[arg(long)]
        folder: Option<String>,

        /// Order by field(s), comma-separated (prefix with - for descending)
        #[arg(long)]
        order_by: Option<String>,

        /// Maximum number of results
        #[arg(long)]
        limit: Option<u64>,

        /// Number of results to skip
        #[arg(long)]
        offset: Option<u64>,

        /// Include body text in results
        #[arg(long)]
        include_body: bool,
    },

    /// Execute a typed mutation batch from JSON
    Batch {
        /// Batch request JSON file (`-` for stdin)
        #[arg(long, value_name = "PATH", default_value = "-")]
        request: String,
    },

    /// Discover and execute saved views
    Views {
        #[command(subcommand)]
        action: ViewAction,
    },

    /// Validate a file or the entire collection
    Validate {
        /// File path (omit for collection-wide validation)
        path: Option<String>,
    },

    /// Backfill missing defaults/generated values
    Backfill {
        /// Restrict backfill to a type
        #[arg(long = "type")]
        file_type: Option<String>,

        /// Filter expression for matching files
        #[arg(long = "where")]
        where_clause: Option<String>,

        /// Comma-separated fields to backfill
        #[arg(long)]
        fields: Option<String>,

        /// Dry run only (do not write files)
        #[arg(long)]
        dry_run: bool,

        /// Apply default values
        #[arg(long)]
        apply_defaults: Option<bool>,

        /// Apply generated values
        #[arg(long)]
        apply_generated: Option<bool>,
    },

    /// Run a migration manifest
    Migrate {
        /// Migration id to resolve from migrations folder
        #[arg(long)]
        id: Option<String>,

        /// Explicit migration manifest path
        #[arg(long)]
        path: Option<String>,

        /// Dry run only
        #[arg(long)]
        dry_run: bool,
    },

    /// Translate a read-only v0.2 collection to canonical v0.3
    MigrateV02 {
        /// Verify and report changes without writing
        #[arg(long)]
        dry_run: bool,

        /// Apply translations that cannot preserve future write behavior
        #[arg(long)]
        allow_lossy: bool,
    },

    /// Cache management
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
}

#[derive(Subcommand)]
enum CacheAction {
    /// Show cache status
    Status,
    /// Rebuild the cache
    Rebuild,
    /// Clear the cache
    Clear,
}

#[derive(Subcommand)]
enum ViewAction {
    /// List saved views from every enabled source
    List,
    /// Execute one named saved view
    Execute {
        /// View source path
        path: String,
        /// Stable named-view ID
        #[arg(long = "view")]
        view_id: String,
        /// Optional invocation-context record path
        #[arg(long)]
        context: Option<String>,
        #[arg(long)]
        limit: Option<u64>,
        #[arg(long)]
        offset: Option<u64>,
    },
}

// Exit codes per spec Appendix C.9
const EXIT_SUCCESS: i32 = 0;
const EXIT_GENERAL_ERROR: i32 = 1;
const EXIT_VALIDATION_ERROR: i32 = 2;
const EXIT_CONFIG_ERROR: i32 = 3;
const EXIT_NOT_FOUND: i32 = 4;
#[allow(dead_code)]
const EXIT_PERMISSION_DENIED: i32 = 5;

fn main() {
    let cli = Cli::parse();

    let root = cli
        .root
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let is_tty = atty_check();
    let pretty = cli.pretty || is_tty;

    if let Command::Init {
        config,
        config_file,
    } = cli.command
    {
        let config_value = match (config, config_file) {
            (Some(c), None) => Some(c),
            (None, Some(path)) => match std::fs::read_to_string(&path) {
                Ok(content) => Some(content),
                Err(e) => {
                    let err = serde_json::json!({
                        "error": {
                            "code": "invalid_path",
                            "message": format!("Failed to read --config-file '{}': {}", path, e),
                        }
                    });
                    output_json(&err, pretty);
                    process::exit(EXIT_GENERAL_ERROR);
                }
            },
            (None, None) => None,
            (Some(_), Some(_)) => unreachable!(),
        };

        let mut input = serde_json::Map::new();
        if let Some(cfg) = config_value {
            input.insert("config".to_string(), serde_json::Value::String(cfg));
        }
        let result = mdbase::init::init_collection(&root, &serde_json::Value::Object(input));
        let exit_code = if result.get("error").is_some() {
            error_to_exit_code(&result)
        } else {
            EXIT_SUCCESS
        };
        output_json(&result, pretty);
        process::exit(exit_code);
    }

    let collection = match mdbase::Collection::open(&root) {
        Ok(c) => c,
        Err(e) => {
            output_json_stderr(&e);
            process::exit(EXIT_CONFIG_ERROR);
        }
    };

    let (result, exit_code) = execute_command(&collection, cli.command);
    output_json(&result, pretty);
    process::exit(exit_code);
}

fn execute_command(collection: &mdbase::Collection, command: Command) -> (serde_json::Value, i32) {
    match command {
        Command::Init { .. } => {
            let result = serde_json::json!({
                "error": {
                    "code": "invalid_request",
                    "message": "init must be executed before collection open"
                }
            });
            (result, EXIT_GENERAL_ERROR)
        }

        Command::Read { path } => {
            let result = collection.typed().and_then(|api| {
                let request = ReadRequest::new(path)?;
                api.read(request)
            });
            typed_result(result)
        }

        Command::Create {
            path,
            file_type,
            fields,
            if_revision,
            dry_run,
        } => {
            let fields_value = parse_fields_or_stdin(fields.as_deref());
            let request: MdbaseResult<CreateRequest> = (|| {
                let mut request = match path {
                    Some(path) => CreateRequest::new(CollectionPath::new(path)?, fields_value),
                    None => CreateRequest::derived(fields_value),
                };
                if let Some(file_type) = file_type {
                    request = request.with_type(file_type);
                }
                request.if_revision = parse_optional_revision(if_revision)?;
                Ok(request)
            })();
            if dry_run {
                typed_result(request.and_then(|request| {
                    let mut batch = BatchRequest::new(vec![BatchOperation::Create(request)])?;
                    batch.dry_run = true;
                    collection.typed()?.batch(batch)
                }))
            } else {
                typed_result(request.and_then(|request| collection.typed()?.create(request)))
            }
        }

        Command::Update {
            path,
            fields,
            if_revision,
            dry_run,
        } => {
            let fields_value = parse_fields_or_stdin(fields.as_deref());
            let request: MdbaseResult<UpdateRequest> = (|| {
                let mut request = UpdateRequest::new(CollectionPath::new(path)?, fields_value);
                request.if_revision = parse_optional_revision(if_revision)?;
                Ok(request)
            })();
            if dry_run {
                typed_result(request.and_then(|request| {
                    let mut batch = BatchRequest::new(vec![BatchOperation::Update(request)])?;
                    batch.dry_run = true;
                    collection.typed()?.batch(batch)
                }))
            } else {
                typed_result(request.and_then(|request| collection.typed()?.update(request)))
            }
        }

        Command::Delete {
            path,
            check_backlinks,
            if_revision,
            dry_run,
        } => {
            let request: MdbaseResult<DeleteRequest> = (|| {
                let mut request = DeleteRequest::new(CollectionPath::new(path)?);
                request.check_backlinks = check_backlinks;
                request.if_revision = parse_optional_revision(if_revision)?;
                Ok(request)
            })();
            if dry_run {
                typed_result(
                    request.and_then(|request| collection.typed()?.preflight_delete(request)),
                )
            } else {
                typed_result(request.and_then(|request| collection.typed()?.delete(request)))
            }
        }

        Command::Rename {
            from,
            to,
            update_refs,
            no_update_refs,
            if_revision,
            dry_run,
        } => {
            let request: MdbaseResult<RenameRequest> = (|| {
                let mut request =
                    RenameRequest::new(CollectionPath::new(from)?, CollectionPath::new(to)?);
                request.update_refs = update_refs || !no_update_refs;
                request.if_revision = parse_optional_revision(if_revision)?;
                Ok(request)
            })();
            if dry_run {
                typed_result(
                    request.and_then(|request| collection.typed()?.preflight_rename(request)),
                )
            } else {
                typed_result(request.and_then(|request| collection.typed()?.rename(request)))
            }
        }

        Command::Query {
            request: request_file,
            types,
            where_clause,
            folder,
            order_by,
            limit,
            offset,
            include_body,
        } => {
            if let Some(request_file) = request_file {
                return typed_result(
                    parse_json_input::<QueryRequest>(&request_file)
                        .and_then(|request| collection.typed()?.query(request)),
                );
            }
            let mut request = QueryRequest::builder();
            if let Some(types) = types {
                for type_name in types
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                {
                    request = request.type_name(type_name);
                }
            }
            let mut expression = where_clause;
            if let Some(folder) = folder {
                let folder = folder.trim_end_matches(['/', '\\']).replace('\\', "/");
                let quoted = serde_json::to_string(&folder).expect("string serializes");
                let child =
                    serde_json::to_string(&format!("{folder}/")).expect("string serializes");
                let folder_expression =
                    format!("(file.folder == {quoted} || file.folder.startsWith({child}))");
                expression = Some(match expression {
                    Some(existing) => format!("({existing}) && {folder_expression}"),
                    None => folder_expression,
                });
            }
            if let Some(expression) = expression {
                request = request.where_expression(expression);
            }
            if let Some(order_by) = order_by {
                for field in order_by
                    .split(',')
                    .map(str::trim)
                    .filter(|field| !field.is_empty())
                {
                    let (field, direction) = match field.strip_prefix('-') {
                        Some(field) => (field, QueryDirection::Desc),
                        None => (field, QueryDirection::Asc),
                    };
                    request = request.order_by(field, direction);
                }
            }
            if let Some(limit) = limit {
                request = request.limit(limit);
            }
            if let Some(offset) = offset {
                request = request.offset(offset);
            }
            request.include_body = include_body;
            typed_result(collection.typed().and_then(|api| api.query(request)))
        }

        Command::Batch { request } => typed_result(
            parse_json_input::<BatchRequest>(&request).and_then(|request| {
                if request.operations.is_empty() {
                    return Err(MdbaseError::InvalidRequest {
                        message: "batch operations must not be empty".to_string(),
                    });
                }
                collection.typed()?.batch(request)
            }),
        ),

        Command::Views { action } => {
            let operations = match collection.v03_operations() {
                Ok(operations) => operations,
                Err(diagnostic) => {
                    return (
                        serde_json::to_value(mdbase::v03::OperationResult {
                            valid: false,
                            result: serde_json::json!({}),
                            diagnostics: vec![*diagnostic],
                        })
                        .expect("operation result serializes"),
                        EXIT_GENERAL_ERROR,
                    )
                }
            };
            let result = match action {
                ViewAction::List => operations.list_views(&serde_json::json!({})),
                ViewAction::Execute {
                    path,
                    view_id,
                    context,
                    limit,
                    offset,
                } => {
                    let mut input = serde_json::json!({"path": path, "view": view_id});
                    if let Some(context) = context {
                        input["context"] = serde_json::json!({"path": context});
                    }
                    if let Some(limit) = limit {
                        input["limit"] = serde_json::json!(limit);
                    }
                    if let Some(offset) = offset {
                        input["offset"] = serde_json::json!(offset);
                    }
                    operations.execute_view(&input)
                }
            };
            let exit = if result.valid {
                EXIT_SUCCESS
            } else {
                EXIT_GENERAL_ERROR
            };
            (
                serde_json::to_value(result).expect("operation result serializes"),
                exit,
            )
        }

        Command::Validate { path } => {
            let input = if let Some(p) = path {
                serde_json::json!({ "path": p })
            } else {
                serde_json::json!({})
            };
            if collection.spec_profile() == mdbase::SpecProfile::V03 {
                let result = match collection.v03_operations() {
                    Ok(operations) => operations.validate(&input),
                    Err(diagnostic) => mdbase::v03::OperationResult {
                        valid: false,
                        result: serde_json::json!({}),
                        diagnostics: vec![*diagnostic],
                    },
                };
                let exit = canonical_exit_code(&result);
                return (
                    serde_json::to_value(result).expect("operation result serializes"),
                    exit,
                );
            }
            let result = collection.validate_op(&input);
            let exit = if result.get("error").is_some() {
                error_to_exit_code(&result)
            } else if result.get("valid") == Some(&serde_json::Value::Bool(false)) {
                EXIT_VALIDATION_ERROR
            } else {
                EXIT_SUCCESS
            };
            (result, exit)
        }

        Command::Backfill {
            file_type,
            where_clause,
            fields,
            dry_run,
            apply_defaults,
            apply_generated,
        } => {
            if collection.spec_profile() == mdbase::SpecProfile::V02 {
                return typed_error_result(MdbaseError::MigrationRequired {
                    operation: "backfill",
                });
            }
            let mut input = serde_json::Map::new();
            if let Some(t) = file_type {
                input.insert("type".to_string(), serde_json::Value::String(t));
            }
            if let Some(w) = where_clause {
                input.insert("where".to_string(), serde_json::Value::String(w));
            }
            if let Some(f) = fields {
                let field_list: Vec<serde_json::Value> = f
                    .split(',')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(|s| serde_json::Value::String(s.to_string()))
                    .collect();
                input.insert("fields".to_string(), serde_json::Value::Array(field_list));
            }
            if dry_run {
                input.insert("dry_run".to_string(), serde_json::Value::Bool(true));
            }
            if apply_defaults.is_some() || apply_generated.is_some() {
                let mut apply = serde_json::Map::new();
                if let Some(v) = apply_defaults {
                    apply.insert("defaults".to_string(), serde_json::Value::Bool(v));
                }
                if let Some(v) = apply_generated {
                    apply.insert("generated".to_string(), serde_json::Value::Bool(v));
                }
                input.insert("apply".to_string(), serde_json::Value::Object(apply));
            }

            let result = collection.backfill(&serde_json::Value::Object(input));
            let exit = if result.get("error").is_some() {
                error_to_exit_code(&result)
            } else {
                EXIT_SUCCESS
            };
            (result, exit)
        }

        Command::Migrate { id, path, dry_run } => {
            if collection.spec_profile() == mdbase::SpecProfile::V02 {
                return typed_error_result(MdbaseError::MigrationRequired {
                    operation: "migrate",
                });
            }
            let mut input = serde_json::Map::new();
            if let Some(v) = id {
                input.insert("id".to_string(), serde_json::Value::String(v));
            }
            if let Some(v) = path {
                input.insert("path".to_string(), serde_json::Value::String(v));
            }
            if dry_run {
                input.insert("dry_run".to_string(), serde_json::Value::Bool(true));
            }
            let result = collection.migrate(&serde_json::Value::Object(input));
            let exit = if result.get("error").is_some() {
                error_to_exit_code(&result)
            } else {
                EXIT_SUCCESS
            };
            (result, exit)
        }

        Command::MigrateV02 {
            dry_run,
            allow_lossy,
        } => {
            let result = collection.typed().and_then(|api| {
                api.migrate_v02(V02MigrationRequest {
                    dry_run,
                    allow_lossy,
                })
            });
            match result {
                Ok(value) => {
                    let diagnostics = value.diagnostics.clone();
                    (
                        serde_json::json!({
                            "valid": true,
                            "result": value,
                            "diagnostics": diagnostics,
                        }),
                        EXIT_SUCCESS,
                    )
                }
                Err(error) => typed_error_result(error),
            }
        }

        Command::Cache { action } => {
            let result = match action {
                CacheAction::Status => {
                    let cache_dir = collection.root().join(&collection.settings().cache_folder);
                    let db_path = cache_dir.join("cache.db");
                    serde_json::json!({
                        "cache_folder": collection.settings().cache_folder,
                        "database_exists": db_path.exists(),
                        "database_path": db_path.to_string_lossy(),
                    })
                }
                CacheAction::Rebuild => collection.cache_rebuild(),
                CacheAction::Clear => collection.cache_clear(),
            };
            let exit = if result.get("success") == Some(&serde_json::Value::Bool(false)) {
                error_to_exit_code(&result)
            } else {
                EXIT_SUCCESS
            };
            (result, exit)
        }
    }
}

fn typed_result<T: Serialize>(
    result: MdbaseResult<OperationOutcome<T>>,
) -> (serde_json::Value, i32) {
    match result {
        Ok(outcome) => (
            serde_json::json!({
                "valid": true,
                "result": outcome.value,
                "diagnostics": outcome.diagnostics,
            }),
            EXIT_SUCCESS,
        ),
        Err(error) => {
            let diagnostics = typed_error_diagnostics(&error);
            let exit = diagnostics
                .first()
                .map(|diagnostic| diagnostic_code_to_exit(diagnostic.code.as_str()))
                .unwrap_or(EXIT_GENERAL_ERROR);
            (
                serde_json::json!({
                    "valid": false,
                    "result": {},
                    "diagnostics": diagnostics,
                }),
                exit,
            )
        }
    }
}

fn typed_error_result(error: MdbaseError) -> (serde_json::Value, i32) {
    let diagnostics = typed_error_diagnostics(&error);
    let exit = diagnostics
        .first()
        .map(|diagnostic| diagnostic_code_to_exit(diagnostic.code.as_str()))
        .unwrap_or(EXIT_GENERAL_ERROR);
    (
        serde_json::json!({
            "valid": false,
            "result": {},
            "diagnostics": diagnostics,
        }),
        exit,
    )
}

fn typed_error_diagnostics(error: &MdbaseError) -> Vec<mdbase::api::Diagnostic> {
    if !error.diagnostics().is_empty() {
        return error.diagnostics().to_vec();
    }
    let code = match error {
        MdbaseError::InvalidPath(_) => "invalid_path",
        MdbaseError::UnsupportedProfile => "migration_required",
        MdbaseError::MigrationRequired { .. } => "migration_required",
        MdbaseError::LossyMigration { .. } => "migration_lossy",
        MdbaseError::InvalidRequest { .. } => "invalid_request",
        MdbaseError::InvalidResult { .. } => "invalid_result",
        MdbaseError::Operation { .. } => "operation_failed",
    };
    vec![mdbase::api::Diagnostic {
        severity: mdbase::api::Severity::Error,
        code: mdbase::api::DiagnosticCode::new(code),
        message: error.to_string(),
        path: None,
        field: None,
        type_name: None,
        schema_location: None,
        details: None,
    }]
}

fn parse_optional_revision(value: Option<String>) -> MdbaseResult<Option<Revision>> {
    value.map(Revision::parse).transpose()
}

fn parse_json_input<T: DeserializeOwned>(source: &str) -> MdbaseResult<T> {
    let content = if source == "-" {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).map_err(|error| {
            MdbaseError::InvalidRequest {
                message: format!("could not read JSON request from stdin: {error}"),
            }
        })?;
        input
    } else {
        std::fs::read_to_string(source).map_err(|error| MdbaseError::InvalidRequest {
            message: format!("could not read JSON request '{source}': {error}"),
        })?
    };
    serde_json::from_str(&content).map_err(|error| MdbaseError::InvalidRequest {
        message: format!("could not parse JSON request: {error}"),
    })
}

fn canonical_exit_code(result: &mdbase::v03::OperationResult) -> i32 {
    if result.valid {
        return EXIT_SUCCESS;
    }
    result
        .diagnostics
        .first()
        .map(|diagnostic| diagnostic_code_to_exit(&diagnostic.code))
        .unwrap_or(EXIT_GENERAL_ERROR)
}

fn diagnostic_code_to_exit(code: &str) -> i32 {
    match code {
        "file_not_found" | "not_found" => EXIT_NOT_FOUND,
        "validation_failed" | "schema_validation_failed" => EXIT_VALIDATION_ERROR,
        "invalid_config" | "config_error" | "missing_config" | "unsupported_version" => {
            EXIT_CONFIG_ERROR
        }
        "permission_denied" => EXIT_PERMISSION_DENIED,
        _ => EXIT_GENERAL_ERROR,
    }
}

/// Parse fields from --fields argument or stdin.
fn parse_fields_or_stdin(fields_arg: Option<&str>) -> serde_json::Value {
    if let Some(fields_str) = fields_arg {
        serde_json::from_str(fields_str).unwrap_or_else(|e| {
            eprintln!("Error parsing --fields JSON: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        })
    } else {
        // Try reading from stdin if not a TTY
        if !atty_check_stdin() {
            let mut input = String::new();
            if std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).is_ok()
                && !input.trim().is_empty()
            {
                return serde_json::from_str(&input).unwrap_or_else(|e| {
                    eprintln!("Error parsing stdin JSON: {}", e);
                    process::exit(EXIT_GENERAL_ERROR);
                });
            }
        }
        serde_json::json!({})
    }
}

/// Map error codes to exit codes.
fn error_to_exit_code(result: &serde_json::Value) -> i32 {
    let code = result
        .pointer("/error/code")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match code {
        "file_not_found" | "not_found" => EXIT_NOT_FOUND,
        "validation_failed" => EXIT_VALIDATION_ERROR,
        "invalid_config"
        | "config_error"
        | "invalid_type_definition"
        | "circular_inheritance"
        | "missing_parent_type" => EXIT_CONFIG_ERROR,
        _ => EXIT_GENERAL_ERROR,
    }
}

fn output_json(value: &serde_json::Value, pretty: bool) {
    let output = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    if let Ok(s) = output {
        println!("{}", s);
    }
}

fn output_json_stderr(value: &serde_json::Value) {
    if let Ok(s) = serde_json::to_string_pretty(value) {
        eprintln!("{}", s);
    }
}

/// Check if stdout is a TTY.
fn atty_check() -> bool {
    std::io::stdout().is_terminal()
}

/// Check if stdin is a TTY.
fn atty_check_stdin() -> bool {
    std::io::stdin().is_terminal()
}
