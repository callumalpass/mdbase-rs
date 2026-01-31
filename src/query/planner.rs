//! SQL vs Rust evaluation decision.

use std::collections::HashMap;
use crate::expressions::evaluator::{EvalContext, evaluate as eval_expr};
use crate::expressions::parser::Parser as ExprParser;
use crate::Collection;

impl Collection {
    /// Validate formula expressions for syntax errors and circular references.
    pub(crate) fn validate_formulas(&self, formulas: &HashMap<String, String>) -> Result<(), serde_json::Value> {
        // Check for circular references between formulas
        for (name, expr_str) in formulas {
            // Check if formula references itself
            if expr_str.contains(&format!("formula.{}", name)) {
                return Err(serde_json::json!({
                    "error": { "code": "circular_formula", "message": format!("Formula '{}' references itself", name) }
                }));
            }
        }

        // Check for circular chains: A -> B -> A
        for (name, expr_str) in formulas {
            let mut visited = std::collections::HashSet::new();
            visited.insert(name.clone());
            let mut to_check: Vec<String> = Vec::new();
            // Find formula references in this expression
            for (other_name, _) in formulas {
                if other_name != name && expr_str.contains(&format!("formula.{}", other_name)) {
                    to_check.push(other_name.clone());
                }
            }
            while let Some(dep) = to_check.pop() {
                if !visited.insert(dep.clone()) {
                    return Err(serde_json::json!({
                        "error": { "code": "circular_formula", "message": format!("Circular formula reference involving '{}'", name) }
                    }));
                }
                if let Some(dep_expr) = formulas.get(&dep) {
                    for (other_name, _) in formulas {
                        if dep_expr.contains(&format!("formula.{}", other_name)) {
                            to_check.push(other_name.clone());
                        }
                    }
                }
            }
        }

        // Validate each formula expression syntax
        for (name, expr_str) in formulas {
            match ExprParser::parse(expr_str) {
                Ok(parsed) => {
                    // Check for literal division by zero
                    if Self::has_literal_div_by_zero(&parsed) {
                        return Err(serde_json::json!({
                            "error": { "code": "formula_evaluation_error", "message": format!("Formula '{}': Division by zero", name) }
                        }));
                    }
                    // Try evaluating with empty context to catch static errors
                    let ctx = EvalContext::empty();
                    match eval_expr(&parsed, &ctx) {
                        Ok(_) => {}
                        Err(e) => {
                            match e.code.as_str() {
                                "unknown_function" | "wrong_argument_count" => {
                                    return Err(serde_json::json!({
                                        "error": { "code": "formula_evaluation_error", "message": format!("Formula '{}': {}", name, e.message) }
                                    }));
                                }
                                _ => {} // Other errors might depend on per-file context
                            }
                        }
                    }
                }
                Err(msg) => {
                    return Err(serde_json::json!({
                        "error": { "code": "invalid_formula", "message": format!("Formula '{}': {}", name, msg) }
                    }));
                }
            }
        }

        Ok(())
    }

    /// Check if an expression AST contains a literal division by zero (e.g., `x / 0`).
    pub(crate) fn has_literal_div_by_zero(expr: &crate::expressions::ast::Expr) -> bool {
        use crate::expressions::ast::{Expr, BinOp};
        match expr {
            Expr::BinOp(left, BinOp::Div, right) | Expr::BinOp(left, BinOp::Mod, right) => {
                if let Expr::Number(n) = right.as_ref() {
                    if *n == 0.0 {
                        return true;
                    }
                }
                Self::has_literal_div_by_zero(left) || Self::has_literal_div_by_zero(right)
            }
            Expr::BinOp(left, _, right) => {
                Self::has_literal_div_by_zero(left) || Self::has_literal_div_by_zero(right)
            }
            Expr::UnaryOp(_, inner) => Self::has_literal_div_by_zero(inner),
            Expr::NullCoalesce(left, right) => {
                Self::has_literal_div_by_zero(left) || Self::has_literal_div_by_zero(right)
            }
            Expr::Call(base, args) => {
                Self::has_literal_div_by_zero(base) || args.iter().any(|a| Self::has_literal_div_by_zero(a))
            }
            Expr::Conditional(cond, then, else_) => {
                Self::has_literal_div_by_zero(cond) || Self::has_literal_div_by_zero(then) || Self::has_literal_div_by_zero(else_)
            }
            Expr::Dot(inner, _) => Self::has_literal_div_by_zero(inner),
            Expr::Index(left, right) => {
                Self::has_literal_div_by_zero(left) || Self::has_literal_div_by_zero(right)
            }
            Expr::Array(items) => items.iter().any(|i| Self::has_literal_div_by_zero(i)),
            _ => false,
        }
    }

    /// Sort formulas in dependency order (topological sort).
    pub(crate) fn topological_sort_formulas(&self, formulas: &HashMap<String, String>) -> Vec<String> {
        // Build dependency graph
        let mut deps: HashMap<String, Vec<String>> = HashMap::new();
        for (name, expr) in formulas {
            let mut name_deps = Vec::new();
            for (other, _) in formulas {
                if other != name && expr.contains(&format!("formula.{}", other)) {
                    name_deps.push(other.clone());
                }
            }
            deps.insert(name.clone(), name_deps);
        }

        let mut result = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut visiting = std::collections::HashSet::new();

        fn visit(
            name: &str,
            deps: &HashMap<String, Vec<String>>,
            visited: &mut std::collections::HashSet<String>,
            visiting: &mut std::collections::HashSet<String>,
            result: &mut Vec<String>,
        ) {
            if visited.contains(name) { return; }
            if visiting.contains(name) { return; } // circular, handled elsewhere
            visiting.insert(name.to_string());
            if let Some(d) = deps.get(name) {
                for dep in d {
                    visit(dep, deps, visited, visiting, result);
                }
            }
            visiting.remove(name);
            visited.insert(name.to_string());
            result.push(name.to_string());
        }

        // Sort formula names for deterministic ordering
        let mut names: Vec<&String> = formulas.keys().collect();
        names.sort();
        for name in names {
            visit(name, &deps, &mut visited, &mut visiting, &mut result);
        }

        result
    }
}
