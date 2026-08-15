use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use super::context::complete_file_value;
use super::diagnostics;
use super::model::{Candidate, Direction, FrontmatterMode, OrderBy, Query};
use super::preflight::{self, CompiledQuery};
use crate::expressions::evaluator::{EvalContext, EvaluationClock};
use crate::v03::{cel, Diagnostic};

pub(super) fn sort_candidates(candidates: &mut [Candidate], order_by: &[OrderBy]) {
    candidates.sort_by(|left, right| {
        for order in order_by {
            let comparison = compare_values(
                &candidate_value(left, &order.field),
                &candidate_value(right, &order.field),
                order.direction,
            );
            if comparison != Ordering::Equal {
                return comparison;
            }
        }
        left.path.cmp(&right.path)
    });
}

pub(super) fn build_groups(
    candidates: &[Candidate],
    compiled: &CompiledQuery,
    clock: &EvaluationClock,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Vec<Value>> {
    if compiled.query.group_by.is_empty() && compiled.query.summaries.is_empty() {
        return None;
    }
    if compiled.query.group_by.is_empty() {
        return Some(vec![json!({
            "values": {},
            "count": candidates.len(),
            "summaries": summarize(candidates.iter(), compiled, clock, diagnostics),
        })]);
    }

    let mut groups = BTreeMap::<String, (Map<String, Value>, Vec<&Candidate>)>::new();
    for candidate in candidates {
        let values = compiled
            .query
            .group_by
            .iter()
            .map(|group| {
                (
                    group.field.clone(),
                    candidate_value(candidate, &group.field),
                )
            })
            .collect::<Map<_, _>>();
        let key = serde_json::to_string(&values).expect("JSON values always serialize");
        groups
            .entry(key)
            .or_insert_with(|| (values, Vec::new()))
            .1
            .push(candidate);
    }
    let mut groups = groups.into_values().collect::<Vec<_>>();
    groups.sort_by(|(left, _), (right, _)| {
        for group in &compiled.query.group_by {
            let comparison = compare_values(
                left.get(&group.field).unwrap_or(&Value::Null),
                right.get(&group.field).unwrap_or(&Value::Null),
                group.direction,
            );
            if comparison != Ordering::Equal {
                return comparison;
            }
        }
        Ordering::Equal
    });
    Some(
        groups
            .into_iter()
            .map(|(values, candidates)| {
                json!({
                    "values": values,
                    "count": candidates.len(),
                    "summaries": summarize(candidates.into_iter(), compiled, clock, diagnostics),
                })
            })
            .collect(),
    )
}

pub(crate) fn serialize_candidate(candidate: &Candidate, query: &Query) -> Value {
    let mut file = candidate.file.clone();
    complete_file_value(&mut file, &candidate.effective, &candidate.body);
    let mut result = Map::from_iter([
        ("path".to_string(), Value::String(candidate.path.clone())),
        ("file".to_string(), file),
        (
            "types".to_string(),
            Value::Array(candidate.types.iter().cloned().map(Value::String).collect()),
        ),
    ]);
    match query.frontmatter_mode {
        FrontmatterMode::Effective => {
            result.insert(
                "effective_frontmatter".to_string(),
                candidate.effective.clone(),
            );
        }
        FrontmatterMode::Persisted => {
            result.insert("frontmatter".to_string(), candidate.raw.clone());
        }
        FrontmatterMode::Both => {
            result.insert("frontmatter".to_string(), candidate.raw.clone());
            result.insert(
                "effective_frontmatter".to_string(),
                candidate.effective.clone(),
            );
        }
    }
    if query.select.is_some() {
        result.insert(
            "values".to_string(),
            Value::Object(candidate.values.clone()),
        );
    }
    if query.include_body {
        result.insert("body".to_string(), Value::String(candidate.body.clone()));
    }
    Value::Object(result)
}

fn candidate_value(candidate: &Candidate, field: &str) -> Value {
    if let Some(name) = field.strip_prefix("projection.") {
        return candidate
            .projections
            .get(name)
            .cloned()
            .unwrap_or(Value::Null);
    }
    if let Some(name) = field.strip_prefix("file.") {
        return candidate.file.get(name).cloned().unwrap_or(Value::Null);
    }
    candidate
        .values
        .get(field)
        .or_else(|| candidate.effective.get(field))
        .cloned()
        .unwrap_or(Value::Null)
}

pub(super) fn compare_values(left: &Value, right: &Value, direction: Direction) -> Ordering {
    let ascending = match (left.is_null(), right.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => match (left, right) {
            (Value::Number(left), Value::Number(right)) => left
                .as_f64()
                .partial_cmp(&right.as_f64())
                .unwrap_or(Ordering::Equal),
            (Value::String(left), Value::String(right)) => left.cmp(right),
            (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
            (Value::Array(left), Value::Array(right)) => left.len().cmp(&right.len()),
            (Value::Object(left), Value::Object(right)) => left.len().cmp(&right.len()),
            _ => Ordering::Equal,
        },
    };
    match direction {
        Direction::Asc => ascending,
        Direction::Desc => ascending.reverse(),
    }
}

fn summarize<'a>(
    candidates: impl Iterator<Item = &'a Candidate>,
    compiled: &CompiledQuery,
    clock: &EvaluationClock,
    diagnostics: &mut Vec<Diagnostic>,
) -> Value {
    let candidates = candidates.collect::<Vec<_>>();
    let mut output = Map::new();
    for summary in &compiled.query.summaries {
        let values = candidates
            .iter()
            .map(|candidate| candidate_value(candidate, &summary.field))
            .collect::<Vec<_>>();
        let result = if preflight::is_builtin_summary(&summary.function) {
            builtin_summary(&summary.function, &values).map_err(|message| Diagnostic {
                severity: "warning".to_string(),
                code: "expression_evaluation_error".to_string(),
                message,
                path: None,
                field: Some(format!("summaries.{}", summary.output_name())),
                type_name: None,
                schema_location: None,
                details: Some(json!({"context": "query_summary"})),
            })
        } else {
            let expression = &compiled.summary_functions[&summary.function];
            let mut context = EvalContext::empty();
            context.frontmatter = json!({"values": values});
            context.string_concat = false;
            cel::evaluate_compiled(expression, &context, clock).map_err(|error| {
                diagnostics::evaluation(
                    "query",
                    &format!("summaries.{}", summary.output_name()),
                    "query_summary",
                    error,
                    None,
                )
            })
        };
        match result {
            Ok(value) => {
                output.insert(summary.output_name().to_string(), value);
            }
            Err(diagnostic) => {
                diagnostics.push(diagnostic);
                output.insert(summary.output_name().to_string(), Value::Null);
            }
        }
    }
    Value::Object(output)
}

fn builtin_summary(function: &str, values: &[Value]) -> Result<Value, String> {
    if function == "count" {
        return Ok(json!(values.len()));
    }
    if matches!(function, "empty" | "filled") {
        let empty = values.iter().filter(|value| is_empty(value)).count();
        return Ok(json!(if function == "empty" {
            empty
        } else {
            values.len() - empty
        }));
    }
    let non_null = values
        .iter()
        .filter(|value| !value.is_null())
        .collect::<Vec<_>>();
    if non_null.is_empty() {
        return Ok(Value::Null);
    }
    match function {
        "sum" | "average" | "minimum" | "maximum" => {
            let numbers = non_null
                .iter()
                .map(|value| value.as_f64())
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| format!("Summary '{function}' received a non-numeric value."))?;
            let value = match function {
                "sum" => numbers.iter().sum(),
                "average" => numbers.iter().sum::<f64>() / numbers.len() as f64,
                "minimum" => numbers.iter().copied().fold(f64::INFINITY, f64::min),
                "maximum" => numbers.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                _ => unreachable!(),
            };
            Ok(number_value(value))
        }
        "earliest" | "latest" => {
            let strings = non_null
                .iter()
                .map(|value| value.as_str())
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| format!("Summary '{function}' received a non-string value."))?;
            let selected = if function == "earliest" {
                strings.into_iter().min()
            } else {
                strings.into_iter().max()
            };
            Ok(selected.map_or(Value::Null, |value| json!(value)))
        }
        _ => unreachable!("preflight accepts only known built-ins"),
    }
}

fn number_value(value: f64) -> Value {
    if value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
        json!(value as i64)
    } else {
        serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(value) => value.is_empty(),
        Value::Array(value) => value.is_empty(),
        Value::Object(value) => value.is_empty(),
        Value::Bool(_) | Value::Number(_) => false,
    }
}
