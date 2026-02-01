//! Expression parsing and evaluation (§11).

pub mod ast;
pub mod compiler;
pub mod evaluator;
pub mod parser;

use crate::expressions::parser::Parser as ExprParser;

/// Check if an expression string references a given field name as an identifier.
pub(crate) fn expression_references_field(expr_str: &str, field_name: &str) -> bool {
    // Parse the expression and walk the AST looking for Ident nodes
    if let Ok(expr) = ExprParser::parse(expr_str) {
        expr_contains_ident(&expr, field_name)
    } else {
        // If parsing fails, do a simple string check as fallback
        expr_str.contains(field_name)
    }
}

/// Check if an expression AST contains a reference to the given identifier name.
pub(crate) fn expr_contains_ident(expr: &crate::expressions::ast::Expr, name: &str) -> bool {
    use crate::expressions::ast::Expr;
    match expr {
        Expr::Ident(s) => s == name,
        Expr::Dot(obj, _) => expr_contains_ident(obj, name),
        Expr::Index(obj, idx) => expr_contains_ident(obj, name) || expr_contains_ident(idx, name),
        Expr::BinOp(l, _, r) => expr_contains_ident(l, name) || expr_contains_ident(r, name),
        Expr::UnaryOp(_, e) => expr_contains_ident(e, name),
        Expr::NullCoalesce(l, r) => expr_contains_ident(l, name) || expr_contains_ident(r, name),
        Expr::Array(elements) => elements.iter().any(|e| expr_contains_ident(e, name)),
        Expr::Call(f, args) => {
            expr_contains_ident(f, name) || args.iter().any(|a| expr_contains_ident(a, name))
        }
        Expr::Conditional(c, t, e) => {
            expr_contains_ident(c, name) || expr_contains_ident(t, name) || expr_contains_ident(e, name)
        }
        _ => false,
    }
}

/// Check if a JSON value is truthy (for where clause evaluation).
pub(crate) fn is_truthy_value(val: &serde_json::Value) -> bool {
    match val {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().map_or(false, |f| f != 0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(_) => true,
    }
}

use std::collections::HashMap;
use crate::expressions::evaluator::{evaluate as eval_expr, EvalContext};
use crate::Collection;

impl Collection {
    /// Evaluate computed fields for a read result (§5.12).
    pub(crate) fn evaluate_computed_fields(
        &self,
        mut frontmatter: serde_json::Value,
        type_names: &[String],
        path: &str,
        body: Option<&str>,
    ) -> serde_json::Value {
        // Collect all computed fields from matched types
        let mut computed: Vec<(String, String)> = Vec::new(); // (name, expression)
        for type_name in type_names {
            if let Some(type_def) = self.types.get(type_name) {
                for (field_name, field_def) in &type_def.fields {
                    if let Some(ref expr) = field_def.computed {
                        // Don't add duplicates
                        if !computed.iter().any(|(n, _)| n == field_name) {
                            computed.push((field_name.clone(), expr.clone()));
                        }
                    }
                }
            }
        }

        if computed.is_empty() {
            return frontmatter;
        }

        // Topological sort to determine evaluation order
        let computed_names: std::collections::HashSet<&str> = computed.iter().map(|(n, _)| n.as_str()).collect();
        let mut deps: HashMap<&str, Vec<&str>> = HashMap::new();
        for (name, expr) in &computed {
            let mut field_deps = Vec::new();
            for dep_name in &computed_names {
                if *dep_name != name.as_str() && expression_references_field(expr, dep_name) {
                    field_deps.push(*dep_name);
                }
            }
            deps.insert(name.as_str(), field_deps);
        }

        // Topological sort
        let mut order: Vec<&str> = Vec::new();
        let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();

        fn topo_visit<'a>(
            node: &'a str,
            deps: &HashMap<&'a str, Vec<&'a str>>,
            visited: &mut std::collections::HashSet<&'a str>,
            order: &mut Vec<&'a str>,
        ) {
            if visited.contains(node) {
                return;
            }
            visited.insert(node);
            if let Some(node_deps) = deps.get(node) {
                for dep in node_deps {
                    topo_visit(dep, deps, visited, order);
                }
            }
            order.push(node);
        }

        for (name, _) in &computed {
            topo_visit(name, &deps, &mut visited, &mut order);
        }

        // Evaluate computed fields in dependency order
        for field_name in order {
            if let Some((_, expr)) = computed.iter().find(|(n, _)| n == field_name) {
                let ctx = EvalContext {
                    frontmatter: frontmatter.clone(),
                    raw_frontmatter: None,
                    file_path: Some(path.to_string()),
                    body: body.map(String::from),
                    file_size: None, file_mtime: None, file_ctime: None,
                    this_context: None,
                    all_files: None,
                    traversal_depth: std::cell::Cell::new(0),
                    backlinks_index: None,
                    type_names: None,
                    types: None,
                    string_concat: true,
                };
                if let Ok(parsed) = ExprParser::parse(expr) {
                    match eval_expr(&parsed, &ctx) {
                        Ok(value) => {
                            if let Some(obj) = frontmatter.as_object_mut() {
                                obj.insert(field_name.to_string(), value);
                            }
                        }
                        Err(_) => {
                            // On evaluation error, set to null
                            if let Some(obj) = frontmatter.as_object_mut() {
                                obj.insert(field_name.to_string(), serde_json::Value::Null);
                            }
                        }
                    }
                }
            }
        }

        frontmatter
    }
}
