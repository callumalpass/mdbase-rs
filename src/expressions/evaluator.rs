//! AST → value (Rust evaluator).

use super::ast::*;
use serde_json::Value;

/// Error types from expression evaluation.
#[derive(Debug)]
pub struct EvalError {
    pub code: String,
    pub message: String,
}

impl EvalError {
    fn type_error(msg: &str) -> Self {
        EvalError { code: "type_error".to_string(), message: msg.to_string() }
    }
    fn invalid_expression(msg: &str) -> Self {
        EvalError { code: "invalid_expression".to_string(), message: msg.to_string() }
    }
    fn wrong_argument_count(msg: &str) -> Self {
        EvalError { code: "wrong_argument_count".to_string(), message: msg.to_string() }
    }
    fn division_by_zero() -> Self {
        EvalError { code: "division_by_zero".to_string(), message: "Division by zero".to_string() }
    }
    fn unknown_function(msg: &str) -> Self {
        EvalError { code: "unknown_function".to_string(), message: msg.to_string() }
    }
}

/// Context for expression evaluation.
pub struct EvalContext {
    pub frontmatter: Value,
    pub file_path: Option<String>,
    pub body: Option<String>,
}

impl EvalContext {
    pub fn empty() -> Self {
        EvalContext {
            frontmatter: Value::Object(serde_json::Map::new()),
            file_path: None,
            body: None,
        }
    }
}

/// Evaluate a parsed expression in a given context.
pub fn evaluate(expr: &Expr, ctx: &EvalContext) -> Result<Value, EvalError> {
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
    // Special case: file.name, file.path, etc.
    if let Expr::Ident(ref name) = obj_expr {
        if name == "file" {
            return eval_file_property(field, ctx);
        }
    }

    let obj = evaluate(obj_expr, ctx)?;
    match &obj {
        Value::Object(map) => Ok(map.get(field).cloned().unwrap_or(Value::Null)),
        Value::String(s) => {
            // String properties
            match field {
                "length" => Ok(Value::Number(s.len().into())),
                _ => Ok(Value::Null),
            }
        }
        Value::Array(arr) => {
            match field {
                "length" => Ok(Value::Number(arr.len().into())),
                _ => Ok(Value::Null),
            }
        }
        Value::Null => Ok(Value::Null),
        _ => Ok(Value::Null),
    }
}

fn eval_file_property(field: &str, ctx: &EvalContext) -> Result<Value, EvalError> {
    match field {
        "path" => Ok(ctx.file_path.clone().map(Value::String).unwrap_or(Value::Null)),
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
        _ => Ok(Value::Null),
    }
}

fn eval_index(obj_expr: &Expr, idx_expr: &Expr, ctx: &EvalContext) -> Result<Value, EvalError> {
    let obj = evaluate(obj_expr, ctx)?;
    let idx = evaluate(idx_expr, ctx)?;

    match &obj {
        Value::Array(arr) => {
            if let Some(i) = idx.as_i64() {
                let i = if i < 0 { (arr.len() as i64 + i) as usize } else { i as usize };
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
                let i = if i < 0 { (s.len() as i64 + i) as usize } else { i as usize };
                Ok(s.chars().nth(i).map(|c| Value::String(c.to_string())).unwrap_or(Value::Null))
            } else {
                Ok(Value::Null)
            }
        }
        _ => Ok(Value::Null),
    }
}

