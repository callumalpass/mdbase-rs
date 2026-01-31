//! Expression → AST (recursive descent parser).

use super::ast::*;

#[derive(Debug, Clone, PartialEq)]
enum Token {
    // Literals
    Null,
    Bool(bool),
    Number(f64),
    Str(String),
    Ident(String),

    // Operators
    Plus, Minus, Star, Slash, Percent,
    Eq, Neq, Lt, Gt, Lte, Gte,
    And, Or, Not,
    QuestionQuestion, // ??
    Question, Colon,  // ternary

    // Delimiters
    LParen, RParen,
    LBracket, RBracket,
    Dot, Comma,

    Eof,
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
}

impl Lexer {
    fn new(input: &str) -> Self {
        Lexer { chars: input.chars().collect(), pos: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied();
        self.pos += 1;
        c
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos].is_whitespace() {
            self.pos += 1;
        }
    }

    fn tokenize(&mut self) -> Result<Vec<Token>, String> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace();
            if self.pos >= self.chars.len() {
                tokens.push(Token::Eof);
                return Ok(tokens);
            }
            let c = self.chars[self.pos];
            match c {
                '+' => { self.advance(); tokens.push(Token::Plus); }
                '*' => { self.advance(); tokens.push(Token::Star); }
                '/' => { self.advance(); tokens.push(Token::Slash); }
                '%' => { self.advance(); tokens.push(Token::Percent); }
                '(' => { self.advance(); tokens.push(Token::LParen); }
                ')' => { self.advance(); tokens.push(Token::RParen); }
                '[' => { self.advance(); tokens.push(Token::LBracket); }
                ']' => { self.advance(); tokens.push(Token::RBracket); }
                ',' => { self.advance(); tokens.push(Token::Comma); }
                ':' => { self.advance(); tokens.push(Token::Colon); }
                '.' => {
                    self.advance();
                    // Check if next char starts a digit (decimal number like .5)
                    if self.peek().map_or(false, |c| c.is_ascii_digit()) {
                        let mut num_str = String::from("0.");
                        while self.peek().map_or(false, |c| c.is_ascii_digit()) {
                            num_str.push(self.advance().unwrap());
                        }
                        let n: f64 = num_str.parse().map_err(|_| "Invalid number".to_string())?;
                        tokens.push(Token::Number(n));
                    } else {
                        tokens.push(Token::Dot);
                    }
                }
                '-' => {
                    self.advance();
                    tokens.push(Token::Minus);
                }
                '?' => {
                    self.advance();
                    if self.peek() == Some('?') {
                        self.advance();
                        tokens.push(Token::QuestionQuestion);
                    } else {
                        tokens.push(Token::Question);
                    }
                }
                '=' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(Token::Eq);
                    } else {
                        return Err("Unexpected '='".to_string());
                    }
                }
                '!' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(Token::Neq);
                    } else {
                        tokens.push(Token::Not);
                    }
                }
                '<' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(Token::Lte);
                    } else {
                        tokens.push(Token::Lt);
                    }
                }
                '>' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(Token::Gte);
                    } else {
                        tokens.push(Token::Gt);
                    }
                }
                '&' => {
                    self.advance();
                    if self.peek() == Some('&') {
                        self.advance();
                        tokens.push(Token::And);
                    } else {
                        return Err("Unexpected '&'".to_string());
                    }
                }
                '|' => {
                    self.advance();
                    if self.peek() == Some('|') {
                        self.advance();
                        tokens.push(Token::Or);
                    } else {
                        return Err("Unexpected '|'".to_string());
                    }
                }
                '"' | '\'' => {
                    let quote = self.advance().unwrap();
                    let mut s = String::new();
                    loop {
                        match self.advance() {
                            Some('\\') => {
                                match self.advance() {
                                    Some('n') => s.push('\n'),
                                    Some('t') => s.push('\t'),
                                    Some('\\') => s.push('\\'),
                                    Some(q) if q == quote => s.push(q),
                                    Some(c) => { s.push('\\'); s.push(c); }
                                    None => return Err("Unterminated string".to_string()),
                                }
                            }
                            Some(c) if c == quote => break,
                            Some(c) => s.push(c),
                            None => return Err("Unterminated string".to_string()),
                        }
                    }
                    tokens.push(Token::Str(s));
                }
                c if c.is_ascii_digit() => {
                    let mut num_str = String::new();
                    while self.peek().map_or(false, |c| c.is_ascii_digit() || c == '.') {
                        num_str.push(self.advance().unwrap());
                    }
                    // Handle scientific notation
                    if self.peek() == Some('e') || self.peek() == Some('E') {
                        num_str.push(self.advance().unwrap());
                        if self.peek() == Some('+') || self.peek() == Some('-') {
                            num_str.push(self.advance().unwrap());
                        }
                        while self.peek().map_or(false, |c| c.is_ascii_digit()) {
                            num_str.push(self.advance().unwrap());
                        }
                    }
                    let n: f64 = num_str.parse().map_err(|_| format!("Invalid number: {}", num_str))?;
                    tokens.push(Token::Number(n));
                }
                c if c.is_ascii_alphabetic() || c == '_' => {
                    let mut ident = String::new();
                    while self.peek().map_or(false, |c| c.is_ascii_alphanumeric() || c == '_') {
                        ident.push(self.advance().unwrap());
                    }
                    // Handle ext::name syntax
                    if ident == "ext" && self.peek() == Some(':') {
                        let saved = self.pos;
                        self.advance(); // first ':'
                        if self.peek() == Some(':') {
                            self.advance(); // second ':'
                            // Read the function name after ::
                            let mut func_name = String::new();
                            while self.peek().map_or(false, |c| c.is_ascii_alphanumeric() || c == '_') {
                                func_name.push(self.advance().unwrap());
                            }
                            if func_name.is_empty() {
                                // ext:: with nothing after it
                                tokens.push(Token::Ident(format!("ext::")));
                            } else {
                                tokens.push(Token::Ident(format!("ext::{}", func_name)));
                            }
                        } else {
                            // Single colon - rewind and just emit "ext"
                            self.pos = saved;
                            tokens.push(Token::Ident(ident));
                        }
                        continue;
                    }
                    match ident.as_str() {
                        "true" => tokens.push(Token::Bool(true)),
                        "false" => tokens.push(Token::Bool(false)),
                        "null" => tokens.push(Token::Null),
                        _ => tokens.push(Token::Ident(ident)),
                    }
                }
                _ => return Err(format!("Unexpected character: '{}'", c)),
            }
        }
    }
}

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    depth: u32,
}

