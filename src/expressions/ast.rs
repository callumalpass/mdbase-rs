//! AST node types for expressions.

#[derive(Debug, Clone)]
pub enum Expr {
    // Literals
    Null,
    Bool(bool),
    Number(f64),
    Str(String),

    // Identifiers / field access
    Ident(String),               // simple field: `title`
    Dot(Box<Expr>, String),      // dot access: `a.b`
    Index(Box<Expr>, Box<Expr>), // bracket access: `a[0]` or `a["key"]`

    // Operators
    BinOp(Box<Expr>, BinOp, Box<Expr>),
    UnaryOp(UnaryOp, Box<Expr>),
    NullCoalesce(Box<Expr>, Box<Expr>), // ??

    // Array literal
    Array(Vec<Expr>), // [expr, expr, ...]

    // Function call
    Call(Box<Expr>, Vec<Expr>), // func(args...) or obj.method(args...)

    // Ternary / if
    Conditional(Box<Expr>, Box<Expr>, Box<Expr>), // cond ? then : else
}

#[derive(Debug, Clone)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Neq,
    Lt,
    Gt,
    Lte,
    Gte,
    And,
    Or,
}

#[derive(Debug, Clone)]
pub enum UnaryOp {
    Not,
    Neg,
}
