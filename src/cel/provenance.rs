//! Link provenance for CEL link helpers (spec Chapter 10).
//!
//! A link value resolves relative to the record it was read from. Link values
//! are plain strings, so the source is recovered statically: a value read from
//! `this` resolves relative to the context record, and a value read from an
//! `asFile()` result resolves relative to that target record. Values read from
//! the candidate need no change.
//!
//! The rewrite passes the source path explicitly, as `link.asFile(sourcePath)`
//! and `file.hasLink(link, sourcePath)`. An `asFile()` result that supplies
//! another link is bound once through a single-element comprehension,
//! `[target].map(r, ...)[0]`, so it is not traversed twice.

use std::collections::HashMap;

use cel::common::ast::{
    operators, CallExpr, EntryExpr, Expr, IdedEntryExpr, IdedExpr, ListExpr, MapExpr, SelectExpr,
    StructExpr,
};
use cel::parser::Parser;

/// The record an expression's value was read from.
#[derive(Clone)]
enum Source {
    /// An expression that evaluates to the source record.
    Record(IdedExpr),
    /// The `asFile()` call with this node id, whose result is the source.
    AsFile(u64),
}

type Scope = HashMap<String, Option<Source>>;

const PASS_THROUGH: [&str; 3] = ["orValue", "value", "or"];

/// Rewrite link helpers whose link comes from a record other than the
/// candidate. Returns `None` when the expression needs no change.
pub(crate) fn rewrite(ast: &IdedExpr) -> Option<IdedExpr> {
    let mut rewriter = Rewriter {
        changed: false,
        fresh: 0,
        next_id: max_id(ast) + 1,
    };
    let rewritten = rewriter.rewrite(ast, &Scope::new());
    rewriter.changed.then_some(rewritten)
}

struct Rewriter {
    changed: bool,
    fresh: usize,
    next_id: u64,
}

impl Rewriter {
    fn rewrite(&mut self, node: &IdedExpr, scope: &Scope) -> IdedExpr {
        match &node.expr {
            Expr::Call(call) if call.func_name == "asFile" && call.args.is_empty() => {
                if let Some(receiver) = &call.target {
                    match source_of(receiver, scope) {
                        Some(Source::AsFile(target)) => return self.bind(target, node, scope),
                        Some(Source::Record(record)) => {
                            self.changed = true;
                            let receiver = self.rewrite(receiver, scope);
                            let path = self.path_of(record);
                            return self.call("asFile", Some(receiver), vec![path]);
                        }
                        None => {}
                    }
                }
            }
            Expr::Call(call) if call.func_name == "hasLink" && call.args.len() == 1 => {
                if let Some(file) = &call.target {
                    match source_of(&call.args[0], scope) {
                        Some(Source::AsFile(target)) => return self.bind(target, node, scope),
                        Some(Source::Record(record)) => {
                            self.changed = true;
                            let file = self.rewrite(file, scope);
                            let link = self.rewrite(&call.args[0], scope);
                            let path = self.path_of(record);
                            return self.call("hasLink", Some(file), vec![link, path]);
                        }
                        None => {}
                    }
                }
            }
            Expr::Comprehension(comprehension) => {
                let source = source_of(&comprehension.iter_range, scope);
                if let Some(Source::AsFile(target)) = source {
                    return self.bind(target, node, scope);
                }
                let mut inner = scope.clone();
                inner.insert(comprehension.iter_var.clone(), source.clone());
                if let Some(second) = &comprehension.iter_var2 {
                    inner.insert(second.clone(), source);
                }
                inner.insert(comprehension.accu_var.clone(), None);
                let mut rewritten = comprehension.as_ref().clone();
                rewritten.iter_range = self.rewrite(&comprehension.iter_range, scope);
                rewritten.accu_init = self.rewrite(&comprehension.accu_init, scope);
                rewritten.loop_cond = self.rewrite(&comprehension.loop_cond, &inner);
                rewritten.loop_step = self.rewrite(&comprehension.loop_step, &inner);
                rewritten.result = self.rewrite(&comprehension.result, &inner);
                return IdedExpr {
                    id: node.id,
                    expr: Expr::Comprehension(Box::new(rewritten)),
                };
            }
            _ => {}
        }
        map_children(node, &mut |child| self.rewrite(child, scope))
    }

