use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};

use chrono::{
    DateTime, Datelike, FixedOffset, Local, Months, NaiveDate, NaiveDateTime, TimeZone, Timelike,
    Utc,
};
use chrono_tz::Tz;
use regex::{Regex, RegexBuilder};
use serde_json::{Map, Value};

use crate::runtime::{
    CandidateComparison, CandidateComparisonOperator, CandidateComparisonPruning, CandidateField,
    CandidatePredicate,
};
use crate::OperationCancellation;

#[derive(Clone, Debug, PartialEq)]
enum Expr {
    Literal(Value),
    Regex(String, String),
    Identifier(String),
    Array(Vec<Expr>),
    Unary(String, Box<Expr>),
    Binary(String, Box<Expr>, Box<Expr>),
    Member(Box<Expr>, Member),
    Call(Box<Expr>, Vec<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
enum Member {
    Named(String),
    Computed(Box<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
enum TokenKind {
    Number(f64),
    String(String),
    Identifier(String),
    Regex(String, String),
    Operator(String),
    Punct(char),
    Eof,
}

#[derive(Clone, Debug, PartialEq)]
struct Token {
    kind: TokenKind,
    raw: String,
}

struct Lexer<'a> {
    source: &'a str,
    chars: Vec<char>,
    index: usize,
    tokens: Vec<Token>,
    previous_ends_expression: bool,
}

impl<'a> Lexer<'a> {
    fn tokenize(source: &'a str) -> Result<Vec<Token>, String> {
        let mut lexer = Self {
            source,
            chars: source.chars().collect(),
            index: 0,
            tokens: Vec::new(),
            previous_ends_expression: false,
        };
        while let Some(character) = lexer.peek(0) {
            if character.is_whitespace() {
                lexer.index += 1;
            } else if character.is_ascii_digit()
                || (character == '.' && lexer.peek(1).is_some_and(|value| value.is_ascii_digit()))
            {
                lexer.number()?;
            } else if matches!(character, '\'' | '"') {
                lexer.string(character)?;
            } else if character == '/' && !lexer.previous_ends_expression {
                lexer.regexp()?;
            } else if is_identifier_start(character) {
                lexer.identifier();
            } else {
                lexer.symbol()?;
            }
        }
        lexer.tokens.push(Token {
            kind: TokenKind::Eof,
            raw: String::new(),
        });
        Ok(lexer.tokens)
    }

    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.index + offset).copied()
    }

    fn slice(&self, start: usize, end: usize) -> String {
        self.chars[start..end].iter().collect()
    }

    fn push(&mut self, kind: TokenKind, raw: String, ends_expression: bool) {
        self.tokens.push(Token { kind, raw });
        self.previous_ends_expression = ends_expression;
    }

    fn number(&mut self) -> Result<(), String> {
        let start = self.index;
        if self.peek(0) != Some('.') {
            while self.peek(0).is_some_and(|value| value.is_ascii_digit()) {
                self.index += 1;
            }
        }
        if self.peek(0) == Some('.') && self.peek(1).is_some_and(|value| value.is_ascii_digit()) {
            self.index += 1;
            while self.peek(0).is_some_and(|value| value.is_ascii_digit()) {
                self.index += 1;
            }
        }
        if self.peek(0).is_some_and(|value| matches!(value, 'e' | 'E')) {
            let exponent = self.index;
            self.index += 1;
            if self.peek(0).is_some_and(|value| matches!(value, '+' | '-')) {
                self.index += 1;
            }
            if self.peek(0).is_some_and(|value| value.is_ascii_digit()) {
                while self.peek(0).is_some_and(|value| value.is_ascii_digit()) {
                    self.index += 1;
                }
            } else {
                self.index = exponent;
            }
        }
        let raw = self.slice(start, self.index);
        let value = raw
            .parse::<f64>()
            .map_err(|_| format!("Invalid number {raw}"))?;
        self.push(TokenKind::Number(value), raw, true);
        Ok(())
    }

    fn string(&mut self, quote: char) -> Result<(), String> {
        let start = self.index;
        self.index += 1;
        let mut value = String::new();
        while let Some(character) = self.peek(0) {
            self.index += 1;
            if character == quote {
                let raw = self.slice(start, self.index);
                self.push(TokenKind::String(value), raw, true);
                return Ok(());
            }
            if character == '\\' {
                let escaped = self
                    .peek(0)
                    .ok_or_else(|| "Unterminated string literal".to_string())?;
                self.index += 1;
                value.push(match escaped {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    'b' => '\u{0008}',
                    'f' => '\u{000c}',
                    other => other,
                });
            } else {
                value.push(character);
            }
        }
        Err(format!("Unterminated string literal in {}", self.source))
    }

    fn regexp(&mut self) -> Result<(), String> {
        let start = self.index;
        self.index += 1;
        let mut pattern = String::new();
        let mut escaped = false;
        let mut in_class = false;
        while let Some(character) = self.peek(0) {
            self.index += 1;
            if escaped {
                pattern.push('\\');
                pattern.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '[' {
                in_class = true;
                pattern.push(character);
            } else if character == ']' {
                in_class = false;
                pattern.push(character);
            } else if character == '/' && !in_class {
                let mut flags = String::new();
                while self
                    .peek(0)
                    .is_some_and(|value| value.is_ascii_alphabetic())
                {
                    flags.push(self.peek(0).expect("checked"));
                    self.index += 1;
                }
                let raw = self.slice(start, self.index);
                self.push(TokenKind::Regex(pattern, flags), raw, true);
                return Ok(());
            } else {
                pattern.push(character);
            }
        }
        Err("Unterminated regular expression literal".to_string())
    }

    fn identifier(&mut self) {
        let start = self.index;
        self.index += 1;
        while self.peek(0).is_some_and(is_identifier_part) {
            self.index += 1;
        }
        let raw = self.slice(start, self.index);
        self.push(TokenKind::Identifier(raw.clone()), raw, true);
    }

    fn symbol(&mut self) -> Result<(), String> {
        let character = self.peek(0).expect("symbol exists");
        let pair = self
            .peek(1)
            .map(|next| format!("{character}{next}"))
            .unwrap_or_default();
        if matches!(pair.as_str(), "==" | "!=" | ">=" | "<=" | "&&" | "||") {
            self.index += 2;
            self.push(TokenKind::Operator(pair.clone()), pair, false);
            return Ok(());
        }
        if matches!(character, '+' | '-' | '*' | '/' | '%' | '!' | '>' | '<') {
            self.index += 1;
            self.push(
                TokenKind::Operator(character.to_string()),
                character.to_string(),
                false,
            );
            return Ok(());
        }
        if matches!(
            character,
            '(' | '[' | '{' | '.' | ',' | ':' | ')' | ']' | '}'
        ) {
            self.index += 1;
            let ends = matches!(character, ')' | ']' | '}');
            self.push(TokenKind::Punct(character), character.to_string(), ends);
            return Ok(());
        }
        Err(format!("Unexpected character {character:?}"))
    }
}

fn is_identifier_start(character: char) -> bool {
    character.is_alphabetic() || character == '_' || character == '$'
}

fn is_identifier_part(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '$')
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
}

impl Parser {
    fn parse(source: &str) -> Result<Expr, String> {
        let mut parser = Self {
            tokens: Lexer::tokenize(source)?,
            index: 0,
        };
        let expression = parser.expression(0)?;
        if !matches!(parser.current().kind, TokenKind::Eof) {
            return Err(format!("Unexpected token {}", parser.current().raw));
        }
        Ok(expression)
    }

    fn current(&self) -> &Token {
        &self.tokens[self.index]
    }

    fn advance(&mut self) -> Token {
        let token = self.tokens[self.index].clone();
        self.index += 1;
        token
    }

