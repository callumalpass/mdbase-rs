//! Expression parsing and evaluation (§11).

pub mod ast;
pub mod compiler;
pub mod evaluator;
pub mod parser;

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
            expr_contains_ident(c, name)
                || expr_contains_ident(t, name)
                || expr_contains_ident(e, name)
        }
        _ => false,
    }
}

/// Whether an expression reads exact body prose rather than projected structural
/// facts such as `file.tags`, `file.links`, or `file.embeds`.
pub(crate) fn expr_reads_body_prose(expr: &crate::expressions::ast::Expr) -> bool {
    use crate::expressions::ast::Expr;
    match expr {
        Expr::Dot(object, field) => {
            (field == "body"
                && matches!(
                    object.as_ref(),
                    Expr::Ident(name) if name == "file"
                ))
                || expr_reads_body_prose(object)
        }
        Expr::Index(object, index) => {
            (matches!(object.as_ref(), Expr::Ident(name) if name == "file")
                && matches!(index.as_ref(), Expr::Str(field) if field == "body"))
                || expr_reads_body_prose(object)
                || expr_reads_body_prose(index)
        }
        Expr::BinOp(left, _, right) | Expr::NullCoalesce(left, right) => {
            expr_reads_body_prose(left) || expr_reads_body_prose(right)
        }
        Expr::UnaryOp(_, expression) => expr_reads_body_prose(expression),
        Expr::Array(elements) => elements.iter().any(expr_reads_body_prose),
        Expr::Call(function, arguments) => {
            expr_reads_body_prose(function) || arguments.iter().any(expr_reads_body_prose)
        }
        Expr::Conditional(condition, then_expression, else_expression) => {
            expr_reads_body_prose(condition)
                || expr_reads_body_prose(then_expression)
                || expr_reads_body_prose(else_expression)
        }
        _ => false,
    }
}

/// Check if a JSON value is truthy (for where clause evaluation).
pub(crate) fn is_truthy_value(val: &serde_json::Value) -> bool {
    match val {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(_) => true,
    }
}

use crate::expressions::evaluator::{evaluate as eval_expr, EvalContext};
use crate::types::compiled::CompiledComputed;
use crate::Collection;
use std::collections::{BTreeMap, BTreeSet, HashSet};

#[cfg(all(test, feature = "legacy-collection-mutation"))]
thread_local! {
    static COMPUTED_FIELD_EVALUATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(all(test, feature = "legacy-collection-mutation"))]
pub(crate) fn reset_computed_field_evaluations_for_test() {
    COMPUTED_FIELD_EVALUATIONS.with(|value| value.set(0));
}

#[cfg(all(test, feature = "legacy-collection-mutation"))]
pub(crate) fn computed_field_evaluations_for_test() -> usize {
    COMPUTED_FIELD_EVALUATIONS.with(std::cell::Cell::get)
}

impl Collection {
    /// Evaluate computed fields for a read result (§5.12).
    pub(crate) fn evaluate_computed_fields(
        &self,
        mut frontmatter: serde_json::Value,
        type_names: &[String],
        path: &str,
        body: Option<&str>,
    ) -> serde_json::Value {
        #[cfg(all(test, feature = "legacy-collection-mutation"))]
        COMPUTED_FIELD_EVALUATIONS.with(|value| value.set(value.get() + 1));
        let mut ordered_types = type_names.to_vec();
        ordered_types.sort();
        ordered_types.dedup();
        let mut computed = BTreeMap::<String, CompiledComputed>::new();
        for type_name in &ordered_types {
            if let Some(plan) = self.type_plans.get(type_name) {
                for (field_name, field) in &plan.computed {
                    computed
                        .entry(field_name.clone())
                        .or_insert_with(|| field.clone());
                }
            }
        }

        if computed.is_empty() {
            return frontmatter;
        }

        let order = if ordered_types.len() == 1 {
            self.type_plans
                .get(&ordered_types[0])
                .map(|plan| plan.computed_order.clone())
                .unwrap_or_default()
        } else {
            combined_computed_order(&computed)
        };

        // Evaluate computed fields in dependency order
        for field_name in order {
            if let Some(field) = computed.get(&field_name) {
                let ctx = EvalContext {
                    frontmatter: frontmatter.clone(),
                    raw_frontmatter: None,
                    file_path: Some(path.to_string()),
                    body: body.map(String::from),
                    file_size: None,
                    file_mtime: None,
                    file_ctime: None,
                    this_context: None,
                    all_files: None,
                    traversal_depth: std::cell::Cell::new(0),
                    backlinks_index: None,
                    type_names: None,
                    types: None,
                    note_namespace_source: Default::default(),
                    string_concat: true,
                };
                match eval_expr(&field.expression, &ctx) {
                    Ok(value) => {
                        if let Some(obj) = frontmatter.as_object_mut() {
                            obj.insert(field_name, value);
                        }
                    }
                    Err(_) => {
                        if let Some(obj) = frontmatter.as_object_mut() {
                            obj.insert(field_name, serde_json::Value::Null);
                        }
                    }
                }
            }
        }

        frontmatter
    }
}

fn combined_computed_order(computed: &BTreeMap<String, CompiledComputed>) -> Vec<String> {
    fn visit(
        name: &str,
        dependencies: &BTreeMap<String, BTreeSet<String>>,
        visiting: &mut HashSet<String>,
        visited: &mut HashSet<String>,
        order: &mut Vec<String>,
    ) {
        if visited.contains(name) || !visiting.insert(name.to_string()) {
            return;
        }
        if let Some(fields) = dependencies.get(name) {
            for dependency in fields {
                visit(dependency, dependencies, visiting, visited, order);
            }
        }
        visiting.remove(name);
        visited.insert(name.to_string());
        order.push(name.to_string());
    }

    let names = computed.keys().collect::<BTreeSet<_>>();
    let dependencies = computed
        .iter()
        .map(|(name, field)| {
            let dependencies = names
                .iter()
                .filter(|candidate| expr_contains_ident(&field.expression, candidate))
                .map(|name| (*name).clone())
                .collect();
            (name.clone(), dependencies)
        })
        .collect::<BTreeMap<_, _>>();
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    let mut order = Vec::new();
    for name in computed.keys() {
        visit(name, &dependencies, &mut visiting, &mut visited, &mut order);
    }
    order
}
