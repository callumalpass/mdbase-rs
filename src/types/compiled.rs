//! Immutable expression plans compiled with the type registry.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use crate::errors::{CIRCULAR_COMPUTED, INVALID_TYPE_DEFINITION};
use crate::expressions::ast::Expr;
use crate::expressions::expr_contains_ident;
use crate::expressions::expr_reads_body_prose;
use crate::expressions::parser::Parser;

use super::schema::{FieldDef, TypeDef};

#[derive(Debug, Clone)]
pub(crate) struct CompiledComputed {
    pub expression: Arc<Expr>,
    pub dependencies: Vec<String>,
    /// Whether evaluating this field can copy exact body prose into its value.
    /// Hosted projections must omit such values and use canonical exact fallback.
    pub body_dependent: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CompiledTypePlan {
    pub computed: BTreeMap<String, CompiledComputed>,
    pub computed_order: Vec<String>,
    pub match_expression: Option<Arc<Expr>>,
    lifecycle_guards: HashMap<(String, usize), Arc<Expr>>,
}

impl CompiledTypePlan {
    pub(crate) fn lifecycle_guard(&self, event: &str, index: usize) -> Option<&Expr> {
        self.lifecycle_guards
            .get(&(event.to_string(), index))
            .map(AsRef::as_ref)
    }
}

pub(crate) fn compile_registry(
    types: &HashMap<String, TypeDef>,
) -> Result<HashMap<String, CompiledTypePlan>, CompileTypeError> {
    types
        .iter()
        .map(|(name, definition)| compile_type(definition).map(|plan| (name.clone(), plan)))
        .collect()
}

fn compile_type(type_def: &TypeDef) -> Result<CompiledTypePlan, CompileTypeError> {
    let mut plan = CompiledTypePlan::default();
    let computed_names = type_def
        .fields
        .iter()
        .filter_map(|(name, field)| field.computed.as_ref().map(|_| name.clone()))
        .collect::<BTreeSet<_>>();

    for (field_name, field) in &type_def.fields {
        let Some(source) = &field.computed else {
            continue;
        };
        validate_computed_constraints(type_def, field_name, field)?;
        let expression = Parser::parse(source).map_err(|message| {
            CompileTypeError::invalid(
                type_def,
                format!("Computed field '{field_name}' is invalid: {message}"),
            )
        })?;
        let dependencies = computed_names
            .iter()
            .filter(|candidate| expr_contains_ident(&expression, candidate))
            .cloned()
            .collect();
        plan.computed.insert(
            field_name.clone(),
            CompiledComputed {
                body_dependent: expr_reads_body_prose(&expression),
                expression: Arc::new(expression),
                dependencies,
            },
        );
    }
    plan.computed_order = computed_order(type_def, &plan.computed)?;
    for field_name in &plan.computed_order {
        let body_dependent = plan.computed.get(field_name).is_some_and(|field| {
            field.body_dependent
                || field.dependencies.iter().any(|dependency| {
                    plan.computed
                        .get(dependency)
                        .is_some_and(|dependency| dependency.body_dependent)
                })
        });
        if body_dependent {
            plan.computed
                .get_mut(field_name)
                .expect("computed order only names compiled fields")
                .body_dependent = true;
        }
    }
    validate_match_fields(type_def, &computed_names)?;

    if let Some(source) = type_def
        .match_rules
        .as_ref()
        .and_then(|rules| rules.match_expr.as_deref())
    {
        let expression = crate::v03::cel::compile(source).map_err(|error| {
            CompileTypeError::invalid(
                type_def,
                format!("Match expression is invalid: {}", error.message),
            )
        })?;
        plan.match_expression = Some(Arc::new(expression));
    }

    if let Some(lifecycle) = type_def
        .lifecycle
        .as_ref()
        .and_then(|value| value.as_object())
    {
        for event in ["on_create", "on_update"] {
            let Some(policy) = lifecycle.get(event) else {
                continue;
            };
            let actions = match policy {
                serde_json::Value::Array(actions) => actions.iter().collect::<Vec<_>>(),
                action => vec![action],
            };
            for (index, action) in actions.into_iter().enumerate() {
                let Some(source) = action.get("if").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let expression = crate::v03::cel::compile(source).map_err(|error| {
                    CompileTypeError::invalid(
                        type_def,
                        format!(
                            "Lifecycle guard at lifecycle.{event}[{index}] is invalid: {}",
                            error.message
                        ),
                    )
                })?;
                plan.lifecycle_guards
                    .insert((event.to_string(), index), Arc::new(expression));
            }
        }
    }

    Ok(plan)
}

fn validate_computed_constraints(
    type_def: &TypeDef,
    field_name: &str,
    field: &FieldDef,
) -> Result<(), CompileTypeError> {
    let conflict = if field.required {
        Some("required")
    } else if field.default.is_some() {
        Some("a default")
    } else if field.generated.is_some() {
        Some("a generated strategy")
    } else {
        None
    };
    if let Some(conflict) = conflict {
        return Err(CompileTypeError::invalid(
            type_def,
            format!("Computed field '{field_name}' cannot have {conflict}"),
        ));
    }
    Ok(())
}

fn validate_match_fields(
    type_def: &TypeDef,
    computed_names: &BTreeSet<String>,
) -> Result<(), CompileTypeError> {
    let Some(rules) = &type_def.match_rules else {
        return Ok(());
    };
    if let Some(where_clause) = rules
        .where_clause
        .as_ref()
        .and_then(|value| value.as_object())
    {
        if let Some(name) = where_clause
            .keys()
            .find(|name| computed_names.contains(*name))
        {
            return Err(CompileTypeError::invalid(
                type_def,
                format!("Match rule 'where' cannot reference computed field '{name}'"),
            ));
        }
    }
    if let Some(fields) = &rules.fields_present {
        if let Some(name) = fields.iter().find(|name| computed_names.contains(*name)) {
            return Err(CompileTypeError::invalid(
                type_def,
                format!("Match rule 'fields_present' cannot reference computed field '{name}'"),
            ));
        }
    }
    Ok(())
}

fn computed_order(
    type_def: &TypeDef,
    computed: &BTreeMap<String, CompiledComputed>,
) -> Result<Vec<String>, CompileTypeError> {
    fn visit(
        name: &str,
        computed: &BTreeMap<String, CompiledComputed>,
        visiting: &mut HashSet<String>,
        visited: &mut HashSet<String>,
        order: &mut Vec<String>,
    ) -> bool {
        if visited.contains(name) {
            return true;
        }
        if !visiting.insert(name.to_string()) {
            return false;
        }
        if let Some(field) = computed.get(name) {
            for dependency in &field.dependencies {
                if !visit(dependency, computed, visiting, visited, order) {
                    return false;
                }
            }
        }
        visiting.remove(name);
        visited.insert(name.to_string());
        order.push(name.to_string());
        true
    }

    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    let mut order = Vec::new();
    for name in computed.keys() {
        if !visit(name, computed, &mut visiting, &mut visited, &mut order) {
            return Err(CompileTypeError {
                code: CIRCULAR_COMPUTED,
                message: format!(
                    "Type '{}' contains a circular computed-field dependency",
                    type_def.name
                ),
            });
        }
    }
    Ok(order)
}

#[derive(Debug)]
pub(crate) struct CompileTypeError {
    pub code: &'static str,
    pub message: String,
}

impl CompileTypeError {
    fn invalid(type_def: &TypeDef, message: String) -> Self {
        Self {
            code: INVALID_TYPE_DEFINITION,
            message: format!("Type '{}': {message}", type_def.name),
        }
    }
}
