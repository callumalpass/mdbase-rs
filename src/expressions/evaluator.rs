//! AST → value (Rust evaluator).

use super::ast::*;
use chrono::{FixedOffset, Local, Utc};
use serde_json::Value;
use std::cell::{Cell, RefCell};

/// Error types from expression evaluation.
#[derive(Debug)]
pub struct EvalError {
    pub code: String,
    pub message: String,
}

impl EvalError {
    fn type_error(msg: &str) -> Self {
        EvalError {
            code: "type_error".to_string(),
            message: msg.to_string(),
        }
    }
    fn invalid_expression(msg: &str) -> Self {
        EvalError {
            code: "invalid_expression".to_string(),
            message: msg.to_string(),
        }
    }
    fn wrong_argument_count(msg: &str) -> Self {
        EvalError {
            code: "wrong_argument_count".to_string(),
            message: msg.to_string(),
        }
    }
    fn unknown_function(msg: &str) -> Self {
        EvalError {
            code: "unknown_function".to_string(),
            message: msg.to_string(),
        }
    }
    fn expression_depth_exceeded() -> Self {
        EvalError {
            code: "expression_depth_exceeded".to_string(),
            message: "Expression nesting depth limit exceeded".to_string(),
        }
    }
    fn expression_work_exceeded(limit: u64) -> Self {
        EvalError {
            code: "expression_work_exceeded".to_string(),
            message: format!("Expression evaluation exceeded the {limit}-step work limit"),
        }
    }
}

const MAX_EVAL_DEPTH: u32 = 64;

thread_local! {
    static EVAL_DEPTH: Cell<u32> = const { Cell::new(0) };
    static EVAL_LIMIT: Cell<u32> = const { Cell::new(MAX_EVAL_DEPTH) };
    static EVAL_WORK: Cell<u64> = const { Cell::new(0) };
    static EVAL_WORK_LIMIT: Cell<u64> = const { Cell::new(u64::MAX) };
    static EVAL_CLOCK: RefCell<Option<EvaluationClock>> = const { RefCell::new(None) };
}

/// A time snapshot shared by every expression in one logical operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvaluationClock {
    now: String,
    today: String,
}

impl EvaluationClock {
    /// Capture UTC `now()` and timezone-aware `today()` values once.
    pub fn capture(timezone: Option<&str>) -> Result<Self, String> {
        let now = Utc::now();
        let today = match timezone.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("local") => now.with_timezone(&Local).date_naive(),
            Some("UTC" | "utc" | "Z") => now.date_naive(),
            Some(value) if value.starts_with('+') || value.starts_with('-') => {
                let offset = parse_fixed_offset(value)
                    .ok_or_else(|| format!("Invalid fixed timezone offset '{value}'"))?;
                now.with_timezone(&offset).date_naive()
            }
            Some(value) => {
                let timezone = value
                    .parse::<chrono_tz::Tz>()
                    .map_err(|_| format!("Unknown IANA timezone '{value}'"))?;
                now.with_timezone(&timezone).date_naive()
            }
        };
        Ok(Self {
            now: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            today: today.format("%Y-%m-%d").to_string(),
        })
    }

    pub fn now(&self) -> &str {
        &self.now
    }

    pub fn today(&self) -> &str {
        &self.today
    }
}

fn parse_fixed_offset(value: &str) -> Option<FixedOffset> {
    let sign = match value.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (hours, minutes) = value.get(1..)?.split_once(':')?;
    let seconds = sign * (hours.parse::<i32>().ok()? * 3600 + minutes.parse::<i32>().ok()? * 60);
    FixedOffset::east_opt(seconds)
}

/// Resolved file data for asFile() traversal
#[derive(Clone, Debug)]
pub struct ResolvedFileData {
    pub path: String,
    pub frontmatter: Value,
    pub body: String,
}

/// Selects which frontmatter view backs the `note` namespace.
///
/// The legacy expression profile exposes persisted frontmatter through
/// `note`; the v0.3 CEL host defines `note` as an alias for `record`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NoteNamespaceSource {
    #[default]
    Raw,
    Effective,
}

/// Context for expression evaluation.
#[derive(Clone)]
pub struct EvalContext {
    pub frontmatter: Value,
    pub raw_frontmatter: Option<Value>,
    pub file_path: Option<String>,
    pub body: Option<String>,
    pub file_size: Option<u64>,
    pub file_mtime: Option<String>,
    pub file_ctime: Option<String>,
    /// Context for 'this' keyword (the containing file's data in embedded queries)
    pub this_context: Option<Box<EvalContext>>,
    /// All files data for asFile() traversal. Key is basename (without extension).
    pub all_files: Option<std::sync::Arc<Vec<ResolvedFileData>>>,
    /// Current asFile() traversal depth (for depth limit enforcement).
    pub traversal_depth: std::cell::Cell<u32>,
    /// Backlinks index: target path → list of source paths that link to it.
    pub backlinks_index: Option<std::sync::Arc<std::collections::HashMap<String, Vec<String>>>>,
    /// Type names for this file (for display_name_key lookup).
    pub type_names: Option<Vec<String>>,
    /// Types map for display_name_key lookup.
    pub types:
        Option<std::sync::Arc<std::collections::HashMap<String, crate::types::schema::TypeDef>>>,
    pub note_namespace_source: NoteNamespaceSource,
    /// Whether string + number should concatenate (true in formulas) or return type error (false in where clauses).
    pub string_concat: bool,
}

impl EvalContext {
    pub fn empty() -> Self {
        EvalContext {
            frontmatter: Value::Object(serde_json::Map::new()),
            raw_frontmatter: None,
            file_path: None,
            body: None,
            file_size: None,
            file_mtime: None,
            file_ctime: None,
            this_context: None,
            all_files: None,
            traversal_depth: std::cell::Cell::new(0),
            backlinks_index: None,
            type_names: None,
            types: None,
            note_namespace_source: NoteNamespaceSource::Raw,
            string_concat: true,
        }
    }
}

/// Evaluate a parsed expression in a given context.
pub fn evaluate(expr: &Expr, ctx: &EvalContext) -> Result<Value, EvalError> {
    EVAL_DEPTH.with(|d| {
        let depth = d.get();
        if EVAL_LIMIT.with(std::cell::Cell::get) < depth {
            return Err(EvalError::expression_depth_exceeded());
        }
        let work = EVAL_WORK.with(Cell::get);
        let work_limit = EVAL_WORK_LIMIT.with(Cell::get);
        if work >= work_limit {
            return Err(EvalError::expression_work_exceeded(work_limit));
        }
        EVAL_WORK.with(|counter| counter.set(work + 1));
        d.set(depth + 1);
        let result = evaluate_inner(expr, ctx);
        d.set(depth);
        result
    })
}

/// Evaluate with a profile-specific recursive depth limit.
///
/// Nested evaluator calls inherit the limit selected by the outer operation.
pub fn evaluate_with_limits(
    expr: &Expr,
    ctx: &EvalContext,
    max_depth: u32,
    max_work: u64,
    clock: &EvaluationClock,
) -> Result<Value, EvalError> {
    let is_outermost = EVAL_DEPTH.with(|depth| depth.get() == 0);
    if !is_outermost {
        return evaluate(expr, ctx);
    }
    let previous_depth_limit = EVAL_LIMIT.with(|limit| limit.replace(max_depth));
    let previous_work_limit = EVAL_WORK_LIMIT.with(|limit| limit.replace(max_work));
    let previous_work = EVAL_WORK.with(|work| work.replace(0));
    let previous_clock = EVAL_CLOCK.with(|value| value.replace(Some(clock.clone())));
    let result = evaluate(expr, ctx);
    EVAL_LIMIT.with(|limit| limit.set(previous_depth_limit));
    EVAL_WORK_LIMIT.with(|limit| limit.set(previous_work_limit));
    EVAL_WORK.with(|work| work.set(previous_work));
    EVAL_CLOCK.with(|value| value.replace(previous_clock));
    result
}

fn evaluate_inner(expr: &Expr, ctx: &EvalContext) -> Result<Value, EvalError> {
    match expr {
        Expr::Null => Ok(Value::Null),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Number(n) => {
            if n.fract() == 0.0 && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 {
                Ok(Value::Number((*n as i64).into()))
            } else {
                Ok(serde_json::Number::from_f64(*n)
                    .map(Value::Number)
                    .unwrap_or(Value::Null))
            }
        }
        Expr::Str(s) => Ok(Value::String(s.clone())),

        Expr::Ident(name) => {
            // Look up in frontmatter
            let val = ctx.frontmatter.get(name).cloned().unwrap_or(Value::Null);
            Ok(val)
        }
        Expr::Array(elements) => {
            let mut vals = Vec::new();
            for elem in elements {
                vals.push(evaluate(elem, ctx)?);
            }
            Ok(Value::Array(vals))
        }

        Expr::Dot(obj, field) => eval_dot(obj, field, ctx),
        Expr::Index(obj, idx) => eval_index(obj, idx, ctx),

        Expr::BinOp(left, op, right) => eval_binop(left, op, right, ctx),
        Expr::UnaryOp(op, expr) => eval_unary(op, expr, ctx),
        Expr::NullCoalesce(left, right) => {
            let lval = evaluate(left, ctx)?;
            if lval.is_null() {
                evaluate(right, ctx)
            } else {
                Ok(lval)
            }
        }

        Expr::Call(func, args) => eval_call(func, args, ctx),
        Expr::Conditional(cond, then_expr, else_expr) => {
            let c = evaluate(cond, ctx)?;
            if is_truthy(&c) {
                evaluate(then_expr, ctx)
            } else {
                evaluate(else_expr, ctx)
            }
        }
    }
}

