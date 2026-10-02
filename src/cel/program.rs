//! Compiled mdbase CEL programs and the static facts hosts need about them.

use std::collections::BTreeSet;
use std::sync::Arc;

use cel::common::ast::{EntryExpr, Expr, IdedExpr, LiteralValue};
use cel::parser::Parser;

/// Stack for recursive work on deep expressions. Parsing always runs on a
/// fresh stack of this size; evaluation and drop do too when the expression
/// is deeper than [`DEEP_AST`], decided from the expression rather than from
/// a platform estimate of the remaining stack.
pub(crate) const STACK_SIZE: usize = 64 * 1024 * 1024;

/// AST depth above which evaluation and drop run on a fresh stack.
const DEEP_AST: usize = 32;

/// Run `work` on a fresh stack when `depth` is deep enough to need one.
pub(crate) fn with_stack_for<T>(depth: usize, work: impl FnOnce() -> T) -> T {
    if depth > DEEP_AST {
        stacker::grow(STACK_SIZE, work)
    } else {
        work()
    }
}

/// Whole-`file` members whose values depend on parsing the record body.
const BODY_FACTS: [&str; 4] = ["tags", "links", "embeds", "backlinks"];

/// A parsed standard CEL expression (spec Chapter 10), shareable across threads.
#[derive(Clone, Debug)]
pub(crate) struct Program {
    ast: Arc<Ast>,
    /// The AST that is evaluated: `ast` with link provenance made explicit.
    executable: Arc<Ast>,
    facts: Arc<ProgramFacts>,
}

/// An AST whose recursive drop runs on a grown stack, so dropping a deeply
/// nested expression cannot overflow a small thread stack.
#[derive(Debug)]
struct Ast(Option<IdedExpr>, usize);

impl Ast {
    fn get(&self) -> &IdedExpr {
        self.0
            .as_ref()
            .expect("an AST is present until it is dropped")
    }
}

