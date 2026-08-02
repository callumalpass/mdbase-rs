use clap::{Parser, Subcommand};
use mdbase::api::{
    BatchOperation, BatchRequest, CollectionPath, CreateRequest, DeleteRequest, MdbaseError,
    MdbaseResult, OperationOutcome, QueryDirection, QueryRequest, ReadRequest, RenameRequest,
    Revision, UpdateRequest, V02MigrationRequest,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

pub mod profile;

/// Version of the transport-neutral command adapter embedded by a host.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version of the mdbase collection engine linked into the final executable.
pub const fn engine_version() -> &'static str {
    mdbase::VERSION
}

/// Standalone parser for the data-plane command surface. The unified binary
/// flattens [`Command`] beside its Connect and profiling namespaces.
#[derive(Debug, Parser)]
#[command(name = "mdbase", about = "Work with mdbase collections")]
pub struct DirectArgs {
    /// Root directory of the collection (defaults to the current directory).
    #[arg(short = 'C', long = "root", global = true)]
    pub root: Option<PathBuf>,

    /// Pretty-print the portable JSON result.
    #[arg(long, global = true)]
    pub pretty: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// Collection data and maintenance commands shared by every mdbase transport.
#[derive(Clone, Debug, Subcommand)]
pub enum Command {
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

    /// Inspect and manage canonical type definition resources.
    Types {
        #[command(subcommand)]
        action: TypeAction,
    },

    /// Assess and apply versioned type-pack definition bundles.
    Packs {
        #[command(subcommand)]
        action: PackAction,
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

    /// Stream portable filesystem change events as JSON lines.
    Watch {
        /// Debounce window for atomic-save event bursts.
        #[arg(long, default_value_t = 150)]
        debounce_ms: u64,
        /// Stop after this many events; omit to continue until interrupted.
        #[arg(long)]
        count: Option<usize>,
    },
}

impl Command {
    /// Stable payload-free command label used by CLI performance telemetry.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Init { .. } => "init",
            Self::Read { .. } => "read",
            Self::Create { .. } => "create",
            Self::Update { .. } => "update",
            Self::Delete { .. } => "delete",
            Self::Rename { .. } => "rename",
            Self::Query { .. } => "query",
            Self::Batch { .. } => "batch",
            Self::Views { .. } => "views",
            Self::Types { .. } => "types",
            Self::Packs { .. } => "packs",
            Self::Validate { .. } => "validate",
            Self::Backfill { .. } => "backfill",
            Self::Migrate { .. } => "migrate",
            Self::MigrateV02 { .. } => "migrate_v02",
            Self::Cache { .. } => "cache",
            Self::Watch { .. } => "watch",
        }
    }
}

#[derive(Clone, Debug, Subcommand)]
pub enum CacheAction {
    /// Show cache status
    Status,
    /// Rebuild the cache
    Rebuild,
    /// Clear the cache
    Clear,
}