    /// Replace the `asFile()` call `target` inside `node` with a fresh
    /// variable bound to it once: `[target].map(r, node[target := r])[0]`.
    fn bind(&mut self, target: u64, node: &IdedExpr, scope: &Scope) -> IdedExpr {
        self.changed = true;
        let name = format!("__mdbase_record_{}", self.fresh);
        self.fresh += 1;
        let target_node = find(node, target)
            .expect("a provenance target is inside its node")
            .clone();
        let variable = self.ident(&name);
        let replaced = replace(node, target, &variable);
        let mut inner = scope.clone();
        inner.insert(name.clone(), Some(Source::Record(variable)));
        let body = self.rewrite(&replaced, &inner);
        let target_node = self.rewrite(&target_node, scope);

        let template = Parser::default()
            .enable_optional_syntax(true)
            .parse(&format!(
                "[__mdbase_bind_target].map({name}, __mdbase_bind_body)[0]"
            ))
            .expect("the binding template parses");
        let template = self.renumber(&template);
        let template = replace_ident(&template, "__mdbase_bind_target", &target_node);
        replace_ident(&template, "__mdbase_bind_body", &body)
    }

    fn path_of(&mut self, record: IdedExpr) -> IdedExpr {
        let file = self.select(record, "file");
        self.select(file, "path")
    }

    fn select(&mut self, operand: IdedExpr, field: &str) -> IdedExpr {
        IdedExpr {
            id: self.id(),
            expr: Expr::Select(SelectExpr {
                operand: Box::new(operand),
                field: field.to_string(),
                test: false,
            }),
        }
    }

    fn call(&mut self, name: &str, target: Option<IdedExpr>, args: Vec<IdedExpr>) -> IdedExpr {
        IdedExpr {
            id: self.id(),
            expr: Expr::Call(CallExpr {
                func_name: name.to_string(),
                target: target.map(Box::new),
                args,
            }),
        }
    }