fn eval_binop(left: &Expr, op: &BinOp, right: &Expr, ctx: &EvalContext) -> Result<Value, EvalError> {
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
        BinOp::Add => eval_add(&lval, &rval),
        BinOp::Sub => eval_arithmetic(&lval, &rval, "-"),
        BinOp::Mul => eval_arithmetic(&lval, &rval, "*"),
        BinOp::Div => eval_arithmetic(&lval, &rval, "/"),
        BinOp::Mod => eval_arithmetic(&lval, &rval, "%"),
        BinOp::Eq => Ok(Value::Bool(values_equal(&lval, &rval))),
        BinOp::Neq => Ok(Value::Bool(!values_equal(&lval, &rval))),
        BinOp::Lt => Ok(Value::Bool(compare_values(&lval, &rval) == Some(std::cmp::Ordering::Less))),
        BinOp::Gt => Ok(Value::Bool(compare_values(&lval, &rval) == Some(std::cmp::Ordering::Greater))),
        BinOp::Lte => Ok(Value::Bool(matches!(compare_values(&lval, &rval), Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)))),
        BinOp::Gte => Ok(Value::Bool(matches!(compare_values(&lval, &rval), Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)))),
        BinOp::And | BinOp::Or => unreachable!(),
    }
}

fn eval_add(left: &Value, right: &Value) -> Result<Value, EvalError> {
    // String concatenation
    if left.is_string() || right.is_string() {
        let ls = value_to_string(left);
        let rs = value_to_string(right);
        return Ok(Value::String(format!("{}{}", ls, rs)));
    }
    eval_arithmetic(left, right, "+")
}

fn eval_arithmetic(left: &Value, right: &Value, op: &str) -> Result<Value, EvalError> {
    let ln = as_number(left).ok_or_else(|| EvalError::type_error(&format!("Cannot apply '{}' to {}", op, type_name(left))))?;
    let rn = as_number(right).ok_or_else(|| EvalError::type_error(&format!("Cannot apply '{}' to {}", op, type_name(right))))?;

    let result = match op {
        "+" => ln + rn,
        "-" => ln - rn,
        "*" => ln * rn,
        "/" => {
            if rn == 0.0 { return Err(EvalError::division_by_zero()); }
            ln / rn
        }
        "%" => {
            if rn == 0.0 { return Err(EvalError::division_by_zero()); }
            ln % rn
        }
        _ => return Err(EvalError::invalid_expression(&format!("Unknown op: {}", op))),
    };

    // Return integer if both inputs were integers and result is integer
    if result.fract() == 0.0 && result >= i64::MIN as f64 && result <= i64::MAX as f64
        && left.is_i64() && right.is_i64()
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
            let n = as_number(&val).ok_or_else(|| EvalError::type_error("Cannot negate non-number"))?;
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
        return eval_method(obj, method, args, ctx);
    }

    // Free function calls: func(args)
    if let Expr::Ident(name) = func_expr {
        // Handle ext::name() calls
        if name.starts_with("ext::") {
            let func_name = &name[5..];
            if func_name.is_empty() {
                return Err(EvalError::invalid_expression("ext:: with no function name"));
            }
            return Err(EvalError::unknown_function(&format!("Unknown extension function: {}()", name)));
        }
        return eval_function(name, args, ctx);
    }

    Err(EvalError::invalid_expression("Not a callable expression"))
}