fn eval_dot(obj_expr: &Expr, field: &str, ctx: &EvalContext) -> Result<Value, EvalError> {
    // Handle this.file.X (where obj_expr is Dot(Ident("this"), "file"))
    if let Expr::Dot(ref inner, ref inner_field) = obj_expr {
        if let Expr::Ident(ref name) = **inner {
            if name == "this" && inner_field == "file" {
                if let Some(ref this_ctx) = ctx.this_context {
                    return eval_file_property(field, this_ctx);
                }
                return Ok(Value::Null);
            }
        }
    }
    // Special case: file.name, file.path, etc.
    if let Expr::Ident(ref name) = obj_expr {
        if name == "file" {
            return eval_file_property(field, ctx);
        }
        // note.* namespace accesses raw frontmatter (pre-defaults)
        if name == "note" {
            if ctx.note_namespace_source == NoteNamespaceSource::Effective {
                let note = ctx.frontmatter.get("note").unwrap_or(&Value::Null);
                return Ok(note.get(field).cloned().unwrap_or(Value::Null));
            }
            let fm = ctx.raw_frontmatter.as_ref().unwrap_or(&ctx.frontmatter);
            return Ok(fm.get(field).cloned().unwrap_or(Value::Null));
        }
        // formula.* namespace
        if name == "formula" {
            return Ok(ctx
                .frontmatter
                .get("formula")
                .and_then(|f| f.get(field))
                .cloned()
                .unwrap_or(Value::Null));
        }
        // this.* namespace: access context file's properties
        if name == "this" {
            if let Some(ref this_ctx) = ctx.this_context {
                if field == "file" {
                    // Return a marker; actual resolution happens in this.file.X
                    return Ok(Value::Null);
                }
                // this.fieldName → access context file's frontmatter
                return Ok(this_ctx
                    .frontmatter
                    .get(field)
                    .cloned()
                    .unwrap_or(Value::Null));
            }
            return Ok(Value::Null);
        }
    }

    let obj = evaluate(obj_expr, ctx)?;
    match &obj {
        Value::Object(map) => Ok(map.get(field).cloned().unwrap_or(Value::Null)),
        Value::String(s) => {
            // String properties
            match field {
                "length" => Ok(Value::Number(s.len().into())),
                // Date component properties on date/datetime strings
                "year" | "month" | "day" | "hour" | "minute" | "second" | "dayOfWeek" => {
                    eval_date_component(s, field)
                }
                _ => Ok(Value::Null),
            }
        }
        Value::Array(arr) => match field {
            "length" => Ok(Value::Number(arr.len().into())),
            _ => Ok(Value::Null),
        },
        Value::Null => Ok(Value::Null),
        _ => Ok(Value::Null),
    }
}

fn eval_file_property(field: &str, ctx: &EvalContext) -> Result<Value, EvalError> {
    match field {
        "path" => Ok(ctx
            .file_path
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null)),
        "name" => {
            if let Some(ref p) = ctx.file_path {
                let name = std::path::Path::new(p)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                Ok(Value::String(name.to_string()))
            } else {
                Ok(Value::Null)
            }
        }
        "basename" => {
            if let Some(ref p) = ctx.file_path {
                let stem = std::path::Path::new(p)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                Ok(Value::String(stem.to_string()))
            } else {
                Ok(Value::Null)
            }
        }
        "ext" => {
            if let Some(ref p) = ctx.file_path {
                let ext = std::path::Path::new(p)
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                Ok(Value::String(ext.to_string()))
            } else {
                Ok(Value::Null)
            }
        }
        "folder" => {
            if let Some(ref p) = ctx.file_path {
                let folder = std::path::Path::new(p)
                    .parent()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                Ok(Value::String(folder.to_string()))
            } else {
                Ok(Value::Null)
            }
        }
        "body" => Ok(ctx.body.clone().map(Value::String).unwrap_or(Value::Null)),
        "properties" => {
            // file.properties returns the raw frontmatter object (pre-defaults)
            let fm = ctx.raw_frontmatter.as_ref().unwrap_or(&ctx.frontmatter);
            Ok(fm.clone())
        }
        "tags" => {
            // Extract tags from body + frontmatter tags field
            let body_tags = ctx
                .body
                .as_deref()
                .map(extract_tags_from_body)
                .unwrap_or_default();
            let mut all_tags = Vec::new();
            // Add frontmatter tags first (handle both array and string)
            if let Some(fm_tags_val) = ctx.frontmatter.get("tags") {
                if let Some(arr) = fm_tags_val.as_array() {
                    for t in arr {
                        if let Some(s) = t.as_str() {
                            all_tags.push(s.to_string());
                        }
                    }
                } else if let Some(s) = fm_tags_val.as_str() {
                    // Single string tag value
                    if !s.is_empty() {
                        all_tags.push(s.to_string());
                    }
                }
            }
            // Add body tags
            for t in body_tags {
                if !all_tags.contains(&t) {
                    all_tags.push(t);
                }
            }
            Ok(Value::Array(
                all_tags.into_iter().map(Value::String).collect(),
            ))
        }
        "links" => {
            let mut all_links: Vec<String> = Vec::new();
            // Extract links from frontmatter values (string fields containing [[...]] or link paths)
            if let Some(obj) = ctx.frontmatter.as_object() {
                for (key, val) in obj {
                    if key == "tags" || key == "type" || key == "types" {
                        continue;
                    }
                    extract_links_from_fm_value(val, &mut all_links);
                }
            }
            // Extract links from body
            let body_links = ctx
                .body
                .as_deref()
                .map(extract_links_from_body)
                .unwrap_or_default();
            for link in body_links {
                if !all_links.contains(&link) {
                    all_links.push(link);
                }
            }
            Ok(Value::Array(
                all_links.into_iter().map(Value::String).collect(),
            ))
        }
        "embeds" => {
            let embeds = ctx
                .body
                .as_deref()
                .map(extract_embeds_from_body)
                .unwrap_or_default();
            Ok(Value::Array(
                embeds.into_iter().map(Value::String).collect(),
            ))
        }
        "display_name" => {
            // file.display_name: use display_name_key from type if available, else file.basename
            if let Some(ref type_names) = ctx.type_names {
                for tn in type_names {
                    if let Some(ref types_map) = ctx.types {
                        if let Some(type_def) = types_map.get(tn) {
                            if let Some(ref key) = type_def.display_name_key {
                                if let Some(val) = ctx.frontmatter.get(key) {
                                    if let Some(s) = val.as_str() {
                                        if !s.is_empty() {
                                            return Ok(Value::String(s.to_string()));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Fallback to file.basename
            if let Some(ref p) = ctx.file_path {
                let stem = std::path::Path::new(p)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                Ok(Value::String(stem.to_string()))
            } else {
                Ok(Value::Null)
            }
        }
        "size" => Ok(ctx
            .file_size
            .map(|s| Value::Number(s.into()))
            .unwrap_or(Value::Null)),
        "mtime" => Ok(ctx
            .file_mtime
            .as_ref()
            .map(|s| Value::String(s.clone()))
            .unwrap_or(Value::Null)),
        "ctime" => Ok(ctx
            .file_ctime
            .as_ref()
            .map(|s| Value::String(s.clone()))
            .unwrap_or(Value::Null)),
        "backlinks" => {
            if let (Some(ref path), Some(ref bl_index)) = (&ctx.file_path, &ctx.backlinks_index) {
                let sources = bl_index.get(path).cloned().unwrap_or_default();
                // Return array of objects with file metadata
                let backlink_objects: Vec<Value> = sources
                    .into_iter()
                    .map(|source_path| {
                        let name = std::path::Path::new(&source_path)
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("")
                            .to_string();
                        let folder = std::path::Path::new(&source_path)
                            .parent()
                            .and_then(|p| p.to_str())
                            .unwrap_or("")
                            .to_string();
                        serde_json::json!({
                            "file": {
                                "path": source_path,
                                "name": name,
                                "folder": folder,
                            }
                        })
                    })
                    .collect();
                Ok(Value::Array(backlink_objects))
            } else if ctx.backlinks_index.is_some() {
                // Index available but no file path — return empty array
                Ok(Value::Array(Vec::new()))
            } else {
                Ok(Value::Null)
            }
        }
        _ => Ok(Value::Null),
    }
}

/// Evaluate file.hasProperty(), file.inFolder(), file.hasTag(), file.hasLink() as method calls
fn eval_file_method(method: &str, args: &[Expr], ctx: &EvalContext) -> Result<Value, EvalError> {
    match method {
        "hasProperty" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "hasProperty() requires 1 argument",
                ));
            }
            let prop_name = evaluate(&args[0], ctx)?;
            let prop_str = prop_name.as_str().unwrap_or("");
            // Check raw frontmatter (pre-defaults) if available, otherwise effective
            let fm = ctx.raw_frontmatter.as_ref().unwrap_or(&ctx.frontmatter);
            let has = fm.as_object().is_some_and(|m| m.contains_key(prop_str));
            Ok(Value::Bool(has))
        }
        "inFolder" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "inFolder() requires 1 argument",
                ));
            }
            let folder = evaluate(&args[0], ctx)?;
            let folder_str = folder.as_str().unwrap_or("");
            if let Some(ref p) = ctx.file_path {
                // Check if the file's path starts with the folder
                let in_folder = p.starts_with(folder_str)
                    && (p.len() == folder_str.len()
                        || p.as_bytes().get(folder_str.len()) == Some(&b'/'));
                Ok(Value::Bool(in_folder))
            } else {
                Ok(Value::Bool(false))
            }
        }
        "hasTag" => {
            if args.is_empty() {
                return Err(EvalError::wrong_argument_count(
                    "hasTag() requires at least 1 argument",
                ));
            }
            // Collect all tags from body + frontmatter
            let body_tags = ctx
                .body
                .as_deref()
                .map(extract_tags_from_body)
                .unwrap_or_default();
            let mut all_tags = Vec::new();
            if let Some(fm_tags_val) = ctx.frontmatter.get("tags") {
                if let Some(arr) = fm_tags_val.as_array() {
                    for t in arr {
                        if let Some(s) = t.as_str() {
                            all_tags.push(s.to_string());
                        }
                    }
                } else if let Some(s) = fm_tags_val.as_str() {
                    if !s.is_empty() {
                        all_tags.push(s.to_string());
                    }
                }
            }
            for t in body_tags {
                if !all_tags.contains(&t) {
                    all_tags.push(t);
                }
            }
            // hasTag returns true if ANY of the supplied arguments match (OR logic)
            // Supports prefix matching for nested tags (e.g. "project" matches "project/alpha")
            for arg_expr in args {
                let arg_val = evaluate(arg_expr, ctx)?;
                if let Some(tag_name) = arg_val.as_str() {
                    for existing_tag in &all_tags {
                        if existing_tag == tag_name
                            || existing_tag.starts_with(&format!("{}/", tag_name))
                        {
                            return Ok(Value::Bool(true));
                        }
                    }
                }
            }
            Ok(Value::Bool(false))
        }
        "hasLink" => {
            if args.is_empty() {
                return Err(EvalError::wrong_argument_count(
                    "hasLink() requires at least 1 argument",
                ));
            }
            // Collect all links (same as file.links)
            let mut all_links: Vec<String> = Vec::new();
            if let Some(obj) = ctx.frontmatter.as_object() {
                for (key, val) in obj {
                    if key == "tags" || key == "type" || key == "types" {
                        continue;
                    }
                    extract_links_from_fm_value(val, &mut all_links);
                }
            }
            let body_links = ctx
                .body
                .as_deref()
                .map(extract_links_from_body)
                .unwrap_or_default();
            for link in body_links {
                if !all_links.contains(&link) {
                    all_links.push(link);
                }
            }

            let arg_val = evaluate(&args[0], ctx)?;
            if let Some(link_target) = arg_val.as_str() {
                // Resolve wikilinks relative to file directory for matching
                let file_dir = ctx
                    .file_path
                    .as_deref()
                    .and_then(|p| std::path::Path::new(p).parent())
                    .and_then(|p| p.to_str())
                    .unwrap_or("");

                for link in &all_links {
                    // Direct match
                    if link == link_target {
                        return Ok(Value::Bool(true));
                    }
                    // Strip .md extension
                    let link_no_ext = link.strip_suffix(".md").unwrap_or(link);
                    let target_no_ext = link_target.strip_suffix(".md").unwrap_or(link_target);
                    if link_no_ext == target_no_ext {
                        return Ok(Value::Bool(true));
                    }
                    // Resolve relative to file directory
                    if !link.contains('/') && !file_dir.is_empty() {
                        let resolved = format!("{}/{}", file_dir, link);
                        if resolved == link_target || resolved == target_no_ext {
                            return Ok(Value::Bool(true));
                        }
                    }
                    // Check basename match
                    let link_basename = link.rsplit('/').next().unwrap_or(link);
                    let target_basename = link_target.rsplit('/').next().unwrap_or(link_target);
                    let link_basename_no_ext =
                        link_basename.strip_suffix(".md").unwrap_or(link_basename);
                    let target_basename_no_ext = target_basename
                        .strip_suffix(".md")
                        .unwrap_or(target_basename);
                    if link_basename_no_ext == target_basename_no_ext && link_target.contains('/') {
                        // Only match by basename if the target has a path and the resolved path matches
                        let resolved = if link.contains('/') {
                            link.to_string()
                        } else if !file_dir.is_empty() {
                            format!("{}/{}", file_dir, link)
                        } else {
                            link.to_string()
                        };
                        let resolved_no_ext = resolved.strip_suffix(".md").unwrap_or(&resolved);
                        if resolved_no_ext == target_no_ext {
                            return Ok(Value::Bool(true));
                        }
                    }
                }
            }
            Ok(Value::Bool(false))
        }
        _ => Err(EvalError::unknown_function(&format!(
            "Unknown file method: .{}()",
            method
        ))),
    }
}

