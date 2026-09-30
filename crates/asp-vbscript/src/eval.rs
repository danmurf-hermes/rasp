//! Deterministic tree-walking evaluator for the M1 VBScript subset.
//!
//! Executes one page render inside an [`ExecEnv`]: a variable scope, a
//! session store, and a buffered `Response`. Loop iterations are capped
//! so a runaway `Do` loop surfaces as a runtime error instead of a hang.

use crate::parser::{Expr, Stmt, Variant, parse_number};
use asp_core::{AspError, AspResult, Diagnostic};
use std::collections::HashMap;

/// Hard cap on loop iterations per loop statement (VBScript default
/// script timeout serves the same purpose in IIS).
const MAX_LOOP_ITERATIONS: u64 = 100_000;

/// `Response` state for one render: a buffer plus a few properties.
#[derive(Debug, Default)]
pub struct ResponseBuffer {
    pub chunks: Vec<String>,
    pub status: Option<u16>,
    pub content_type: Option<String>,
    pub ended: bool,
}

impl ResponseBuffer {
    pub fn write(&mut self, text: &str) {
        self.chunks.push(text.to_string());
    }

    /// Full buffered body so far.
    pub fn body(&self) -> String {
        self.chunks.join("")
    }
}

/// Per-request execution state.
#[derive(Debug, Default)]
pub struct ExecEnv {
    vars: HashMap<String, Variant>,
    consts: HashMap<String, Variant>,
    pub session: HashMap<String, Variant>,
    pub response: ResponseBuffer,
    /// Values for `Request.Collection("key")`, keyed `collection\0key`.
    pub request_data: HashMap<String, String>,
}

impl ExecEnv {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-populate `Request` data (query-string/form/cookies).
    pub fn with_request_data(mut self, data: HashMap<String, String>) -> Self {
        self.request_data = data;
        self
    }
}

/// Execute a parsed statement list to completion.
pub fn exec_block(stmts: &[Stmt], env: &mut ExecEnv) -> AspResult<()> {
    for stmt in stmts {
        exec_stmt(stmt, env)?;
        if env.response.ended {
            return Ok(());
        }
    }
    Ok(())
}

/// A frame in the render-loop interpreter's control stack: one open
/// `For` (waiting for its `Next`) or one open `Do` (waiting for its
/// `Loop`). The stack machine walks a statement list once; because
/// `Next`/`Loop` always belong to the *innermost* opener, frames only
/// need the opener plus its position — bodies re-run in place.
#[derive(Debug)]
enum LoopFrame {
    For {
        var: String,
        start: Expr,
        end: Expr,
        step: Option<Expr>,
        body_start: usize,
    },
    Do {
        cond: Option<Expr>,
        pre: bool,
        while_form: bool,
        /// Where the body begins (the statement after the opener).
        body_start: usize,
    },
}

