//! Shared projection and selection evaluation for local and hosted candidates.
use super::{
    context::namespace_value,
    diagnostics,
    preflight::{CompiledQuery, CompiledSelection},
};
use crate::{
    cel,
    diagnostic::Diagnostic,
    expressions::evaluator::{EvalContext, EvaluationClock},
};
use serde_json::{Map, Value};

pub(crate) fn projections(
    compiled: &CompiledQuery,
    context: &mut EvalContext,
    clock: &EvaluationClock,
    path: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Map<String, Value> {
    let mut values = Map::new();
    for (name, expression) in &compiled.projections {
        bind(context, &values);
        let value = cel::evaluate_compiled(expression, context, clock).unwrap_or_else(|error| {
            diagnostics.push(diagnostics::evaluation(
                path,
                &format!("projections.{name}"),
                "query_projection",
                error,
                None,
            ));
            Value::Null
        });
        values.insert(name.clone(), value);
    }
    bind(context, &values);
    values
}
fn bind(context: &mut EvalContext, projections: &Map<String, Value>) {
    if let Some(bindings) = context.frontmatter.as_object_mut() {
        bindings.insert("projection".into(), Value::Object(projections.clone()));
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn selections(
    compiled: &CompiledQuery,
    context: &EvalContext,
    clock: &EvaluationClock,
    path: &str,
    effective: &Value,
    file: &Value,
    projections: &Map<String, Value>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Map<String, Value> {
    let mut values = Map::new();
    for selection in &compiled.selections {
        let (name, value) = match selection {
            CompiledSelection::Field { source, name } => (
                name,
                namespace_value(source, effective, projections, &values, file),
            ),
            CompiledSelection::Expression { expression, name } => {
                let value =
                    cel::evaluate_compiled(expression, context, clock).unwrap_or_else(|error| {
                        diagnostics.push(diagnostics::evaluation(
                            path,
                            &format!("select.{name}"),
                            "query_selection",
                            error,
                            None,
                        ));
                        Value::Null
                    });
                (name, value)
            }
        };
        values.insert(name.clone(), value);
    }
    values
}