fn eval_index(obj_expr: &Expr, idx_expr: &Expr, ctx: &EvalContext) -> Result<Value, EvalError> {
    // Special case: note["field"] accesses raw frontmatter
    if let Expr::Ident(ref name) = obj_expr {
        if name == "note" {
            let idx = evaluate(idx_expr, ctx)?;
            if let Some(key) = idx.as_str() {
                if ctx.note_namespace_source == NoteNamespaceSource::Effective {
                    let note = ctx.frontmatter.get("note").unwrap_or(&Value::Null);
                    return Ok(note.get(key).cloned().unwrap_or(Value::Null));
                }
                let fm = ctx.raw_frontmatter.as_ref().unwrap_or(&ctx.frontmatter);
                return Ok(fm.get(key).cloned().unwrap_or(Value::Null));
            }
        }
    }

    let obj = evaluate(obj_expr, ctx)?;
    let idx = evaluate(idx_expr, ctx)?;

    match &obj {
        Value::Array(arr) => {
            if let Some(i) = idx.as_i64() {
                let i = if i < 0 {
                    (arr.len() as i64 + i) as usize
                } else {
                    i as usize
                };
                Ok(arr.get(i).cloned().unwrap_or(Value::Null))
            } else {
                Ok(Value::Null)
            }
        }
        Value::Object(map) => {
            if let Some(key) = idx.as_str() {
                Ok(map.get(key).cloned().unwrap_or(Value::Null))
            } else {
                Ok(Value::Null)
            }
        }
        Value::String(s) => {
            if let Some(i) = idx.as_i64() {
                let i = if i < 0 {
                    (s.len() as i64 + i) as usize
                } else {
                    i as usize
                };
                Ok(s.chars()
                    .nth(i)
                    .map(|c| Value::String(c.to_string()))
                    .unwrap_or(Value::Null))
            } else {
                Ok(Value::Null)
            }
        }
        _ => Ok(Value::Null),
    }
}

fn eval_binop(
    left: &Expr,
    op: &BinOp,
    right: &Expr,
    ctx: &EvalContext,
) -> Result<Value, EvalError> {
    // Short-circuit for logical operators
    match op {
        BinOp::And => {
            let lval = evaluate(left, ctx)?;
            if !is_truthy(&lval) {
                return Ok(lval);
            }
            return evaluate(right, ctx);
        }
        BinOp::Or => {
            let lval = evaluate(left, ctx)?;
            if is_truthy(&lval) {
                return Ok(lval);
            }
            return evaluate(right, ctx);
        }
        _ => {}
    }

    let lval = evaluate(left, ctx)?;
    let rval = evaluate(right, ctx)?;

    match op {
        BinOp::Add => eval_add(&lval, &rval, ctx.string_concat),
        BinOp::Sub => eval_arithmetic(&lval, &rval, "-"),
        BinOp::Mul => eval_arithmetic(&lval, &rval, "*"),
        BinOp::Div => eval_arithmetic(&lval, &rval, "/"),
        BinOp::Mod => eval_arithmetic(&lval, &rval, "%"),
        BinOp::Eq => Ok(Value::Bool(values_equal(&lval, &rval))),
        BinOp::Neq => Ok(Value::Bool(!values_equal(&lval, &rval))),
        BinOp::Lt => Ok(Value::Bool(
            compare_values(&lval, &rval) == Some(std::cmp::Ordering::Less),
        )),
        BinOp::Gt => Ok(Value::Bool(
            compare_values(&lval, &rval) == Some(std::cmp::Ordering::Greater),
        )),
        BinOp::Lte => Ok(Value::Bool(matches!(
            compare_values(&lval, &rval),
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        ))),
        BinOp::Gte => Ok(Value::Bool(matches!(
            compare_values(&lval, &rval),
            Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        ))),
        BinOp::And | BinOp::Or => unreachable!(),
    }
}

fn value_to_concat_string(v: &Value) -> Result<String, EvalError> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.to_string())
            } else {
                Ok(n.as_f64().map(|f| f.to_string()).unwrap_or_default())
            }
        }
        Value::Bool(b) => Ok(b.to_string()),
        Value::Null => Ok("null".to_string()),
        _ => Err(EvalError::type_error(&format!(
            "Cannot concatenate {}",
            type_name(v)
        ))),
    }
}

fn eval_add(left: &Value, right: &Value, string_concat: bool) -> Result<Value, EvalError> {
    // Date + duration string arithmetic
    if let (Value::String(date_str), Value::String(dur_str)) = (left, right) {
        if is_date_string(date_str) || is_datetime_string(date_str) {
            if parse_duration_ms(dur_str).is_some() {
                if let Some(result) = add_duration_to_date(date_str, dur_str) {
                    return Ok(Value::String(result));
                }
            }
            // Date + non-duration string is a type error
            return Err(EvalError::type_error(
                "Cannot add date and non-duration string",
            ));
        }
    }
    // Duration + date (commutative)
    if let (Value::String(dur_str), Value::String(date_str)) = (left, right) {
        if (is_date_string(date_str) || is_datetime_string(date_str))
            && parse_duration_ms(dur_str).is_some()
        {
            if let Some(result) = add_duration_to_date(date_str, dur_str) {
                return Ok(Value::String(result));
            }
        }
    }
    // Array + anything is type_error
    if left.is_array() || right.is_array() {
        return Err(EvalError::type_error(&format!(
            "Cannot add {} and {}",
            type_name(left),
            type_name(right)
        )));
    }
    // String concatenation: if either side is a string, coerce the other to string
    // Only allowed in formula context (string_concat=true), not in where clauses
    if left.is_string() || right.is_string() {
        // String + String is always concatenation
        if left.is_string() && right.is_string() {
            let ls = value_to_concat_string(left)?;
            let rs = value_to_concat_string(right)?;
            return Ok(Value::String(format!("{}{}", ls, rs)));
        }
        // String + non-string: depends on context
        if string_concat {
            let ls = value_to_concat_string(left)?;
            let rs = value_to_concat_string(right)?;
            return Ok(Value::String(format!("{}{}", ls, rs)));
        }
        // In where context: type mismatch
        return Err(EvalError::type_error(&format!(
            "Cannot add {} and {}",
            type_name(left),
            type_name(right)
        )));
    }
    eval_arithmetic(left, right, "+")
}

fn eval_arithmetic(left: &Value, right: &Value, op: &str) -> Result<Value, EvalError> {
    // Date - duration arithmetic
    if op == "-" {
        if let (Value::String(date_str), Value::String(dur_str)) = (left, right) {
            if is_date_string(date_str) && parse_duration_ms(dur_str).is_some() {
                // Negate the duration and add
                if let Some(_ms) = parse_duration_ms(dur_str) {
                    let unit_start = dur_str
                        .trim()
                        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
                        .unwrap_or(dur_str.len());
                    let unit = &dur_str.trim()[unit_start..];
                    let num_str = &dur_str.trim()[..unit_start];
                    let num: f64 = num_str.parse().unwrap_or(0.0);
                    let neg_dur = format!("{}{}", -num, unit);
                    if let Some(result) = add_duration_to_date(date_str, &neg_dur) {
                        return Ok(Value::String(result));
                    }
                }
            }
            // Date - Date = milliseconds
            if is_date_string(date_str) && is_date_string(dur_str) {
                if let Some(diff_ms) = date_diff_ms(date_str, dur_str) {
                    return Ok(Value::Number(diff_ms.into()));
                }
            }
        }
    }
    // Null propagation: null op anything = null
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let ln = as_number(left).ok_or_else(|| {
        EvalError::type_error(&format!("Cannot apply '{}' to {}", op, type_name(left)))
    })?;
    let rn = as_number(right).ok_or_else(|| {
        EvalError::type_error(&format!("Cannot apply '{}' to {}", op, type_name(right)))
    })?;

    let result = match op {
        "+" => ln + rn,
        "-" => ln - rn,
        "*" => ln * rn,
        "/" => {
            if rn == 0.0 {
                return Ok(Value::Null);
            }
            ln / rn
        }
        "%" => {
            if rn == 0.0 {
                return Ok(Value::Null);
            }
            ln % rn
        }
        _ => {
            return Err(EvalError::invalid_expression(&format!(
                "Unknown op: {}",
                op
            )))
        }
    };

    // Return integer if both inputs were integers and result is integer
    if result.fract() == 0.0
        && result >= i64::MIN as f64
        && result <= i64::MAX as f64
        && left.is_i64()
        && right.is_i64()
    {
        Ok(Value::Number((result as i64).into()))
    } else {
        Ok(serde_json::Number::from_f64(result)
            .map(Value::Number)
            .unwrap_or(Value::Null))
    }
}

fn eval_unary(op: &UnaryOp, expr: &Expr, ctx: &EvalContext) -> Result<Value, EvalError> {
    let val = evaluate(expr, ctx)?;
    match op {
        UnaryOp::Not => Ok(Value::Bool(!is_truthy(&val))),
        UnaryOp::Neg => {
            let n =
                as_number(&val).ok_or_else(|| EvalError::type_error("Cannot negate non-number"))?;
            if n.fract() == 0.0 {
                Ok(Value::Number(((-n) as i64).into()))
            } else {
                Ok(serde_json::Number::from_f64(-n)
                    .map(Value::Number)
                    .unwrap_or(Value::Null))
            }
        }
    }
}