/// Execute statements, pairing deferred loop openers (`ForLoopOpen`,
/// `DoOpen`) with their closing delimiters (`Next`, `LoopClose`/
/// `DoClose`). This exists because Classic ASP splits scripts across
/// `<% %>` blocks — `For` may live in one block and `Next` in another.
pub fn exec_block_loops(stmts: &[Stmt], env: &mut ExecEnv) -> AspResult<()> {
    let mut stack: Vec<LoopFrame> = Vec::new();
    let mut i = 0usize;
    while i < stmts.len() {
        if env.response.ended {
            return Ok(());
        }
        match &stmts[i] {
            Stmt::ForLoopOpen {
                var,
                start,
                end,
                step,
            } => {
                stack.push(LoopFrame::For {
                    var: var.clone(),
                    start: start.clone(),
                    end: end.clone(),
                    step: step.clone(),
                    body_start: i + 1,
                });
                i += 1;
            }
            Stmt::DoOpen {
                cond,
                pre,
                while_form,
            } => {
                stack.push(LoopFrame::Do {
                    cond: cond.clone(),
                    pre: *pre,
                    while_form: *while_form,
                    body_start: i + 1,
                });
                i += 1;
            }
            Stmt::Next => {
                match stack.pop() {
                    Some(LoopFrame::For {
                        var,
                        start,
                        end,
                        step,
                        body_start,
                    }) => {
                        exec_for_from(
                            var,
                            &start,
                            &end,
                            step.as_ref(),
                            body_start - 1,
                            i,
                            stmts,
                            env,
                        )?;
                        // exec_for_from leaves the stack tidy: the
                        // frames it pushed were nested inside and have
                        // been popped by their own delimiters.
                        i += 1;
                    }
                    Some(LoopFrame::Do { .. }) => {
                        return Err(rt_error(1, "'Next' without a matching 'For'"));
                    }
                    None => return Err(rt_error(1, "'Next' without a matching 'For'")),
                }
            }
            Stmt::LoopClose | Stmt::DoClose { .. } => {
                let frame = stack.pop();
                match frame {
                    Some(LoopFrame::Do {
                        cond,
                        pre,
                        while_form,
                        body_start,
                    }) => {
                        let trailing = match &stmts[i] {
                            Stmt::DoClose { cond, .. } => cond.clone(),
                            _ => None,
                        };
                        if trailing.is_some() && cond.is_some() {
                            return Err(rt_error(
                                1,
                                "Do loop cannot have both a leading and a trailing condition",
                            ));
                        }
                        let pre_now = if trailing.is_some() { false } else { pre };
                        let while_now = match (&stmts[i], trailing.is_some()) {
                            (Stmt::DoClose { while_form, .. }, true) => *while_form,
                            _ => while_form,
                        };
                        exec_do_from(
                            cond.as_ref().or(trailing.as_ref()),
                            pre_now,
                            while_now,
                            body_start - 1,
                            i,
                            stmts,
                            env,
                        )?;
                        i += 1;
                    }
                    Some(LoopFrame::For { .. }) => {
                        return Err(rt_error(1, "'Loop' without a matching 'Do'"));
                    }
                    None => return Err(rt_error(1, "'Loop' without a matching 'Do'")),
                }
            }
            other => {
                exec_stmt(other, env)?;
                i += 1;
            }
        }
    }
    if !stack.is_empty() {
        return Err(rt_error(1, "unterminated loop: missing 'Next' or 'Loop'"));
    }
    Ok(())
}

/// Run the For loop body in `stmts[body_first..next_idx]` against the
/// evaluator, re-entering the stack machine for nested constructs.
#[allow(clippy::too_many_arguments)]
fn exec_for_from(
    var: String,
    start: &Expr,
    end: &Expr,
    step: Option<&Expr>,
    opener_idx: usize,
    next_idx: usize,
    stmts: &[Stmt],
    env: &mut ExecEnv,
) -> AspResult<()> {
    let start_v = eval_expr(start, env)?;
    let end_v = eval_expr(end, env)?;
    let step_v = match step {
        Some(e) => eval_expr(e, env)?,
        None => Variant::Int(1),
    };
    let mut current = start_v.as_number(1)? as i64;
    let target = end_v.as_number(1)? as i64;
    let step_n = if matches!(step_v, Variant::Empty) {
        1i64
    } else {
        step_v.as_number(1)? as i64
    };
    if step_n == 0 {
        return Err(rt_error(1, "For loop Step cannot be zero"));
    }
    let body = &stmts[body_range(opener_idx, next_idx)];
    let mut iterations = 0u64;
    while (step_n > 0 && current <= target) || (step_n < 0 && current >= target) {
        iterations += 1;
        if iterations > MAX_LOOP_ITERATIONS {
            return Err(rt_error(1, "For loop exceeded the iteration limit"));
        }
        env.vars.insert(key(&var), Variant::Int(current));
        exec_block_loops(body, env)?;
        if env.response.ended {
            return Ok(());
        }
        current += step_n;
    }
    Ok(())
}

