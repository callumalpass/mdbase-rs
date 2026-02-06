use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand};

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
    },

    /// Update an existing file
    Update {
        /// File path
        path: String,

        /// Fields as JSON string
        #[arg(long)]
        fields: Option<String>,
    },

    /// Delete a file
    Delete {
        /// File path
        path: String,

        /// Check for backlinks before deleting
        #[arg(long)]
        check_backlinks: bool,
    },

    /// Rename or move a file
    Rename {
        /// Source path
        from: String,

        /// Destination path
        to: String,

        /// Update references in other files
        #[arg(long)]
        update_refs: bool,
    },

    /// Query the collection
    Query {
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

    /// Validate a file or the entire collection
    Validate {
        /// File path (omit for collection-wide validation)
        path: Option<String>,
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

    let root = cli.root.unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    });

    let is_tty = atty_check();
    let pretty = cli.pretty || is_tty;

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
        Command::Read { path } => {
            let input = serde_json::json!({ "path": path });
            let result = collection.read(&input);
            let exit = if result.get("error").is_some() {
                error_to_exit_code(&result)
            } else {
                EXIT_SUCCESS
            };
            (result, exit)
        }

        Command::Create { path, file_type, fields } => {
            let fields_value = parse_fields_or_stdin(fields.as_deref());
            let mut input = serde_json::json!({ "fields": fields_value });
            if let Some(p) = path {
                input["path"] = serde_json::Value::String(p);
            }
            if let Some(t) = file_type {
                input["type"] = serde_json::Value::String(t);
            }
            let result = collection.create(&input);
            let exit = if result.get("error").is_some() {
                error_to_exit_code(&result)
            } else {
                EXIT_SUCCESS
            };
            (result, exit)
        }

        Command::Update { path, fields } => {
            let fields_value = parse_fields_or_stdin(fields.as_deref());
            let input = serde_json::json!({ "path": path, "fields": fields_value });
            let result = collection.update(&input);
            let exit = if result.get("error").is_some() {
                error_to_exit_code(&result)
            } else {
                EXIT_SUCCESS
            };
            (result, exit)
        }

        Command::Delete { path, check_backlinks } => {
            let mut input = serde_json::json!({ "path": path });
            if check_backlinks {
                input["check_backlinks"] = serde_json::Value::Bool(true);
            }
            let result = collection.delete(&input);
            let exit = if result.get("error").is_some() {
                error_to_exit_code(&result)
            } else {
                EXIT_SUCCESS
            };
            (result, exit)
        }

        Command::Rename { from, to, update_refs } => {
            let input = serde_json::json!({
                "from": from,
                "to": to,
                "update_refs": update_refs,
            });
            let result = collection.rename(&input);
            let exit = if result.get("error").is_some() {
                error_to_exit_code(&result)
            } else {
                EXIT_SUCCESS
            };
            (result, exit)
        }

        Command::Query { types, where_clause, folder, order_by, limit, offset, include_body } => {
            let mut query = serde_json::Map::new();

            if let Some(t) = types {
                let type_list: Vec<serde_json::Value> = t.split(',')
                    .map(|s| serde_json::Value::String(s.trim().to_string()))
                    .collect();
                query.insert("types".to_string(), serde_json::Value::Array(type_list));
            }
            if let Some(w) = where_clause {
                query.insert("where".to_string(), serde_json::Value::String(w));
            }
            if let Some(f) = folder {
                query.insert("folder".to_string(), serde_json::Value::String(f));
            }
            if let Some(ob) = order_by {
                let order_list: Vec<serde_json::Value> = ob.split(',')
                    .map(|s| {
                        let s = s.trim();
                        if let Some(field) = s.strip_prefix('-') {
                            serde_json::json!({ "field": field, "direction": "desc" })
                        } else {
                            serde_json::json!({ "field": s, "direction": "asc" })
                        }
                    })
                    .collect();
                query.insert("order_by".to_string(), serde_json::Value::Array(order_list));
            }
            if let Some(l) = limit {
                query.insert("limit".to_string(), serde_json::json!(l));
            }
            if let Some(o) = offset {
                query.insert("offset".to_string(), serde_json::json!(o));
            }
            if include_body {
                query.insert("include_body".to_string(), serde_json::Value::Bool(true));
            }

            let input = serde_json::json!({ "query": query });
            let result = collection.query(&input);
            let exit = if result.get("error").is_some() {
                error_to_exit_code(&result)
            } else {
                EXIT_SUCCESS
            };
            (result, exit)
        }

        Command::Validate { path } => {
            let input = if let Some(p) = path {
                serde_json::json!({ "path": p })
            } else {
                serde_json::json!({})
            };
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

        Command::Cache { action } => {
            let result = match action {
                CacheAction::Status => {
                    let cache_dir = collection.root.join(&collection.settings.cache_folder);
                    let db_path = cache_dir.join("cache.db");
                    serde_json::json!({
                        "cache_folder": collection.settings.cache_folder,
                        "database_exists": db_path.exists(),
                        "database_path": db_path.to_string_lossy(),
                    })
                }
                CacheAction::Rebuild => collection.cache_rebuild(),
                CacheAction::Clear => collection.cache_clear(),
            };
            (result, EXIT_SUCCESS)
        }
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
            if std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).is_ok() && !input.trim().is_empty() {
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
    let code = result.pointer("/error/code")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match code {
        "file_not_found" | "not_found" => EXIT_NOT_FOUND,
        "validation_failed" => EXIT_VALIDATION_ERROR,
        "invalid_config" | "config_error" | "invalid_type_definition"
            | "circular_inheritance" | "missing_parent_type" => EXIT_CONFIG_ERROR,
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
    unsafe { libc_isatty(1) != 0 }
}

/// Check if stdin is a TTY.
fn atty_check_stdin() -> bool {
    unsafe { libc_isatty(0) != 0 }
}

extern "C" {
    #[link_name = "isatty"]
    fn libc_isatty(fd: i32) -> i32;
}