fn eval_call(func_expr: &Expr, args: &[Expr], ctx: &EvalContext) -> Result<Value, EvalError> {
    // Method calls: obj.method(args)
    if let Expr::Dot(obj, method) = func_expr {
        // Special case: file.hasProperty(), file.inFolder(), etc.
        if let Expr::Ident(ref name) = **obj {
            if name == "file" {
                return eval_file_method(method, args, ctx);
            }
        }
        return eval_method(obj, method, args, ctx);
    }

    // Free function calls: func(args)
    if let Expr::Ident(name) = func_expr {
        // Handle ext::name() calls
        if let Some(func_name) = name.strip_prefix("ext::") {
            if func_name.is_empty() {
                return Err(EvalError::invalid_expression("ext:: with no function name"));
            }
            return Err(EvalError::unknown_function(&format!(
                "Unknown extension function: {}()",
                name
            )));
        }
        return eval_function(name, args, ctx);
    }

    Err(EvalError::invalid_expression("Not a callable expression"))
}

fn eval_function(name: &str, args: &[Expr], ctx: &EvalContext) -> Result<Value, EvalError> {
    match name {
        "exists" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "exists() requires 1 argument",
                ));
            }
            // exists() checks if a field exists in frontmatter (even if null)
            if let Expr::Ident(ref field) = args[0] {
                let has = ctx
                    .frontmatter
                    .as_object()
                    .is_some_and(|m| m.contains_key(field));
                Ok(Value::Bool(has))
            } else {
                let val = evaluate(&args[0], ctx)?;
                Ok(Value::Bool(!val.is_null()))
            }
        }
        "default" => {
            if args.len() != 2 {
                return Err(EvalError::wrong_argument_count(
                    "default() requires 2 arguments",
                ));
            }
            let val = evaluate(&args[0], ctx)?;
            if val.is_null() {
                evaluate(&args[1], ctx)
            } else {
                Ok(val)
            }
        }
        "isTruthy" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "isTruthy() requires 1 argument",
                ));
            }
            let val = evaluate(&args[0], ctx)?;
            Ok(Value::Bool(is_truthy(&val)))
        }
        "number" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "number() requires 1 argument",
                ));
            }
            let val = evaluate(&args[0], ctx)?;
            match &val {
                Value::Number(_) => Ok(val),
                Value::String(s) => {
                    // Try parsing as number first
                    if let Ok(n) = s.parse::<f64>() {
                        if n.fract() == 0.0 {
                            return Ok(Value::Number((n as i64).into()));
                        } else {
                            return Ok(serde_json::Number::from_f64(n)
                                .map(Value::Number)
                                .unwrap_or(Value::Null));
                        }
                    }
                    // Try parsing as datetime → epoch milliseconds
                    if is_date_string(s) || is_datetime_string(s) {
                        if let Some(ms) = date_to_epoch_ms(s) {
                            return Ok(Value::Number(ms.into()));
                        }
                    }
                    Ok(Value::Null)
                }
                Value::Bool(b) => Ok(Value::Number(if *b { 1 } else { 0 }.into())),
                _ => Ok(Value::Null),
            }
        }
        "toString" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "toString() requires 1 argument",
                ));
            }
            let val = evaluate(&args[0], ctx)?;
            Ok(Value::String(value_to_string(&val)))
        }
        "if" => {
            if args.len() != 3 {
                return Err(EvalError::wrong_argument_count("if() requires 3 arguments"));
            }
            let cond = evaluate(&args[0], ctx)?;
            if is_truthy(&cond) {
                evaluate(&args[1], ctx)
            } else {
                evaluate(&args[2], ctx)
            }
        }
        "date" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "date() requires 1 argument",
                ));
            }
            let val = evaluate(&args[0], ctx)?;
            // Return the date string as-is
            Ok(val)
        }
        "today" => Ok(Value::String(EVAL_CLOCK.with(|clock| {
            clock
                .borrow()
                .as_ref()
                .map(|clock| clock.today.clone())
                .unwrap_or_else(|| Local::now().format("%Y-%m-%d").to_string())
        }))),
        "now" => Ok(Value::String(EVAL_CLOCK.with(|clock| {
            clock
                .borrow()
                .as_ref()
                .map(|clock| clock.now.clone())
                .unwrap_or_else(|| Utc::now().to_rfc3339())
        }))),
        "abs" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count("abs() requires 1 argument"));
            }
            let val = evaluate(&args[0], ctx)?;
            let n =
                as_number(&val).ok_or_else(|| EvalError::type_error("abs() requires a number"))?;
            if n.fract() == 0.0 {
                Ok(Value::Number((n.abs() as i64).into()))
            } else {
                Ok(serde_json::Number::from_f64(n.abs())
                    .map(Value::Number)
                    .unwrap_or(Value::Null))
            }
        }
        "min" => {
            if args.len() < 2 {
                return Err(EvalError::wrong_argument_count(
                    "min() requires at least 2 arguments",
                ));
            }
            let mut result = evaluate(&args[0], ctx)?;
            for arg in &args[1..] {
                let val = evaluate(arg, ctx)?;
                if compare_values(&val, &result) == Some(std::cmp::Ordering::Less) {
                    result = val;
                }
            }
            Ok(result)
        }
        "max" => {
            if args.len() < 2 {
                return Err(EvalError::wrong_argument_count(
                    "max() requires at least 2 arguments",
                ));
            }
            let mut result = evaluate(&args[0], ctx)?;
            for arg in &args[1..] {
                let val = evaluate(arg, ctx)?;
                if compare_values(&val, &result) == Some(std::cmp::Ordering::Greater) {
                    result = val;
                }
            }
            Ok(result)
        }
        "round" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "round() requires 1 argument",
                ));
            }
            let val = evaluate(&args[0], ctx)?;
            let n = as_number(&val)
                .ok_or_else(|| EvalError::type_error("round() requires a number"))?;
            Ok(Value::Number((n.round() as i64).into()))
        }
        "floor" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "floor() requires 1 argument",
                ));
            }
            let val = evaluate(&args[0], ctx)?;
            let n = as_number(&val)
                .ok_or_else(|| EvalError::type_error("floor() requires a number"))?;
            Ok(Value::Number((n.floor() as i64).into()))
        }
        "ceil" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "ceil() requires 1 argument",
                ));
            }
            let val = evaluate(&args[0], ctx)?;
            let n =
                as_number(&val).ok_or_else(|| EvalError::type_error("ceil() requires a number"))?;
            Ok(Value::Number((n.ceil() as i64).into()))
        }
        "length" | "upper" | "lower" | "trim" | "trimStart" | "trimEnd" | "isEmpty"
        | "contains" | "startsWith" | "endsWith" | "replace" | "split" | "slice" | "matches"
        | "reverse" | "repeat" | "join" | "unique" | "flat" | "sort" | "first" | "last"
        | "keys" | "values" => {
            // These are method-only, not free functions
            Err(EvalError::unknown_function(&format!(
                "{}() is a method, not a free function",
                name
            )))
        }
        "duration" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "duration() requires 1 argument",
                ));
            }
            let val = evaluate(&args[0], ctx)?;
            let s = value_to_string(&val);
            // Parse simple duration like "1h", "7d", etc. and return milliseconds
            match parse_duration_ms(&s) {
                Some(ms) => Ok(Value::Number(ms.into())),
                None => Err(EvalError::type_error(&format!("Invalid duration: {}", s))),
            }
        }
        "datetime" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "datetime() requires 1 argument",
                ));
            }
            let val = evaluate(&args[0], ctx)?;
            // Return the datetime string as-is (like date())
            Ok(val)
        }
        "list" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "list() requires 1 argument",
                ));
            }
            let val = evaluate(&args[0], ctx)?;
            match val {
                Value::Array(_) => Ok(val),
                _ => Ok(Value::Array(vec![val])),
            }
        }
        "link" => {
            // link("target") creates a link value (returns the string as-is)
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "link() requires 1 argument",
                ));
            }
            let val = evaluate(&args[0], ctx)?;
            Ok(val)
        }
        _ => Err(EvalError::unknown_function(&format!(
            "Unknown function: {}()",
            name
        ))),
    }
}

fn eval_method(
    obj_expr: &Expr,
    method: &str,
    args: &[Expr],
    ctx: &EvalContext,
) -> Result<Value, EvalError> {
    // Handle ext.name() and ext::name() as extension function calls
    if let Expr::Ident(ref name) = obj_expr {
        if name == "ext" || name.starts_with("ext::") {
            return Err(EvalError::unknown_function(&format!(
                "Unknown extension function: ext.{}()",
                method
            )));
        }
    }

    let obj = evaluate(obj_expr, ctx)?;

    match &obj {
        Value::String(s) => eval_string_method(s, method, args, ctx),
        Value::Array(arr) => eval_array_method(arr, method, args, ctx),
        Value::Object(map) => eval_object_method(map, method, args, ctx),
        Value::Null => {
            // Null-safe: some methods have special null behavior
            match method {
                "isEmpty" => Ok(Value::Bool(true)),
                "isTruthy" => Ok(Value::Bool(false)),
                "toString" => Ok(Value::String("null".to_string())),
                "isType" => {
                    if args.len() != 1 {
                        return Err(EvalError::wrong_argument_count(
                            "isType() requires 1 argument",
                        ));
                    }
                    let type_name_val = evaluate(&args[0], ctx)?;
                    Ok(Value::Bool(type_name_val.as_str() == Some("null")))
                }
                "length" => Ok(Value::Number(0.into())),
                _ => Ok(Value::Null),
            }
        }
        other => match method {
            "isEmpty" => Ok(Value::Bool(other.is_null())),
            "isType" => {
                if args.len() != 1 {
                    return Err(EvalError::wrong_argument_count(
                        "isType() requires 1 argument",
                    ));
                }
                let type_name_val = evaluate(&args[0], ctx)?;
                let type_str = type_name_val.as_str().unwrap_or("");
                let result = match other {
                    Value::Bool(_) => type_str == "boolean",
                    Value::Number(n) => {
                        type_str == "number" || (type_str == "integer" && n.is_i64())
                    }
                    _ => false,
                };
                Ok(Value::Bool(result))
            }
            "toString" => Ok(Value::String(value_to_string(other))),
            "isTruthy" => Ok(Value::Bool(is_truthy(other))),
            _ => Err(EvalError::unknown_function(&format!(
                "Unknown {} method: .{}()",
                type_name(other),
                method
            ))),
        },
    }
}

