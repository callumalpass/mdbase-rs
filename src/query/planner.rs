//! SQL vs Rust evaluation decision.

use crate::expressions::ast::Expr;
use crate::expressions::evaluator::{evaluate as eval_expr, EvalContext};
use crate::expressions::parser::Parser as ExprParser;
use crate::Collection;
use std::collections::{HashMap, HashSet};

impl Collection {
    /// Validate formula expressions for syntax errors and circular references.
    pub(crate) fn validate_formulas(
        &self,
        formulas: &HashMap<String, String>,
    ) -> Result<(), serde_json::Value> {
        let parsed_formulas = self.parse_formulas(formulas)?;
        let deps = Self::build_formula_deps(formulas, &parsed_formulas);

        // Self-reference check
        for (name, expr) in &parsed_formulas {
            let refs = Self::extract_formula_refs(expr);
            if refs.contains(name) {
                return Err(serde_json::json!({
                    "error": { "code": "circular_formula", "message": format!("Formula '{}' references itself", name) }
                }));
            }
        }

        // Cycle check
        let mut visited = HashSet::new();
        let mut visiting = HashSet::new();
        for name in formulas.keys() {
            if Self::formula_cycle_from(name, &deps, &mut visited, &mut visiting) {
                return Err(serde_json::json!({
                    "error": { "code": "circular_formula", "message": format!("Circular formula reference involving '{}'", name) }
                }));
            }
        }

        // Validate each formula expression semantics
        for (name, parsed) in &parsed_formulas {
            // Check for literal division by zero
            if Self::has_literal_div_by_zero(parsed) {
                return Err(serde_json::json!({
                    "error": { "code": "formula_evaluation_error", "message": format!("Formula '{}': Division by zero", name) }
                }));
            }
            // Try evaluating with empty context to catch static errors
            let ctx = EvalContext::empty();
            match eval_expr(parsed, &ctx) {
                Ok(_) => {}
                Err(e) => match e.code.as_str() {
                    "unknown_function" | "wrong_argument_count" => {
                        return Err(serde_json::json!({
                            "error": { "code": "formula_evaluation_error", "message": format!("Formula '{}': {}", name, e.message) }
                        }));
                    }
                    _ => {} // Other errors might depend on per-file context
                },
            }
        }

        Ok(())
    }

    fn parse_formulas(
        &self,
        formulas: &HashMap<String, String>,
    ) -> Result<HashMap<String, Expr>, serde_json::Value> {
        let mut parsed_formulas = HashMap::new();
        for (name, expr_str) in formulas {
            match ExprParser::parse(expr_str) {
                Ok(parsed) => {
                    parsed_formulas.insert(name.clone(), parsed);
                }
                Err(msg) => {
                    return Err(serde_json::json!({
                        "error": { "code": "invalid_formula", "message": format!("Formula '{}': {}", name, msg) }
                    }));
                }
            }
        }
        Ok(parsed_formulas)
    }

    fn extract_formula_refs(expr: &Expr) -> HashSet<String> {
        let mut refs = HashSet::new();
        Self::extract_formula_refs_into(expr, &mut refs);
        refs
    }

