use std::collections::{BTreeMap, BTreeSet};

use crate::expressions::ast::Expr;

use super::model::{Query, Selection};
use crate::v03::{cel, Diagnostic};

pub(crate) struct CompiledQuery {
    pub query: Query,
    pub projections: Vec<(String, Expr)>,
    pub where_expression: Option<Expr>,
    pub selections: Vec<CompiledSelection>,
    pub summary_functions: BTreeMap<String, Expr>,
}

pub(crate) enum CompiledSelection {
    Field { source: String, name: String },
    Expression { expression: Expr, name: String },
}

pub(crate) fn compile(query: Query) -> Result<CompiledQuery, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    let mut parsed_projections = BTreeMap::new();
    for (name, projection) in &query.projections {
        match cel::compile(&projection.expr) {
            Ok(expression) => {
                check_system_bindings(
                    &expression,
                    &format!("projections.{name}.expr"),
                    QueryExpressionContext::Record,
                    &mut diagnostics,
                );
                parsed_projections.insert(name.clone(), expression);
            }
            Err(error) => diagnostics.push(invalid_query(
                format!("projections.{name}.expr"),
                format!("Projection '{name}' did not compile: {}", error.message),
                Some(error.code),
            )),
        }
    }

    let projection_order = match projection_order(&parsed_projections) {
        Ok(order) => order,
        Err(message) => {
            diagnostics.push(invalid_query("projections", message, None));
            Vec::new()
        }
    };
    let projections = projection_order
        .into_iter()
        .filter_map(|name| {
            parsed_projections
                .remove(&name)
                .map(|expression| (name, expression))
        })
        .collect();

    let where_expression =
        query
            .where_expression
            .as_ref()
            .and_then(|source| match cel::compile(source) {
                Ok(expression) => {
                    check_system_bindings(
                        &expression,
                        "where",
                        QueryExpressionContext::Record,
                        &mut diagnostics,
                    );
                    Some(expression)
                }
                Err(error) => {
                    diagnostics.push(invalid_query(
                        "where",
                        format!("Query filter did not compile: {}", error.message),
                        Some(error.code),
                    ));
                    None
                }
            });

    let mut output_names = BTreeSet::new();
    let mut selections = Vec::new();
    for (index, selection) in query.select.iter().flatten().enumerate() {
        let name = selection.output_name().to_string();
        if !output_names.insert(name.clone()) {
            diagnostics.push(invalid_query(
                format!("select.{index}"),
                format!("Selection output name '{name}' is duplicated."),
                None,
            ));
            continue;
        }
        match selection {
            Selection::Field(source) => selections.push(CompiledSelection::Field {
                source: source.clone(),
                name,
            }),
            Selection::Expression(selection) => match cel::compile(&selection.expr) {
                Ok(expression) => {
                    check_system_bindings(
                        &expression,
                        &format!("select.{index}.expr"),
                        QueryExpressionContext::Record,
                        &mut diagnostics,
                    );
                    selections.push(CompiledSelection::Expression { expression, name })
                }
                Err(error) => diagnostics.push(invalid_query(
                    format!("select.{index}.expr"),
                    format!("Selection '{name}' did not compile: {}", error.message),
                    Some(error.code),
                )),
            },
        }
    }

    let mut summary_functions = BTreeMap::new();
    for (name, function) in &query.summary_functions {
        match cel::compile(&function.expr) {
            Ok(expression) => {
                check_system_bindings(
                    &expression,
                    &format!("summary_functions.{name}.expr"),
                    QueryExpressionContext::Summary,
                    &mut diagnostics,
                );
                summary_functions.insert(name.clone(), expression);
            }
            Err(error) => diagnostics.push(invalid_query(
                format!("summary_functions.{name}.expr"),
                format!(
                    "Summary function '{name}' did not compile: {}",
                    error.message
                ),
                Some(error.code),
            )),
        }
    }

    let mut summary_names = BTreeSet::new();
    for (index, summary) in query.summaries.iter().enumerate() {
        if !summary_names.insert(summary.output_name()) {
            diagnostics.push(invalid_query(
                format!("summaries.{index}"),
                format!(
                    "Summary output name '{}' is duplicated.",
                    summary.output_name()
                ),
                None,
            ));
        }
        if !is_builtin_summary(&summary.function)
            && !query.summary_functions.contains_key(&summary.function)
        {
            diagnostics.push(invalid_query(
                format!("summaries.{index}.function"),
                format!("Unknown summary function '{}'.", summary.function),
                None,
            ));
        }
    }

    if diagnostics.is_empty() {
        Ok(CompiledQuery {
            query,
            projections,
            where_expression,
            selections,
            summary_functions,
        })
    } else {
        Err(diagnostics)
    }
}