fn eval_string_method(
    s: &str,
    method: &str,
    args: &[Expr],
    ctx: &EvalContext,
) -> Result<Value, EvalError> {
    match method {
        "length" | "size" => {
            if !args.is_empty() {
                return Err(EvalError::wrong_argument_count(
                    "length()/size() takes no arguments",
                ));
            }
            Ok(Value::Number(s.len().into()))
        }
        "upper" => {
            if !args.is_empty() {
                return Err(EvalError::wrong_argument_count(
                    "upper() takes no arguments",
                ));
            }
            Ok(Value::String(s.to_uppercase()))
        }
        "lower" => {
            if !args.is_empty() {
                return Err(EvalError::wrong_argument_count(
                    "lower() takes no arguments",
                ));
            }
            Ok(Value::String(s.to_lowercase()))
        }
        "trim" => {
            if !args.is_empty() {
                return Err(EvalError::wrong_argument_count("trim() takes no arguments"));
            }
            Ok(Value::String(s.trim().to_string()))
        }
        "trimStart" => {
            if !args.is_empty() {
                return Err(EvalError::wrong_argument_count(
                    "trimStart() takes no arguments",
                ));
            }
            Ok(Value::String(s.trim_start().to_string()))
        }
        "trimEnd" => {
            if !args.is_empty() {
                return Err(EvalError::wrong_argument_count(
                    "trimEnd() takes no arguments",
                ));
            }
            Ok(Value::String(s.trim_end().to_string()))
        }
        "isEmpty" => {
            if !args.is_empty() {
                return Err(EvalError::wrong_argument_count(
                    "isEmpty() takes no arguments",
                ));
            }
            Ok(Value::Bool(s.is_empty()))
        }
        "contains" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "contains() requires 1 argument",
                ));
            }
            let needle = evaluate(&args[0], ctx)?;
            let needle_str = match needle.as_str() {
                Some(s) => s.to_string(),
                None => value_to_string(&needle),
            };
            Ok(Value::Bool(s.contains(&needle_str)))
        }
        "startsWith" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "startsWith() requires 1 argument",
                ));
            }
            let prefix = evaluate(&args[0], ctx)?;
            let prefix_str = match prefix.as_str() {
                Some(s) => s.to_string(),
                None => value_to_string(&prefix),
            };
            Ok(Value::Bool(s.starts_with(&prefix_str)))
        }
        "endsWith" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "endsWith() requires 1 argument",
                ));
            }
            let suffix = evaluate(&args[0], ctx)?;
            let suffix_str = match suffix.as_str() {
                Some(s) => s.to_string(),
                None => value_to_string(&suffix),
            };
            Ok(Value::Bool(s.ends_with(&suffix_str)))
        }
        "replace" => {
            if args.len() != 2 {
                return Err(EvalError::wrong_argument_count(
                    "replace() requires 2 arguments",
                ));
            }
            let from = evaluate(&args[0], ctx)?;
            let to = evaluate(&args[1], ctx)?;
            let from_str = match from.as_str() {
                Some(s) => s.to_string(),
                None => value_to_string(&from),
            };
            let to_str = match to.as_str() {
                Some(s) => s.to_string(),
                None => value_to_string(&to),
            };
            Ok(Value::String(s.replace(&from_str, &to_str)))
        }
        "split" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EvalError::wrong_argument_count(
                    "split() requires 1-2 arguments",
                ));
            }
            let sep = evaluate(&args[0], ctx)?;
            let sep_str = match sep.as_str() {
                Some(s) => s.to_string(),
                None => value_to_string(&sep),
            };
            let parts: Vec<Value> = if args.len() > 1 {
                let limit = evaluate(&args[1], ctx)?.as_u64().unwrap_or(0) as usize;
                s.splitn(limit, &sep_str)
                    .map(|p| Value::String(p.to_string()))
                    .collect()
            } else {
                s.split(&sep_str)
                    .map(|p| Value::String(p.to_string()))
                    .collect()
            };
            Ok(Value::Array(parts))
        }
        "toString" => Ok(Value::String(s.to_string())),
        "isTruthy" => Ok(Value::Bool(!s.is_empty())),
        "slice" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EvalError::wrong_argument_count(
                    "slice() requires 1-2 arguments",
                ));
            }
            let start = evaluate(&args[0], ctx)?.as_i64().unwrap_or(0);
            let chars: Vec<char> = s.chars().collect();
            let len = chars.len() as i64;
            let start = if start < 0 {
                (len + start).max(0) as usize
            } else {
                start.min(len) as usize
            };
            let end = if args.len() > 1 {
                let e = evaluate(&args[1], ctx)?.as_i64().unwrap_or(len);
                if e < 0 {
                    (len + e).max(0) as usize
                } else {
                    e.min(len) as usize
                }
            } else {
                len as usize
            };
            let result: String = chars[start..end.max(start)].iter().collect();
            Ok(Value::String(result))
        }
        "matches" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "matches() requires 1 argument",
                ));
            }
            let pattern = evaluate(&args[0], ctx)?;
            let pat_str = match pattern.as_str() {
                Some(s) => s.to_string(),
                None => value_to_string(&pattern),
            };
            match fancy_regex::Regex::new(&pat_str) {
                Ok(re) => Ok(Value::Bool(re.is_match(s).unwrap_or(false))),
                Err(_) => Err(EvalError::invalid_expression(&format!(
                    "Invalid regex: {}",
                    pat_str
                ))),
            }
        }
        "reverse" => Ok(Value::String(s.chars().rev().collect())),
        "repeat" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "repeat() requires 1 argument",
                ));
            }
            let n = evaluate(&args[0], ctx)?.as_u64().unwrap_or(0);
            Ok(Value::String(s.repeat(n as usize)))
        }
        "containsAll" => {
            for arg in args {
                let needle = evaluate(arg, ctx)?;
                let needle_str = match needle.as_str() {
                    Some(s) => s.to_string(),
                    None => value_to_string(&needle),
                };
                if !s.contains(&needle_str) {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        }
        "containsAny" => {
            for arg in args {
                let needle = evaluate(arg, ctx)?;
                let needle_str = match needle.as_str() {
                    Some(s) => s.to_string(),
                    None => value_to_string(&needle),
                };
                if s.contains(&needle_str) {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        }
        "title" => {
            if !args.is_empty() {
                return Err(EvalError::wrong_argument_count(
                    "title() takes no arguments",
                ));
            }
            let result = s
                .split_whitespace()
                .map(|word| {
                    let mut chars = word.chars();
                    match chars.next() {
                        Some(c) => {
                            let upper: String = c.to_uppercase().collect();
                            format!("{}{}", upper, chars.as_str().to_lowercase())
                        }
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            Ok(Value::String(result))
        }
        "isType" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "isType() requires 1 argument",
                ));
            }
            let type_name = evaluate(&args[0], ctx)?;
            let type_str = type_name.as_str().unwrap_or("");
            let result = match type_str {
                "string" => true,
                "date" => is_date_string(s),
                "datetime" => is_datetime_string(s),
                _ => false,
            };
            Ok(Value::Bool(result))
        }
        // Date/time component properties
        "year" | "month" | "day" | "hour" | "minute" | "second" | "dayOfWeek" => {
            eval_date_component(s, method)
        }
        "date" => {
            // .date() extracts date portion from datetime string
            if !args.is_empty() {
                return Err(EvalError::wrong_argument_count(
                    "date() takes no arguments as method",
                ));
            }
            if let Some(date_part) = s.split('T').next() {
                Ok(Value::String(date_part.to_string()))
            } else {
                Ok(Value::String(s.to_string()))
            }
        }
        "time" => {
            // .time() extracts time portion from datetime string
            if !args.is_empty() {
                return Err(EvalError::wrong_argument_count("time() takes no arguments"));
            }
            if let Some(time_part) = s.split('T').nth(1) {
                // Strip timezone suffix
                let time_clean = time_part
                    .trim_end_matches('Z')
                    .split('+')
                    .next()
                    .unwrap_or(time_part)
                    .split('-')
                    .next()
                    .unwrap_or(time_part);
                Ok(Value::String(time_clean.to_string()))
            } else {
                Ok(Value::Null)
            }
        }
        "format" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "format() requires 1 argument",
                ));
            }
            let fmt = evaluate(&args[0], ctx)?;
            let fmt_str = fmt.as_str().unwrap_or("");
            Ok(Value::String(format_date_string(s, fmt_str)))
        }
        "asFile" => resolve_as_file(s, ctx),
        _ => Err(EvalError::unknown_function(&format!(
            "Unknown string method: .{}()",
            method
        ))),
    }
}

