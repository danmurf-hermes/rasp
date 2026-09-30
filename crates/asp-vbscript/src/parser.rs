//! Hand-written recursive-descent parser and AST for the classic ASP
//! subset of VBScript, and the deterministic evaluator for it.
//!
//! M1 covers: literals, variables, `Dim`, `Const`, assignment, the
//! arithmetic / concatenation / comparison / logical operators, `If`
//! (single-line and block), `For ... Next`, `Do While|Until ... Loop`,
//! `Response.Write`, `Response.Buffer`, and `Session("name") = value`.
//! Unsupported constructs (Sub/Function/objects/arrays) produce clear
//! `Unsupported` diagnostics instead of misbehaving.

use crate::lexer::{Tok, is_reserved, lex};
use asp_core::{AspError, AspResult};

/// One evaluated expression value. VBScript has only Variant; M1 keeps
/// the useful subset with real `Empty` semantics.
#[derive(Debug, Clone, PartialEq)]
pub enum Variant {
    /// Uninitialised; converts to "" when written, 0 in arithmetic.
    Empty,
    Null,
    Int(i64),
    Float(f64),
    /// Booleans are stored as integers internally (0 / ~0).
    Bool(bool),
    Str(String),
    /// A value assigned through an object reference (`Set`); opaque.
    ObjectRef(String),
}

impl Variant {
    /// Render for `Response.Write` / `<%= %>` with VBScript output rules:
    /// Empty -> "", Null -> "", True -> "True", False -> "False".
    pub fn display(&self) -> String {
        match self {
            Variant::Empty | Variant::Null => String::new(),
            Variant::Int(i) => i.to_string(),
            Variant::Float(f) => format_float(*f),
            Variant::Bool(true) => "True".to_string(),
            Variant::Bool(false) => "False".to_string(),
            Variant::Str(s) => s.clone(),
            Variant::ObjectRef(name) => name.clone(),
        }
    }

    /// Convert to a number for arithmetic, VBScript-style.
    pub fn as_number(&self, line: usize) -> AspResult<f64> {
        match self {
            Variant::Empty => Ok(0.0),
            Variant::Null => Err(AspError::Runtime(asp_core::Diagnostic::new(
                line,
                "invalid use of Null",
            ))),
            Variant::Int(i) => Ok(*i as f64),
            Variant::Float(f) => Ok(*f),
            Variant::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
            Variant::Str(s) => parse_number(s).ok_or_else(|| {
                AspError::Runtime(asp_core::Diagnostic::new(
                    line,
                    format!("type mismatch: cannot convert {s:?} to a number"),
                ))
            }),
            Variant::ObjectRef(_) => Err(AspError::Runtime(asp_core::Diagnostic::new(
                line,
                "type mismatch: object reference in arithmetic",
            ))),
        }
    }
}

/// Match VBScript's "up to 15 significant digits" float rendering.
fn format_float(f: f64) -> String {
    let shortest = format!("{f}");
    if shortest.len() <= 15 {
        return shortest;
    }
    for precision in (1..=15).rev() {
        let candidate = format!("{f:.precision$}");
        if candidate.parse::<f64>() == Ok(f) {
            return candidate;
        }
    }
    format!("{f:e}")
}

/// Parse a string as a VBScript number: optional sign, decimal integer,
/// optional fraction, optional exponent; also accepts `&h`/`&o` literals.
pub fn parse_number(s: &str) -> Option<f64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix("&h") {
        return i64::from_str_radix(hex, 16).ok().map(|v| v as f64);
    }
    if let Some(oct) = lower.strip_prefix("&o") {
        return i64::from_str_radix(oct, 8).ok().map(|v| v as f64);
    }
    if !trimmed
        .bytes()
        .all(|b| b.is_ascii_digit() || b == b'.' || b == b'+' || b == b'-' || b == b'e')
    {
        return None;
    }
    trimmed.parse::<f64>().ok()
}