fn eval_function(name: &str, args: &[Expr], ctx: &EvalContext) -> Result<Value, EvalError> {
    match name {
        "exists" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count("exists() requires 1 argument"));
            }
            // exists() checks if a field exists in frontmatter (even if null)
            if let Expr::Ident(ref field) = args[0] {
                let has = ctx.frontmatter.as_object()
                    .map_or(false, |m| m.contains_key(field));
                Ok(Value::Bool(has))
            } else {
                let val = evaluate(&args[0], ctx)?;
                Ok(Value::Bool(!val.is_null()))
            }
        }
        "default" => {
            if args.len() != 2 {
                return Err(EvalError::wrong_argument_count("default() requires 2 arguments"));
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
                return Err(EvalError::wrong_argument_count("isTruthy() requires 1 argument"));
            }
            let val = evaluate(&args[0], ctx)?;
            Ok(Value::Bool(is_truthy(&val)))
        }
        "number" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count("number() requires 1 argument"));
            }
            let val = evaluate(&args[0], ctx)?;
            match &val {
                Value::Number(_) => Ok(val),
                Value::String(s) => {
                    if let Ok(n) = s.parse::<f64>() {
                        if n.fract() == 0.0 {
                            Ok(Value::Number((n as i64).into()))
                        } else {
                            Ok(serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null))
                        }
                    } else {
                        Ok(Value::Null)
                    }
                }
                Value::Bool(b) => Ok(Value::Number(if *b { 1 } else { 0 }.into())),
                _ => Ok(Value::Null),
            }
        }
        "toString" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count("toString() requires 1 argument"));
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
                return Err(EvalError::wrong_argument_count("date() requires 1 argument"));
            }
            let val = evaluate(&args[0], ctx)?;
            // Return the date string as-is
            Ok(val)
        }
        "today" => {
            Ok(Value::String(chrono::Local::now().format("%Y-%m-%d").to_string()))
        }
        "now" => {
            Ok(Value::String(chrono::Utc::now().to_rfc3339()))
        }
        "abs" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count("abs() requires 1 argument"));
            }
            let val = evaluate(&args[0], ctx)?;
            let n = as_number(&val).ok_or_else(|| EvalError::type_error("abs() requires a number"))?;
            if n.fract() == 0.0 {
                Ok(Value::Number((n.abs() as i64).into()))
            } else {
                Ok(serde_json::Number::from_f64(n.abs()).map(Value::Number).unwrap_or(Value::Null))
            }
        }
        "min" => {
            if args.len() < 2 {
                return Err(EvalError::wrong_argument_count("min() requires at least 2 arguments"));
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
                return Err(EvalError::wrong_argument_count("max() requires at least 2 arguments"));
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
                return Err(EvalError::wrong_argument_count("round() requires 1 argument"));
            }
            let val = evaluate(&args[0], ctx)?;
            let n = as_number(&val).ok_or_else(|| EvalError::type_error("round() requires a number"))?;
            Ok(Value::Number((n.round() as i64).into()))
        }
        "floor" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count("floor() requires 1 argument"));
            }
            let val = evaluate(&args[0], ctx)?;
            let n = as_number(&val).ok_or_else(|| EvalError::type_error("floor() requires a number"))?;
            Ok(Value::Number((n.floor() as i64).into()))
        }
        "ceil" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count("ceil() requires 1 argument"));
            }
            let val = evaluate(&args[0], ctx)?;
            let n = as_number(&val).ok_or_else(|| EvalError::type_error("ceil() requires a number"))?;
            Ok(Value::Number((n.ceil() as i64).into()))
        }
        "length" | "upper" | "lower" | "trim" | "trimStart" | "trimEnd" | "isEmpty"
        | "contains" | "startsWith" | "endsWith" | "replace" | "split" | "slice"
        | "matches" | "reverse" | "repeat" | "join" | "unique" | "flat" | "sort"
        | "first" | "last" | "keys" | "values" => {
            // These are method-only, not free functions
            Err(EvalError::unknown_function(&format!("{}() is a method, not a free function", name)))
        }
        "duration" => {
            // duration() is recognized but returns type_error for compound strings
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count("duration() requires 1 argument"));
            }
            let val = evaluate(&args[0], ctx)?;
            Err(EvalError::type_error(&format!("duration() is not supported: {}", value_to_string(&val))))
        }
        _ => Err(EvalError::unknown_function(&format!("Unknown function: {}()", name))),
    }
}

fn eval_method(obj_expr: &Expr, method: &str, args: &[Expr], ctx: &EvalContext) -> Result<Value, EvalError> {
    // Handle ext.name() and ext::name() as extension function calls
    if let Expr::Ident(ref name) = obj_expr {
        if name == "ext" || name.starts_with("ext::") {
            return Err(EvalError::unknown_function(&format!("Unknown extension function: ext.{}()", method)));
        }
    }

    let obj = evaluate(obj_expr, ctx)?;

    match &obj {
        Value::String(s) => eval_string_method(s, method, args, ctx),
        Value::Array(arr) => eval_array_method(arr, method, args, ctx),
        Value::Object(map) => eval_object_method(map, method, args, ctx),
        Value::Null => {
            // Null-safe: method on null returns null
            Ok(Value::Null)
        }
        _ => {
            match method {
                "isEmpty" => Ok(Value::Bool(obj.is_null())),
                _ => Ok(Value::Null),
            }
        }
    }
}