fn eval_array_method(
    arr: &[Value],
    method: &str,
    args: &[Expr],
    ctx: &EvalContext,
) -> Result<Value, EvalError> {
    match method {
        "length" | "size" => {
            if !args.is_empty() {
                return Err(EvalError::wrong_argument_count(
                    "length()/size() takes no arguments",
                ));
            }
            Ok(Value::Number(arr.len().into()))
        }
        "isEmpty" => Ok(Value::Bool(arr.is_empty())),
        "contains" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "contains() requires 1 argument",
                ));
            }
            let needle = evaluate(&args[0], ctx)?;
            Ok(Value::Bool(
                arr.iter().any(|item| values_equal(item, &needle)),
            ))
        }
        "join" => {
            let sep = if !args.is_empty() {
                evaluate(&args[0], ctx)?.as_str().unwrap_or(",").to_string()
            } else {
                ",".to_string()
            };
            let strings: Vec<String> = arr.iter().map(value_to_string).collect();
            Ok(Value::String(strings.join(&sep)))
        }
        "reverse" => {
            let mut result = arr.to_vec();
            result.reverse();
            Ok(Value::Array(result))
        }
        "unique" => {
            let mut result = Vec::new();
            for item in arr {
                if !result.iter().any(|r| values_equal(r, item)) {
                    result.push(item.clone());
                }
            }
            Ok(Value::Array(result))
        }
        "flat" => {
            let mut result = Vec::new();
            for item in arr {
                if let Value::Array(inner) = item {
                    result.extend(inner.iter().cloned());
                } else {
                    result.push(item.clone());
                }
            }
            Ok(Value::Array(result))
        }
        "slice" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EvalError::wrong_argument_count(
                    "slice() requires 1-2 arguments",
                ));
            }
            let start = evaluate(&args[0], ctx)?.as_i64().unwrap_or(0);
            let len = arr.len() as i64;
            let start = if start < 0 {
                (len + start).max(0) as usize
            } else {
                start.min(len) as usize
            };
            let end = if args.len() > 1 {
                let e = evaluate(&args[1], ctx)?.as_i64().unwrap_or(len);
                if e < 0 {
                    (len + e).max(0) as usize
                } else {
                    e.min(len) as usize
                }
            } else {
                len as usize
            };
            Ok(Value::Array(arr[start..end.max(start)].to_vec()))
        }
        "sort" => {
            let mut result = arr.to_vec();
            result.sort_by(|a, b| compare_values(a, b).unwrap_or(std::cmp::Ordering::Equal));
            Ok(Value::Array(result))
        }
        "first" => Ok(arr.first().cloned().unwrap_or(Value::Null)),
        "last" => Ok(arr.last().cloned().unwrap_or(Value::Null)),
        "isType" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "isType() requires 1 argument",
                ));
            }
            let type_name = evaluate(&args[0], ctx)?;
            Ok(Value::Bool(
                type_name.as_str() == Some("list") || type_name.as_str() == Some("array"),
            ))
        }
        "containsAll" => {
            for arg in args {
                let needle = evaluate(arg, ctx)?;
                if !arr.iter().any(|item| values_equal(item, &needle)) {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        }
        "containsAny" => {
            for arg in args {
                let needle = evaluate(arg, ctx)?;
                if arr.iter().any(|item| values_equal(item, &needle)) {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        }
        "filter" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "filter() requires 1 argument",
                ));
            }
            let mut result = Vec::new();
            for (i, item) in arr.iter().enumerate() {
                // Create context where "value" is the current item and "index" is the index
                let mut item_fm = ctx.frontmatter.clone();
                if let Value::Object(ref mut map) = item_fm {
                    map.insert("value".to_string(), item.clone());
                    map.insert("index".to_string(), Value::Number(i.into()));
                }
                let item_ctx = EvalContext {
                    frontmatter: item_fm,
                    raw_frontmatter: ctx.raw_frontmatter.clone(),
                    file_path: ctx.file_path.clone(),
                    body: ctx.body.clone(),
                    file_size: ctx.file_size,
                    file_mtime: ctx.file_mtime.clone(),
                    file_ctime: ctx.file_ctime.clone(),
                    this_context: ctx.this_context.clone(),
                    all_files: ctx.all_files.clone(),
                    traversal_depth: ctx.traversal_depth.clone(),
                    backlinks_index: ctx.backlinks_index.clone(),
                    type_names: ctx.type_names.clone(),
                    types: ctx.types.clone(),
                    note_namespace_source: ctx.note_namespace_source,
                    string_concat: ctx.string_concat,
                };
                if let Ok(val) = evaluate(&args[0], &item_ctx) {
                    if is_truthy(&val) {
                        result.push(item.clone());
                    }
                }
            }
            Ok(Value::Array(result))
        }
        "map" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count("map() requires 1 argument"));
            }
            let mut result = Vec::new();
            for (i, item) in arr.iter().enumerate() {
                let mut item_fm = ctx.frontmatter.clone();
                if let Value::Object(ref mut map) = item_fm {
                    map.insert("value".to_string(), item.clone());
                    map.insert("index".to_string(), Value::Number(i.into()));
                }
                let item_ctx = EvalContext {
                    frontmatter: item_fm,
                    raw_frontmatter: ctx.raw_frontmatter.clone(),
                    file_path: ctx.file_path.clone(),
                    body: ctx.body.clone(),
                    file_size: ctx.file_size,
                    file_mtime: ctx.file_mtime.clone(),
                    file_ctime: ctx.file_ctime.clone(),
                    this_context: ctx.this_context.clone(),
                    all_files: ctx.all_files.clone(),
                    traversal_depth: ctx.traversal_depth.clone(),
                    backlinks_index: ctx.backlinks_index.clone(),
                    type_names: ctx.type_names.clone(),
                    types: ctx.types.clone(),
                    note_namespace_source: ctx.note_namespace_source,
                    string_concat: ctx.string_concat,
                };
                match evaluate(&args[0], &item_ctx) {
                    Ok(val) => result.push(val),
                    Err(_) => result.push(Value::Null),
                }
            }
            Ok(Value::Array(result))
        }
        "reduce" => {
            if args.len() != 2 {
                return Err(EvalError::wrong_argument_count(
                    "reduce() requires 2 arguments",
                ));
            }
            let mut acc = if args.len() > 1 {
                evaluate(&args[1], ctx)?
            } else {
                Value::Null
            };
            for item in arr {
                let mut item_fm = ctx.frontmatter.clone();
                if let Value::Object(ref mut map) = item_fm {
                    map.insert("value".to_string(), item.clone());
                    map.insert("acc".to_string(), acc.clone());
                }
                let item_ctx = EvalContext {
                    frontmatter: item_fm,
                    raw_frontmatter: ctx.raw_frontmatter.clone(),
                    file_path: ctx.file_path.clone(),
                    body: ctx.body.clone(),
                    file_size: ctx.file_size,
                    file_mtime: ctx.file_mtime.clone(),
                    file_ctime: ctx.file_ctime.clone(),
                    this_context: ctx.this_context.clone(),
                    all_files: ctx.all_files.clone(),
                    traversal_depth: ctx.traversal_depth.clone(),
                    backlinks_index: ctx.backlinks_index.clone(),
                    type_names: ctx.type_names.clone(),
                    types: ctx.types.clone(),
                    note_namespace_source: ctx.note_namespace_source,
                    string_concat: ctx.string_concat,
                };
                acc = evaluate(&args[0], &item_ctx)?;
            }
            Ok(acc)
        }
        _ => Err(EvalError::unknown_function(&format!(
            "Unknown array method: .{}()",
            method
        ))),
    }
}

fn eval_object_method(
    map: &serde_json::Map<String, Value>,
    method: &str,
    args: &[Expr],
    ctx: &EvalContext,
) -> Result<Value, EvalError> {
    match method {
        "length" | "size" => {
            if !args.is_empty() {
                return Err(EvalError::wrong_argument_count(
                    "length()/size() takes no arguments",
                ));
            }
            Ok(Value::Number(map.len().into()))
        }
        "keys" => {
            let keys: Vec<Value> = map.keys().map(|k| Value::String(k.clone())).collect();
            Ok(Value::Array(keys))
        }
        "values" => {
            let values: Vec<Value> = map.values().cloned().collect();
            Ok(Value::Array(values))
        }
        "isEmpty" => Ok(Value::Bool(map.is_empty())),
        "isType" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count(
                    "isType() requires 1 argument",
                ));
            }
            let type_name = evaluate(&args[0], ctx)?;
            let type_str = type_name.as_str().unwrap_or("");
            Ok(Value::Bool(type_str == "object"))
        }
        "toString" => Ok(Value::String(
            serde_json::to_string(&Value::Object(map.clone())).unwrap_or_default(),
        )),
        "isTruthy" => Ok(Value::Bool(true)),
        _ => Err(EvalError::unknown_function(&format!(
            "Unknown object method: .{}()",
            method
        ))),
    }
}

// Helper functions

fn is_truthy(val: &Value) -> bool {
    match val {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(_) => true,
    }
}

fn as_number(val: &Value) -> Option<f64> {
    match val {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::String(a), Value::String(b)) => {
            if a == b {
                return true;
            }
            // Try datetime-aware equality for offset-aware strings
            if let (Some(da), Some(db)) =
                (parse_datetime_for_compare(a), parse_datetime_for_compare(b))
            {
                return da == db;
            }
            false
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| values_equal(x, y))
        }
        // Cross-type numeric equality
        (Value::Number(_), Value::String(s)) | (Value::String(s), Value::Number(_)) => {
            if let Ok(n) = s.parse::<f64>() {
                let other = if a.is_number() {
                    a.as_f64().unwrap_or(f64::NAN)
                } else {
                    b.as_f64().unwrap_or(f64::NAN)
                };
                n == other
            } else {
                false
            }
        }
        _ => false,
    }
}

fn compare_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&b.as_f64().unwrap_or(0.0)),
        (Value::String(a), Value::String(b)) => {
            // Try datetime comparison if both look like ISO dates
            if let (Some(da), Some(db)) =
                (parse_datetime_for_compare(a), parse_datetime_for_compare(b))
            {
                return da.partial_cmp(&db);
            }
            Some(a.cmp(b))
        }
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// Parse a string as a datetime for comparison purposes.
/// Returns milliseconds since epoch if parseable.
fn parse_datetime_for_compare(s: &str) -> Option<i64> {
    // Try RFC3339 / ISO 8601 with timezone
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    // Try ISO 8601 with Z suffix
    if let Some(without_z) = s.strip_suffix('Z') {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(without_z, "%Y-%m-%dT%H:%M:%S") {
            return Some(dt.and_utc().timestamp_millis());
        }
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(without_z, "%Y-%m-%dT%H:%M:%S%.f") {
            return Some(dt.and_utc().timestamp_millis());
        }
    }
    // Try date-only
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
    }
    // Try datetime without timezone
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc().timestamp_millis());
    }
    None
}

fn value_to_string(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(f) = n.as_f64() {
                f.to_string()
            } else {
                "null".to_string()
            }
        }
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(val).unwrap_or_default(),
    }
}

fn type_name(val: &Value) -> &'static str {
    match val {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ---------------------------------------------------------------------------
// Date/Time helpers
// ---------------------------------------------------------------------------

fn is_date_string(s: &str) -> bool {
    // Simple check: YYYY-MM-DD format
    if s.len() >= 10 {
        let bytes = s.as_bytes();
        bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes[0..4].iter().all(|b| b.is_ascii_digit())
            && bytes[5..7].iter().all(|b| b.is_ascii_digit())
            && bytes[8..10].iter().all(|b| b.is_ascii_digit())
    } else {
        false
    }
}

fn is_datetime_string(s: &str) -> bool {
    is_date_string(s) && s.len() > 10 && (s.as_bytes()[10] == b'T' || s.as_bytes()[10] == b' ')
}

fn eval_date_component(s: &str, component: &str) -> Result<Value, EvalError> {
    use chrono::Datelike;
    // Try to parse as NaiveDate or NaiveDateTime
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return eval_chrono_component_dt(&dt, component);
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ") {
        return eval_chrono_component_dt(&dt, component);
    }
    // Try with timezone offset
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        let ndt = dt.naive_utc();
        return eval_chrono_component_dt(&ndt, component);
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return match component {
            "year" => Ok(Value::Number(d.year().into())),
            "month" => Ok(Value::Number(d.month().into())),
            "day" => Ok(Value::Number(d.day().into())),
            "dayOfWeek" => Ok(Value::Number(
                (d.weekday().num_days_from_sunday() as i64).into(),
            )),
            "hour" | "minute" | "second" => Ok(Value::Number(0.into())),
            _ => Ok(Value::Null),
        };
    }
    Ok(Value::Null)
}

fn eval_chrono_component_dt(
    dt: &chrono::NaiveDateTime,
    component: &str,
) -> Result<Value, EvalError> {
    use chrono::Datelike;
    use chrono::Timelike;
    match component {
        "year" => Ok(Value::Number(dt.date().year().into())),
        "month" => Ok(Value::Number(dt.date().month().into())),
        "day" => Ok(Value::Number(dt.date().day().into())),
        "hour" => Ok(Value::Number(dt.time().hour().into())),
        "minute" => Ok(Value::Number(dt.time().minute().into())),
        "second" => Ok(Value::Number(dt.time().second().into())),
        "dayOfWeek" => Ok(Value::Number(
            (dt.date().weekday().num_days_from_sunday() as i64).into(),
        )),
        _ => Ok(Value::Null),
    }
}