const MAX_PARSE_DEPTH: u32 = 64;

impl Parser {
    pub fn parse(input: &str) -> Result<Expr, String> {
        if input.trim().is_empty() {
            return Err("Empty expression".to_string());
        }
        let mut lexer = Lexer::new(input);
        let tokens = lexer.tokenize()?;
        let mut parser = Parser { tokens, pos: 0, depth: 0 };
        let expr = parser.parse_ternary()?;
        if parser.peek() != &Token::Eof {
            return Err(format!("Unexpected token: {:?}", parser.peek()));
        }
        Ok(expr)
    }

    fn check_depth(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            return Err("expression_depth_exceeded".to_string());
        }
        Ok(())
    }

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        self.pos += 1;
        tok
    }

    fn expect(&mut self, expected: &Token) -> Result<(), String> {
        let tok = self.advance();
        if &tok == expected {
            Ok(())
        } else {
            Err(format!("Expected {:?}, got {:?}", expected, tok))
        }
    }

    // Precedence (lowest to highest):
    // ?? (null coalesce)
    // || (logical or)
    // && (logical and)
    // ==, != (equality)
    // <, >, <=, >= (comparison)
    // +, - (additive)
    // *, /, % (multiplicative)
    // !, - (unary)
    // . [] () (postfix)

    fn parse_ternary(&mut self) -> Result<Expr, String> {
        self.check_depth()?;
        let expr = self.parse_null_coalesce()?;
        if self.peek() == &Token::Question {
            self.advance();
            let then_expr = self.parse_ternary()?;
            self.expect(&Token::Colon)?;
            let else_expr = self.parse_ternary()?;
            return Ok(Expr::Conditional(Box::new(expr), Box::new(then_expr), Box::new(else_expr)));
        }
        Ok(expr)
    }

    fn parse_null_coalesce(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_or()?;
        while self.peek() == &Token::QuestionQuestion {
            self.advance();
            let right = self.parse_or()?;
            left = Expr::NullCoalesce(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and()?;
        while self.peek() == &Token::Or {
            self.advance();
            let right = self.parse_and()?;
            left = Expr::BinOp(Box::new(left), BinOp::Or, Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_equality()?;
        while self.peek() == &Token::And {
            self.advance();
            let right = self.parse_equality()?;
            left = Expr::BinOp(Box::new(left), BinOp::And, Box::new(right));
        }
        Ok(left)
    }

    fn parse_equality(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_comparison()?;
        loop {
            let op = match self.peek() {
                Token::Eq => BinOp::Eq,
                Token::Neq => BinOp::Neq,
                _ => break,
            };
            self.advance();
            let right = self.parse_comparison()?;
            left = Expr::BinOp(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_additive()?;
        loop {
            let op = match self.peek() {
                Token::Lt => BinOp::Lt,
                Token::Gt => BinOp::Gt,
                Token::Lte => BinOp::Lte,
                Token::Gte => BinOp::Gte,
                _ => break,
            };
            self.advance();
            let right = self.parse_additive()?;
            left = Expr::BinOp(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinOp::Add,
                Token::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_multiplicative()?;
            left = Expr::BinOp(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinOp::Mul,
                Token::Slash => BinOp::Div,
                Token::Percent => BinOp::Mod,
                _ => break,
            };
            self.advance();
            let right = self.parse_unary()?;
            left = Expr::BinOp(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Token::Not => {
                self.advance();
                let expr = self.parse_unary()?;
                Ok(Expr::UnaryOp(UnaryOp::Not, Box::new(expr)))
            }
            Token::Minus => {
                self.advance();
                let expr = self.parse_unary()?;
                Ok(Expr::UnaryOp(UnaryOp::Neg, Box::new(expr)))
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_primary()?;
        loop {
            match self.peek() {
                Token::Dot => {
                    self.advance();
                    let name = match self.advance() {
                        Token::Ident(s) => s,
                        t => return Err(format!("Expected identifier after '.', got {:?}", t)),
                    };
                    // Check for method call: .name(args)
                    if self.peek() == &Token::LParen {
                        self.advance(); // consume (
                        let args = self.parse_args()?;
                        self.expect(&Token::RParen)?;
                        expr = Expr::Call(
                            Box::new(Expr::Dot(Box::new(expr), name)),
                            args,
                        );
                    } else {
                        expr = Expr::Dot(Box::new(expr), name);
                    }
                }
                Token::LBracket => {
                    self.advance();
                    let index = self.parse_ternary()?;
                    self.expect(&Token::RBracket)?;
                    expr = Expr::Index(Box::new(expr), Box::new(index));
                }
                Token::LParen => {
                    // Function call: expr(args)
                    self.advance();
                    let args = self.parse_args()?;
                    self.expect(&Token::RParen)?;
                    expr = Expr::Call(Box::new(expr), args);
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            Token::Null => { self.advance(); Ok(Expr::Null) }
            Token::Bool(b) => { let b = b; self.advance(); Ok(Expr::Bool(b)) }
            Token::Number(n) => { let n = n; self.advance(); Ok(Expr::Number(n)) }
            Token::Str(s) => { let s = s.clone(); self.advance(); Ok(Expr::Str(s)) }
            Token::Ident(s) => { let s = s.clone(); self.advance(); Ok(Expr::Ident(s)) }
            Token::LParen => {
                self.advance();
                let expr = self.parse_ternary()?;
                self.expect(&Token::RParen)?;
                Ok(expr)
            }
            Token::LBracket => {
                // Array literal: [expr, expr, ...]
                self.advance();
                let mut elements = Vec::new();
                if self.peek() != &Token::RBracket {
                    elements.push(self.parse_ternary()?);
                    while self.peek() == &Token::Comma {
                        self.advance();
                        if self.peek() == &Token::RBracket {
                            break; // trailing comma
                        }
                        elements.push(self.parse_ternary()?);
                    }
                }
                self.expect(&Token::RBracket)?;
                Ok(Expr::Array(elements))
            }
            t => Err(format!("Unexpected token: {:?}", t)),
        }
    }

    fn parse_args(&mut self) -> Result<Vec<Expr>, String> {
        let mut args = Vec::new();
        if self.peek() == &Token::RParen {
            return Ok(args);
        }
        args.push(self.parse_ternary()?);
        while self.peek() == &Token::Comma {
            self.advance();
            args.push(self.parse_ternary()?);
        }
        Ok(args)
    }
}