#[derive(Clone, Debug, Subcommand)]
pub enum ViewAction {
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

#[derive(Clone, Debug, Subcommand)]
pub enum TypeAction {
    /// List the collection's canonical type resources.
    List,
    /// Read a complete type definition and its revision.
    Show { name: String },
    /// Create a type definition from a complete Markdown document.
    Create {
        /// Markdown document file (`-` for stdin).
        #[arg(long, default_value = "-")]
        document: String,
        /// Optional path inside the configured types folder.
        #[arg(long)]
        path: Option<String>,
    },
    /// Replace a complete type definition.
    Update {
        name: String,
        /// Markdown document file (`-` for stdin).
        #[arg(long, default_value = "-")]
        document: String,
        /// Require the current opaque revision.
        #[arg(long)]
        if_revision: Option<String>,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum PackAction {
    /// Inspect the exact resource and provenance changes without writing.
    Assess {
        #[arg(long, default_value = "mdbase-pack.yaml")]
        manifest: String,
        #[arg(long, default_value = ".")]
        resources: String,
        #[arg(long, default_value = "dev.mdbase.cli")]
        installed_by: String,
        /// Explicitly adopt an unmanaged target as TARGET=sha256:DIGEST.
        #[arg(long = "adopt", value_name = "TARGET=DIGEST")]
        adoptions: Vec<String>,
        /// Record a seed as intentionally omitted instead of creating it.
        #[arg(long = "preserve-seed", value_name = "TARGET")]
        preserve_seed_targets: Vec<String>,
        /// Resolve a canonical pack target to a collection path as FROM=TO.
        #[arg(long = "target", value_name = "MANIFEST_TARGET=COLLECTION_TARGET")]
        target_overrides: Vec<String>,
    },
    /// Apply a previously reviewed assessment transactionally.
    Apply {
        #[arg(long, default_value = "mdbase-pack.yaml")]
        manifest: String,
        #[arg(long, default_value = ".")]
        resources: String,
        #[arg(long, default_value = "dev.mdbase.cli")]
        installed_by: String,
        #[arg(long)]
        assessment_digest: String,
        #[arg(long)]
        allow_downgrade: bool,
        /// Explicitly adopt an unmanaged target as TARGET=sha256:DIGEST.
        #[arg(long = "adopt", value_name = "TARGET=DIGEST")]
        adoptions: Vec<String>,
        /// Record a seed as intentionally omitted instead of creating it.
        #[arg(long = "preserve-seed", value_name = "TARGET")]
        preserve_seed_targets: Vec<String>,
        /// Resolve a canonical pack target to a collection path as FROM=TO.
        #[arg(long = "target", value_name = "MANIFEST_TARGET=COLLECTION_TARGET")]
        target_overrides: Vec<String>,
    },
}

/// A canonical operation that can be executed by either the filesystem engine
/// or the local Connect authority.
#[derive(Debug)]
pub struct PortableInvocation {
    pub operation: &'static str,
    pub input: serde_json::Value,
}

/// Convert a data command into the portable operation envelope used by
/// Connect. Filesystem-only maintenance commands return a stable diagnostic.
pub fn into_portable(command: Command) -> Result<PortableInvocation, CommandResult> {
    use serde_json::{json, Map, Value};

    let invocation = match command {
        Command::Read { path } => PortableInvocation {
            operation: "read",
            input: json!({"path": normalized_path(path).map_err(command_error)?}),
        },
        Command::Create {
            path,
            file_type,
            fields,
            if_revision,
            dry_run,
        } => {
            let frontmatter = parse_fields_or_stdin(fields.as_deref()).map_err(command_error)?;
            let mut input = Map::new();
            insert_option(
                &mut input,
                "path",
                path.map(normalized_path)
                    .transpose()
                    .map_err(command_error)?,
            );
            insert_option(&mut input, "type", file_type);
            insert_option(&mut input, "if_revision", if_revision);
            input.insert("frontmatter".to_string(), frontmatter);
            if dry_run {
                PortableInvocation {
                    operation: "batch",
                    input: json!({
                        "operations": [{"kind": "create", "input": Value::Object(input)}],
                        "allow_partial": false,
                        "dry_run": true,
                    }),
                }
            } else {
                PortableInvocation {
                    operation: "create",
                    input: Value::Object(input),
                }
            }
        }
        Command::Update {
            path,
            fields,
            if_revision,
            dry_run,
        } => {
            let patch = parse_fields_or_stdin(fields.as_deref()).map_err(command_error)?;
            let mut input = Map::from_iter([
                (
                    "path".to_string(),
                    Value::String(normalized_path(path).map_err(command_error)?),
                ),
                ("patch".to_string(), patch),
            ]);
            insert_option(&mut input, "if_revision", if_revision);
            if dry_run {
                PortableInvocation {
                    operation: "batch",
                    input: json!({
                        "operations": [{"kind": "update", "input": Value::Object(input)}],
                        "allow_partial": false,
                        "dry_run": true,
                    }),
                }
            } else {
                PortableInvocation {
                    operation: "update",
                    input: Value::Object(input),
                }
            }
        }
        Command::Delete {
            path,
            check_backlinks,
            if_revision,
            dry_run,
        } => {
            let mut input = Map::from_iter([
                (
                    "path".to_string(),
                    Value::String(normalized_path(path).map_err(command_error)?),
                ),
                ("check_backlinks".to_string(), Value::Bool(check_backlinks)),
                ("dry_run".to_string(), Value::Bool(dry_run)),
            ]);
            insert_option(&mut input, "if_revision", if_revision);
            PortableInvocation {
                operation: "delete",
                input: Value::Object(input),
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
            let mut input = Map::from_iter([
                (
                    "from".to_string(),
                    Value::String(normalized_path(from).map_err(command_error)?),
                ),
                (
                    "to".to_string(),
                    Value::String(normalized_path(to).map_err(command_error)?),
                ),
                (
                    "update_refs".to_string(),
                    Value::Bool(update_refs || !no_update_refs),
                ),
                ("dry_run".to_string(), Value::Bool(dry_run)),
            ]);
            insert_option(&mut input, "if_revision", if_revision);
            PortableInvocation {
                operation: "rename",
                input: Value::Object(input),
            }
        }
        Command::Query {
            request,
            types,
            where_clause,
            folder,
            order_by,
            limit,
            offset,
            include_body,
        } => PortableInvocation {
            operation: "query",
            input: portable_query(
                request.as_deref(),
                types,
                where_clause,
                folder,
                order_by,
                limit,
                offset,
                include_body,
            )
            .map_err(command_error)?,
        },
        Command::Batch { request } => PortableInvocation {
            operation: "batch",
            input: parse_json_input::<BatchRequest>(&request)
                .map_err(command_error)?
                .to_wire(),
        },
        Command::Views { action } => match action {
            ViewAction::List => PortableInvocation {
                operation: "list_views",
                input: json!({}),
            },
            ViewAction::Execute {
                path,
                view_id,
                context,
                limit,
                offset,
            } => {
                let mut input = Map::from_iter([
                    (
                        "path".to_string(),
                        Value::String(normalized_path(path).map_err(command_error)?),
                    ),
                    ("view".to_string(), Value::String(view_id)),
                ]);
                if let Some(context) = context {
                    input.insert(
                        "context".to_string(),
                        json!({"path": normalized_path(context).map_err(command_error)?}),
                    );
                }
                if let Some(limit) = limit {
                    input.insert("limit".to_string(), json!(limit));
                }
                if let Some(offset) = offset {
                    input.insert("offset".to_string(), json!(offset));
                }
                PortableInvocation {
                    operation: "execute_view",
                    input: Value::Object(input),
                }
            }
        },
        Command::Types { action } => match action {
            TypeAction::List => PortableInvocation {
                operation: "list_types",
                input: json!({}),
            },
            TypeAction::Show { name } => PortableInvocation {
                operation: "read_type",
                input: json!({"name": name}),
            },
            TypeAction::Create { document, path } => {
                let document = read_text_input(&document).map_err(command_error)?;
                let mut input = Map::from_iter([("document".to_string(), Value::String(document))]);
                insert_option(
                    &mut input,
                    "path",
                    path.map(normalized_path)
                        .transpose()
                        .map_err(command_error)?,
                );
                PortableInvocation {
                    operation: "create_type",
                    input: Value::Object(input),
                }
            }
            TypeAction::Update {
                name,
                document,
                if_revision,
            } => {
                let document = read_text_input(&document).map_err(command_error)?;
                let mut input = Map::from_iter([
                    ("name".to_string(), Value::String(name)),
                    ("document".to_string(), Value::String(document)),
                ]);
                insert_option(&mut input, "if_revision", if_revision);
                PortableInvocation {
                    operation: "update_type",
                    input: Value::Object(input),
                }
            }
        },
        Command::Packs { action } => match action {
            PackAction::Assess {
                manifest,
                resources,
                installed_by,
                adoptions,
                preserve_seed_targets,
                target_overrides,
            } => PortableInvocation {
                operation: "assess_type_pack",
                input: pack_operation_input(
                    &manifest,
                    &resources,
                    PackOperationOptions {
                        installed_by,
                        adoptions,
                        preserve_seed_targets,
                        target_overrides,
                        assessment_digest: None,
                        allow_downgrade: false,
                    },
                )
                .map_err(command_error)?,
            },
            PackAction::Apply {
                manifest,
                resources,
                installed_by,
                assessment_digest,
                allow_downgrade,
                adoptions,
                preserve_seed_targets,
                target_overrides,
            } => PortableInvocation {
                operation: "apply_type_pack",
                input: pack_operation_input(
                    &manifest,
                    &resources,
                    PackOperationOptions {
                        installed_by,
                        adoptions,
                        preserve_seed_targets,
                        target_overrides,
                        assessment_digest: Some(assessment_digest),
                        allow_downgrade,
                    },
                )
                .map_err(command_error)?,
            },
        },
        Command::Validate { path } => PortableInvocation {
            operation: "validate",
            input: path
                .map(normalized_path)
                .transpose()
                .map_err(command_error)?
                .map_or_else(|| json!({}), |path| json!({"path": path})),
        },
        Command::Init { .. }
        | Command::Backfill { .. }
        | Command::Migrate { .. }
        | Command::MigrateV02 { .. }
        | Command::Cache { .. }
        | Command::Watch { .. } => {
            return Err(CommandResult::diagnostic(
                json!({
                    "valid": false,
                    "result": {},
                    "diagnostics": [{
                        "severity": "error",
                        "code": "unsupported_target",
                        "message": "This command requires direct filesystem access; use --root.",
                    }]
                }),
                EXIT_GENERAL_ERROR,
            ));
        }
    };
    Ok(invocation)
}

fn is_portable(command: &Command) -> bool {
    matches!(
        command,
        Command::Read { .. }
            | Command::Create { .. }
            | Command::Update { .. }
            | Command::Delete { .. }
            | Command::Rename { .. }
            | Command::Query { .. }
            | Command::Batch { .. }
            | Command::Views { .. }
            | Command::Types { .. }
            | Command::Packs { .. }
            | Command::Validate { .. }
    )
}

fn insert_option(
    map: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: Option<String>,
) {
    if let Some(value) = value {
        map.insert(key.to_string(), serde_json::Value::String(value));
    }
}

fn normalized_path(path: String) -> MdbaseResult<String> {
    Ok(CollectionPath::new(path)?.to_string())
}

fn command_error(error: MdbaseError) -> CommandResult {
    let (value, exit_code) = typed_error_result(error);
    CommandResult::diagnostic(value, exit_code)
}

// Exit codes per spec Appendix C.9
const EXIT_SUCCESS: i32 = 0;
const EXIT_GENERAL_ERROR: i32 = 1;
const EXIT_VALIDATION_ERROR: i32 = 2;
const EXIT_CONFIG_ERROR: i32 = 3;
const EXIT_NOT_FOUND: i32 = 4;
#[allow(dead_code)]
const EXIT_PERMISSION_DENIED: i32 = 5;

/// Result of executing one data command.
#[derive(Debug)]
pub struct CommandResult {
    pub value: serde_json::Value,
    pub exit_code: i32,
    pub diagnostic: bool,
}

impl CommandResult {
    fn output(value: serde_json::Value, exit_code: i32) -> Self {
        Self {
            value,
            exit_code,
            diagnostic: false,
        }
    }

    fn diagnostic(value: serde_json::Value, exit_code: i32) -> Self {
        Self {
            value,
            exit_code,
            diagnostic: true,
        }
    }
}

/// Execute a command against a filesystem collection without terminating the
/// host process. The unified CLI owns final rendering and process exit.
pub fn execute_direct(root: &std::path::Path, command: Command) -> CommandResult {
    let command = match command {
        Command::Init {
            config,
            config_file,
        } => {
            let config_value = match (config, config_file) {
                (Some(c), None) => Some(c),
                (None, Some(path)) => match std::fs::read_to_string(&path) {
                    Ok(content) => Some(content),
                    Err(e) => {
                        return CommandResult::diagnostic(
                            serde_json::json!({
                                "error": {
                                    "code": "invalid_path",
                                    "message": format!(
                                        "Failed to read --config-file '{}': {}",
                                        path, e
                                    ),
                                }
                            }),
                            EXIT_GENERAL_ERROR,
                        );
                    }
                },
                (None, None) => None,
                (Some(_), Some(_)) => unreachable!(),
            };

            let mut input = serde_json::Map::new();
            if let Some(cfg) = config_value {
                input.insert("config".to_string(), serde_json::Value::String(cfg));
            }
            let result = mdbase::init::init_collection(root, &serde_json::Value::Object(input));
            let exit_code = if result.get("error").is_some() {
                error_to_exit_code(&result)
            } else {
                EXIT_SUCCESS
            };
            return CommandResult::output(result, exit_code);
        }
        command => command,
    };

    let collection = match mdbase::Collection::open(root) {
        Ok(c) => c,
        Err(e) => {
            return CommandResult::diagnostic(e, EXIT_CONFIG_ERROR);
        }
    };

    if collection.spec_profile() == mdbase::SpecProfile::V03 && is_portable(&command) {
        let invocation = match into_portable(command.clone()) {
            Ok(invocation) => invocation,
            Err(result) => return result,
        };
        if matches!(invocation.operation, "assess_type_pack" | "apply_type_pack") {
            let result = execute_direct_pack(&collection, &invocation);
            let value = serde_json::to_value(result)
                .expect("the type-pack operation result must serialize");
            let exit_code = portable_exit_code(&value);
            return CommandResult::output(value, exit_code);
        }
        let operations = match collection.v03_operations() {
            Ok(operations) => operations,
            Err(diagnostic) => {
                let value = serde_json::json!({
                    "valid": false,
                    "result": {},
                    "diagnostics": [*diagnostic],
                });
                return CommandResult::output(value, EXIT_GENERAL_ERROR);
            }
        };
        let result = match invocation.operation {
            "read" => operations.read(&invocation.input),
            "query" => operations.query(&invocation.input),
            "list_views" => operations.list_views(&invocation.input),
            "execute_view" => operations.execute_view(&invocation.input),
            "list_types" => operations.list_types(&invocation.input),
            "read_type" => operations.read_type(&invocation.input),
            "create_type" => operations.create_type(&invocation.input),
            "update_type" => operations.update_type(&invocation.input),
            "validate" => operations.validate(&invocation.input),
            "batch" => operations.batch(&invocation.input),
            "create" => operations.create(&invocation.input),
            "update" => operations.update(&invocation.input),
            "delete" => operations.delete(&invocation.input),
            "rename" => operations.rename(&invocation.input),
            operation => {
                let value = serde_json::json!({
                    "valid": false,
                    "result": {},
                    "diagnostics": [{
                        "severity": "error",
                        "code": "unsupported_operation",
                        "message": format!("Unsupported portable operation '{operation}'."),
                    }],
                });
                return CommandResult::output(value, EXIT_GENERAL_ERROR);
            }
        };
        let value = serde_json::to_value(result)
            .expect("the canonical portable operation result must serialize");
        let exit_code = portable_exit_code(&value);
        return CommandResult::output(value, exit_code);
    }

    let (value, exit_code) = execute_command(&collection, command);
    CommandResult::output(value, exit_code)
}

fn execute_direct_pack(
    collection: &mdbase::Collection,
    invocation: &PortableInvocation,
) -> mdbase::v03::OperationResult {
    let provision = serde_json::from_value::<mdbase::v03::TypePackProvision>(
        invocation.input["provision"].clone(),
    )
    .expect("CLI pack input constructs a valid provision");
    let adoptions = serde_json::from_value(invocation.input["adopt_resources"].clone())
        .expect("CLI pack input constructs valid adoptions");
    let preserve_seed_targets =
        serde_json::from_value(invocation.input["preserve_seed_targets"].clone())
            .unwrap_or_default();
    let target_overrides =
        serde_json::from_value(invocation.input["target_overrides"].clone()).unwrap_or_default();
    let installed_by = invocation.input["installed_by"]
        .as_str()
        .expect("CLI pack input includes installed_by")
        .to_string();
    if invocation.operation == "assess_type_pack" {
        collection.assess_type_pack(
            &provision,
            &mdbase::v03::TypePackAssessmentOptions {
                installed_by,
                adopt_resources: adoptions,
                preserve_seed_targets,
                target_overrides,
                contract_setups: Vec::new(),
            },
        )
    } else {
        collection.apply_type_pack(
            &provision,
            &mdbase::v03::TypePackApplyOptions {
                installed_by,
                expected_assessment_digest: invocation.input["expected_assessment_digest"]
                    .as_str()
                    .expect("CLI apply input includes the assessment digest")
                    .to_string(),
                allow_downgrade: invocation.input["allow_downgrade"]
                    .as_bool()
                    .unwrap_or(false),
                adopt_resources: adoptions,
                preserve_seed_targets,
                target_overrides,
                contract_setups: Vec::new(),
            },
        )
    }
}

pub fn execute_args(args: DirectArgs) -> CommandResult {
    let root = args
        .root
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    execute_direct(&root, args.command)
}

/// Derive the stable CLI exit status from a portable operation result.
pub fn portable_exit_code(value: &serde_json::Value) -> i32 {
    if value.get("valid").and_then(serde_json::Value::as_bool) != Some(false) {
        return EXIT_SUCCESS;
    }
    value
        .pointer("/diagnostics/0/code")
        .and_then(serde_json::Value::as_str)
        .map(diagnostic_code_to_exit)
        .unwrap_or(EXIT_GENERAL_ERROR)
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
            let fields_value = match parse_fields_or_stdin(fields.as_deref()) {
                Ok(value) => value,
                Err(error) => return typed_error_result(error),
            };
            let request: MdbaseResult<CreateRequest> = (|| {
                let mut request = match path {
                    Some(path) => CreateRequest::new(CollectionPath::new(path)?)
                        .with_frontmatter(fields_value),
                    None => CreateRequest::derived().with_frontmatter(fields_value),
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
            let fields_value = match parse_fields_or_stdin(fields.as_deref()) {
                Ok(value) => value,
                Err(error) => return typed_error_result(error),
            };
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

        Command::Types { .. } => {
            typed_error_result(MdbaseError::MigrationRequired { operation: "types" })
        }

        Command::Packs { .. } => {
            typed_error_result(MdbaseError::MigrationRequired { operation: "packs" })
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

        Command::Watch { .. } => {
            let result = serde_json::json!({
                "valid": false,
                "result": {},
                "diagnostics": [{
                    "severity": "error",
                    "code": "streaming_command_required",
                    "message": "Watch must be run by a streaming CLI host.",
                }],
            });
            (result, EXIT_GENERAL_ERROR)
        }
    }
}

/// Stream payload-bearing collection events intentionally requested by the
/// local operator. Performance telemetry remains separate and payload-free.
pub fn run_watch(
    root: &std::path::Path,
    debounce_ms: u64,
    count: Option<usize>,
) -> Result<(), String> {
    if debounce_ms == 0 {
        return Err("--debounce-ms must be greater than zero".to_string());
    }
    if count == Some(0) {
        return Err("--count must be greater than zero".to_string());
    }
    let watcher =
        mdbase::watch::CollectionWatcher::open(root, std::time::Duration::from_millis(debounce_ms))
            .map_err(|error| error.to_string())?;
    let mut emitted = 0usize;
    loop {
        let event = watcher.recv_portable().map_err(|error| error.to_string())?;
        let rendered = serde_json::to_string(&event).map_err(|error| error.to_string())?;
        println!("{rendered}");
        std::io::stdout()
            .flush()
            .map_err(|error| error.to_string())?;
        emitted += 1;
        if count.is_some_and(|count| emitted >= count) {
            return Ok(());
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

fn read_text_input(source: &str) -> MdbaseResult<String> {
    if source == "-" {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).map_err(|error| {
            MdbaseError::InvalidRequest {
                message: format!("could not read document from stdin: {error}"),
            }
        })?;
        return Ok(input);
    }
    std::fs::read_to_string(source).map_err(|error| MdbaseError::InvalidRequest {
        message: format!("could not read document '{source}': {error}"),
    })
}

struct PackOperationOptions {
    installed_by: String,
    adoptions: Vec<String>,
    preserve_seed_targets: Vec<String>,
    target_overrides: Vec<String>,
    assessment_digest: Option<String>,
    allow_downgrade: bool,
}

fn pack_operation_input(
    manifest_path: &str,
    resources_root: &str,
    options: PackOperationOptions,
) -> MdbaseResult<serde_json::Value> {
    let source = read_text_input(manifest_path)?;
    let manifest = serde_yaml::from_str::<serde_json::Value>(&source).map_err(|error| {
        MdbaseError::InvalidRequest {
            message: format!("could not parse type-pack manifest '{manifest_path}': {error}"),
        }
    })?;
    let declarations = manifest
        .get("resources")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| MdbaseError::InvalidRequest {
            message: "type-pack manifest resources must be an array".to_string(),
        })?;
    let root = std::path::Path::new(resources_root);
    let mut resources = Vec::with_capacity(declarations.len());
    for declaration in declarations {
        let source = declaration
            .get("source")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| MdbaseError::InvalidRequest {
                message: "each type-pack resource requires source".to_string(),
            })?;
        let safe_source = CollectionPath::new(source)?;
        let path = root.join(safe_source.as_str());
        let document =
            std::fs::read_to_string(&path).map_err(|error| MdbaseError::InvalidRequest {
                message: format!(
                    "could not read type-pack resource '{}': {error}",
                    path.display()
                ),
            })?;
        resources.push(serde_json::json!({ "source": source, "document": document }));
    }
    let mut adopt_resources = serde_json::Map::new();
    for adoption in options.adoptions {
        let (target, digest) =
            adoption
                .split_once('=')
                .ok_or_else(|| MdbaseError::InvalidRequest {
                    message: format!("invalid --adopt '{adoption}'; expected TARGET=sha256:DIGEST"),
                })?;
        CollectionPath::new(target)?;
        if !is_sha256_digest(digest) {
            return Err(MdbaseError::InvalidRequest {
                message: format!("invalid adoption digest for '{target}'"),
            });
        }
        if adopt_resources
            .insert(
                target.to_string(),
                serde_json::Value::String(digest.to_string()),
            )
            .is_some()
        {
            return Err(MdbaseError::InvalidRequest {
                message: format!("duplicate adoption target '{target}'"),
            });
        }
    }
    let mut preserved_seeds = Vec::with_capacity(options.preserve_seed_targets.len());
    for target in options.preserve_seed_targets {
        let target = CollectionPath::new(target)?;
        preserved_seeds.push(target.as_str().to_string());
    }
    let mut resolved_targets = serde_json::Map::new();
    for target_override in options.target_overrides {
        let (source, target) = target_override.split_once('=').ok_or_else(|| {
            MdbaseError::InvalidRequest {
                message: format!(
                    "invalid --target '{target_override}'; expected MANIFEST_TARGET=COLLECTION_TARGET"
                ),
            }
        })?;
        let source = CollectionPath::new(source)?;
        let target = CollectionPath::new(target)?;
        if resolved_targets
            .insert(
                source.as_str().to_string(),
                serde_json::Value::String(target.as_str().to_string()),
            )
            .is_some()
        {
            return Err(MdbaseError::InvalidRequest {
                message: format!("duplicate target override '{}'", source.as_str()),
            });
        }
    }
    let mut input = serde_json::json!({
        "provision": { "manifest": manifest, "resources": resources, "provides": [] },
        "installed_by": options.installed_by,
        "adopt_resources": adopt_resources,
        "preserve_seed_targets": preserved_seeds,
        "target_overrides": resolved_targets,
    });
    if let Some(digest) = options.assessment_digest {
        input["expected_assessment_digest"] = serde_json::Value::String(digest);
        input["allow_downgrade"] = serde_json::Value::Bool(options.allow_downgrade);
    }
    Ok(input)
}

fn is_sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[allow(clippy::too_many_arguments)]
fn portable_query(
    request_file: Option<&str>,
    types: Option<String>,
    where_clause: Option<String>,
    folder: Option<String>,
    order_by: Option<String>,
    limit: Option<u64>,
    offset: Option<u64>,
    include_body: bool,
) -> MdbaseResult<serde_json::Value> {
    let request = if let Some(request_file) = request_file {
        parse_json_input::<QueryRequest>(request_file)?
    } else {
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
            let child = serde_json::to_string(&format!("{folder}/")).expect("string serializes");
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
        request
    };
    Ok(request.to_wire())
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
fn parse_fields_or_stdin(fields_arg: Option<&str>) -> MdbaseResult<serde_json::Value> {
    if let Some(fields_str) = fields_arg {
        serde_json::from_str(fields_str).map_err(|error| MdbaseError::InvalidRequest {
            message: format!("could not parse --fields JSON: {error}"),
        })
    } else {
        // Try reading from stdin if not a TTY
        if !atty_check_stdin() {
            let mut input = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).map_err(|error| {
                MdbaseError::InvalidRequest {
                    message: format!("could not read fields JSON from stdin: {error}"),
                }
            })?;
            if !input.trim().is_empty() {
                return serde_json::from_str(&input).map_err(|error| MdbaseError::InvalidRequest {
                    message: format!("could not parse stdin fields JSON: {error}"),
                });
            }
        }
        Ok(serde_json::json!({}))
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

/// Check if stdin is a TTY.
fn atty_check_stdin() -> bool {
    std::io::stdin().is_terminal()
}