fn projection_order(projections: &BTreeMap<String, Expr>) -> Result<Vec<String>, String> {
    let names = projections.keys().cloned().collect::<BTreeSet<_>>();
    let dependencies = projections
        .iter()
        .map(|(name, expression)| {
            let mut referenced = BTreeSet::new();
            collect_projection_references(expression, &mut referenced);
            if let Some(unknown) = referenced
                .iter()
                .find(|reference| !names.contains(*reference))
            {
                return Err(format!(
                    "Projection '{name}' references unknown projection '{unknown}'."
                ));
            }
            Ok((name.clone(), referenced))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;

    let mut remaining = dependencies;
    let mut resolved = BTreeSet::new();
    let mut order = Vec::new();
    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .filter(|(_, dependencies)| dependencies.is_subset(&resolved))
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        if ready.is_empty() {
            return Err("Named projections contain a dependency cycle.".to_string());
        }
        for name in ready {
            remaining.remove(&name);
            resolved.insert(name.clone());
            order.push(name);
        }
    }
    Ok(order)
}

fn collect_projection_references(expression: &Expr, references: &mut BTreeSet<String>) {
    match expression {
        Expr::Dot(object, field) => {
            if matches!(object.as_ref(), Expr::Ident(name) if name == "projection") {
                references.insert(field.clone());
            }
            collect_projection_references(object, references);
        }
        Expr::Index(object, index) => {
            if matches!(object.as_ref(), Expr::Ident(name) if name == "projection") {
                if let Expr::Str(name) = index.as_ref() {
                    references.insert(name.clone());
                }
            }
            collect_projection_references(object, references);
            collect_projection_references(index, references);
        }
        Expr::BinOp(left, _, right) | Expr::NullCoalesce(left, right) => {
            collect_projection_references(left, references);
            collect_projection_references(right, references);
        }
        Expr::UnaryOp(_, inner) => collect_projection_references(inner, references),
        Expr::Call(function, arguments) => {
            collect_projection_references(function, references);
            for argument in arguments {
                collect_projection_references(argument, references);
            }
        }
        Expr::Conditional(condition, then_expression, else_expression) => {
            collect_projection_references(condition, references);
            collect_projection_references(then_expression, references);
            collect_projection_references(else_expression, references);
        }
        Expr::Array(values) => {
            for value in values {
                collect_projection_references(value, references);
            }
        }
        Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::Str(_) | Expr::Ident(_) => {}
    }
}

#[derive(Clone, Copy)]
enum QueryExpressionContext {
    Record,
    Summary,
}

const SYSTEM_BINDINGS: &[&str] = &[
    "record",
    "raw",
    "present",
    "file",
    "note",
    "projection",
    "this",
    "values",
    "old",
    "operation",
    "event",
    "workflow",
    "trigger",
    "steps",
    "vars",
    "item",
];

fn check_system_bindings(
    expression: &Expr,
    field: &str,
    context: QueryExpressionContext,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut identifiers = BTreeSet::new();
    collect_identifiers(expression, &mut identifiers);
    let allowed: &[&str] = match context {
        QueryExpressionContext::Record => &[
            "record",
            "raw",
            "present",
            "file",
            "note",
            "projection",
            "this",
        ],
        QueryExpressionContext::Summary => &["values"],
    };
    for identifier in identifiers {
        if SYSTEM_BINDINGS.contains(&identifier.as_str()) && !allowed.contains(&identifier.as_str())
        {
            diagnostics.push(invalid_query(
                field,
                format!(
                    "System binding '{identifier}' is unavailable in this query expression context."
                ),
                None,
            ));
        }
    }
}

fn collect_identifiers(expression: &Expr, identifiers: &mut BTreeSet<String>) {
    match expression {
        Expr::Ident(name) => {
            identifiers.insert(name.clone());
        }
        Expr::Dot(object, _) | Expr::UnaryOp(_, object) => {
            collect_identifiers(object, identifiers);
        }
        Expr::Index(left, right)
        | Expr::BinOp(left, _, right)
        | Expr::NullCoalesce(left, right) => {
            collect_identifiers(left, identifiers);
            collect_identifiers(right, identifiers);
        }
        Expr::Call(function, arguments) => {
            collect_identifiers(function, identifiers);
            for argument in arguments {
                collect_identifiers(argument, identifiers);
            }
        }
        Expr::Conditional(condition, then_expression, else_expression) => {
            collect_identifiers(condition, identifiers);
            collect_identifiers(then_expression, identifiers);
            collect_identifiers(else_expression, identifiers);
        }
        Expr::Array(values) => {
            for value in values {
                collect_identifiers(value, identifiers);
            }
        }
        Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::Str(_) => {}
    }
}

pub(crate) fn is_builtin_summary(name: &str) -> bool {
    matches!(
        name,
        "count"
            | "sum"
            | "average"
            | "minimum"
            | "maximum"
            | "earliest"
            | "latest"
            | "empty"
            | "filled"
    )
}

fn invalid_query(
    field: impl Into<String>,
    message: impl Into<String>,
    evaluator_code: Option<String>,
) -> Diagnostic {
    let mut diagnostic = Diagnostic::error("invalid_query", message, None);
    diagnostic.field = Some(field.into());
    diagnostic.details = evaluator_code.map(|code| serde_json::json!({"evaluator_code": code}));
    diagnostic
}