/// Resolve a link string to a file object via asFile() traversal.
fn resolve_as_file(link_str: &str, ctx: &EvalContext) -> Result<Value, EvalError> {
    // Check traversal depth limit (10 hops max)
    let depth = ctx.traversal_depth.get();
    if depth >= 10 {
        return Err(EvalError {
            code: "expression_depth_exceeded".to_string(),
            message: "asFile() traversal depth exceeded 10-hop limit".to_string(),
        });
    }
    ctx.traversal_depth.set(depth + 1);

    let all_files = match &ctx.all_files {
        Some(f) => f,
        None => return Ok(Value::Null),
    };

    // Extract the target from wikilink syntax
    let target = if link_str.starts_with("[[") && link_str.ends_with("]]") {
        let inner = &link_str[2..link_str.len() - 2];
        // Strip display text after |
        let inner = inner.split('|').next().unwrap_or(inner);
        // Strip anchor after #
        inner.split('#').next().unwrap_or(inner).trim()
    } else {
        link_str.trim()
    };

    if target.is_empty() {
        return Ok(Value::Null);
    }

    // Try to find the file:
    // 1. Exact path match
    // 2. Basename match (without extension)
    // 3. ID field match
    let mut found: Option<&ResolvedFileData> = None;

    for file_data in all_files.iter() {
        // Exact path match
        if file_data.path == target || file_data.path == format!("{}.md", target) {
            found = Some(file_data);
            break;
        }
    }

    if found.is_none() {
        // Basename match
        for file_data in all_files.iter() {
            let basename = std::path::Path::new(&file_data.path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if basename == target {
                found = Some(file_data);
                break;
            }
        }
    }

    if found.is_none() {
        // ID field match
        for file_data in all_files.iter() {
            if let Some(id) = file_data.frontmatter.get("id").and_then(|v| v.as_str()) {
                if id == target {
                    found = Some(file_data);
                    break;
                }
            }
        }
    }

    match found {
        Some(file_data) => {
            // Build a result object with frontmatter fields + file metadata
            let mut result = file_data.frontmatter.clone();
            if let Value::Object(ref mut map) = result {
                let path = &file_data.path;
                let name = std::path::Path::new(path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                let folder = std::path::Path::new(path)
                    .parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or("");
                let basename = std::path::Path::new(path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                map.insert(
                    "file".to_string(),
                    serde_json::json!({
                        "path": path,
                        "name": name,
                        "folder": folder,
                        "basename": basename,
                    }),
                );
            }
            Ok(result)
        }
        None => Ok(Value::Null),
    }
}

fn format_date_string(s: &str, fmt: &str) -> String {
    // Parse the date/datetime string
    let result = if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        Some(dt)
    } else if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ") {
        Some(dt)
    } else if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        Some(dt.naive_local())
    } else if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        Some(d.and_hms_opt(0, 0, 0).unwrap())
    } else {
        None
    };

    if let Some(dt) = result {
        // Convert custom format tokens to chrono format
        let chrono_fmt = fmt
            .replace("YYYY", "%Y")
            .replace("MM", "%m")
            .replace("DD", "%d")
            .replace("HH", "%H")
            .replace("mm", "%M")
            .replace("ss", "%S");
        dt.format(&chrono_fmt).to_string()
    } else {
        s.to_string()
    }
}

/// Parse a duration string like "7d", "1w", "24h", "30m", "1M", "1y" into milliseconds.
fn parse_duration_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    if s.starts_with('P') || s.starts_with("-P") {
        return parse_iso8601_duration_ms(s);
    }

    // Try to parse as "<number><unit>"
    let mut num_end = 0;
    for (i, c) in s.char_indices() {
        if c.is_ascii_digit() || c == '.' || (c == '-' && i == 0) {
            num_end = i + c.len_utf8();
        } else {
            break;
        }
    }
    if num_end == 0 {
        return None;
    }

    let num: f64 = s[..num_end].parse().ok()?;
    let unit = s[num_end..].trim_start();

    let ms_per_unit = match unit {
        "ms" => 1.0,
        "s" | "second" | "seconds" => 1000.0,
        "m" | "minute" | "minutes" => 60_000.0,
        "h" | "hour" | "hours" => 3_600_000.0,
        "d" | "day" | "days" => 86_400_000.0,
        "w" | "week" | "weeks" => 604_800_000.0,
        "M" | "month" | "months" => 2_592_000_000.0, // 30 days
        "y" | "year" | "years" => 31_536_000_000.0,  // 365 days
        _ => return None,
    };

    Some((num * ms_per_unit) as i64)
}

/// Parse the fixed-length subset of ISO 8601 durations used by CEL.
/// Calendar years and months are deliberately excluded because their length
/// depends on an anchor date; date arithmetic handles those separately.
fn parse_iso8601_duration_ms(source: &str) -> Option<i64> {
    let (sign, source) = source
        .strip_prefix('-')
        .map_or((1.0, source), |rest| (-1.0, rest));
    let mut chars = source.strip_prefix('P')?.chars().peekable();
    let mut in_time = false;
    let mut saw_component = false;
    let mut total_ms = 0.0;

    while chars.peek().is_some() {
        if chars.peek() == Some(&'T') {
            chars.next();
            if in_time {
                return None;
            }
            in_time = true;
            continue;
        }

        let mut number = String::new();
        while chars
            .peek()
            .is_some_and(|value| value.is_ascii_digit() || *value == '.')
        {
            number.push(chars.next()?);
        }
        if number.is_empty() {
            return None;
        }
        let value = number.parse::<f64>().ok()?;
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        let unit = chars.next()?;
        let multiplier = match (in_time, unit) {
            (false, 'W') => 604_800_000.0,
            (false, 'D') => 86_400_000.0,
            (true, 'H') => 3_600_000.0,
            (true, 'M') => 60_000.0,
            (true, 'S') => 1_000.0,
            _ => return None,
        };
        total_ms += value * multiplier;
        saw_component = true;
    }

    let signed = sign * total_ms;
    (saw_component && signed.is_finite() && signed >= i64::MIN as f64 && signed <= i64::MAX as f64)
        .then_some(signed.round() as i64)
}

/// Add a duration string to a date/datetime string.
fn add_duration_to_date(date_str: &str, duration_str: &str) -> Option<String> {
    use chrono::Datelike;

    let dur_str = duration_str.trim();
    let mut num_end = 0;
    for (i, c) in dur_str.char_indices() {
        if c.is_ascii_digit() || c == '.' || (c == '-' && i == 0) {
            num_end = i + c.len_utf8();
        } else {
            break;
        }
    }
    if num_end == 0 {
        return None;
    }
    let num: i64 = dur_str[..num_end].parse().ok()?;
    let unit = dur_str[num_end..].trim();

    // Check for calendar units (months, years) that need special handling
    let is_calendar_unit = matches!(unit, "M" | "month" | "months" | "y" | "year" | "years");

    if is_calendar_unit {
        let (months, years) = match unit {
            "M" | "month" | "months" => (num as i32, 0i32),
            "y" | "year" | "years" => (0i32, num as i32),
            _ => return None,
        };

        // Calendar arithmetic with month clamping
        if let Ok(d) = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
            let total_months = d.month() as i32 - 1 + months + years * 12;
            let new_year = d.year() + total_months.div_euclid(12);
            let new_month = (total_months.rem_euclid(12) + 1) as u32;
            let max_day = days_in_month(new_year, new_month);
            let new_day = d.day().min(max_day);
            let new_date = chrono::NaiveDate::from_ymd_opt(new_year, new_month, new_day)?;
            return Some(new_date.format("%Y-%m-%d").to_string());
        }
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(date_str, "%Y-%m-%dT%H:%M:%S") {
            let d = dt.date();
            let total_months = d.month() as i32 - 1 + months + years * 12;
            let new_year = d.year() + total_months.div_euclid(12);
            let new_month = (total_months.rem_euclid(12) + 1) as u32;
            let max_day = days_in_month(new_year, new_month);
            let new_day = d.day().min(max_day);
            let new_date = chrono::NaiveDate::from_ymd_opt(new_year, new_month, new_day)?;
            let new_dt = new_date.and_time(dt.time());
            return Some(new_dt.format("%Y-%m-%dT%H:%M:%S").to_string());
        }
        // Offset-aware datetime: preserve local time
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(date_str) {
            let d = dt.naive_local().date();
            let total_months = d.month() as i32 - 1 + months + years * 12;
            let new_year = d.year() + total_months.div_euclid(12);
            let new_month = (total_months.rem_euclid(12) + 1) as u32;
            let max_day = days_in_month(new_year, new_month);
            let new_day = d.day().min(max_day);
            let new_date = chrono::NaiveDate::from_ymd_opt(new_year, new_month, new_day)?;
            let new_dt = new_date.and_time(dt.naive_local().time());
            return Some(new_dt.format("%Y-%m-%dT%H:%M:%S").to_string());
        }
        return None;
    }

    // Duration-based arithmetic
    let ms = parse_duration_ms(dur_str)?;
    let days = ms / 86_400_000;
    if let Ok(d) = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
        let new_date = d + chrono::Duration::days(days);
        return Some(new_date.format("%Y-%m-%d").to_string());
    }

    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(date_str, "%Y-%m-%dT%H:%M:%S") {
        let new_dt = dt + chrono::Duration::milliseconds(ms);
        return Some(new_dt.format("%Y-%m-%dT%H:%M:%S").to_string());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(date_str, "%Y-%m-%dT%H:%M:%SZ") {
        let new_dt = dt + chrono::Duration::milliseconds(ms);
        return Some(new_dt.format("%Y-%m-%dT%H:%M:%SZ").to_string());
    }
    // Offset-aware datetime: preserve local time
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(date_str) {
        let local = dt.naive_local();
        let new_dt = local + chrono::Duration::milliseconds(ms);
        return Some(new_dt.format("%Y-%m-%dT%H:%M:%S").to_string());
    }

    None
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Compute difference between two date/datetime strings in milliseconds.
fn date_diff_ms(a: &str, b: &str) -> Option<i64> {
    let a_ms = date_to_epoch_ms(a)?;
    let b_ms = date_to_epoch_ms(b)?;
    Some(a_ms - b_ms)
}

fn date_to_epoch_ms(s: &str) -> Option<i64> {
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc().timestamp_millis());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ") {
        return Some(dt.and_utc().timestamp_millis());
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
    }
    None
}

// ─── Body extraction ───────────────────────────────────────────────────────