impl Drop for Ast {
    fn drop(&mut self) {
        if let Some(ast) = self.0.take() {
            with_stack_for(self.1, move || drop(ast));
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ProgramFacts {
    /// Nesting depth of the expression.
    pub depth: usize,
    /// Identifiers that are not comprehension variables.
    pub free_identifiers: BTreeSet<String>,
    /// Names read through `projection.<name>` or `projection["name"]`.
    pub projection_references: BTreeSet<String>,
    /// Whether any file value needs tags, links, embeds, or backlinks.
    pub needs_body_facts: bool,
    /// Whether the expression traverses links or reads backlinks.
    pub needs_link_graph: bool,
    /// Whether traversal may use an explicit policy (dynamic single arguments
    /// are conservative: they can evaluate to either a source string or map).
    pub needs_link_resolution_options: bool,
    /// Whether the expression reads the record body or body-derived facts.
    pub needs_file_body: bool,
}

impl Program {
    pub(crate) fn parse(source: &str, max_depth: u32) -> Result<Self, String> {
        // Parsing and the AST passes below recurse once per nesting level; give
        // them room so the depth check, not the thread's stack, decides the limit.
        stacker::grow(STACK_SIZE, || {
            let ast = Parser::default()
                .enable_optional_syntax(true)
                .max_recursion_depth(u16::try_from(max_depth.saturating_mul(2)).unwrap_or(u16::MAX))
                .parse(source)
                .map_err(|errors| errors.to_string())?;
            let ast_depth = depth(&ast);
            if ast_depth > max_depth as usize {
                return Err("expression_depth_exceeded".to_string());
            }
            let mut facts = ProgramFacts {
                depth: ast_depth,
                ..ProgramFacts::default()
            };
            collect(&ast, &mut Vec::new(), &mut facts);
            let executable = super::provenance::rewrite(&ast)
                .map(|rewritten| Arc::new(Ast(Some(rewritten), ast_depth)));
            let ast = Arc::new(Ast(Some(ast), ast_depth));
            let executable = executable.unwrap_or_else(|| ast.clone());
            Ok(Self {
                ast,
                executable,
                facts: Arc::new(facts),
            })
        })
    }

    /// The expression as written, for static analysis and query lowering.
    pub(crate) fn ast(&self) -> &IdedExpr {
        self.ast.get()
    }

    /// The expression to evaluate.
    pub(crate) fn executable(&self) -> &IdedExpr {
        self.executable.get()
    }

    pub(crate) fn facts(&self) -> &ProgramFacts {
        &self.facts
    }
}

fn depth(expression: &IdedExpr) -> usize {
    1 + children(expression).map(depth).max().unwrap_or(0)
}

fn children(expression: &IdedExpr) -> Box<dyn Iterator<Item = &IdedExpr> + '_> {
    match &expression.expr {
        Expr::Call(call) => Box::new(call.target.as_deref().into_iter().chain(&call.args)),
        Expr::Comprehension(comprehension) => Box::new(
            [
                &comprehension.iter_range,
                &comprehension.accu_init,
                &comprehension.loop_cond,
                &comprehension.loop_step,
                &comprehension.result,
            ]
            .into_iter(),
        ),
        Expr::List(list) => Box::new(list.elements.iter()),
        Expr::Map(map) => Box::new(map.entries.iter().flat_map(|entry| match &entry.expr {
            EntryExpr::MapEntry(entry) => vec![&entry.key, &entry.value],
            EntryExpr::StructField(field) => vec![&field.value],
        })),
        Expr::Struct(value) => Box::new(value.entries.iter().flat_map(|entry| match &entry.expr {
            EntryExpr::MapEntry(entry) => vec![&entry.key, &entry.value],
            EntryExpr::StructField(field) => vec![&field.value],
        })),
        Expr::Select(select) => Box::new(std::iter::once(select.operand.as_ref())),
        Expr::Ident(_) | Expr::Literal(_) | Expr::Unspecified => Box::new(std::iter::empty()),
    }
}

fn collect(expression: &IdedExpr, bound: &mut Vec<String>, facts: &mut ProgramFacts) {
    match &expression.expr {
        Expr::Ident(name) => {
            if !bound.iter().any(|variable| variable == name) {
                facts.free_identifiers.insert(name.clone());
            }
        }
        Expr::Select(select) => {
            if is_ident(&select.operand, "projection") {
                facts.projection_references.insert(select.field.clone());
            }
            if BODY_FACTS.contains(&select.field.as_str()) {
                facts.needs_body_facts = true;
                facts.needs_file_body = true;
            }
            if select.field == "body" {
                facts.needs_file_body = true;
            }
            if select.field == "backlinks" {
                facts.needs_link_graph = true;
            }
        }
        Expr::Call(call) => {
            if matches!(call.func_name.as_str(), "hasTag" | "hasLink") {
                facts.needs_body_facts = true;
                facts.needs_file_body = true;
            }
            if matches!(call.func_name.as_str(), "asFile" | "hasLink") {
                facts.needs_link_graph = true;
            }
            if call.func_name == "asFile" && !call.args.is_empty() {
                facts.needs_link_resolution_options |= call.args.len() > 1
                    || !matches!(&call.args[0].expr, Expr::Literal(LiteralValue::String(_)));
            }
            if call.func_name == "_[_]"
                && call.args.len() == 2
                && is_ident(&call.args[0], "projection")
            {
                if let Expr::Literal(LiteralValue::String(name)) = &call.args[1].expr {
                    facts.projection_references.insert(name.inner().to_string());
                }
            }
        }
        Expr::Comprehension(comprehension) => {
            collect(&comprehension.iter_range, bound, facts);
            collect(&comprehension.accu_init, bound, facts);
            let scoped = [
                Some(&comprehension.iter_var),
                comprehension.iter_var2.as_ref(),
                Some(&comprehension.accu_var),
            ];
            let added = scoped.iter().flatten().count();
            bound.extend(scoped.into_iter().flatten().cloned());
            for part in [
                &comprehension.loop_cond,
                &comprehension.loop_step,
                &comprehension.result,
            ] {
                collect(part, bound, facts);
            }
            bound.truncate(bound.len() - added);
            return;
        }
        _ => {}
    }
    for child in children(expression) {
        collect(child, bound, facts);
    }
}

fn is_ident(expression: &IdedExpr, name: &str) -> bool {
    matches!(&expression.expr, Expr::Ident(identifier) if identifier == name)
}

#[cfg(test)]
mod tests {
    use super::Program;

    #[test]
    fn facts_distinguish_free_identifiers_projections_and_body_needs() {
        let program = Program::parse(
            r#"tags.exists(t, t == title) && projection.urgency > projection["score"]"#,
            100,
        )
        .unwrap();
        let facts = program.facts();
        assert!(facts.free_identifiers.contains("tags"));
        assert!(facts.free_identifiers.contains("title"));
        assert!(!facts.free_identifiers.contains("t"));
        assert_eq!(
            facts.projection_references.iter().collect::<Vec<_>>(),
            ["score", "urgency"]
        );
        assert!(!facts.needs_body_facts);
        assert!(!facts.needs_link_graph);
        assert!(
            Program::parse("file.hasTag(\"a\")", 100)
                .unwrap()
                .facts()
                .needs_body_facts
        );
        let traversal = Program::parse("parent.asFile().file.backlinks", 100).unwrap();
        assert!(traversal.facts().needs_link_graph && traversal.facts().needs_file_body);
        assert!(Program::parse("record.?due.orValue(null)", 100).is_ok());
    }

    #[test]
    fn depth_limits_and_parse_errors_are_reported() {
        assert_eq!(
            Program::parse("1 + (2 + (3 + 4))", 2).unwrap_err(),
            "expression_depth_exceeded"
        );
        assert!(Program::parse("status ==", 100).is_err());
    }
}