fn eval_string_method(s: &str, method: &str, args: &[Expr], ctx: &EvalContext) -> Result<Value, EvalError> {
    match method {
        "length" => {
            if !args.is_empty() { return Err(EvalError::wrong_argument_count("length() takes no arguments")); }
            Ok(Value::Number(s.len().into()))
        }
        "upper" => {
            if !args.is_empty() { return Err(EvalError::wrong_argument_count("upper() takes no arguments")); }
            Ok(Value::String(s.to_uppercase()))
        }
        "lower" => {
            if !args.is_empty() { return Err(EvalError::wrong_argument_count("lower() takes no arguments")); }
            Ok(Value::String(s.to_lowercase()))
        }
        "trim" => {
            if !args.is_empty() { return Err(EvalError::wrong_argument_count("trim() takes no arguments")); }
            Ok(Value::String(s.trim().to_string()))
        }
        "trimStart" => {
            if !args.is_empty() { return Err(EvalError::wrong_argument_count("trimStart() takes no arguments")); }
            Ok(Value::String(s.trim_start().to_string()))
        }
        "trimEnd" => {
            if !args.is_empty() { return Err(EvalError::wrong_argument_count("trimEnd() takes no arguments")); }
            Ok(Value::String(s.trim_end().to_string()))
        }
        "isEmpty" => {
            if !args.is_empty() { return Err(EvalError::wrong_argument_count("isEmpty() takes no arguments")); }
            Ok(Value::Bool(s.is_empty()))
        }
        "contains" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count("contains() requires 1 argument"));
            }
            let needle = evaluate(&args[0], ctx)?;
            let needle_str = match needle.as_str() { Some(s) => s.to_string(), None => value_to_string(&needle) };
            Ok(Value::Bool(s.contains(&needle_str)))
        }
        "startsWith" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count("startsWith() requires 1 argument"));
            }
            let prefix = evaluate(&args[0], ctx)?;
            let prefix_str = match prefix.as_str() { Some(s) => s.to_string(), None => value_to_string(&prefix) };
            Ok(Value::Bool(s.starts_with(&prefix_str)))
        }
        "endsWith" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count("endsWith() requires 1 argument"));
            }
            let suffix = evaluate(&args[0], ctx)?;
            let suffix_str = match suffix.as_str() { Some(s) => s.to_string(), None => value_to_string(&suffix) };
            Ok(Value::Bool(s.ends_with(&suffix_str)))
        }
        "replace" => {
            if args.len() != 2 {
                return Err(EvalError::wrong_argument_count("replace() requires 2 arguments"));
            }
            let from = evaluate(&args[0], ctx)?;
            let to = evaluate(&args[1], ctx)?;
            let from_str = match from.as_str() { Some(s) => s.to_string(), None => value_to_string(&from) };
            let to_str = match to.as_str() { Some(s) => s.to_string(), None => value_to_string(&to) };
            Ok(Value::String(s.replace(&from_str, &to_str)))
        }
        "split" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count("split() requires 1 argument"));
            }
            let sep = evaluate(&args[0], ctx)?;
            let sep_str = match sep.as_str() { Some(s) => s.to_string(), None => value_to_string(&sep) };
            let parts: Vec<Value> = s.split(&sep_str).map(|p| Value::String(p.to_string())).collect();
            Ok(Value::Array(parts))
        }
        "slice" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EvalError::wrong_argument_count("slice() requires 1-2 arguments"));
            }
            let start = evaluate(&args[0], ctx)?.as_i64().unwrap_or(0);
            let chars: Vec<char> = s.chars().collect();
            let len = chars.len() as i64;
            let start = if start < 0 { (len + start).max(0) as usize } else { start.min(len) as usize };
            let end = if args.len() > 1 {
                let e = evaluate(&args[1], ctx)?.as_i64().unwrap_or(len);
                if e < 0 { (len + e).max(0) as usize } else { e.min(len) as usize }
            } else {
                len as usize
            };
            let result: String = chars[start..end.max(start)].iter().collect();
            Ok(Value::String(result))
        }
        "matches" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count("matches() requires 1 argument"));
            }
            let pattern = evaluate(&args[0], ctx)?;
            let pat_str = match pattern.as_str() { Some(s) => s.to_string(), None => value_to_string(&pattern) };
            match fancy_regex::Regex::new(&pat_str) {
                Ok(re) => Ok(Value::Bool(re.is_match(s).unwrap_or(false))),
                Err(_) => Err(EvalError::invalid_expression(&format!("Invalid regex: {}", pat_str))),
            }
        }
        "reverse" => Ok(Value::String(s.chars().rev().collect())),
        "repeat" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count("repeat() requires 1 argument"));
            }
            let n = evaluate(&args[0], ctx)?.as_u64().unwrap_or(0);
            Ok(Value::String(s.repeat(n as usize)))
        }
        _ => Err(EvalError::unknown_function(&format!("Unknown string method: .{}()", method))),
    }
}