/// Strip inline code spans from body text before extracting links/tags/embeds.
/// Handles both single backtick `code` and multi-backtick ``code`` spans.
fn strip_code_blocks_and_inline_code(body: &str) -> String {
    // First pass: strip fenced code blocks (``` or ~~~)
    let mut lines_out = Vec::new();
    let mut in_fence = false;
    let mut fence_marker = "";
    let mut fence_backtick_count = 0;

    for line in body.lines() {
        let trimmed = line.trim_start();
        if !in_fence {
            // Check for opening fence
            let (is_fence, marker, count) = detect_fence_open(trimmed);
            if is_fence {
                in_fence = true;
                fence_marker = marker;
                fence_backtick_count = count;
                // Skip the fence line itself
                continue;
            }
            lines_out.push(line);
        } else {
            // Check for closing fence (must use same marker and at least as many chars)
            if is_fence_close(trimmed, fence_marker, fence_backtick_count) {
                in_fence = false;
                // Skip the closing fence line
                continue;
            }
            // Skip content inside the fenced code block
        }
    }

    let after_fences = lines_out.join("\n");

    // Second pass: strip inline code spans
    let mut result = String::with_capacity(after_fences.len());
    let chars: Vec<char> = after_fences.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '`' {
            // Count consecutive backticks
            let start = i;
            while i < len && chars[i] == '`' {
                i += 1;
            }
            let backtick_count = i - start;
            // Find matching closing backticks
            let mut found_close = false;
            let search_start = i;
            while i <= len - backtick_count {
                if chars[i] == '`' {
                    let mut close_count = 0;
                    while i < len && chars[i] == '`' {
                        close_count += 1;
                        i += 1;
                    }
                    if close_count == backtick_count {
                        found_close = true;
                        break;
                    }
                    // Not a match, continue searching
                } else {
                    i += 1;
                }
            }
            if !found_close {
                // No closing backticks found - include the opening backticks as literal
                for c in &chars[start..search_start] {
                    result.push(*c);
                }
                // i is already past end or at a non-matching position
            }
            // else: skip the entire inline code span
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

fn detect_fence_open(trimmed: &str) -> (bool, &'static str, usize) {
    if trimmed.starts_with("```") {
        let count = trimmed.chars().take_while(|&c| c == '`').count();
        // Opening fence: at least 3 backticks, rest is info string (no closing backticks)
        (true, "`", count)
    } else if trimmed.starts_with("~~~") {
        let count = trimmed.chars().take_while(|&c| c == '~').count();
        (true, "~", count)
    } else {
        (false, "", 0)
    }
}

fn is_fence_close(trimmed: &str, marker: &str, min_count: usize) -> bool {
    let fence_char = if marker == "`" { '`' } else { '~' };
    if !trimmed.starts_with(fence_char) {
        return false;
    }
    let count = trimmed.chars().take_while(|&c| c == fence_char).count();
    if count < min_count {
        return false;
    }
    // Closing fence: only fence chars and optional whitespace
    trimmed[count * fence_char.len_utf8()..].trim().is_empty()
}

/// Extract inline tags from markdown body.
/// Tags match #[A-Za-z0-9_/-]+ and must be preceded by whitespace or start of line.
/// Tags inside inline code spans are excluded.
pub fn extract_tags_from_body(body: &str) -> Vec<String> {
    let clean = strip_code_blocks_and_inline_code(body);
    let mut tags = Vec::new();

    for line in clean.lines() {
        let chars: Vec<char> = line.chars().collect();
        let len = chars.len();
        let mut i = 0;

        while i < len {
            if chars[i] == '#' {
                // Check if preceded by whitespace or start of line
                let at_start = i == 0;
                let preceded_by_space = i > 0 && chars[i - 1].is_whitespace();

                // Check it's NOT preceded by a quote (URL fragment exclusion)
                let preceded_by_quote =
                    i > 0 && (chars[i - 1] == '"' || chars[i - 1] == '\'' || chars[i - 1] == '(');

                if (at_start || preceded_by_space) && !preceded_by_quote {
                    // Scan tag characters
                    let tag_start = i + 1;
                    i = tag_start;
                    while i < len
                        && (chars[i].is_ascii_alphanumeric()
                            || chars[i] == '_'
                            || chars[i] == '-'
                            || chars[i] == '/')
                    {
                        i += 1;
                    }
                    if i > tag_start {
                        let tag: String = chars[tag_start..i].iter().collect();
                        // Reject tags that look like hex color codes:
                        // 3 or 6 hex characters containing at least one letter (A-F/a-f)
                        let is_hex_color = (tag.len() == 3 || tag.len() == 6)
                            && tag.chars().all(|c| c.is_ascii_hexdigit())
                            && tag.chars().any(|c| c.is_ascii_alphabetic());
                        if !is_hex_color && !tags.contains(&tag) {
                            tags.push(tag);
                        }
                    }
                } else {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
    }

    tags
}

/// Extract links from markdown body (wikilinks and markdown links).
/// Returns a list of link target strings.
/// Links inside inline code spans are excluded.
pub fn extract_links_from_body(body: &str) -> Vec<String> {
    let clean = strip_code_blocks_and_inline_code(body);
    let mut links = Vec::new();

    let chars: Vec<char> = clean.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Wikilink: [[target]] or [[target|alias]]  (but NOT ![[embed]] or \[[escaped]])
        if i + 1 < len && chars[i] == '[' && chars[i + 1] == '[' {
            // Check this is not an embed (preceded by !) or escaped (preceded by \)
            let is_embed = i > 0 && chars[i - 1] == '!';
            let is_escaped = i > 0 && chars[i - 1] == '\\';
            if !is_embed && !is_escaped {
                i += 2; // skip [[
                let start = i;
                while i < len && !(chars[i] == ']' && i + 1 < len && chars[i + 1] == ']') {
                    i += 1;
                }
                if i < len {
                    let content: String = chars[start..i].iter().collect();
                    // Split on | for alias, then strip anchor (#)
                    let target = content.split('|').next().unwrap_or(&content).trim();
                    let target = target.split('#').next().unwrap_or(target).to_string();
                    if !target.is_empty() {
                        links.push(target);
                    }
                    i += 2; // skip ]]
                }
            } else {
                // Skip past the escaped/embed wikilink content and closing ]]
                i += 2; // skip [[
                while i < len && !(chars[i] == ']' && i + 1 < len && chars[i + 1] == ']') {
                    i += 1;
                }
                if i < len {
                    i += 2; // skip ]]
                }
            }
        }
        // Markdown link: [text](path) - but NOT ![text](path) which is an image
        else if chars[i] == '[' && (i == 0 || chars[i - 1] != '!') {
            // Find closing ]
            i += 1;
            let mut bracket_depth = 1;
            while i < len && bracket_depth > 0 {
                if chars[i] == '[' {
                    bracket_depth += 1;
                }
                if chars[i] == ']' {
                    bracket_depth -= 1;
                }
                i += 1;
            }
            // Check for (path)
            if i < len && chars[i] == '(' {
                i += 1;
                let paren_start = i;
                let mut paren_depth = 1;
                while i < len && paren_depth > 0 {
                    if chars[i] == '(' {
                        paren_depth += 1;
                    }
                    if chars[i] == ')' {
                        paren_depth -= 1;
                    }
                    i += 1;
                }
                let path: String = chars[paren_start..i - 1].iter().collect();
                let path = path.trim().to_string();
                // Skip external URLs
                if !path.is_empty() && !path.starts_with("http://") && !path.starts_with("https://")
                {
                    // Strip anchor
                    let path = path.split('#').next().unwrap_or(&path).to_string();
                    if !path.is_empty() {
                        links.push(path);
                    }
                }
            }
        } else {
            i += 1;
        }
    }

    links
}

/// Extract embeds from Markdown body (`![[target]]`).
/// Embeds inside inline code spans are excluded.
pub fn extract_embeds_from_body(body: &str) -> Vec<String> {
    let clean = strip_code_blocks_and_inline_code(body);
    let mut embeds = Vec::new();

    let chars: Vec<char> = clean.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Wikilink embed: ![[target]]
        if i + 2 < len && chars[i] == '!' && chars[i + 1] == '[' && chars[i + 2] == '[' {
            i += 3; // skip ![[
            let start = i;
            while i < len && !(chars[i] == ']' && i + 1 < len && chars[i + 1] == ']') {
                i += 1;
            }
            if i < len {
                let content: String = chars[start..i].iter().collect();
                let target = content
                    .split('|')
                    .next()
                    .unwrap_or(&content)
                    .trim()
                    .to_string();
                if !target.is_empty() {
                    embeds.push(target);
                }
                i += 2; // skip ]]
            }
        }
        // Markdown embed: ![alt](path)
        else if i + 1 < len && chars[i] == '!' && chars[i + 1] == '[' {
            i += 2; // skip ![
                    // Find closing ]
            let mut bracket_depth = 1;
            while i < len && bracket_depth > 0 {
                if chars[i] == '[' {
                    bracket_depth += 1;
                }
                if chars[i] == ']' {
                    bracket_depth -= 1;
                }
                i += 1;
            }
            // Check for (path)
            if i < len && chars[i] == '(' {
                i += 1;
                let paren_start = i;
                let mut paren_depth = 1;
                while i < len && paren_depth > 0 {
                    if chars[i] == '(' {
                        paren_depth += 1;
                    }
                    if chars[i] == ')' {
                        paren_depth -= 1;
                    }
                    i += 1;
                }
                let path: String = chars[paren_start..i - 1].iter().collect();
                let path = path.trim().to_string();
                if !path.is_empty() && !path.starts_with("http://") && !path.starts_with("https://")
                {
                    let path = path.split('#').next().unwrap_or(&path).to_string();
                    if !path.is_empty() {
                        embeds.push(path);
                    }
                }
            }
        } else {
            i += 1;
        }
    }

    embeds
}

/// Extract link targets from a frontmatter value (recursively handles strings, arrays, objects).
pub fn extract_links_from_fm_value(val: &Value, links: &mut Vec<String>) {
    match val {
        Value::String(s) => {
            // Check for [[wikilink]] pattern in the string
            let mut found_wikilink = false;
            let chars: Vec<char> = s.chars().collect();
            let len = chars.len();
            let mut i = 0;
            while i + 1 < len {
                if chars[i] == '[' && chars[i + 1] == '[' {
                    found_wikilink = true;
                    i += 2;
                    let start = i;
                    while i < len && !(chars[i] == ']' && i + 1 < len && chars[i + 1] == ']') {
                        i += 1;
                    }
                    if i < len {
                        let content: String = chars[start..i].iter().collect();
                        let target = content.split('|').next().unwrap_or(&content).trim();
                        let target = target.split('#').next().unwrap_or(target).to_string();
                        if !target.is_empty() && !links.contains(&target) {
                            links.push(target);
                        }
                        i += 2;
                    }
                } else {
                    i += 1;
                }
            }
            // If no wikilinks found, check if the string itself is a link path
            if !found_wikilink
                && !s.is_empty()
                && !s.starts_with("http://")
                && !s.starts_with("https://")
                && (s.contains('.') || s.contains('/'))
            {
                let path = s.trim().to_string();
                if !path.is_empty() && !links.contains(&path) {
                    links.push(path);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                extract_links_from_fm_value(item, links);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;
    use crate::expressions::parser::Parser;

    #[test]
    fn profile_specific_work_budget_stops_evaluation() {
        let expression = Parser::parse("[1, 2, 3].map(value + 1)").unwrap();
        let clock = EvaluationClock::capture(Some("UTC")).unwrap();
        let error =
            evaluate_with_limits(&expression, &EvalContext::empty(), 128, 3, &clock).unwrap_err();
        assert_eq!(error.code, "expression_work_exceeded");
        assert!(error.message.contains("3-step"));
    }
}