/// Run the Do loop body in `stmts[body_first..loop_idx]`.
#[allow(clippy::too_many_arguments)]
fn exec_do_from(
    cond: Option<&Expr>,
    pre: bool,
    while_form: bool,
    opener_idx: usize,
    loop_idx: usize,
    stmts: &[Stmt],
    env: &mut ExecEnv,
) -> AspResult<()> {
    let body = &stmts[body_range(opener_idx, loop_idx)];
    fn cond_holds(cond: Option<&Expr>, while_form: bool, env: &mut ExecEnv) -> AspResult<bool> {
        match cond {
            None => Ok(true),
            Some(e) => {
                let v = eval_expr(e, env)?;
                let holds = truthy(v);
                Ok(if while_form { holds } else { !holds })
            }
        }
    }
    let mut iterations = 0u64;
    if pre {
        while cond_holds(cond, while_form, env)? {
            iterations += 1;
            if iterations > MAX_LOOP_ITERATIONS {
                return Err(rt_error(1, "Do loop exceeded the iteration limit"));
            }
            exec_block_loops(body, env)?;
            if env.response.ended {
                return Ok(());
            }
        }
    } else {
        loop {
            iterations += 1;
            if iterations > MAX_LOOP_ITERATIONS {
                return Err(rt_error(1, "Do loop exceeded the iteration limit"));
            }
            exec_block_loops(body, env)?;
            if env.response.ended {
                return Ok(());
            }
            if !cond_holds(cond, while_form, env)? {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Statement range between an opener (exclusive) and its delimiter.
fn body_range(opener_idx: usize, delimiter_idx: usize) -> std::ops::Range<usize> {
    opener_idx + 1..delimiter_idx
}

fn rt_error(line: usize, message: impl Into<String>) -> AspError {
    AspError::Runtime(Diagnostic::new(line, message))
}

fn unimplemented(line: usize, message: impl Into<String>) -> AspError {
    AspError::Runtime(Diagnostic::new(
        line,
        format!("unsupported VBScript feature: {}", message.into()),
    ))
}

/// Case-insensitive variable key.
fn key(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn exec_stmt(stmt: &Stmt, env: &mut ExecEnv) -> AspResult<()> {
    match stmt {
        Stmt::Dim { name } => {
            env.vars.entry(key(name)).or_insert(Variant::Empty);
            Ok(())
        }
        Stmt::Const { name, value } => {
            env.consts.insert(key(name), value.clone());
            Ok(())
        }
        Stmt::Assign { name, value } => {
            if env.consts.contains_key(&key(name)) {
                return Err(rt_error(1, format!("cannot assign to Const '{name}'")));
            }
            let v = eval_expr(value, env)?;
            env.vars.insert(key(name), v);
            Ok(())
        }
        Stmt::SessionAssign { name, value } => {
            let v = eval_expr(value, env)?;
            env.session.insert(key(name), v);
            Ok(())
        }
        Stmt::ResponseCall { verb, arg } => exec_response_call(verb, arg.as_ref(), env),
        Stmt::If {
            branches,
            else_body,
            ..
        } => {
            for (cond, body) in branches {
                if truthy(eval_expr(cond, env)?) {
                    return exec_block(body, env);
                }
            }
            if let Some(body) = else_body {
                return exec_block(body, env);
            }
            Ok(())
        }
        // Loop openers/delimiters: evaluation is deferred to the render
        // loop's stack machine (see `exec_block`), which pairs `Next`
        // with its `ForLoopOpen` and `Loop` with its `DoOpen`.
        Stmt::ForLoopOpen { .. }
        | Stmt::Next
        | Stmt::DoOpen { .. }
        | Stmt::LoopClose
        | Stmt::DoClose { .. } => Err(unimplemented(
            1,
            "deferred loop statement outside exec_block",
        )),
    }
}

fn exec_response_call(verb: &str, arg: Option<&Expr>, env: &mut ExecEnv) -> AspResult<()> {
    match verb {
        "write" => {
            let v = match arg {
                Some(e) => eval_expr(e, env)?,
                None => Variant::Empty,
            };
            env.response.write(&v.display());
            Ok(())
        }
        "buffer" => {
            // M1 always buffers; accept and ignore the property value.
            if let Some(e) = arg {
                let _ = eval_expr(e, env)?;
            }
            Ok(())
        }
        "flush" | "clear" => {
            // Real Clear drops buffered output; supported faithfully.
            if verb == "clear" {
                // Classic ASP Clear only works while buffering; M1 always buffers.
                // Drop everything written so far.
                env.response.chunks.clear();
            }
            Ok(())
        }
        "end" => {
            env.response.ended = true;
            Ok(())
        }
        "binarywrite" | "redirect" | "addheader" | "appendtolog" | "contenttype" | "status"
        | "charset" | "expires" | "cachecontrol" | "cookies" => {
            // Accepted grammar; effects land with the full Response model (M3+).
            if let Some(e) = arg {
                eval_expr(e, env)?;
            }
            Ok(())
        }
        other => Err(unimplemented(1, format!("Response.{other}"))),
    }
}

/// VBScript truthiness: False = 0 = "" = Empty; everything else true.
fn truthy(v: Variant) -> bool {
    match v {
        Variant::Empty | Variant::Null => false,
        Variant::Int(i) => i != 0,
        Variant::Float(f) => f != 0.0,
        Variant::Bool(b) => b,
        Variant::Str(s) => !s.is_empty(),
        Variant::ObjectRef(_) => true,
    }
}

fn eval_expr(expr: &Expr, env: &mut ExecEnv) -> AspResult<Variant> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Variable(name) => {
            let k = key(name);
            if let Some(c) = env.consts.get(&k) {
                return Ok(c.clone());
            }
            Ok(env.vars.get(&k).cloned().unwrap_or(Variant::Empty))
        }
        Expr::SessionRead(name) => Ok(env
            .session
            .get(&key(name))
            .cloned()
            .unwrap_or(Variant::Empty)),
        Expr::RequestRead {
            collection,
            key: name,
        } => {
            let lookup = format!(
                "{}\u{0}{}",
                collection.to_ascii_lowercase(),
                name.to_ascii_lowercase()
            );
            Ok(env
                .request_data
                .get(&lookup)
                .cloned()
                .map(Variant::Str)
                .unwrap_or(Variant::Empty))
        }
        Expr::Neg(inner) => {
            let v = eval_expr(inner, env)?;
            match v {
                Variant::Int(i) => Ok(Variant::Int(-i)),
                other => Ok(Variant::Float(-other.as_number(1)?)),
            }
        }
        Expr::Not(inner) => {
            let v = eval_expr(inner, env)?;
            Ok(Variant::Bool(!truthy(v)))
        }
        Expr::Binary(op, l, r) => {
            let lv = eval_expr(l, env)?;
            let rv = eval_expr(r, env)?;
            apply_binary(op, lv, rv)
        }
        Expr::Builtin(name, args, line) => eval_builtin(name, args, *line, env),
        Expr::ResponseValue(verb) => eval_response_value(verb, env),
    }
}

/// Binary operators with VBScript semantics: `+` is numeric addition
/// (string operands are coerced), `&` always concatenates, comparisons
/// compare strings lexically when both sides are strings, `^` etc use
/// float math but keep integers when exact.
fn apply_binary(op: &str, lv: Variant, rv: Variant) -> AspResult<Variant> {
    match op {
        "&" => Ok(Variant::Str(format!("{}{}", lv.display(), rv.display()))),
        "+" => {
            if matches!(lv, Variant::Str(_))
                && matches!(rv, Variant::Str(ref s) if s.parse::<f64>().is_err())
            {
                // `+` between non-numeric strings concatenates in VBScript.
                return Ok(Variant::Str(format!("{}{}", lv.display(), rv.display())));
            }
            let n = lv.as_number(1)? + rv.as_number(1)?;
            Ok(exact_number(n))
        }
        "-" => num_result(lv.as_number(1)? - rv.as_number(1)?),
        "*" => num_result(lv.as_number(1)? * rv.as_number(1)?),
        "/" => {
            let d = rv.as_number(1)?;
            if d == 0.0 {
                return Err(rt_error(1, "division by zero"));
            }
            Ok(Variant::Float(lv.as_number(1)? / d))
        }
        "\\" => {
            let d = rv.as_number(1)?;
            if d == 0.0 {
                return Err(rt_error(1, "division by zero"));
            }
            Ok(Variant::Int((lv.as_number(1)? / d) as i64))
        }
        "mod" => {
            let d = rv.as_number(1)?;
            if d == 0.0 {
                return Err(rt_error(1, "division by zero"));
            }
            Ok(Variant::Int((lv.as_number(1)? % d) as i64))
        }
        "^" => Ok(Variant::Float(lv.as_number(1)?.powf(rv.as_number(1)?))),
        "=" | "<>" | "<" | ">" | "<=" | ">=" => Ok(Variant::Bool(compare(op, &lv, &rv))),
        "and" | "or" | "xor" => {
            let a = lv.as_number(1)? as i64;
            let b = rv.as_number(1)? as i64;
            let n = match op {
                "and" => a & b,
                "or" => a | b,
                _ => a ^ b,
            };
            Ok(Variant::Int(n))
        }
        other => Err(unimplemented(1, format!("operator {other}"))),
    }
}

/// Keep integers exact when the float math produced an integral value.
fn exact_number(n: f64) -> Variant {
    if n.fract() == 0.0 && n.abs() < i64::MAX as f64 {
        Variant::Int(n as i64)
    } else {
        Variant::Float(n)
    }
}

fn num_result(n: f64) -> AspResult<Variant> {
    Ok(exact_number(n))
}

/// Comparison with VBScript semantics: two strings compare lexically,
/// otherwise numeric (string sides are coerced, failing loudly).
fn compare(op: &str, lv: &Variant, rv: &Variant) -> bool {
    let both_strings = matches!(lv, Variant::Str(_)) && matches!(rv, Variant::Str(_));
    let ord = if both_strings {
        lv.display().cmp(&rv.display())
    } else {
        let ln = lv.as_number(1).unwrap_or(f64::NEG_INFINITY);
        let rn = rv.as_number(1).unwrap_or(f64::NEG_INFINITY);
        ln.partial_cmp(&rn).unwrap_or(std::cmp::Ordering::Equal)
    };
    match op {
        "=" => ord == std::cmp::Ordering::Equal,
        "<>" => ord != std::cmp::Ordering::Equal,
        "<" => ord == std::cmp::Ordering::Less,
        ">" => ord == std::cmp::Ordering::Greater,
        "<=" => ord != std::cmp::Ordering::Greater,
        _ => ord != std::cmp::Ordering::Less,
    }
}

/// The M1 builtin function set. Each is small and total.
fn eval_builtin(name: &str, args: &[Expr], line: usize, env: &mut ExecEnv) -> AspResult<Variant> {
    let vals: Vec<Variant> = args
        .iter()
        .map(|a| eval_expr(a, env))
        .collect::<AspResult<_>>()?;
    match (name, vals.as_slice()) {
        ("len", [v]) => Ok(Variant::Int(v.display().chars().count() as i64)),
        ("ucase", [v]) => Ok(Variant::Str(v.display().to_uppercase())),
        ("lcase", [v]) => Ok(Variant::Str(v.display().to_lowercase())),
        ("trim", [v]) => Ok(Variant::Str(v.display().trim().to_string())),
        ("ltrim", [v]) => Ok(Variant::Str(v.display().trim_start().to_string())),
        ("rtrim", [v]) => Ok(Variant::Str(v.display().trim_end().to_string())),
        ("left", [v, n]) => {
            let n = n.as_number(line)? as usize;
            let s: String = v.display().chars().take(n).collect();
            Ok(Variant::Str(s))
        }
        ("right", [v, n]) => {
            let n = n.as_number(line)? as usize;
            let s = v.display();
            let start = s.chars().count().saturating_sub(n);
            Ok(Variant::Str(s.chars().skip(start).collect()))
        }
        ("mid", [v, start]) => {
            let start = (start.as_number(line)? as usize).max(1);
            let s = v.display();
            Ok(Variant::Str(s.chars().skip(start - 1).collect()))
        }
        ("mid", [v, start, len]) => {
            let start = (start.as_number(line)? as usize).max(1);
            let len = len.as_number(line)? as usize;
            let s = v.display();
            Ok(Variant::Str(s.chars().skip(start - 1).take(len).collect()))
        }
        ("instr", [hay, needle]) => {
            let hay = hay.display();
            let needle = needle.display();
            let pos = hay
                .find(&needle)
                .map(|byte_index| hay[..byte_index].chars().count() + 1)
                .unwrap_or(0);
            Ok(Variant::Int(pos as i64))
        }
        ("cstr", [v]) => Ok(Variant::Str(v.display())),
        ("cint", [v]) | ("clng", [v]) => {
            let n = v.as_number(line)?;
            Ok(Variant::Int(n as i64))
        }
        ("cdbl", [v]) => Ok(Variant::Float(v.as_number(line)?)),
        ("cbool", [v]) => Ok(Variant::Bool(truthy(v.clone()))),
        ("isnull", [v]) => Ok(Variant::Bool(matches!(v, Variant::Null))),
        ("isempty", [v]) => Ok(Variant::Bool(matches!(v, Variant::Empty))),
        ("isnumeric", [v]) => Ok(Variant::Bool(
            matches!(v, Variant::Int(_) | Variant::Float(_) | Variant::Bool(_))
                || parse_number(&v.display()).is_some(),
        )),
        ("strcomp", [a, b]) => {
            let equal = a.display() == b.display();
            Ok(Variant::Int(if equal { 0 } else { 1 }))
        }
        ("vartype", []) => Err(unimplemented(line, "VarType (no arguments)")),
        ("now", []) => Ok(Variant::Str(asp_runtime_clock_now())),
        (other, _) => Err(unimplemented(line, format!("function {other}"))),
    }
}

/// `Response.Write` used inside an expression returns Empty (the M1
/// subset only exposes Write in statement position anyway).
fn eval_response_value(verb: &str, env: &mut ExecEnv) -> AspResult<Variant> {
    match verb {
        "write" => Ok(Variant::Empty),
        _ => {
            exec_response_call(verb, None, env)?;
            Ok(Variant::Empty)
        }
    }
}

/// Wall-clock string for `Now`, rendered like VBScript's default locale.
fn asp_runtime_clock_now() -> String {
    // chrono is wired for the HTTP server anyway; a placeholder keeps M1
    // deterministic-free of locale surprises by using RFC3339.
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}