    fn match_punct(&mut self, expected: char) -> bool {
        if matches!(&self.current().kind, TokenKind::Punct(value) if *value == expected) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn expect_punct(&mut self, expected: char) -> Result<(), String> {
        if self.match_punct(expected) {
            Ok(())
        } else {
            Err(format!("Expected {expected}, found {}", self.current().raw))
        }
    }

    fn expression(&mut self, minimum: u8) -> Result<Expr, String> {
        let mut left = self.prefix()?;
        left = self.postfix(left)?;
        while let TokenKind::Operator(operator) = &self.current().kind {
            let precedence = match operator.as_str() {
                "||" => 1,
                "&&" => 2,
                "==" | "!=" | ">" | "<" | ">=" | "<=" => 3,
                "+" | "-" => 4,
                "*" | "/" | "%" => 5,
                _ => 0,
            };
            if precedence == 0 || precedence < minimum {
                break;
            }
            let operator = operator.clone();
            self.advance();
            let right = self.expression(precedence + 1)?;
            left = Expr::Binary(operator, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn prefix(&mut self) -> Result<Expr, String> {
        let token = self.advance();
        match token.kind {
            TokenKind::Number(value) => Ok(Expr::Literal(number_json(value))),
            TokenKind::String(value) => Ok(Expr::Literal(Value::String(value))),
            TokenKind::Regex(pattern, flags) => Ok(Expr::Regex(pattern, flags)),
            TokenKind::Identifier(value) if value == "true" => Ok(Expr::Literal(Value::Bool(true))),
            TokenKind::Identifier(value) if value == "false" => {
                Ok(Expr::Literal(Value::Bool(false)))
            }
            TokenKind::Identifier(value) if value == "null" => Ok(Expr::Literal(Value::Null)),
            TokenKind::Identifier(value) => Ok(Expr::Identifier(value)),
            TokenKind::Operator(operator) if matches!(operator.as_str(), "!" | "-" | "+") => {
                Ok(Expr::Unary(operator, Box::new(self.expression(6)?)))
            }
            TokenKind::Punct('(') => {
                let value = self.expression(0)?;
                self.expect_punct(')')?;
                Ok(value)
            }
            TokenKind::Punct('[') => {
                let mut values = Vec::new();
                if !self.match_punct(']') {
                    loop {
                        values.push(self.expression(0)?);
                        if !self.match_punct(',') {
                            self.expect_punct(']')?;
                            break;
                        }
                    }
                }
                Ok(Expr::Array(values))
            }
            TokenKind::Punct('{') => {
                Err("Object literals are not supported by Obsidian Bases".to_string())
            }
            _ => Err(format!("Expected expression, found {}", token.raw)),
        }
    }

    fn postfix(&mut self, mut expression: Expr) -> Result<Expr, String> {
        loop {
            if self.match_punct('.') {
                let token = self.advance();
                let TokenKind::Identifier(property) = token.kind else {
                    return Err("Expected property name after dot".to_string());
                };
                expression = Expr::Member(Box::new(expression), Member::Named(property));
            } else if self.match_punct('[') {
                let property = self.expression(0)?;
                self.expect_punct(']')?;
                expression =
                    Expr::Member(Box::new(expression), Member::Computed(Box::new(property)));
            } else if self.match_punct('(') {
                let mut arguments = Vec::new();
                if !self.match_punct(')') {
                    loop {
                        arguments.push(self.expression(0)?);
                        if !self.match_punct(',') {
                            self.expect_punct(')')?;
                            break;
                        }
                    }
                }
                expression = Expr::Call(Box::new(expression), arguments);
            } else {
                break;
            }
        }
        Ok(expression)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct BasesEvaluationContext {
    pub note: Map<String, Value>,
    pub file: BasesFile,
    pub this_file: Option<BasesFile>,
    pub files: Arc<Vec<BasesFile>>,
    pub formulas: Arc<BTreeMap<String, String>>,
    pub property_types: Arc<BTreeMap<String, String>>,
    pub link_resolutions: Arc<BTreeMap<String, Option<String>>>,
    pub now: Option<String>,
    pub timezone: BasesTimezone,
    /// Optional semantic work ceiling used by bounded hosted execution. One
    /// unit is charged for every AST node evaluation, including nested list
    /// callbacks and formulas.
    pub work_limit: Option<usize>,
    /// Cooperative host cancellation checked at every AST node. Filesystem
    /// execution leaves this unset and uses its outer operation boundaries.
    pub cancellation: Option<OperationCancellation>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct BasesFile {
    pub path: String,
    pub name: String,
    pub basename: String,
    pub folder: String,
    pub extension: String,
    pub size: u64,
    pub properties: Map<String, Value>,
    pub tags: Vec<String>,
    pub links: Vec<BasesLink>,
    pub embeds: Vec<BasesLink>,
    pub backlinks: Vec<BasesLink>,
    pub ctime: Option<String>,
    pub mtime: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct BasesLink {
    pub path: String,
    pub display: Option<String>,
    pub resolved_path: Option<Option<String>>,
    pub external: bool,
}

pub(crate) fn serialize_bases_file(file: &BasesFile) -> Value {
    fn link_value(link: &BasesLink) -> Value {
        serde_json::json!({
            "path": link.path,
            "display": link.display,
            "resolved_path": link.resolved_path.clone().flatten(),
            "external": link.external,
        })
    }

    serde_json::json!({
        "path": file.path,
        "name": file.name,
        "basename": file.basename,
        "folder": file.folder,
        "ext": file.extension,
        "size": file.size,
        "mtime": file.mtime,
        "ctime": file.ctime,
        "properties": file.properties,
        "tags": file.tags,
        "links": file.links.iter().map(link_value).collect::<Vec<_>>(),
        "embeds": file.embeds.iter().map(link_value).collect::<Vec<_>>(),
        "backlinks": file.backlinks.iter().map(link_value).collect::<Vec<_>>(),
    })
}

#[derive(Clone, Debug)]
struct DateValue {
    millis: i64,
    date_only: bool,
    timezone: BasesTimezone,
}

#[derive(Clone, Debug, Default)]
pub(crate) enum BasesTimezone {
    #[default]
    Local,
    Fixed(FixedOffset),
    Named(Tz),
}

impl BasesTimezone {
    pub(crate) fn from_setting(value: Option<&str>) -> Result<Self, String> {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(Self::Local);
        };
        match value {
            "local" => Ok(Self::Local),
            "UTC" | "utc" | "Z" => Ok(Self::Fixed(FixedOffset::east_opt(0).expect("UTC offset"))),
            value if value.starts_with('+') || value.starts_with('-') => {
                parse_timezone_offset(value)
                    .map(Self::Fixed)
                    .ok_or_else(|| format!("Invalid fixed timezone offset '{value}'"))
            }
            value => value
                .parse::<Tz>()
                .map(Self::Named)
                .map_err(|_| format!("Unknown IANA timezone '{value}'")),
        }
    }

    fn local_datetime(&self, millis: i64) -> NaiveDateTime {
        let utc = Utc
            .timestamp_millis_opt(millis)
            .single()
            .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
        match self {
            Self::Local => utc.with_timezone(&Local).naive_local(),
            Self::Fixed(offset) => utc.with_timezone(offset).naive_local(),
            Self::Named(timezone) => utc.with_timezone(timezone).naive_local(),
        }
    }

    fn millis_from_local(&self, value: NaiveDateTime) -> i64 {
        match self {
            Self::Local => Local
                .from_local_datetime(&value)
                .earliest()
                .unwrap_or_else(|| Local.from_utc_datetime(&value))
                .timestamp_millis(),
            Self::Fixed(offset) => offset
                .from_local_datetime(&value)
                .single()
                .expect("fixed offsets are unambiguous")
                .timestamp_millis(),
            Self::Named(timezone) => timezone
                .from_local_datetime(&value)
                .earliest()
                .unwrap_or_else(|| timezone.from_utc_datetime(&value))
                .timestamp_millis(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct DurationValue {
    years: f64,
    months: f64,
    weeks: f64,
    days: f64,
    hours: f64,
    minutes: f64,
    seconds: f64,
    milliseconds: f64,
}

#[derive(Clone, Debug)]
enum RuntimeValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Date(DateValue),
    Duration(DurationValue),
    List(Vec<RuntimeValue>),
    Object(BTreeMap<String, RuntimeValue>),
    File(Box<BasesFile>),
    Link(BasesLink),
    Regex(String, String),
    Html(String),
    Image(String),
    Icon(String),
    Error(String),
}

pub(crate) fn evaluate(
    expression: &str,
    context: &BasesEvaluationContext,
) -> Result<Value, String> {
    stacker::maybe_grow(2 * 1024 * 1024, 4 * 1024 * 1024, || {
        evaluate_on_base_stack(expression, context)
    })
}

fn evaluate_on_base_stack(
    expression: &str,
    context: &BasesEvaluationContext,
) -> Result<Value, String> {
    let ast = match parse_cached(expression) {
        Ok(ast) => ast,
        Err(error) if error == "Object literals are not supported by Obsidian Bases" => {
            return Ok(Value::Null)
        }
        Err(error) => return Err(error),
    };
    let mut evaluator = Evaluator::new(context);
    let value = evaluator.evaluate(ast.as_ref(), &BTreeMap::new());
    match value {
        RuntimeValue::Error(message) => Err(message),
        value => Ok(to_plain(&value)),
    }
}

/// Validate the source language without evaluating it against a record.
///
/// Runtime lookup errors are meaningful during execution, but they must not be
/// used as a proxy for syntax validation: an otherwise valid formula can refer
/// to fields that are absent from an arbitrary validation context.
pub(crate) fn validate(expression: &str) -> Result<(), String> {
    stacker::maybe_grow(2 * 1024 * 1024, 4 * 1024 * 1024, || {
        validate_on_base_stack(expression)
    })
}

fn validate_on_base_stack(expression: &str) -> Result<(), String> {
    match parse_cached(expression) {
        Ok(_) => Ok(()),
        // Obsidian currently accepts this source through its public expression
        // surface even though its internal parser produces a null result.
        Err(error) if error == "Object literals are not supported by Obsidian Bases" => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn matches(expression: &str, context: &BasesEvaluationContext) -> Result<bool, String> {
    stacker::maybe_grow(2 * 1024 * 1024, 4 * 1024 * 1024, || {
        matches_on_base_stack(expression, context)
    })
}

fn matches_on_base_stack(
    expression: &str,
    context: &BasesEvaluationContext,
) -> Result<bool, String> {
    let ast = parse_cached(expression)?;
    let mut evaluator = Evaluator::new(context);
    match evaluator.evaluate(ast.as_ref(), &BTreeMap::new()) {
        RuntimeValue::Error(message) => Err(message),
        value => Ok(is_truthy(&value)),
    }
}

fn parse_cached(expression: &str) -> Result<Arc<Expr>, String> {
    const MAX_ENTRIES: usize = 1024;
    static CACHE: OnceLock<RwLock<HashMap<String, Arc<Expr>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    if let Ok(cache) = cache.read() {
        if let Some(ast) = cache.get(expression) {
            return Ok(ast.clone());
        }
    }
    let ast = Arc::new(Parser::parse(expression)?);
    if let Ok(mut cache) = cache.write() {
        if cache.len() >= MAX_ENTRIES {
            cache.clear();
        }
        cache.insert(expression.to_string(), ast.clone());
    }
    Ok(ast)
}

pub(crate) fn lower_hosted_candidate(expression: &str) -> CandidatePredicate {
    parse_cached(expression)
        .map(|expression| lower_candidate_expression(expression.as_ref()))
        .unwrap_or(CandidatePredicate::All)
}

/// Return whether an Obsidian Bases expression requires the collection
/// relationship graph or link-resolution namespace. This intentionally walks
/// the parsed syntax tree rather than searching source text, so string
/// literals and similarly named frontmatter fields do not create false
/// dependencies.
pub(crate) fn uses_relationships(expression: &str) -> bool {
    parse_cached(expression)
        .map(|expression| expression_uses_relationships(expression.as_ref()))
        // Validation normally rejects parse failures before planning. Treat
        // any exceptional accepted syntax conservatively so hosted execution
        // cannot select a projection-only path without proving independence.
        .unwrap_or(true)
}

/// Return whether an expression reads the filesystem creation/change time.
/// Hosted authorities do not possess a portable canonical ctime, so planning
/// must reject this dependency rather than silently evaluating it as null.
pub(crate) fn uses_file_ctime(expression: &str) -> bool {
    parse_cached(expression)
        .map(|expression| expression_uses_file_ctime(expression.as_ref()))
        .unwrap_or(true)
}

fn expression_uses_file_ctime(expression: &Expr) -> bool {
    match expression {
        Expr::Literal(_) | Expr::Regex(_, _) | Expr::Identifier(_) => false,
        Expr::Array(values) => values.iter().any(expression_uses_file_ctime),
        Expr::Unary(_, value) => expression_uses_file_ctime(value),
        Expr::Binary(_, left, right) => {
            expression_uses_file_ctime(left) || expression_uses_file_ctime(right)
        }
        Expr::Member(object, member) => {
            let ctime_member = matches!(
                (object.as_ref(), member),
                (Expr::Identifier(namespace), Member::Named(property))
                    if namespace == "file" && property == "ctime"
            ) || matches!(
                (object.as_ref(), member),
                (
                    Expr::Identifier(namespace),
                    Member::Computed(value)
                ) if namespace == "file"
                    && matches!(value.as_ref(), Expr::Literal(Value::String(property)) if property == "ctime")
            );
            ctime_member
                || expression_uses_file_ctime(object)
                || matches!(member, Member::Computed(value) if expression_uses_file_ctime(value))
        }
        Expr::Call(callee, arguments) => {
            expression_uses_file_ctime(callee) || arguments.iter().any(expression_uses_file_ctime)
        }
    }
}

fn expression_uses_relationships(expression: &Expr) -> bool {
    match expression {
        Expr::Literal(_) | Expr::Regex(_, _) | Expr::Identifier(_) => false,
        Expr::Array(values) => values.iter().any(expression_uses_relationships),
        Expr::Unary(_, value) => expression_uses_relationships(value),
        Expr::Binary(_, left, right) => {
            expression_uses_relationships(left) || expression_uses_relationships(right)
        }
        Expr::Member(object, member) => {
            let relationship_member = matches!(
                member,
                Member::Named(name)
                    if matches!(name.as_str(), "links" | "embeds" | "backlinks")
            );
            relationship_member
                || expression_uses_relationships(object)
                || matches!(member, Member::Computed(value) if expression_uses_relationships(value))
        }
        Expr::Call(callee, arguments) => {
            let relationship_call = match callee.as_ref() {
                Expr::Identifier(name) => name == "file",
                Expr::Member(_, Member::Named(name)) => {
                    matches!(name.as_str(), "hasLink" | "asFile" | "asLink")
                }
                _ => false,
            };
            relationship_call
                || expression_uses_relationships(callee)
                || arguments.iter().any(expression_uses_relationships)
        }
    }
}

fn lower_candidate_expression(expression: &Expr) -> CandidatePredicate {
    match expression {
        Expr::Binary(operator, left, right) if operator == "&&" => candidate_and(vec![
            lower_candidate_expression(left),
            lower_candidate_expression(right),
        ]),
        Expr::Binary(operator, left, right) if operator == "||" => {
            let terms = vec![
                lower_candidate_expression(left),
                lower_candidate_expression(right),
            ];
            if terms
                .iter()
                .any(|term| matches!(term, CandidatePredicate::All))
            {
                CandidatePredicate::All
            } else {
                CandidatePredicate::Or { terms }
            }
        }
        Expr::Binary(operator, left, right) if matches!(operator.as_str(), "==" | "!=") => {
            let pair = candidate_field(left)
                .and_then(|field| candidate_literal(right).map(|value| (field, value)))
                .or_else(|| {
                    candidate_field(right)
                        .and_then(|field| candidate_literal(left).map(|value| (field, value)))
                });
            pair.map_or(CandidatePredicate::All, |(field, value)| {
                CandidatePredicate::Compare {
                    comparison: CandidateComparison {
                        field,
                        operator: if operator == "==" {
                            CandidateComparisonOperator::Equal
                        } else {
                            CandidateComparisonOperator::NotEqual
                        },
                        value,
                        pruning: CandidateComparisonPruning::ExactJson,
                        value_kind: None,
                    },
                }
            })
        }
        Expr::Call(callee, arguments) if arguments.len() == 1 => {
            let Expr::Member(object, Member::Named(method)) = callee.as_ref() else {
                return CandidatePredicate::All;
            };
            if method == "hasTag" && candidate_path(object).as_deref() == Some("file") {
                if let Some(value) = candidate_literal(&arguments[0])
                    .and_then(|value| value.as_str().map(normalize_tag).map(Value::String))
                {
                    return CandidatePredicate::Compare {
                        comparison: CandidateComparison {
                            field: CandidateField::BodyTags,
                            operator: CandidateComparisonOperator::Contains,
                            value,
                            pruning: CandidateComparisonPruning::NormalizedTagHierarchy,
                            value_kind: None,
                        },
                    };
                }
            }
            CandidatePredicate::All
        }
        _ => CandidatePredicate::All,
    }
}

fn candidate_and(terms: Vec<CandidatePredicate>) -> CandidatePredicate {
    let terms = terms
        .into_iter()
        .filter(|term| !matches!(term, CandidatePredicate::All))
        .collect::<Vec<_>>();
    match terms.len() {
        0 => CandidatePredicate::All,
        1 => terms.into_iter().next().unwrap_or(CandidatePredicate::All),
        _ => CandidatePredicate::And { terms },
    }
}

fn candidate_field(expression: &Expr) -> Option<CandidateField> {
    let path = candidate_path(expression)?;
    let segments = path.split('.').collect::<Vec<_>>();
    match segments.as_slice() {
        [name] if !matches!(*name, "file" | "this" | "formula" | "note") => {
            Some(CandidateField::EffectiveFrontmatter(vec![name.to_string()]))
        }
        ["note", name] => Some(CandidateField::EffectiveFrontmatter(vec![name.to_string()])),
        ["file", "path"] => Some(CandidateField::Path),
        ["file", name] if matches!(*name, "name" | "basename" | "ext" | "size" | "mtime") => {
            Some(CandidateField::File(name.to_string()))
        }
        _ => None,
    }
}

fn candidate_path(expression: &Expr) -> Option<String> {
    match expression {
        Expr::Identifier(value) => Some(value.clone()),
        Expr::Member(object, Member::Named(value)) => {
            Some(format!("{}.{}", candidate_path(object)?, value))
        }
        Expr::Member(object, Member::Computed(value)) => {
            let Value::String(value) = candidate_literal(value)? else {
                return None;
            };
            Some(format!("{}.{}", candidate_path(object)?, value))
        }
        _ => None,
    }
}

fn candidate_literal(expression: &Expr) -> Option<Value> {
    match expression {
        Expr::Literal(value) => Some(value.clone()),
        Expr::Array(values) => values
            .iter()
            .map(candidate_literal)
            .collect::<Option<Vec<_>>>()
            .map(Value::Array),
        _ => None,
    }
}

struct Evaluator<'a> {
    context: &'a BasesEvaluationContext,
    now: DateValue,
    timezone: BasesTimezone,
    formula_cache: HashMap<String, RuntimeValue>,
    formula_stack: HashSet<String>,
    remaining_work: usize,
}

pub(crate) const BASES_WORK_BUDGET_EXCEEDED: &str = "Obsidian Base expression work budget exceeded";
pub(crate) const BASES_OPERATION_CANCELLED: &str = "Obsidian Base expression evaluation cancelled";

type Scope = BTreeMap<String, RuntimeValue>;

impl<'a> Evaluator<'a> {
    fn new(context: &'a BasesEvaluationContext) -> Self {
        let timezone = context.timezone.clone();
        Self {
            context,
            now: parse_date(
                context.now.as_deref().unwrap_or("1970-01-01T00:00:00Z"),
                &timezone,
            ),
            timezone,
            formula_cache: HashMap::new(),
            formula_stack: HashSet::new(),
            remaining_work: context.work_limit.unwrap_or(2_000_000),
        }
    }

    fn evaluate(&mut self, expression: &Expr, scope: &Scope) -> RuntimeValue {
        if self
            .context
            .cancellation
            .as_ref()
            .is_some_and(|cancellation| cancellation.stop_reason().is_some())
        {
            return RuntimeValue::Error(BASES_OPERATION_CANCELLED.to_string());
        }
        let Some(remaining) = self.remaining_work.checked_sub(1) else {
            return RuntimeValue::Error(BASES_WORK_BUDGET_EXCEEDED.to_string());
        };
        self.remaining_work = remaining;
        match expression {
            Expr::Literal(value) => from_json(value, None),
            Expr::Regex(pattern, flags) => RuntimeValue::Regex(pattern.clone(), flags.clone()),
            Expr::Identifier(name) => self.identifier(name, scope),
            Expr::Array(values) => RuntimeValue::List(
                values
                    .iter()
                    .map(|value| self.evaluate(value, scope))
                    .collect(),
            ),
            Expr::Unary(operator, value) => {
                let value = self.evaluate(value, scope);
                match operator.as_str() {
                    "!" => RuntimeValue::Bool(!is_truthy(&value)),
                    "-" => self
                        .number(&value)
                        .map(|value| RuntimeValue::Number(-value))
                        .unwrap_or_else(RuntimeValue::Error),
                    "+" => self
                        .number(&value)
                        .map(RuntimeValue::Number)
                        .unwrap_or_else(RuntimeValue::Error),
                    _ => RuntimeValue::Error(format!("Unsupported unary operator {operator}")),
                }
            }
            Expr::Binary(operator, left, right) => self.binary(operator, left, right, scope),
            Expr::Member(object, member) => self.member(object, member, scope),
            Expr::Call(callee, arguments) => self.call(callee, arguments, scope),
        }
    }

    fn identifier(&mut self, name: &str, scope: &Scope) -> RuntimeValue {
        if let Some(value) = scope.get(name) {
            return value.clone();
        }
        match name {
            "note" => RuntimeValue::Object(
                self.context
                    .note
                    .iter()
                    .map(|(key, value)| (key.clone(), self.note_property(key, value)))
                    .collect(),
            ),
            "file" => RuntimeValue::File(Box::new(self.context.file.clone())),
            "this" => RuntimeValue::Object(BTreeMap::from([(
                "file".to_string(),
                RuntimeValue::File(Box::new(
                    self.context
                        .this_file
                        .clone()
                        .unwrap_or_else(|| self.context.file.clone()),
                )),
            )])),
            "formula" => RuntimeValue::Object(BTreeMap::new()),
            "values" => self
                .context
                .note
                .get("values")
                .map(|value| from_json(value, None))
                .unwrap_or(RuntimeValue::Null),
            _ => self
                .context
                .note
                .get(name)
                .map(|value| self.note_property(name, value))
                .unwrap_or(RuntimeValue::Null),
        }
    }

    fn note_property(&self, name: &str, value: &Value) -> RuntimeValue {
        let hint = self.context.property_types.get(name).map(String::as_str);
        if hint == Some("date") {
            return RuntimeValue::Date(parse_date(
                value.as_str().unwrap_or_default(),
                &self.timezone,
            ));
        }
        if hint == Some("link") {
            if let Some(link) = runtime_link_from_json(value) {
                return RuntimeValue::Link(link);
            }
            return RuntimeValue::Link(
                self.make_link(&plain_string(&from_json(value, None)), None),
            );
        }
        from_json(value, hint)
    }

    fn binary(&mut self, operator: &str, left: &Expr, right: &Expr, scope: &Scope) -> RuntimeValue {
        if operator == "&&" {
            let left = self.evaluate(left, scope);
            return RuntimeValue::Bool(is_truthy(&left) && is_truthy(&self.evaluate(right, scope)));
        }
        if operator == "||" {
            let left = self.evaluate(left, scope);
            return RuntimeValue::Bool(is_truthy(&left) || is_truthy(&self.evaluate(right, scope)));
        }
        let left = self.evaluate(left, scope);
        let right = self.evaluate(right, scope);
        if let RuntimeValue::Error(_) = left {
            return left;
        }
        if let RuntimeValue::Error(_) = right {
            return right;
        }
        match operator {
            "+" => self.add(left, right),
            "-" => self.subtract(left, right),
            "*" => match (&left, &right) {
                (RuntimeValue::Duration(value), RuntimeValue::Number(scale)) => {
                    RuntimeValue::Duration(scale_duration(value, *scale))
                }
                (RuntimeValue::Number(_), RuntimeValue::Duration(_)) => {
                    RuntimeValue::Error("Invalid operator between Number and Duration".to_string())
                }
                _ => numeric_pair(self, &left, &right, |a, b| a * b),
            },
            "/" => numeric_pair(self, &left, &right, |a, b| a / b),
            "%" => numeric_pair(self, &left, &right, |a, b| a % b),
            "==" => RuntimeValue::Bool(self.equals(&left, &right)),
            "!=" => RuntimeValue::Bool(!self.equals(&left, &right)),
            ">" | "<" | ">=" | "<=" => self
                .compare(&left, &right, operator)
                .map(RuntimeValue::Bool)
                .unwrap_or_else(RuntimeValue::Error),
            _ => RuntimeValue::Error(format!("Unsupported operator {operator}")),
        }
    }

    fn add(&self, left: RuntimeValue, right: RuntimeValue) -> RuntimeValue {
        if let RuntimeValue::Date(date) = &left {
            if let Some(duration) = coerce_duration(&right) {
                return RuntimeValue::Date(add_duration(date, &duration, 1.0));
            }
        }
        if matches!(left, RuntimeValue::String(_)) || matches!(right, RuntimeValue::String(_)) {
            return RuntimeValue::String(format!(
                "{}{}",
                plain_string(&left),
                plain_string(&right)
            ));
        }
        if let (RuntimeValue::Duration(left), RuntimeValue::Duration(right)) = (&left, &right) {
            return RuntimeValue::Duration(add_durations(left, right));
        }
        numeric_pair(self, &left, &right, |a, b| a + b)
    }

    fn subtract(&self, left: RuntimeValue, right: RuntimeValue) -> RuntimeValue {
        if let (RuntimeValue::Date(left), RuntimeValue::Date(right)) = (&left, &right) {
            return RuntimeValue::Duration(DurationValue {
                milliseconds: (left.millis - right.millis) as f64,
                ..Default::default()
            });
        }
        if let RuntimeValue::Date(date) = &left {
            if let Some(duration) = coerce_duration(&right) {
                return RuntimeValue::Date(add_duration(date, &duration, -1.0));
            }
        }
        numeric_pair(self, &left, &right, |a, b| a - b)
    }

    fn member(&mut self, object: &Expr, member: &Member, scope: &Scope) -> RuntimeValue {
        if let (Expr::Identifier(namespace), Member::Named(property)) = (object, member) {
            if namespace == "formula" {
                return self.formula(property);
            }
            if namespace == "note" {
                return self
                    .context
                    .note
                    .get(property)
                    .map(|value| self.note_property(property, value))
                    .unwrap_or(RuntimeValue::Null);
            }
        }
        let object = self.evaluate(object, scope);
        let property = match member {
            Member::Named(value) => value.clone(),
            Member::Computed(value) => plain_string(&self.evaluate(value, scope)),
        };
        self.property(object, &property)
    }

    fn call(&mut self, callee: &Expr, arguments: &[Expr], scope: &Scope) -> RuntimeValue {
        match callee {
            Expr::Identifier(name) if name == "if" => {
                let condition = arguments
                    .first()
                    .map(|value| self.evaluate(value, scope))
                    .unwrap_or(RuntimeValue::Null);
                if is_truthy(&condition) {
                    arguments
                        .get(1)
                        .map(|value| self.evaluate(value, scope))
                        .unwrap_or(RuntimeValue::Null)
                } else {
                    arguments
                        .get(2)
                        .map(|value| self.evaluate(value, scope))
                        .unwrap_or(RuntimeValue::Null)
                }
            }
            Expr::Identifier(name) => {
                let arguments = arguments
                    .iter()
                    .map(|value| self.evaluate(value, scope))
                    .collect::<Vec<_>>();
                self.global(name, &arguments)
            }
            Expr::Member(object, Member::Named(method)) => {
                let receiver = self.evaluate(object, scope);
                self.method(receiver, method, arguments, scope)
            }
            _ => RuntimeValue::Error("Expression is not callable".to_string()),
        }
    }

    fn global(&self, name: &str, arguments: &[RuntimeValue]) -> RuntimeValue {
        let first = arguments.first().cloned().unwrap_or(RuntimeValue::Null);
        match name {
            "escapeHTML" => RuntimeValue::String(escape_html(&plain_string(&first))),
            "date" => RuntimeValue::Date(parse_date(&plain_string(&first), &self.timezone)),
            "duration" => parse_duration(&plain_string(&first))
                .map(RuntimeValue::Duration)
                .unwrap_or_else(|| RuntimeValue::Error("Invalid duration".to_string())),
            "file" => match first {
                RuntimeValue::File(file) => RuntimeValue::File(file),
                RuntimeValue::Link(link) => self.file_from_link(&link),
                value => self.file_from_target(&plain_string(&value)),
            },
            "html" => RuntimeValue::Html(plain_string(&first)),
            "image" => RuntimeValue::Image(match first {
                RuntimeValue::File(file) => file.path,
                RuntimeValue::Link(link) => link.path,
                value => plain_string(&value),
            }),
            "icon" => RuntimeValue::Icon(plain_string(&first)),
            "link" => RuntimeValue::Link(
                self.make_link(&plain_string(&first), arguments.get(1).map(plain_string)),
            ),
            "list" => match first {
                RuntimeValue::List(_) => first,
                value => RuntimeValue::List(vec![value]),
            },
            "max" | "min" => {
                let numbers = arguments
                    .iter()
                    .map(|value| self.number(value))
                    .collect::<Result<Vec<_>, _>>();
                match numbers {
                    Ok(values) => RuntimeValue::Number(
                        values
                            .into_iter()
                            .reduce(if name == "max" { f64::max } else { f64::min })
                            .unwrap_or(if name == "max" {
                                f64::NEG_INFINITY
                            } else {
                                f64::INFINITY
                            }),
                    ),
                    Err(error) => RuntimeValue::Error(error),
                }
            }
            "now" => RuntimeValue::Date(self.now.clone()),
            "number" => self
                .number(&first)
                .map(RuntimeValue::Number)
                .unwrap_or_else(RuntimeValue::Error),
            "today" => RuntimeValue::Date(start_of_day(&self.now)),
            "random" => RuntimeValue::Number(0.5),
            _ => RuntimeValue::Error(format!("Cannot find function \"{name}\"")),
        }
    }

    fn method(
        &mut self,
        receiver: RuntimeValue,
        name: &str,
        arguments: &[Expr],
        scope: &Scope,
    ) -> RuntimeValue {
        if let RuntimeValue::Error(_) = receiver {
            return receiver;
        }
        if name == "isTruthy" {
            return RuntimeValue::Bool(is_truthy(&receiver));
        }
        if name == "isEmpty" {
            return RuntimeValue::Bool(is_empty(&receiver));
        }
        if name == "toString" {
            return RuntimeValue::String(stringify(&receiver));
        }
        if name == "isType" {
            let expected = arguments
                .first()
                .map(|value| plain_string(&self.evaluate(value, scope)))
                .unwrap_or_default();
            return RuntimeValue::Bool(value_type(&receiver).eq_ignore_ascii_case(&expected));
        }
        match receiver {
            RuntimeValue::String(value) => {
                let values = arguments
                    .iter()
                    .map(|argument| self.evaluate(argument, scope))
                    .collect::<Vec<_>>();
                self.string_method(&value, name, &values)
            }
            RuntimeValue::Number(value) => {
                let values = arguments
                    .iter()
                    .map(|argument| self.evaluate(argument, scope))
                    .collect::<Vec<_>>();
                self.number_method(value, name, &values)
            }
            RuntimeValue::Date(value) => {
                let values = arguments
                    .iter()
                    .map(|argument| self.evaluate(argument, scope))
                    .collect::<Vec<_>>();
                self.date_method(&value, name, &values)
            }
            RuntimeValue::List(values) => self.list_method(&values, name, arguments, scope),
            RuntimeValue::Object(value) => match name {
                "keys" => {
                    RuntimeValue::List(value.keys().cloned().map(RuntimeValue::String).collect())
                }
                "values" => RuntimeValue::List(value.values().cloned().collect()),
                _ => method_not_found("Object", name),
            },
            RuntimeValue::Regex(pattern, flags) if name == "matches" => {
                let candidate = arguments
                    .first()
                    .map(|value| plain_string(&self.evaluate(value, scope)))
                    .unwrap_or_default();
                match regex_is_match(&pattern, &flags, &candidate) {
                    Ok(matches) => RuntimeValue::Bool(matches),
                    Err(error) => RuntimeValue::Error(error),
                }
            }
            RuntimeValue::File(file) => {
                let values = arguments
                    .iter()
                    .map(|argument| self.evaluate(argument, scope))
                    .collect::<Vec<_>>();
                self.file_method(&file, name, &values)
            }
            RuntimeValue::Link(link) => {
                let values = arguments
                    .iter()
                    .map(|argument| self.evaluate(argument, scope))
                    .collect::<Vec<_>>();
                self.link_method(&link, name, &values)
            }
            value => method_not_found(value_type(&value), name),
        }
    }

    fn string_method(&self, value: &str, name: &str, arguments: &[RuntimeValue]) -> RuntimeValue {
        let first = arguments.first().cloned().unwrap_or(RuntimeValue::Null);
        match name {
            "contains" => RuntimeValue::Bool(value.contains(&plain_string(&first))),
            "containsAll" => RuntimeValue::Bool(
                arguments
                    .iter()
                    .all(|argument| value.contains(&plain_string(argument))),
            ),
            "containsAny" => RuntimeValue::Bool(
                arguments
                    .iter()
                    .any(|argument| value.contains(&plain_string(argument))),
            ),
            "endsWith" => RuntimeValue::Bool(value.ends_with(&plain_string(&first))),
            "lower" => RuntimeValue::String(value.to_lowercase()),
            "replace" => {
                let replacement = arguments.get(1).map(plain_string).unwrap_or_default();
                match first {
                    RuntimeValue::Regex(pattern, flags) => {
                        regex_replace(value, &pattern, &flags, &replacement)
                            .map(RuntimeValue::String)
                            .unwrap_or_else(RuntimeValue::Error)
                    }
                    other => {
                        RuntimeValue::String(value.replace(&plain_string(&other), &replacement))
                    }
                }
            }
            "repeat" => self
                .number(&first)
                .map(|count| RuntimeValue::String(value.repeat(count.max(0.0) as usize)))
                .unwrap_or_else(RuntimeValue::Error),
            "reverse" => RuntimeValue::String(value.chars().rev().collect()),
            "slice" => string_slice(
                value,
                self.number(&first).unwrap_or(0.0) as isize,
                arguments
                    .get(1)
                    .and_then(|item| self.number(item).ok())
                    .map(|value| value as isize),
            ),
            "split" => string_split(
                value,
                &first,
                arguments
                    .get(1)
                    .and_then(|item| self.number(item).ok())
                    .map(|value| value as usize),
            ),
            "startsWith" => RuntimeValue::Bool(value.starts_with(&plain_string(&first))),
            "title" => RuntimeValue::String(title_case(value)),
            "trim" => RuntimeValue::String(value.trim().to_string()),
            _ => method_not_found("String", name),
        }
    }

    fn number_method(&self, value: f64, name: &str, arguments: &[RuntimeValue]) -> RuntimeValue {
        match name {
            "abs" => RuntimeValue::Number(value.abs()),
            "ceil" => RuntimeValue::Number(value.ceil()),
            "floor" => RuntimeValue::Number(value.floor()),
            "round" => {
                let digits = arguments
                    .first()
                    .and_then(|item| self.number(item).ok())
                    .unwrap_or(0.0);
                let factor = 10_f64.powf(digits);
                RuntimeValue::Number((value * factor).round() / factor)
            }
            "toFixed" => {
                let digits = arguments
                    .first()
                    .and_then(|item| self.number(item).ok())
                    .unwrap_or(0.0)
                    .max(0.0) as usize;
                RuntimeValue::String(format!("{value:.digits$}"))
            }
            _ => method_not_found("Number", name),
        }
    }

    fn date_method(
        &self,
        value: &DateValue,
        name: &str,
        arguments: &[RuntimeValue],
    ) -> RuntimeValue {
        match name {
            "date" => RuntimeValue::Date(start_of_day(value)),
            "format" => RuntimeValue::String(format_date(
                value,
                &arguments.first().map(plain_string).unwrap_or_default(),
            )),
            "time" => RuntimeValue::String(format_date(value, "HH:mm:ss")),
            "relative" => RuntimeValue::String(relative_date(value, &self.now)),
            _ => method_not_found("Date", name),
        }
    }

    fn list_method(
        &mut self,
        values: &[RuntimeValue],
        name: &str,
        arguments: &[Expr],
        scope: &Scope,
    ) -> RuntimeValue {
        match name {
            "contains" => {
                let needle = arguments
                    .first()
                    .map(|value| self.evaluate(value, scope))
                    .unwrap_or(RuntimeValue::Null);
                RuntimeValue::Bool(values.iter().any(|value| self.equals(value, &needle)))
            }
            "containsAll" => RuntimeValue::Bool(arguments.iter().all(|argument| {
                let needle = self.evaluate(argument, scope);
                values.iter().any(|value| self.equals(value, &needle))
            })),
            "containsAny" => RuntimeValue::Bool(arguments.iter().any(|argument| {
                let needle = self.evaluate(argument, scope);
                values.iter().any(|value| self.equals(value, &needle))
            })),
            "filter" => RuntimeValue::List(
                values
                    .iter()
                    .enumerate()
                    .filter_map(|(index, value)| {
                        let mut nested = scope.clone();
                        nested.insert("value".to_string(), value.clone());
                        nested.insert("index".to_string(), RuntimeValue::Number(index as f64));
                        arguments
                            .first()
                            .is_some_and(|expression| {
                                is_truthy(&self.evaluate(expression, &nested))
                            })
                            .then(|| value.clone())
                    })
                    .collect(),
            ),
            "flat" => RuntimeValue::List(
                values
                    .iter()
                    .flat_map(|value| match value {
                        RuntimeValue::List(values) => values.clone(),
                        value => vec![value.clone()],
                    })
                    .collect(),
            ),
            "join" => {
                let separator = arguments
                    .first()
                    .map(|value| plain_string(&self.evaluate(value, scope)))
                    .unwrap_or_default();
                RuntimeValue::String(
                    values
                        .iter()
                        .map(stringify)
                        .collect::<Vec<_>>()
                        .join(&separator),
                )
            }
            "map" => RuntimeValue::List(
                values
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        let mut nested = scope.clone();
                        nested.insert("value".to_string(), value.clone());
                        nested.insert("index".to_string(), RuntimeValue::Number(index as f64));
                        arguments
                            .first()
                            .map(|expression| self.evaluate(expression, &nested))
                            .unwrap_or(RuntimeValue::Null)
                    })
                    .collect(),
            ),
            "reduce" => {
                let mut accumulator = arguments
                    .get(1)
                    .map(|value| self.evaluate(value, scope))
                    .unwrap_or(RuntimeValue::Null);
                for (index, value) in values.iter().enumerate() {
                    let mut nested = scope.clone();
                    nested.insert("acc".to_string(), accumulator);
                    nested.insert("value".to_string(), value.clone());
                    nested.insert("index".to_string(), RuntimeValue::Number(index as f64));
                    accumulator = arguments
                        .first()
                        .map(|expression| self.evaluate(expression, &nested))
                        .unwrap_or(RuntimeValue::Null);
                }
                accumulator
            }
            "reverse" => RuntimeValue::List(values.iter().cloned().rev().collect()),
            "slice" => {
                let start = arguments
                    .first()
                    .map(|value| self.evaluate(value, scope))
                    .and_then(|value| self.number(&value).ok())
                    .unwrap_or(0.0) as isize;
                let end = arguments
                    .get(1)
                    .map(|value| self.evaluate(value, scope))
                    .and_then(|value| self.number(&value).ok())
                    .map(|value| value as isize);
                RuntimeValue::List(slice_values(values, start, end))
            }
            "sort" => {
                let mut output = values.to_vec();
                output.sort_by(compare_sort_keys);
                RuntimeValue::List(output)
            }
            "unique" => {
                let mut seen = HashSet::new();
                RuntimeValue::List(
                    values
                        .iter()
                        .filter(|value| seen.insert(unique_key(value)))
                        .cloned()
                        .collect(),
                )
            }
            "sum" => values
                .iter()
                .map(|value| self.number(value))
                .sum::<Result<f64, _>>()
                .map(RuntimeValue::Number)
                .unwrap_or_else(RuntimeValue::Error),
            "mean" => {
                if values.is_empty() {
                    RuntimeValue::Null
                } else {
                    values
                        .iter()
                        .map(|value| self.number(value))
                        .sum::<Result<f64, _>>()
                        .map(|sum| RuntimeValue::Number(sum / values.len() as f64))
                        .unwrap_or_else(RuntimeValue::Error)
                }
            }
            _ => method_not_found("List", name),
        }
    }

    fn file_method(
        &self,
        file: &BasesFile,
        name: &str,
        arguments: &[RuntimeValue],
    ) -> RuntimeValue {
        match name {
            "asLink" => RuntimeValue::Link(BasesLink {
                path: file.path.clone(),
                display: arguments.first().map(plain_string),
                resolved_path: Some(Some(file.path.clone())),
                external: false,
            }),
            "hasLink" => RuntimeValue::Bool(file.links.iter().any(|link| {
                arguments
                    .first()
                    .is_some_and(|target| self.link_matches(link, target))
            })),
            "hasProperty" => RuntimeValue::Bool(
                arguments
                    .first()
                    .is_some_and(|value| file.properties.contains_key(&plain_string(value))),
            ),
            "hasTag" => {
                let needles = arguments
                    .iter()
                    .map(|value| normalize_tag(&plain_string(value)))
                    .collect::<Vec<_>>();
                RuntimeValue::Bool(needles.iter().any(|needle| {
                    file.tags
                        .iter()
                        .map(|tag| normalize_tag(tag))
                        .any(|tag| tag == *needle || tag.starts_with(&format!("{needle}/")))
                }))
            }
            "inFolder" => {
                let folder = arguments
                    .first()
                    .map(plain_string)
                    .unwrap_or_default()
                    .trim_end_matches('/')
                    .to_string();
                RuntimeValue::Bool(
                    file.folder == folder || file.folder.starts_with(&format!("{folder}/")),
                )
            }
            _ => method_not_found("File", name),
        }
    }

    fn link_method(
        &self,
        link: &BasesLink,
        name: &str,
        arguments: &[RuntimeValue],
    ) -> RuntimeValue {
        match name {
            "asFile" => self.file_from_link(link),
            "linksTo" => match self.file_from_link(link) {
                RuntimeValue::File(file) => self.file_method(&file, "hasLink", arguments),
                _ => RuntimeValue::Error("Could not coerce link to file".to_string()),
            },
            _ => method_not_found("Link", name),
        }
    }

    fn property(&self, object: RuntimeValue, property: &str) -> RuntimeValue {
        match object {
            RuntimeValue::Null => RuntimeValue::Null,
            RuntimeValue::String(value) if property == "length" => {
                RuntimeValue::Number(value.chars().count() as f64)
            }
            RuntimeValue::List(values) if property == "length" => {
                RuntimeValue::Number(values.len() as f64)
            }
            RuntimeValue::List(values) => property
                .parse::<isize>()
                .ok()
                .and_then(|index| index_value(&values, index))
                .unwrap_or_else(|| member_not_found("List", property)),
            RuntimeValue::Object(values) => {
                values.get(property).cloned().unwrap_or(RuntimeValue::Null)
            }
            RuntimeValue::Date(value) => date_property(&value, property),
            RuntimeValue::File(value) => file_property(&value, property, &self.timezone),
            RuntimeValue::Error(message) => RuntimeValue::Error(message),
            value => member_not_found(value_type(&value), property),
        }
    }

    fn formula(&mut self, name: &str) -> RuntimeValue {
        if let Some(value) = self.formula_cache.get(name) {
            return value.clone();
        }
        let Some(expression) = self.context.formulas.get(name) else {
            return RuntimeValue::Null;
        };
        if !self.formula_stack.insert(name.to_string()) {
            return RuntimeValue::Error(format!("Circular formula reference {name}"));
        }
        let value = parse_cached(expression)
            .map(|expression| self.evaluate(expression.as_ref(), &BTreeMap::new()))
            .unwrap_or_else(RuntimeValue::Error);
        self.formula_stack.remove(name);
        self.formula_cache.insert(name.to_string(), value.clone());
        value
    }

    fn number(&self, value: &RuntimeValue) -> Result<f64, String> {
        match value {
            RuntimeValue::Number(value) => Ok(*value),
            RuntimeValue::Bool(value) => Ok(if *value { 1.0 } else { 0.0 }),
            RuntimeValue::String(value) => value
                .parse()
                .map_err(|_| format!("Unable to parse {value:?} as a number.")),
            RuntimeValue::Date(value) => Ok(value.millis as f64),
            RuntimeValue::Duration(value) => Ok(duration_millis(value)),
            RuntimeValue::Null => Ok(0.0),
            value => Err(format!("Cannot convert {} to number", value_type(value))),
        }
    }

    fn equals(&self, left: &RuntimeValue, right: &RuntimeValue) -> bool {
        match (left, right) {
            (RuntimeValue::Link(left), RuntimeValue::Link(right)) => {
                self.link_identity(left) == self.link_identity(right)
            }
            (RuntimeValue::Link(left), RuntimeValue::File(right)) => self
                .link_resolved(left)
                .is_some_and(|path| path == right.path),
            (RuntimeValue::File(left), RuntimeValue::Link(right)) => self
                .link_resolved(right)
                .is_some_and(|path| path == left.path),
            (RuntimeValue::Date(left), RuntimeValue::Date(right)) => left.millis == right.millis,
            _ => to_plain(left) == to_plain(right),
        }
    }

    fn compare(
        &self,
        left: &RuntimeValue,
        right: &RuntimeValue,
        operator: &str,
    ) -> Result<bool, String> {
        let ordering =
            if let (RuntimeValue::String(left), RuntimeValue::String(right)) = (left, right) {
                left.cmp(right)
            } else {
                self.number(left)?
                    .partial_cmp(&self.number(right)?)
                    .unwrap_or(std::cmp::Ordering::Equal)
            };
        Ok(match operator {
            ">" => ordering.is_gt(),
            "<" => ordering.is_lt(),
            ">=" => !ordering.is_lt(),
            _ => !ordering.is_gt(),
        })
    }

    fn make_link(&self, target: &str, display: Option<String>) -> BasesLink {
        let (target, parsed_display) = parse_link_text(target);
        BasesLink {
            resolved_path: Some(self.resolve_link(&target)),
            external: is_external(&target),
            path: target,
            display: display.or(parsed_display),
        }
    }

    fn resolve_link(&self, target: &str) -> Option<String> {
        for candidate in link_resolution_keys(target) {
            if let Some(value) = self.context.link_resolutions.get(&candidate) {
                return value.clone();
            }
            if let Some(value) = self.context.link_resolutions.get(&candidate.to_lowercase()) {
                return value.clone();
            }
        }
        self.find_file(target).map(|file| file.path.clone())
    }

    fn find_file(&self, target: &str) -> Option<&BasesFile> {
        let target = strip_subpath(target);
        let markdown = ensure_markdown_extension(&target);
        let lower_target = target.to_lowercase();
        let lower_markdown = markdown.to_lowercase();
        let lower_basename = strip_markdown_extension(&target).to_lowercase();
        self.context
            .files
            .iter()
            .find(|file| normalize_path(&file.path) == normalize_path(&target))
            .or_else(|| {
                self.context
                    .files
                    .iter()
                    .find(|file| normalize_path(&file.path) == normalize_path(&markdown))
            })
            .or_else(|| {
                self.context
                    .files
                    .iter()
                    .find(|file| normalize_path(&file.path).ends_with(&format!("/{markdown}")))
            })
            .or_else(|| {
                self.context.files.iter().find(|file| {
                    file.basename == target || strip_markdown_extension(&file.name) == target
                })
            })
            .or_else(|| {
                self.context
                    .files
                    .iter()
                    .find(|file| normalize_path(&file.path).to_lowercase() == lower_target)
            })
            .or_else(|| {
                self.context
                    .files
                    .iter()
                    .find(|file| normalize_path(&file.path).to_lowercase() == lower_markdown)
            })
            .or_else(|| {
                self.context.files.iter().find(|file| {
                    file.name.to_lowercase() == lower_target
                        || file.name.to_lowercase() == lower_markdown
                })
            })
            .or_else(|| {
                self.context
                    .files
                    .iter()
                    .find(|file| file.basename.to_lowercase() == lower_basename)
            })
    }

    fn file_from_target(&self, target: &str) -> RuntimeValue {
        let resolved = self.resolve_link(target);
        match resolved {
            None => RuntimeValue::Null,
            Some(path) => self
                .context
                .files
                .iter()
                .find(|file| file.path == path)
                .cloned()
                .map(|file| RuntimeValue::File(Box::new(file)))
                .unwrap_or_else(|| RuntimeValue::File(Box::new(file_defaults(&path)))),
        }
    }

    fn file_from_link(&self, link: &BasesLink) -> RuntimeValue {
        match link
            .resolved_path
            .clone()
            .flatten()
            .or_else(|| self.resolve_link(&link.path))
        {
            None => RuntimeValue::Null,
            Some(path) => self
                .context
                .files
                .iter()
                .find(|file| file.path == path)
                .cloned()
                .map(|file| RuntimeValue::File(Box::new(file)))
                .unwrap_or_else(|| RuntimeValue::File(Box::new(file_defaults(&path)))),
        }
    }

    fn link_resolved(&self, link: &BasesLink) -> Option<String> {
        link.resolved_path
            .clone()
            .flatten()
            .or_else(|| self.resolve_link(&link.path))
            .or_else(|| Some(strip_subpath(&link.path)))
    }

    fn link_identity(&self, link: &BasesLink) -> String {
        self.link_resolved(link)
            .map(|path| format!("file:{path}"))
            .unwrap_or_else(|| format!("link:{}", link.path))
    }

    fn link_matches(&self, link: &BasesLink, target: &RuntimeValue) -> bool {
        let target_path = match target {
            RuntimeValue::File(file) => file.path.clone(),
            RuntimeValue::Link(link) => link.path.clone(),
            value => parse_link_text(&plain_string(value)).0,
        };
        if link.path == target_path {
            return true;
        }
        if self.link_is_broken(link) || self.target_is_broken(target, &target_path) {
            return false;
        }
        let left = self.link_resolved(link);
        let right = match target {
            RuntimeValue::File(file) => Some(file.path.clone()),
            RuntimeValue::Link(link) => self.link_resolved(link),
            _ => self.resolve_link(&target_path),
        };
        match (left, right) {
            (Some(left), Some(right)) => left == right,
            _ => same_markdown_variant(&link.path, &target_path),
        }
    }

    fn link_is_broken(&self, link: &BasesLink) -> bool {
        if link.resolved_path == Some(None) {
            return true;
        }
        link_resolution_keys(&link.path)
            .into_iter()
            .any(|candidate| {
                self.context.link_resolutions.get(&candidate) == Some(&None)
                    || self.context.link_resolutions.get(&candidate.to_lowercase()) == Some(&None)
            })
    }

    fn target_is_broken(&self, target: &RuntimeValue, target_path: &str) -> bool {
        match target {
            RuntimeValue::File(_) => false,
            RuntimeValue::Link(link) => self.link_is_broken(link),
            _ => link_resolution_keys(target_path)
                .into_iter()
                .any(|candidate| {
                    self.context.link_resolutions.get(&candidate) == Some(&None)
                        || self.context.link_resolutions.get(&candidate.to_lowercase())
                            == Some(&None)
                }),
        }
    }
}

fn numeric_pair(
    evaluator: &Evaluator<'_>,
    left: &RuntimeValue,
    right: &RuntimeValue,
    operation: impl FnOnce(f64, f64) -> f64,
) -> RuntimeValue {
    match (evaluator.number(left), evaluator.number(right)) {
        (Ok(left), Ok(right)) => RuntimeValue::Number(operation(left, right)),
        (Err(error), _) | (_, Err(error)) => RuntimeValue::Error(error),
    }
}

fn from_json(value: &Value, _hint: Option<&str>) -> RuntimeValue {
    if let Some(link) = runtime_link_from_json(value) {
        return RuntimeValue::Link(link);
    }
    match value {
        Value::Null => RuntimeValue::Null,
        Value::Bool(value) => RuntimeValue::Bool(*value),
        Value::Number(value) => RuntimeValue::Number(value.as_f64().unwrap_or(f64::NAN)),
        Value::String(value) => RuntimeValue::String(value.clone()),
        Value::Array(values) => {
            RuntimeValue::List(values.iter().map(|value| from_json(value, None)).collect())
        }
        Value::Object(values) => RuntimeValue::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), from_json(value, None)))
                .collect(),
        ),
    }
}

fn runtime_link_from_json(value: &Value) -> Option<BasesLink> {
    let object = value.as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("Link") {
        return None;
    }
    let link = object.get("value")?.as_object()?;
    Some(BasesLink {
        path: link.get("path")?.as_str()?.to_string(),
        display: link.get("display").map(runtime_display_string),
        resolved_path: link
            .get("resolvedPath")
            .map(|value| value.as_str().map(String::from)),
        external: link
            .get("external")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn runtime_display_string(value: &Value) -> String {
    value
        .as_object()
        .filter(|value| value.get("type").and_then(Value::as_str) == Some("String"))
        .and_then(|value| value.get("value"))
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| plain_string(&from_json(value, None)))
}

fn to_plain(value: &RuntimeValue) -> Value {
    match value {
        RuntimeValue::Null => Value::Null,
        RuntimeValue::Bool(value) => Value::Bool(*value),
        RuntimeValue::Number(value) => number_json(*value),
        RuntimeValue::String(value)
        | RuntimeValue::Html(value)
        | RuntimeValue::Icon(value)
        | RuntimeValue::Image(value) => Value::String(value.clone()),
        RuntimeValue::Date(value) => Value::String(format_date(
            value,
            if value.date_only {
                "YYYY-MM-DD"
            } else {
                "YYYY-MM-DDTHH:mm:ss"
            },
        )),
        RuntimeValue::Duration(value) => Value::String(format_duration(value)),
        RuntimeValue::List(values) => Value::Array(values.iter().map(to_plain).collect()),
        RuntimeValue::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), to_plain(value)))
                .collect(),
        ),
        RuntimeValue::File(value) => Value::String(value.path.clone()),
        RuntimeValue::Link(value) => Value::String(value.path.clone()),
        RuntimeValue::Regex(pattern, flags) => Value::String(format!("/{pattern}/{flags}")),
        RuntimeValue::Error(message) => Value::Object(Map::from_iter([(
            "error".to_string(),
            Value::String(message.clone()),
        )])),
    }
}

fn plain_string(value: &RuntimeValue) -> String {
    match value {
        RuntimeValue::Null => String::new(),
        RuntimeValue::Bool(value) => value.to_string(),
        RuntimeValue::Number(value) => format_number(*value),
        RuntimeValue::String(value) | RuntimeValue::Html(value) | RuntimeValue::Icon(value) => {
            value.clone()
        }
        RuntimeValue::Date(value) => format_date(
            value,
            if value.date_only {
                "YYYY-MM-DD"
            } else {
                "YYYY-MM-DDTHH:mm:ss"
            },
        ),
        RuntimeValue::Duration(value) => format_duration(value),
        RuntimeValue::List(values) => values
            .iter()
            .map(plain_string)
            .collect::<Vec<_>>()
            .join(","),
        RuntimeValue::Object(_) => serde_json::to_string(&to_plain(value)).unwrap_or_default(),
        RuntimeValue::File(value) => value.path.clone(),
        RuntimeValue::Link(value) if value.external => value.path.clone(),
        RuntimeValue::Link(value) => value
            .display
            .as_ref()
            .map(|display| format!("[[{}|{display}]]", value.path))
            .unwrap_or_else(|| format!("[[{}]]", value.path)),
        RuntimeValue::Regex(pattern, flags) => format!("/{pattern}/{flags}"),
        RuntimeValue::Image(value) => format!("![]({value})"),
        RuntimeValue::Error(message) => message.clone(),
    }
}

fn stringify(value: &RuntimeValue) -> String {
    plain_string(value)
}

fn is_truthy(value: &RuntimeValue) -> bool {
    match value {
        RuntimeValue::Null | RuntimeValue::Error(_) => false,
        RuntimeValue::Bool(value) => *value,
        RuntimeValue::Number(value) => *value != 0.0 && !value.is_nan(),
        RuntimeValue::String(value) => !value.is_empty(),
        _ => true,
    }
}

fn is_empty(value: &RuntimeValue) -> bool {
    match value {
        RuntimeValue::Null => true,
        RuntimeValue::String(value) => value.is_empty(),
        RuntimeValue::Number(value) => value.is_nan(),
        RuntimeValue::List(values) => values.is_empty(),
        RuntimeValue::Object(values) => values.is_empty(),
        _ => false,
    }
}

fn value_type(value: &RuntimeValue) -> &'static str {
    match value {
        RuntimeValue::Null => "Null",
        RuntimeValue::Bool(_) => "Boolean",
        RuntimeValue::Number(_) => "Number",
        RuntimeValue::String(_) => "String",
        RuntimeValue::Date(_) => "Date",
        RuntimeValue::Duration(_) => "Duration",
        RuntimeValue::List(_) => "List",
        RuntimeValue::Object(_) => "Object",
        RuntimeValue::File(_) => "File",
        RuntimeValue::Link(_) => "Link",
        RuntimeValue::Regex(_, _) => "RegExp",
        RuntimeValue::Html(_) => "HTML",
        RuntimeValue::Image(_) => "Image",
        RuntimeValue::Icon(_) => "Icon",
        RuntimeValue::Error(_) => "Error",
    }
}

fn member_not_found(kind: &str, property: &str) -> RuntimeValue {
    RuntimeValue::Error(format!("Cannot find \"{property}\" on type {kind}"))
}
fn method_not_found(kind: &str, method: &str) -> RuntimeValue {
    RuntimeValue::Error(format!("Cannot find function \"{method}\" on type {kind}"))
}

fn number_json(value: f64) -> Value {
    if value.is_finite()
        && value.fract() == 0.0
        && value >= i64::MIN as f64
        && value <= i64::MAX as f64
    {
        return Value::Number((value as i64).into());
    }
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn parse_timezone_offset(value: &str) -> Option<FixedOffset> {
    let sign = match value.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (hours, minutes) = value.get(1..)?.split_once(':')?;
    let seconds = sign * (hours.parse::<i32>().ok()? * 3_600 + minutes.parse::<i32>().ok()? * 60);
    FixedOffset::east_opt(seconds)
}

fn parse_date(value: &str, timezone: &BasesTimezone) -> DateValue {
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        let local = date.and_hms_opt(0, 0, 0).expect("valid midnight");
        return DateValue {
            millis: timezone.millis_from_local(local),
            date_only: true,
            timezone: timezone.clone(),
        };
    }
    if let Ok(value) = DateTime::parse_from_rfc3339(value) {
        return DateValue {
            millis: value.timestamp_millis(),
            date_only: false,
            timezone: timezone.clone(),
        };
    }
    for format in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(value) = NaiveDateTime::parse_from_str(value, format) {
            return DateValue {
                millis: timezone.millis_from_local(value),
                date_only: false,
                timezone: timezone.clone(),
            };
        }
    }
    DateValue {
        millis: 0,
        date_only: false,
        timezone: timezone.clone(),
    }
}

fn start_of_day(value: &DateValue) -> DateValue {
    let date = value.timezone.local_datetime(value.millis).date();
    let local = date.and_hms_opt(0, 0, 0).expect("valid midnight");
    DateValue {
        millis: value.timezone.millis_from_local(local),
        date_only: true,
        timezone: value.timezone.clone(),
    }
}

fn format_date(value: &DateValue, pattern: &str) -> String {
    let date = value.timezone.local_datetime(value.millis);
    let mut format = pattern.to_string();
    let literals = Regex::new(r"\[([^]]*)\]").expect("literal expression");
    let mut stored = Vec::new();
    format = literals
        .replace_all(&format, |captures: &regex::Captures<'_>| {
            stored.push(captures[1].to_string());
            format!("__L{}__", stored.len() - 1)
        })
        .to_string();
    for (source, target) in [
        ("YYYY", "%Y"),
        ("yyyy", "%Y"),
        ("WW", "%V"),
        ("MM", "%m"),
        ("DD", "%d"),
        ("dd", "%d"),
        ("HH", "%H"),
        ("mm", "%M"),
        ("ss", "%S"),
    ] {
        format = format.replace(source, target);
    }
    let mut output = date.format(&format).to_string();
    for (index, literal) in stored.iter().enumerate() {
        output = output.replace(&format!("__L{index}__"), literal);
    }
    output
}

fn date_property(value: &DateValue, property: &str) -> RuntimeValue {
    let date = value.timezone.local_datetime(value.millis);
    let number = match property {
        "year" => date.year() as f64,
        "month" => date.month() as f64,
        "day" => date.day() as f64,
        "hour" => date.hour() as f64,
        "minute" => date.minute() as f64,
        "second" => date.second() as f64,
        "millisecond" => date.and_utc().timestamp_subsec_millis() as f64,
        _ => return member_not_found("Date", property),
    };
    RuntimeValue::Number(number)
}

fn parse_duration(value: &str) -> Option<DurationValue> {
    let pattern = Regex::new(r"(?i)([+-]?(?:\d+(?:\.\d+)?|\.\d+))\s*(years?|y|months?|M|weeks?|w|days?|d|hours?|h|minutes?|m|seconds?|s|milliseconds?|ms)\b").expect("duration expression");
    let mut duration = DurationValue::default();
    let mut matched = false;
    for captures in pattern.captures_iter(value.trim()) {
        matched = true;
        let amount = captures[1].parse::<f64>().ok()?;
        let unit = &captures[2];
        if unit == "y" || unit.to_lowercase().starts_with("year") {
            duration.years += amount;
        } else if unit == "M" || unit.to_lowercase().starts_with("month") {
            duration.months += amount;
        } else if unit == "w" || unit.to_lowercase().starts_with("week") {
            duration.weeks += amount;
        } else if unit == "d" || unit.to_lowercase().starts_with("day") {
            duration.days += amount;
        } else if unit == "h" || unit.to_lowercase().starts_with("hour") {
            duration.hours += amount;
        } else if unit == "m" || unit.to_lowercase().starts_with("minute") {
            duration.minutes += amount;
        } else if unit == "s" || unit.to_lowercase().starts_with("second") {
            duration.seconds += amount;
        } else {
            duration.milliseconds += amount;
        }
    }
    matched.then_some(duration)
}

fn duration_millis(value: &DurationValue) -> f64 {
    value.milliseconds
        + value.seconds * 1_000.0
        + value.minutes * 60_000.0
        + value.hours * 3_600_000.0
        + value.days * 86_400_000.0
        + value.weeks * 604_800_000.0
}

fn add_duration(value: &DateValue, duration: &DurationValue, direction: f64) -> DateValue {
    let mut date = value.timezone.local_datetime(value.millis);
    let months = ((duration.years * 12.0 + duration.months) * direction).round() as i32;
    if months > 0 {
        date = date
            .checked_add_months(Months::new(months as u32))
            .unwrap_or(date);
    } else if months < 0 {
        date = date
            .checked_sub_months(Months::new((-months) as u32))
            .unwrap_or(date);
    }
    let millis = ((duration.weeks * 604_800_000.0
        + duration.days * 86_400_000.0
        + duration.hours * 3_600_000.0
        + duration.minutes * 60_000.0
        + duration.seconds * 1_000.0
        + duration.milliseconds)
        * direction)
        .round() as i64;
    DateValue {
        millis: value.timezone.millis_from_local(date) + millis,
        date_only: value.date_only,
        timezone: value.timezone.clone(),
    }
}

fn add_durations(left: &DurationValue, right: &DurationValue) -> DurationValue {
    DurationValue {
        years: left.years + right.years,
        months: left.months + right.months,
        weeks: left.weeks + right.weeks,
        days: left.days + right.days,
        hours: left.hours + right.hours,
        minutes: left.minutes + right.minutes,
        seconds: left.seconds + right.seconds,
        milliseconds: left.milliseconds + right.milliseconds,
    }
}
fn scale_duration(value: &DurationValue, scale: f64) -> DurationValue {
    DurationValue {
        years: value.years * scale,
        months: value.months * scale,
        weeks: value.weeks * scale,
        days: value.days * scale,
        hours: value.hours * scale,
        minutes: value.minutes * scale,
        seconds: value.seconds * scale,
        milliseconds: value.milliseconds * scale,
    }
}
fn coerce_duration(value: &RuntimeValue) -> Option<DurationValue> {
    match value {
        RuntimeValue::Duration(value) => Some(value.clone()),
        RuntimeValue::String(value) => parse_duration(value),
        _ => None,
    }
}

fn format_duration(value: &DurationValue) -> String {
    let millis = duration_millis(value).abs();
    if value.years.abs() >= 1.0 {
        if value.years.abs() < 1.5 {
            "a year".to_string()
        } else {
            pluralize(value.years, "year")
        }
    } else if value.months.abs() >= 1.0 {
        if value.months.abs() < 1.5 {
            "a month".to_string()
        } else {
            pluralize(value.months, "month")
        }
    } else if millis >= 548.0 * 86_400_000.0 {
        pluralize(millis / (365.25 * 86_400_000.0), "year")
    } else if millis >= 320.0 * 86_400_000.0 {
        "a year".to_string()
    } else if millis >= 45.0 * 86_400_000.0 {
        pluralize(millis / (30.0 * 86_400_000.0), "month")
    } else if millis >= 26.0 * 86_400_000.0 {
        "a month".to_string()
    } else if millis >= 36.0 * 3_600_000.0 {
        pluralize(millis / 86_400_000.0, "day")
    } else if millis >= 22.0 * 3_600_000.0 {
        "a day".to_string()
    } else if millis >= 90.0 * 60_000.0 {
        pluralize(millis / 3_600_000.0, "hour")
    } else if millis >= 45.0 * 60_000.0 {
        "an hour".to_string()
    } else if millis >= 90_000.0 {
        pluralize(millis / 60_000.0, "minute")
    } else if millis >= 45_000.0 {
        "a minute".to_string()
    } else {
        "a few seconds".to_string()
    }
}

fn pluralize(value: f64, unit: &str) -> String {
    let rounded = value.round();
    format!(
        "{} {unit}{}",
        format_number(rounded),
        if rounded.abs() == 1.0 { "" } else { "s" }
    )
}

fn relative_date(value: &DateValue, now: &DateValue) -> String {
    let delta = value.millis - now.millis;
    let future = delta > 0;
    let amount = delta.unsigned_abs() as f64;
    let phrase = if amount < 45_000.0 {
        "a few seconds".to_string()
    } else if amount < 90_000.0 {
        "a minute".to_string()
    } else if amount < 45.0 * 60_000.0 {
        format!("{} minutes", (amount / 60_000.0).round())
    } else if amount < 90.0 * 60_000.0 {
        "an hour".to_string()
    } else if amount < 22.0 * 3_600_000.0 {
        format!("{} hours", (amount / 3_600_000.0).round())
    } else if amount < 36.0 * 3_600_000.0 {
        "a day".to_string()
    } else if amount < 26.0 * 86_400_000.0 {
        format!("{} days", (amount / 86_400_000.0).round())
    } else if amount < 45.0 * 86_400_000.0 {
        "a month".to_string()
    } else if amount < 320.0 * 86_400_000.0 {
        format!("{} months", (amount / (30.0 * 86_400_000.0)).round())
    } else if amount < 548.0 * 86_400_000.0 {
        "a year".to_string()
    } else {
        format!("{} years", (amount / (365.25 * 86_400_000.0)).round())
    };
    if future {
        format!("in {phrase}")
    } else {
        format!("{phrase} ago")
    }
}

fn file_defaults(path: &str) -> BasesFile {
    let name = path.rsplit('/').next().unwrap_or(path).to_string();
    let (basename, extension) = name
        .rsplit_once('.')
        .map(|(base, extension)| (base.to_string(), extension.to_string()))
        .unwrap_or((name.clone(), String::new()));
    BasesFile {
        path: path.to_string(),
        name,
        basename,
        folder: path
            .rsplit_once('/')
            .map(|(folder, _)| folder.to_string())
            .unwrap_or_default(),
        extension,
        ..Default::default()
    }
}

fn file_property(file: &BasesFile, property: &str, timezone: &BasesTimezone) -> RuntimeValue {
    match property {
        "name" | "basename" => RuntimeValue::String(file.basename.clone()),
        "path" => RuntimeValue::String(file.path.clone()),
        "folder" => RuntimeValue::String(file.folder.clone()),
        "ext" => RuntimeValue::String(file.extension.clone()),
        "size" => RuntimeValue::Number(file.size as f64),
        "properties" => from_json(&Value::Object(file.properties.clone()), None),
        "tags" => RuntimeValue::List(
            file.tags
                .iter()
                .cloned()
                .map(RuntimeValue::String)
                .collect(),
        ),
        "links" => RuntimeValue::List(file.links.iter().cloned().map(RuntimeValue::Link).collect()),
        "embeds" => RuntimeValue::List(
            file.embeds
                .iter()
                .cloned()
                .map(RuntimeValue::Link)
                .collect(),
        ),
        "backlinks" => RuntimeValue::List(
            file.backlinks
                .iter()
                .cloned()
                .map(RuntimeValue::Link)
                .collect(),
        ),
        "ctime" => RuntimeValue::Date(parse_date(
            file.ctime.as_deref().unwrap_or("1970-01-01T00:00:00Z"),
            timezone,
        )),
        "mtime" => RuntimeValue::Date(parse_date(
            file.mtime.as_deref().unwrap_or("1970-01-01T00:00:00Z"),
            timezone,
        )),
        "file" => RuntimeValue::File(Box::new(file.clone())),
        _ => member_not_found("File", property),
    }
}

fn normalize_tag(value: &str) -> String {
    value.trim_start_matches('#').to_string()
}
fn is_external(value: &str) -> bool {
    Regex::new(r"^[A-Za-z][A-Za-z0-9+.-]*:")
        .expect("external link expression")
        .is_match(value)
}
fn strip_subpath(value: &str) -> String {
    value
        .split_once('#')
        .map(|(path, _)| path)
        .unwrap_or(value)
        .to_string()
}
fn ensure_markdown_extension(value: &str) -> String {
    let (head, tail) = value
        .split_once('#')
        .map(|(head, tail)| (head, format!("#{tail}")))
        .unwrap_or((value, String::new()));
    if head
        .rsplit('/')
        .next()
        .is_some_and(|name| name.contains('.'))
    {
        value.to_string()
    } else {
        format!("{head}.md{tail}")
    }
}
fn strip_markdown_extension(value: &str) -> String {
    value.strip_suffix(".md").unwrap_or(value).to_string()
}
fn normalize_path(value: &str) -> String {
    value.replace('\\', "/").trim_start_matches('/').to_string()
}
fn same_markdown_variant(left: &str, right: &str) -> bool {
    normalize_path(&strip_markdown_extension(&strip_subpath(left)))
        == normalize_path(&strip_markdown_extension(&strip_subpath(right)))
}
fn link_resolution_keys(target: &str) -> Vec<String> {
    let base = strip_subpath(target);
    let candidates = [
        target.to_string(),
        base.clone(),
        ensure_markdown_extension(target),
        ensure_markdown_extension(&base),
        strip_markdown_extension(target),
        strip_markdown_extension(&base),
    ];
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}
fn parse_link_text(value: &str) -> (String, Option<String>) {
    let value = value
        .trim()
        .strip_prefix('!')
        .unwrap_or(value.trim())
        .trim();
    let value = value
        .strip_prefix("[[")
        .and_then(|value| value.strip_suffix("]]"))
        .unwrap_or(value);
    value
        .split_once('|')
        .map(|(target, display)| (target.to_string(), Some(display.to_string())))
        .unwrap_or((value.to_string(), None))
}

fn index_value(values: &[RuntimeValue], index: isize) -> Option<RuntimeValue> {
    let index = if index < 0 {
        values.len() as isize + index
    } else {
        index
    };
    (index >= 0)
        .then(|| values.get(index as usize).cloned())
        .flatten()
}
fn slice_values(values: &[RuntimeValue], start: isize, end: Option<isize>) -> Vec<RuntimeValue> {
    let len = values.len() as isize;
    let start = if start < 0 {
        (len + start).max(0)
    } else {
        start.min(len)
    };
    let end = end
        .map(|end| {
            if end < 0 {
                (len + end).max(0)
            } else {
                end.min(len)
            }
        })
        .unwrap_or(len);
    if end <= start {
        Vec::new()
    } else {
        values[start as usize..end as usize].to_vec()
    }
}
fn string_slice(value: &str, start: isize, end: Option<isize>) -> RuntimeValue {
    let chars = value.chars().collect::<Vec<_>>();
    RuntimeValue::String(
        slice_values(
            &chars
                .iter()
                .map(|character| RuntimeValue::String(character.to_string()))
                .collect::<Vec<_>>(),
            start,
            end,
        )
        .iter()
        .map(plain_string)
        .collect(),
    )
}
fn string_split(value: &str, separator: &RuntimeValue, limit: Option<usize>) -> RuntimeValue {
    let mut values = match separator {
        RuntimeValue::Regex(pattern, flags) => {
            regex_split(value, pattern, flags).unwrap_or_else(|_| vec![value.to_string()])
        }
        other if plain_string(other).is_empty() => value
            .chars()
            .map(|character| character.to_string())
            .collect(),
        other => value
            .split(&plain_string(other))
            .map(String::from)
            .collect(),
    };
    if let Some(limit) = limit {
        values.truncate(limit);
    }
    RuntimeValue::List(values.into_iter().map(RuntimeValue::String).collect())
}
fn compare_sort_keys(left: &RuntimeValue, right: &RuntimeValue) -> std::cmp::Ordering {
    match (left, right) {
        (RuntimeValue::Number(left), RuntimeValue::Number(right)) => {
            left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal)
        }
        _ => plain_string(left).cmp(&plain_string(right)),
    }
}
fn unique_key(value: &RuntimeValue) -> String {
    match value {
        RuntimeValue::Link(_) => stringify(value),
        _ => serde_json::to_string(&to_plain(value)).unwrap_or_default(),
    }
}