/// Statements.
#[derive(Debug, Clone)]
pub enum Stmt {
    /// `Dim name`
    Dim { name: String },
    /// `Const name = expr`
    Const { name: String, value: Variant },
    /// `name = expr` (no `Set`); index targets arrive in M3.
    Assign { name: String, value: Expr },
    /// `Session("name") = expr`
    SessionAssign { name: String, value: Expr },
    /// Any `Response.verb [args]` call statement.
    ResponseCall { verb: String, arg: Option<Expr> },
    /// `If cond Then stmt(s)` — block form when `else/elseif` present.
    If {
        branches: Vec<(Expr, Vec<Stmt>)>,
        else_body: Option<Vec<Stmt>>,
        line: usize,
    },
    /// `For name = a To b [Step s]` — the body arrives as the following
    /// statements up to (and consumed by) the `Next` block delimiter.
    ForLoopOpen {
        var: String,
        start: Expr,
        end: Expr,
        step: Option<Expr>,
    },
    /// `Next` — closes the innermost open `For` (or `Do`-family) block.
    Next,
    /// `Do [While|Until cond]` — body arrives up to the closing `Loop`.
    DoOpen {
        cond: Option<Expr>,
        /// true = check before the body (While/Until at the top).
        pre: bool,
        /// While cond -> continue=true; Until cond -> continue=(!cond).
        while_form: bool,
    },
    /// `Loop` (no same-line condition: closes the innermost `Do`).
    LoopClose,
    /// `Do ... Loop While|Until cond` — trailing-condition form.
    DoClose {
        cond: Option<Expr>,
        while_form: bool,
    },
}

/// Expressions.
#[derive(Debug, Clone)]
pub enum Expr {
    Literal(Variant),
    Variable(String),
    /// `Session("name")` read.
    SessionRead(String),
    /// `Request.String("key")` read — wired to per-request data.
    RequestRead {
        collection: String,
        key: String,
    },
    /// Unary minus applied by the parser.
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Binary(String, Box<Expr>, Box<Expr>),
    /// Any builtin name call that is not an ASP intrinsic object.
    Builtin(String, Vec<Expr>, usize),
    /// A method call on the Response object for value-returning use.
    ResponseValue(String),
}

/// Parse one `<%` block body (source text) into a statement list.
pub fn parse_block(body: &str, base_line: usize) -> AspResult<Vec<Stmt>> {
    let tokens = lex(body, base_line)?;
    let mut p = P { tokens, pos: 0 };
    let mut stmts = Vec::new();
    while !p.at_end() {
        let before = p.pos;
        if matches!(p.peek(), Tok::LineEnd) {
            p.advance();
            continue;
        }
        let stmt = p.parse_statement()?;
        stmts.push(stmt);
        if p.pos == before {
            return Err(AspError::Syntax(asp_core::Diagnostic::new(
                base_line,
                "could not make progress while parsing a statement",
            )));
        }
    }
    Ok(stmts)
}

struct P {
    tokens: Vec<Tok>,
    pos: usize,
}

impl P {
    fn peek(&self) -> &Tok {
        self.tokens.get(self.pos).unwrap_or(&Tok::LineEnd)
    }

    fn advance(&mut self) {
        self.pos += 1;
    }

    fn at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    /// Consume an expected symbol, with a precise error if absent.
    fn expect_sym(&mut self, what: &str) -> AspResult<()> {
        if matches!(self.peek(), Tok::Sym(s) if s == what) {
            self.advance();
            Ok(())
        } else {
            Err(self.error(format!("expected '{what}'")))
        }
    }

    /// Consume `Tok::LineEnd`, allowing several in a row.
    fn expect_line_end(&mut self) -> AspResult<()> {
        if matches!(self.peek(), Tok::LineEnd) {
            self.advance();
            Ok(())
        } else {
            Err(self.error("expected end of statement"))
        }
    }

    fn error(&self, message: impl Into<String>) -> AspError {
        AspError::Syntax(asp_core::Diagnostic::new(1, message))
    }