fn eval_array_method(arr: &[Value], method: &str, args: &[Expr], ctx: &EvalContext) -> Result<Value, EvalError> {
    match method {
        "length" => Ok(Value::Number(arr.len().into())),
        "isEmpty" => Ok(Value::Bool(arr.is_empty())),
        "contains" => {
            if args.len() != 1 {
                return Err(EvalError::wrong_argument_count("contains() requires 1 argument"));
            }
            let needle = evaluate(&args[0], ctx)?;
            Ok(Value::Bool(arr.iter().any(|item| values_equal(item, &needle))))
        }
        "join" => {
            let sep = if args.len() > 0 {
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
                return Err(EvalError::wrong_argument_count("slice() requires 1-2 arguments"));
            }
            let start = evaluate(&args[0], ctx)?.as_i64().unwrap_or(0);
            let len = arr.len() as i64;
            let start = if start < 0 { (len + start).max(0) as usize } else { start.min(len) as usize };
            let end = if args.len() > 1 {
                let e = evaluate(&args[1], ctx)?.as_i64().unwrap_or(len);
                if e < 0 { (len + e).max(0) as usize } else { e.min(len) as usize }
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
        _ => Err(EvalError::unknown_function(&format!("Unknown array method: .{}()", method))),
    }
}

fn eval_object_method(map: &serde_json::Map<String, Value>, method: &str, _args: &[Expr], _ctx: &EvalContext) -> Result<Value, EvalError> {
    match method {
        "keys" => {
            let keys: Vec<Value> = map.keys().map(|k| Value::String(k.clone())).collect();
            Ok(Value::Array(keys))
        }
        "values" => {
            let values: Vec<Value> = map.values().cloned().collect();
            Ok(Value::Array(values))
        }
        "isEmpty" => Ok(Value::Bool(map.is_empty())),
        _ => Err(EvalError::unknown_function(&format!("Unknown object method: .{}()", method))),
    }
}

// Helper functions

fn is_truthy(val: &Value) -> bool {
    match val {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map_or(false, |f| f != 0.0),
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
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| values_equal(x, y))
        }
        // Cross-type numeric equality
        (Value::Number(_), Value::String(s)) | (Value::String(s), Value::Number(_)) => {
            if let Some(n) = s.parse::<f64>().ok() {
                let other = if a.is_number() { a.as_f64().unwrap_or(f64::NAN) } else { b.as_f64().unwrap_or(f64::NAN) };
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
        (Value::Number(a), Value::Number(b)) => a.as_f64().unwrap_or(0.0).partial_cmp(&b.as_f64().unwrap_or(0.0)),
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

fn value_to_string(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() { i.to_string() }
            else if let Some(f) = n.as_f64() { f.to_string() }
            else { "null".to_string() }
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