    fn extract_formula_refs_into(expr: &Expr, refs: &mut HashSet<String>) {
        use crate::expressions::ast::{BinOp, Expr as AstExpr, UnaryOp};
        match expr {
            AstExpr::Dot(obj, field) => {
                if let AstExpr::Ident(name) = obj.as_ref() {
                    if name == "formula" {
                        refs.insert(field.clone());
                    }
                }
                Self::extract_formula_refs_into(obj, refs);
            }
            AstExpr::Index(obj, idx) => {
                if let AstExpr::Ident(name) = obj.as_ref() {
                    if name == "formula" {
                        if let AstExpr::Str(s) = idx.as_ref() {
                            refs.insert(s.clone());
                        }
                    }
                }
                Self::extract_formula_refs_into(obj, refs);
                Self::extract_formula_refs_into(idx, refs);
            }
            AstExpr::BinOp(left, BinOp::Add, right)
            | AstExpr::BinOp(left, BinOp::Sub, right)
            | AstExpr::BinOp(left, BinOp::Mul, right)
            | AstExpr::BinOp(left, BinOp::Div, right)
            | AstExpr::BinOp(left, BinOp::Mod, right)
            | AstExpr::BinOp(left, BinOp::Eq, right)
            | AstExpr::BinOp(left, BinOp::Neq, right)
            | AstExpr::BinOp(left, BinOp::Lt, right)
            | AstExpr::BinOp(left, BinOp::Gt, right)
            | AstExpr::BinOp(left, BinOp::Lte, right)
            | AstExpr::BinOp(left, BinOp::Gte, right)
            | AstExpr::BinOp(left, BinOp::And, right)
            | AstExpr::BinOp(left, BinOp::Or, right) => {
                Self::extract_formula_refs_into(left, refs);
                Self::extract_formula_refs_into(right, refs);
            }
            AstExpr::UnaryOp(UnaryOp::Not, inner) | AstExpr::UnaryOp(UnaryOp::Neg, inner) => {
                Self::extract_formula_refs_into(inner, refs);
            }
            AstExpr::NullCoalesce(left, right) => {
                Self::extract_formula_refs_into(left, refs);
                Self::extract_formula_refs_into(right, refs);
            }
            AstExpr::Call(base, args) => {
                Self::extract_formula_refs_into(base, refs);
                for arg in args {
                    Self::extract_formula_refs_into(arg, refs);
                }
            }
            AstExpr::Conditional(cond, then_expr, else_expr) => {
                Self::extract_formula_refs_into(cond, refs);
                Self::extract_formula_refs_into(then_expr, refs);
                Self::extract_formula_refs_into(else_expr, refs);
            }
            AstExpr::Array(items) => {
                for item in items {
                    Self::extract_formula_refs_into(item, refs);
                }
            }
            AstExpr::Null
            | AstExpr::Bool(_)
            | AstExpr::Number(_)
            | AstExpr::Str(_)
            | AstExpr::Ident(_) => {}
        }
    }

    fn build_formula_deps(
        formulas: &HashMap<String, String>,
        parsed_formulas: &HashMap<String, Expr>,
    ) -> HashMap<String, Vec<String>> {
        let mut deps: HashMap<String, Vec<String>> = HashMap::new();
        for (name, parsed) in parsed_formulas {
            let mut name_deps: Vec<String> = Self::extract_formula_refs(parsed)
                .into_iter()
                .filter(|dep| dep != name && formulas.contains_key(dep))
                .collect();
            name_deps.sort();
            deps.insert(name.clone(), name_deps);
        }
        for name in formulas.keys() {
            deps.entry(name.clone()).or_default();
        }
        deps
    }

    fn formula_cycle_from(
        name: &str,
        deps: &HashMap<String, Vec<String>>,
        visited: &mut HashSet<String>,
        visiting: &mut HashSet<String>,
    ) -> bool {
        if visited.contains(name) {
            return false;
        }
        if visiting.contains(name) {
            return true;
        }
        visiting.insert(name.to_string());
        if let Some(children) = deps.get(name) {
            for dep in children {
                if Self::formula_cycle_from(dep, deps, visited, visiting) {
                    return true;
                }
            }
        }
        visiting.remove(name);
        visited.insert(name.to_string());
        false
    }

    /// Check if an expression AST contains a literal division by zero (e.g., `x / 0`).
    pub(crate) fn has_literal_div_by_zero(expr: &crate::expressions::ast::Expr) -> bool {
        use crate::expressions::ast::{BinOp, Expr};
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
                Self::has_literal_div_by_zero(base)
                    || args.iter().any(Self::has_literal_div_by_zero)
            }
            Expr::Conditional(cond, then, else_) => {
                Self::has_literal_div_by_zero(cond)
                    || Self::has_literal_div_by_zero(then)
                    || Self::has_literal_div_by_zero(else_)
            }
            Expr::Dot(inner, _) => Self::has_literal_div_by_zero(inner),
            Expr::Index(left, right) => {
                Self::has_literal_div_by_zero(left) || Self::has_literal_div_by_zero(right)
            }
            Expr::Array(items) => items.iter().any(Self::has_literal_div_by_zero),
            _ => false,
        }
    }

    /// Sort formulas in dependency order (topological sort).
    pub(crate) fn topological_sort_formulas(
        &self,
        formulas: &HashMap<String, String>,
    ) -> Vec<String> {
        let mut parsed_formulas = HashMap::new();
        for (name, expr) in formulas {
            if let Ok(parsed) = ExprParser::parse(expr) {
                parsed_formulas.insert(name.clone(), parsed);
            }
        }
        let deps = Self::build_formula_deps(formulas, &parsed_formulas);

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
            if visited.contains(name) {
                return;
            }
            if visiting.contains(name) {
                return;
            } // circular, handled elsewhere
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