fn compile_regex(pattern: &str, flags: &str) -> Result<Regex, String> {
    let mut builder = RegexBuilder::new(pattern);
    builder
        .case_insensitive(flags.contains('i'))
        .multi_line(flags.contains('m'))
        .dot_matches_new_line(flags.contains('s'));
    builder.build().map_err(|error| error.to_string())
}
fn regex_is_match(pattern: &str, flags: &str, value: &str) -> Result<bool, String> {
    match compile_regex(pattern, flags) {
        Ok(regex) => Ok(regex.is_match(value)),
        Err(_) => fancy_regex::Regex::new(pattern)
            .map_err(|error| error.to_string())?
            .is_match(value)
            .map_err(|error| error.to_string()),
    }
}
fn regex_replace(
    value: &str,
    pattern: &str,
    flags: &str,
    replacement: &str,
) -> Result<String, String> {
    let regex = compile_regex(pattern, flags)?;
    Ok(if flags.contains('g') {
        regex.replace_all(value, replacement)
    } else {
        regex.replace(value, replacement)
    }
    .to_string())
}
fn regex_split(value: &str, pattern: &str, flags: &str) -> Result<Vec<String>, String> {
    Ok(compile_regex(pattern, flags)?
        .split(value)
        .map(String::from)
        .collect())
}
fn title_case(value: &str) -> String {
    Regex::new(r"\p{L}+")
        .expect("word expression")
        .replace_all(value, |captures: &regex::Captures<'_>| {
            let word = &captures[0];
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| format!("{}{}", first.to_uppercase(), chars.as_str().to_lowercase()))
                .unwrap_or_default()
        })
        .to_string()
}
fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture_context(value: &Value) -> BasesEvaluationContext {
        let object = value.as_object().expect("oracle context object");
        BasesEvaluationContext {
            note: object
                .get("note")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            file: object.get("file").map(fixture_file).unwrap_or_default(),
            this_file: object.get("thisFile").map(fixture_file),
            files: Arc::new(
                object
                    .get("files")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(fixture_file)
                    .collect(),
            ),
            formulas: Arc::new(
                object
                    .get("formulas")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
                    .collect(),
            ),
            property_types: Arc::new(
                object
                    .get("propertyTypes")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
                    .collect(),
            ),
            link_resolutions: Arc::new(
                object
                    .get("linkResolutions")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                    .map(|(key, value)| (key.clone(), value.as_str().map(String::from)))
                    .collect(),
            ),
            now: object.get("now").and_then(Value::as_str).map(String::from),
            timezone: BasesTimezone::from_setting(object.get("timezone").and_then(Value::as_str))
                .expect("oracle timezone"),
            work_limit: None,
            cancellation: None,
        }
    }

    fn fixture_file(value: &Value) -> BasesFile {
        let object = value.as_object().expect("oracle file object");
        BasesFile {
            path: string_field(object, "path"),
            name: string_field(object, "name"),
            basename: string_field(object, "basename"),
            folder: string_field(object, "folder"),
            extension: string_field(object, "ext"),
            size: object
                .get("size")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            properties: object
                .get("properties")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            tags: object
                .get("tags")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect(),
            links: fixture_links(object.get("links")),
            embeds: fixture_links(object.get("embeds")),
            backlinks: fixture_links(object.get("backlinks")),
            ctime: object
                .get("ctime")
                .and_then(Value::as_str)
                .map(String::from),
            mtime: object
                .get("mtime")
                .and_then(Value::as_str)
                .map(String::from),
        }
    }

    fn fixture_links(value: Option<&Value>) -> Vec<BasesLink> {
        value
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|value| {
                let object = value.as_object()?;
                Some(BasesLink {
                    path: object.get("path")?.as_str()?.to_string(),
                    display: object.get("display").map(runtime_display_string),
                    resolved_path: object
                        .get("resolvedPath")
                        .map(|value| value.as_str().map(String::from)),
                    external: object
                        .get("external")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .collect()
    }

    fn string_field(object: &Map<String, Value>, field: &str) -> String {
        object
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn evaluates_formula_dependencies_and_file_predicates() {
        let mut context = BasesEvaluationContext {
            note: Map::from_iter([
                ("price".to_string(), Value::from(12.5)),
                ("quantity".to_string(), Value::from(4)),
            ]),
            file: BasesFile {
                path: "TaskNotes/task.md".to_string(),
                folder: "TaskNotes".to_string(),
                tags: vec!["task".to_string(), "project/a".to_string()],
                ..file_defaults("TaskNotes/task.md")
            },
            ..Default::default()
        };
        Arc::make_mut(&mut context.formulas)
            .insert("total".to_string(), "price * quantity".to_string());
        assert_eq!(
            evaluate("formula.total", &context).unwrap(),
            Value::from(50)
        );
        assert!(matches(
            "file.hasTag(\"task\") && file.inFolder(\"TaskNotes\")",
            &context
        )
        .unwrap());
    }

    #[test]
    fn lowers_only_canonical_hierarchical_tag_candidates() {
        let candidate = lower_hosted_candidate("file.hasTag(\"#task\")");
        assert!(matches!(
            candidate,
            CandidatePredicate::Compare {
                comparison: CandidateComparison {
                    field: CandidateField::BodyTags,
                    operator: CandidateComparisonOperator::Contains,
                    value: Value::String(ref value),
                    pruning: CandidateComparisonPruning::NormalizedTagHierarchy,
                    value_kind: None,
                }
            } if value == "task"
        ));
        assert_eq!(
            lower_hosted_candidate("file.hasTag(42)"),
            CandidatePredicate::All
        );
    }

    #[test]
    fn evaluates_tasknotes_date_formulas() {
        let mut context = BasesEvaluationContext {
            now: Some("2026-07-22T10:00:00Z".to_string()),
            ..Default::default()
        };
        context
            .note
            .insert("due".to_string(), Value::String("2026-07-24".to_string()));
        assert_eq!(
            evaluate("number(date(due) - today()) / 86400000", &context).unwrap(),
            Value::from(2)
        );
        assert_eq!(
            evaluate("date(due).format(\"yyyy-MM-dd\")", &context).unwrap(),
            Value::String("2026-07-24".to_string())
        );
    }

    #[test]
    fn evaluates_fixed_and_dst_timezones_without_machine_local_fallbacks() {
        let mut context = BasesEvaluationContext {
            timezone: BasesTimezone::from_setting(Some("+10:00")).unwrap(),
            ..Default::default()
        };
        assert_eq!(
            evaluate("number(date(\"1970-01-02\"))", &context).unwrap(),
            Value::from(50_400_000)
        );

        context.timezone = BasesTimezone::from_setting(Some("-05:00")).unwrap();
        assert_eq!(
            evaluate("number(date(\"1970-01-02\"))", &context).unwrap(),
            Value::from(104_400_000)
        );

        context.timezone = BasesTimezone::from_setting(Some("Australia/Melbourne")).unwrap();
        assert_eq!(
            evaluate("number(date(\"2026-04-05\"))", &context).unwrap(),
            Value::from(1_775_307_600_000_i64)
        );
        assert_eq!(
            evaluate("number(date(\"2026-04-06\"))", &context).unwrap(),
            Value::from(1_775_397_600_000_i64)
        );
    }

    #[test]
    fn rejects_invalid_timezone_settings() {
        assert_eq!(
            BasesTimezone::from_setting(Some("+25:00")).unwrap_err(),
            "Invalid fixed timezone offset '+25:00'"
        );
        assert_eq!(
            BasesTimezone::from_setting(Some("Australia/Atlantis")).unwrap_err(),
            "Unknown IANA timezone 'Australia/Atlantis'"
        );
    }

    #[test]
    fn matches_the_shared_obsidian_oracle() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/obsidian-bases-oracle.json"
        ))
        .unwrap();
        let context = fixture_context(&fixture["context"]);
        let mut mismatches = Vec::new();
        for test_case in fixture["cases"].as_array().unwrap() {
            if test_case.get("knownDivergence").is_some() {
                continue;
            }
            let name = test_case["name"].as_str().unwrap();
            let expression = test_case["expression"].as_str().unwrap();
            let actual = if test_case.get("assertion").and_then(Value::as_str) == Some("range01") {
                let value = evaluate(expression, &context).unwrap_or(Value::Null);
                if value
                    .as_f64()
                    .is_some_and(|value| (0.0..1.0).contains(&value))
                {
                    test_case["expected"].clone()
                } else {
                    value
                }
            } else {
                evaluate(expression, &context).unwrap_or_else(|error| json!({"error": error}))
            };
            if actual != test_case["expected"] {
                mismatches.push(format!(
                    "{name}: {expression}\n  expected: {}\n  actual:   {}",
                    test_case["expected"], actual
                ));
            }
        }
        assert!(
            mismatches.is_empty(),
            "{} oracle mismatches:\n{}",
            mismatches.len(),
            mismatches.join("\n")
        );
    }
}