    fn parse_statement(&mut self) -> AspResult<Stmt> {
        match self.peek().clone() {
            Tok::Name(name) if name == "dim" => self.parse_dim(),
            Tok::Name(name) if name == "const" => self.parse_const(),
            Tok::Name(name) if name == "if" => self.parse_if(),
            Tok::Name(name) if name == "for" => self.parse_for(),
            Tok::Name(name) if name == "do" => self.parse_do(),
            Tok::Name(name) if name == "response" => self.parse_response_stmt(),
            Tok::Name(name) if name == "next" => {
                self.advance();
                self.consume_loop_tail()?;
                Ok(Stmt::Next)
            }
            Tok::Name(name) if name == "loop" => {
                self.advance();
                // Bare `Loop [While|Until cond]` statement: the trailing
                // condition arrives right here (cross-block `Loop While`).
                if let Tok::Name(n) = self.peek().clone()
                    && (n == "while" || n == "until")
                {
                    let while_form = n == "while";
                    self.advance();
                    let cond = self.parse_expr()?;
                    self.consume_loop_tail()?;
                    return Ok(Stmt::DoClose {
                        cond: Some(cond),
                        while_form,
                    });
                }
                self.consume_loop_tail()?;
                Ok(Stmt::LoopClose)
            }
            Tok::Name(name) if name == "session" => self.parse_session_target(),
            Tok::Name(name) if name == "call" => {
                self.advance();
                self.parse_statement()
            }
            Tok::Name(name) if is_reserved(&name) => Err(self.error(format!(
                "VBScript feature '{name}' is not supported in Milestone 1"
            ))),
            Tok::Name(name) => {
                self.advance();
                match self.peek() {
                    Tok::Sym(op) if op == "=" => {
                        self.advance();
                        let value = self.parse_expr()?;
                        self.expect_line_end()?;
                        Ok(Stmt::Assign { name, value })
                    }
                    Tok::Sym(op) if op == "." => {
                        // SomeObject.member — unsupported outside Response/Request.
                        let member = self.peek_member_name()?;
                        Err(self.error(format!(
                            "object '{name}.{member}' is not supported in Milestone 1"
                        )))
                    }
                    _ => Err(self.error(format!("expected '=' after variable '{name}'"))),
                }
            }
            other => Err(self.error(format!("expected a statement, found {}", other.describe()))),
        }
    }

    /// Consume `.name` after an object token and return the member name.
    fn peek_member_name(&mut self) -> AspResult<String> {
        self.advance(); // the '.'
        match self.peek().clone() {
            Tok::Name(n) => {
                self.advance();
                Ok(n)
            }
            other => Err(self.error(format!(
                "expected a member name after '.', found {}",
                other.describe()
            ))),
        }
    }

    fn parse_dim(&mut self) -> AspResult<Stmt> {
        self.advance();
        let mut names = Vec::new();
        loop {
            match self.peek().clone() {
                Tok::Name(n) => {
                    self.advance();
                    names.push(n);
                }
                other => {
                    return Err(self.error(format!(
                        "expected a name after Dim, found {}",
                        other.describe()
                    )));
                }
            }
            if matches!(self.peek(), Tok::Sym(s) if s == ",") {
                self.advance();
            } else {
                break;
            }
        }
        self.expect_line_end()?;
        // M1 lowers `Dim a, b` to a Dim of the first name; further names
        // are declared implicitly on first assignment.
        let name = names.remove(0);
        Ok(Stmt::Dim { name })
    }

    fn parse_const(&mut self) -> AspResult<Stmt> {
        self.advance();
        let name = match self.peek().clone() {
            Tok::Name(n) => {
                self.advance();
                n
            }
            other => {
                return Err(self.error(format!(
                    "expected a name after Const, found {}",
                    other.describe()
                )));
            }
        };
        self.expect_sym("=")?;
        // Constants must be literals in real VBScript; accept literal-only
        // expressions to keep the grammar small.
        let value = self.parse_literal_only()?;
        self.expect_line_end()?;
        Ok(Stmt::Const { name, value })
    }