    fn ident(&mut self, name: &str) -> IdedExpr {
        IdedExpr {
            id: self.id(),
            expr: Expr::Ident(name.to_string()),
        }
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Give a parsed template ids that do not collide with the expression's.
    fn renumber(&mut self, node: &IdedExpr) -> IdedExpr {
        let mut renumbered = map_children(node, &mut |child| self.renumber(child));
        renumbered.id = self.id();
        renumbered
    }
}

/// The record an expression's value was read from, or `None` for the candidate.
fn source_of(node: &IdedExpr, scope: &Scope) -> Option<Source> {
    match &node.expr {
        Expr::Ident(name) if name == "this" => Some(Source::Record(node.clone())),
        Expr::Ident(name) => scope.get(name).cloned().flatten(),
        Expr::Select(select) => source_of(&select.operand, scope),
        Expr::Call(call) => match (call.func_name.as_str(), &call.target) {
            ("asFile", Some(_)) => Some(Source::AsFile(node.id)),
            (name, Some(target)) if PASS_THROUGH.contains(&name) => source_of(target, scope),
            (operators::INDEX | operators::OPT_INDEX | operators::OPT_SELECT, None) => call
                .args
                .first()
                .and_then(|operand| source_of(operand, scope)),
            ("link", None) if call.args.len() == 1 => source_of(&call.args[0], scope),
            _ => None,
        },
        _ => None,
    }
}

fn max_id(node: &IdedExpr) -> u64 {
    children(node)
        .into_iter()
        .map(max_id)
        .fold(node.id, u64::max)
}

fn find(node: &IdedExpr, id: u64) -> Option<&IdedExpr> {
    if node.id == id {
        return Some(node);
    }
    children(node).into_iter().find_map(|child| find(child, id))
}

fn replace(node: &IdedExpr, id: u64, replacement: &IdedExpr) -> IdedExpr {
    if node.id == id {
        return replacement.clone();
    }
    map_children(node, &mut |child| replace(child, id, replacement))
}

fn replace_ident(node: &IdedExpr, name: &str, replacement: &IdedExpr) -> IdedExpr {
    if matches!(&node.expr, Expr::Ident(ident) if ident == name) {
        return replacement.clone();
    }
    map_children(node, &mut |child| replace_ident(child, name, replacement))
}

fn children(node: &IdedExpr) -> Vec<&IdedExpr> {
    match &node.expr {
        Expr::Call(call) => call
            .target
            .as_deref()
            .into_iter()
            .chain(&call.args)
            .collect(),
        Expr::Comprehension(c) => vec![
            &c.iter_range,
            &c.accu_init,
            &c.loop_cond,
            &c.loop_step,
            &c.result,
        ],
        Expr::List(list) => list.elements.iter().collect(),
        Expr::Map(MapExpr { entries }) | Expr::Struct(StructExpr { entries, .. }) => entries
            .iter()
            .flat_map(|entry| match &entry.expr {
                EntryExpr::MapEntry(entry) => vec![&entry.key, &entry.value],
                EntryExpr::StructField(field) => vec![&field.value],
            })
            .collect(),
        Expr::Select(select) => vec![select.operand.as_ref()],
        Expr::Ident(_) | Expr::Literal(_) | Expr::Unspecified => Vec::new(),
    }
}

/// Rebuild `node` with `f` applied to each direct child.
fn map_children(node: &IdedExpr, f: &mut dyn FnMut(&IdedExpr) -> IdedExpr) -> IdedExpr {
    let expr = match &node.expr {
        Expr::Call(call) => Expr::Call(CallExpr {
            func_name: call.func_name.clone(),
            target: call.target.as_deref().map(|target| Box::new(f(target))),
            args: call.args.iter().map(&mut *f).collect(),
        }),
        Expr::Comprehension(c) => {
            let mut rebuilt = c.as_ref().clone();
            rebuilt.iter_range = f(&c.iter_range);
            rebuilt.accu_init = f(&c.accu_init);
            rebuilt.loop_cond = f(&c.loop_cond);
            rebuilt.loop_step = f(&c.loop_step);
            rebuilt.result = f(&c.result);
            Expr::Comprehension(Box::new(rebuilt))
        }
        Expr::List(list) => Expr::List(ListExpr {
            elements: list.elements.iter().map(&mut *f).collect(),
            optional_indices: list.optional_indices.clone(),
        }),
        Expr::Map(MapExpr { entries }) => Expr::Map(MapExpr {
            entries: map_entries(entries, f),
        }),
        Expr::Struct(StructExpr { type_name, entries }) => Expr::Struct(StructExpr {
            type_name: type_name.clone(),
            entries: map_entries(entries, f),
        }),
        Expr::Select(select) => Expr::Select(SelectExpr {
            operand: Box::new(f(&select.operand)),
            field: select.field.clone(),
            test: select.test,
        }),
        other => other.clone(),
    };
    IdedExpr { id: node.id, expr }
}

fn map_entries(
    entries: &[IdedEntryExpr],
    f: &mut dyn FnMut(&IdedExpr) -> IdedExpr,
) -> Vec<IdedEntryExpr> {
    entries
        .iter()
        .map(|entry| IdedEntryExpr {
            id: entry.id,
            expr: match &entry.expr {
                EntryExpr::MapEntry(map) => {
                    let mut map = map.clone();
                    map.key = f(&map.key);
                    map.value = f(&map.value);
                    EntryExpr::MapEntry(map)
                }
                EntryExpr::StructField(field) => {
                    let mut field = field.clone();
                    field.value = f(&field.value);
                    EntryExpr::StructField(field)
                }
            },
        })
        .collect()
}