    fn parse_literal_only(&mut self) -> AspResult<Variant> {
        match self.peek().clone() {
            Tok::Int(i) => {
                self.advance();
                Ok(Variant::Int(i))
            }
            Tok::Float(f) => {
                self.advance();
                Ok(Variant::Float(f))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Variant::Str(s))
            }
            Tok::Name(n) if n == "true" => {
                self.advance();
                Ok(Variant::Bool(true))
            }
            Tok::Name(n) if n == "false" => {
                self.advance();
                Ok(Variant::Bool(false))
            }
            Tok::Sym(s) if s == "-" => {
                self.advance();
                match self.parse_literal_only()? {
                    Variant::Int(i) => Ok(Variant::Int(-i)),
                    Variant::Float(f) => Ok(Variant::Float(-f)),
                    _ => Err(self.error("expected a number after '-'")),
                }
            }
            other => Err(self.error(format!(
                "Const requires a literal value, found {}",
                other.describe()
            ))),
        }
    }

    /// `If cond Then stmt` (single-line) or the block form.
    fn parse_if(&mut self) -> AspResult<Stmt> {
        let line = self.current_line();
        self.advance(); // if
        let cond = self.parse_expr()?;
        if !matches!(self.peek(), Tok::Name(n) if n == "then") {
            return Err(self.error("expected 'Then' after If condition"));
        }
        self.advance();

        if matches!(self.peek(), Tok::LineEnd) {
            self.advance();
            let (branches, else_body) = self.parse_if_block(cond, line)?;
            return Ok(Stmt::If {
                branches,
                else_body,
                line,
            });
        }
        // Single-line form: one or two statements follow.
        // `If cond Then stmt [Else stmt]` on one line.
        let then_stmt = self.parse_inline_statement()?;
        if matches!(self.peek(), Tok::Name(n) if n == "else") {
            self.advance();
            let else_stmt = self.parse_inline_statement()?;
            self.expect_line_end()?;
            return Ok(Stmt::If {
                branches: vec![(cond, vec![then_stmt])],
                else_body: Some(vec![else_stmt]),
                line,
            });
        }
        self.expect_line_end()?;
        Ok(Stmt::If {
            branches: vec![(cond, vec![then_stmt])],
            else_body: None,
            line,
        })
    }

    /// One statement that occupies part of a line (single-line If arm).
    /// Unlike `parse_statement`, this does not insist on the line end:
    /// the caller handles the trailing `Else` or newline.
    fn parse_inline_statement(&mut self) -> AspResult<Stmt> {
        if matches!(self.peek(), Tok::LineEnd) {
            return Err(self.error("expected a statement after 'Then'"));
        }
        let stmt = self.parse_statement_content()?;
        // Tolerate the LineEnd that ends this same-line statement.
        if matches!(self.peek(), Tok::LineEnd) {
            self.advance();
        }
        Ok(stmt)
    }

    /// A statement without consuming a trailing LineEnd.
    fn parse_statement_content(&mut self) -> AspResult<Stmt> {
        match self.peek().clone() {
            Tok::Name(name) if name == "if" => self.parse_if(),
            Tok::Name(name) if name == "for" => self.parse_for(),
            Tok::Name(name) if name == "do" => self.parse_do(),
            Tok::Name(name) if name == "response" => self.parse_response_stmt(),
            Tok::Name(name) if name == "session" => self.parse_session_target(),
            Tok::Name(name) if name == "call" => {
                self.advance();
                self.parse_statement_content()
            }
            // Assignments and Dim/Const forms end with their own LineEnd;
            // for single-line use these are unusual, so route everything
            // through the normal parser and tolerate its LineEnd handling
            // where it has already consumed one by returning the stmt.
            Tok::Name(name) if name == "dim" || name == "const" => {
                self.parse_dim_or_const_inline(&name)
            }
            Tok::Name(name) => {
                self.advance();
                if matches!(self.peek(), Tok::Sym(s) if s == "=") {
                    self.advance();
                    let value = self.parse_expr()?;
                    // Consume nothing further: caller owns the boundary.
                    return Ok(Stmt::Assign { name, value });
                }
                Err(self.error(format!("expected '=' after variable '{name}'")))
            }
            other => Err(self.error(format!("expected a statement, found {}", other.describe()))),
        }
    }

    /// Dim/Const inline for single-line If arms (accept and consume
    /// through the line end; rare but legal).
    fn parse_dim_or_const_inline(&mut self, kind: &str) -> AspResult<Stmt> {
        let stmt = if kind == "dim" {
            self.parse_dim()?
        } else {
            self.parse_const()?
        };
        Ok(stmt)
    }

    /// Block `If`: branch bodies until `Else`/`ElseIf`/`End If`.
    #[allow(clippy::type_complexity)]
    fn parse_if_block(
        &mut self,
        cond: Expr,
        _line: usize,
    ) -> AspResult<(Vec<(Expr, Vec<Stmt>)>, Option<Vec<Stmt>>)> {
        let mut branches = vec![(cond, Vec::new())];
        let mut else_body = None;
        loop {
            if self.at_end() {
                return Err(self.error("missing 'End If'"));
            }
            match self.peek().clone() {
                Tok::Name(n) if n == "elseif" => {
                    self.advance();
                    let cond = self.parse_expr()?;
                    if !matches!(self.peek(), Tok::Name(x) if x == "then") {
                        return Err(self.error("expected 'Then' after ElseIf condition"));
                    }
                    self.advance();
                    self.expect_line_end()?;
                    branches.push((cond, Vec::new()));
                }
                Tok::Name(n) if n == "else" => {
                    self.advance();
                    self.expect_line_end()?;
                    else_body = Some(Vec::new());
                }
                Tok::Name(n) if n == "end" => {
                    self.advance();
                    if !matches!(self.peek(), Tok::Name(x) if x == "if") {
                        return Err(self.error("expected 'If' after 'End'"));
                    }
                    self.advance();
                    self.expect_line_end()?;
                    return Ok((branches, else_body));
                }
                Tok::LineEnd => {
                    self.advance();
                }
                _ => {
                    let stmt = self.parse_statement()?;
                    match branches.last_mut() {
                        Some((_, body)) if else_body.is_none() => body.push(stmt),
                        _ => {
                            else_body.as_mut().expect("else body exists").push(stmt);
                        }
                    }
                }
            }
        }
    }

    fn parse_for(&mut self) -> AspResult<Stmt> {
        self.advance(); // for
        let var = match self.peek().clone() {
            Tok::Name(n) => {
                self.advance();
                n
            }
            other => {
                return Err(self.error(format!(
                    "expected loop variable after For, found {}",
                    other.describe()
                )));
            }
        };
        self.expect_sym("=")?;
        let start = self.parse_expr()?;
        if !matches!(self.peek(), Tok::Name(n) if n == "to") {
            return Err(self.error("expected 'To' in For statement"));
        }
        self.advance();
        let end = self.parse_expr()?;
        let step = if matches!(self.peek(), Tok::Name(n) if n == "step") {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.consume_loop_tail()?;
        Ok(Stmt::ForLoopOpen {
            var,
            start,
            end,
            step,
        })
    }

    fn parse_do(&mut self) -> AspResult<Stmt> {
        self.advance(); // do
        let mut cond = None;
        // Leading conditions run pre-checked; a trailing `Loop While`
        // forces post-check (handled by `DoClose`), so `pre` is only
        // ever true here.
        let pre = true;
        let mut while_form = true;
        if let Tok::Name(n) = self.peek().clone()
            && (n == "while" || n == "until")
        {
            while_form = n == "while";
            self.advance();
            cond = Some(self.parse_expr()?);
        }
        // `Do ... Loop While cond` on one line: the Loop arrives now.
        if matches!(self.peek(), Tok::Name(n) if n == "loop") {
            self.advance();
            if let Tok::Name(n) = self.peek().clone()
                && (n == "while" || n == "until")
            {
                if cond.is_some() {
                    return Err(
                        self.error("Do loop cannot have both a leading and a trailing condition")
                    );
                }
                while_form = n == "while";
                self.advance();
                let trailing = self.parse_expr()?;
                self.consume_loop_tail()?;
                return Ok(Stmt::DoClose {
                    cond: Some(trailing),
                    while_form,
                });
            }
            self.consume_loop_tail()?;
            return Ok(Stmt::LoopClose);
        }
        self.consume_loop_tail()?;
        Ok(Stmt::DoOpen {
            cond,
            pre,
            while_form,
        })
    }

    /// After a loop-closing keyword, consume an optional same-line
    /// variable name (e.g. `Next i`) — only when no condition word
    /// follows — then the newline. `Loop Until cond` must keep its
    /// `Until`, and `Next` never takes one, so reserved words are left
    /// for the next statement.
    fn consume_loop_tail(&mut self) -> AspResult<()> {
        if let Tok::Name(n) = self.peek().clone()
            && !matches!(n.as_str(), "while" | "until" | "loop" | "for")
        {
            self.advance();
        }
        self.expect_line_end()
    }

    /// `Response.<verb>` as a statement; verbs checked against a whitelist.
    fn parse_response_stmt(&mut self) -> AspResult<Stmt> {
        self.advance(); // response
        self.expect_sym(".")?;
        let verb = match self.peek().clone() {
            Tok::Name(n) => {
                self.advance();
                n
            }
            other => {
                return Err(self.error(format!(
                    "expected a Response method, found {}",
                    other.describe()
                )));
            }
        };
        // `Response.Write <expr>`: the argument is always a full
        // expression. A leading `(` is just a parenthesised operand
        // (`Write (2+3) * 4` is valid), not a call-paren.
        let arg = if matches!(self.peek(), Tok::LineEnd)
            || matches!(self.peek(), Tok::Name(n) if n == "else")
        {
            None
        } else {
            Some(self.parse_expr()?)
        };
        // `Else` on the same line belongs to a single-line If arm; do not
        // treat it as this call's argument or the end-of-statement marker.
        if matches!(self.peek(), Tok::LineEnd) {
            self.advance();
        }
        Ok(Stmt::ResponseCall { verb, arg })
    }

    /// `Session("name") = expr` as a statement.
    fn parse_session_target(&mut self) -> AspResult<Stmt> {
        self.advance();
        self.expect_sym("(")?;
        let key = match self.parse_expr()? {
            Expr::Literal(Variant::Str(s)) => s,
            _ => {
                return Err(self.error("Session requires a string key"));
            }
        };
        self.expect_sym(")")?;
        self.expect_sym("=")?;
        let value = self.parse_expr()?;
        self.expect_line_end()?;
        Ok(Stmt::SessionAssign { name: key, value })
    }

    fn current_line(&self) -> usize {
        1
    }

    fn parse_expr(&mut self) -> AspResult<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> AspResult<Expr> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Tok::Name(n) if n == "or") {
            self.advance();
            let right = self.parse_and()?;
            left = Expr::Binary("or".into(), Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> AspResult<Expr> {
        let mut left = self.parse_not()?;
        while matches!(self.peek(), Tok::Name(n) if n == "and") {
            self.advance();
            let right = self.parse_not()?;
            left = Expr::Binary("and".into(), Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> AspResult<Expr> {
        if matches!(self.peek(), Tok::Name(n) if n == "not") {
            self.advance();
            let inner = self.parse_not()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> AspResult<Expr> {
        let mut left = self.parse_concat()?;
        loop {
            let op = match self.peek() {
                Tok::Sym(s) if matches!(s.as_str(), "=" | "<>" | "<" | ">" | "<=" | ">=") => {
                    s.clone()
                }
                _ => break,
            };
            self.advance();
            let right = self.parse_concat()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_concat(&mut self) -> AspResult<Expr> {
        let mut left = self.parse_additive()?;
        while matches!(self.peek(), Tok::Sym(s) if s == "&") {
            self.advance();
            let right = self.parse_additive()?;
            left = Expr::Binary("&".into(), Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> AspResult<Expr> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Sym(s) if s == "+" || s == "-" => s.clone(),
                _ => break,
            };
            self.advance();
            let right = self.parse_multiplicative()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> AspResult<Expr> {
        let mut left = self.parse_power()?;
        loop {
            let op = match self.peek() {
                Tok::Sym(s) if matches!(s.as_str(), "*" | "/" | "\\" | "mod") => s.clone(),
                Tok::Name(n) if n == "mod" => "mod".to_string(),
                _ => break,
            };
            self.advance();
            let right = self.parse_power()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_power(&mut self) -> AspResult<Expr> {
        let mut left = self.parse_unary()?;
        while matches!(self.peek(), Tok::Sym(s) if s == "^") {
            self.advance();
            let right = self.parse_unary()?;
            left = Expr::Binary("^".into(), Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> AspResult<Expr> {
        match self.peek().clone() {
            Tok::Sym(s) if s == "-" => {
                self.advance();
                let inner = self.parse_unary()?;
                Ok(Expr::Neg(Box::new(inner)))
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> AspResult<Expr> {
        match self.peek().clone() {
            Tok::Int(i) => {
                self.advance();
                Ok(Expr::Literal(Variant::Int(i)))
            }
            Tok::Float(f) => {
                self.advance();
                Ok(Expr::Literal(Variant::Float(f)))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Literal(Variant::Str(s)))
            }
            Tok::Name(n) if n == "true" => {
                self.advance();
                Ok(Expr::Literal(Variant::Bool(true)))
            }
            Tok::Name(n) if n == "response" => {
                // Expression-position Response calls are beyond M1's
                // grammar; statement-position `Response.<verb>` is parsed
                // by `parse_response_stmt`.
                Err(self.error("Response in expression position is not supported in Milestone 1"))
            }
            Tok::Name(n) if n == "false" => {
                self.advance();
                Ok(Expr::Literal(Variant::Bool(false)))
            }
            Tok::Name(n) if n == "empty" => {
                self.advance();
                Ok(Expr::Literal(Variant::Empty))
            }
            Tok::Name(n) if n == "session" => {
                self.advance();
                self.expect_sym("(")?;
                let key = match self.parse_expr()? {
                    Expr::Literal(Variant::Str(s)) => s,
                    _ => return Err(self.error("Session requires a string key")),
                };
                self.expect_sym(")")?;
                Ok(Expr::SessionRead(key))
            }
            Tok::Name(n) if n == "request" => {
                self.advance();
                self.expect_sym(".")?;
                let collection = match self.peek().clone() {
                    Tok::Name(c) => {
                        self.advance();
                        c
                    }
                    other => {
                        return Err(self.error(format!(
                            "expected a Request collection, found {}",
                            other.describe()
                        )));
                    }
                };
                self.expect_sym("(")?;
                let key = match self.parse_expr()? {
                    Expr::Literal(Variant::Str(s)) => s,
                    _ => return Err(self.error("Request collections require a string key")),
                };
                self.expect_sym(")")?;
                Ok(Expr::RequestRead { collection, key })
            }
            Tok::Name(n) if !is_reserved(&n) => {
                self.advance();
                if matches!(self.peek(), Tok::Sym(s) if s == "(") {
                    self.advance();
                    let mut args = Vec::new();
                    if !matches!(self.peek(), Tok::Sym(s) if s == ")") {
                        args.push(self.parse_expr()?);
                        while matches!(self.peek(), Tok::Sym(s) if s == ",") {
                            self.advance();
                            args.push(self.parse_expr()?);
                        }
                    }
                    self.expect_sym(")")?;
                    let line = self.current_line();
                    return Ok(Expr::Builtin(n, args, line));
                }
                Ok(Expr::Variable(n))
            }
            Tok::Sym(s) if s == "(" => {
                self.advance();
                let inner = self.parse_expr()?;
                self.expect_sym(")")?;
                Ok(inner)
            }
            other => Err(self.error(format!(
                "expected an expression, found {}",
                other.describe()
            ))),
        }
    }
}
