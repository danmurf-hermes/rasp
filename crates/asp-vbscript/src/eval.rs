//! Deterministic tree-walking evaluator for the M1/M2 VBScript subset.
//!
//! Executes one page render inside an [`ExecEnv`]: a variable scope, a
//! session store, a procedure table, and a buffered `Response`.
//!
//! ## Two phase execution
//!
//! The parser emits **deferred delimiters** (`ForLoopOpen`/`Next`,
//! `DoOpen`/`LoopClose`/`DoClose`, `ProcOpen`/`ProcClose`) because
//! Classic ASP splits scripts across `<% %>` blocks: a `For` opened in
//! one block must pair with a `Next` in a later one, with the page's
//! literal HTML re-emitted on every iteration. Before execution [`exec_block_loops`]
//! runs a normalization pass that pairs delimiters (checking kinds), hoists
//! every `Sub`/`Function` into the procedure table (callable before their
//! textual definition), and rejects unbalanced delimiters with the M1
//! diagnostics. The resulting nested statement tree is then executed
//! directly, which is what keeps **nested** loops correct.
//!
//! ## Scope
//!
//! Script variables are global within a render; procedures shadow them
//! with parameter locals (`Frame`). ByRef parameters write back to the
//! caller's variable after the call; paren-wrapped single-argument
//! calls are ByVal, matching VBScript.
//!
//! Runaway loops and runaway recursion are capped so a bad page
//! surfaces as a runtime error instead of a hang.

use crate::parser::{Expr, Param, Stmt, Variant, parse_number};
use crate::vb_datetime;
use asp_core::{AspError, AspResult, Diagnostic};
use chrono::{Datelike, Timelike};
use std::collections::HashMap;

/// Hard cap on loop iterations per loop statement (VBScript default
/// script timeout serves the same purpose in IIS).
const MAX_LOOP_ITERATIONS: u64 = 100_000;

/// Hard cap on procedure call depth (recursion guard). Kept low
/// because each VBScript call level spans many native frames in debug
/// builds; real pages never approach this.
const MAX_CALL_DEPTH: usize = 32;

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

/// A procedure call frame: parameter/`Dim` locals shadowing globals,
/// the name of the enclosing `Function` (so `name = value` sets the
/// return value), and ByRef bindings for post-call write-back.
#[derive(Debug)]
struct Frame {
    locals: HashMap<String, Variant>,
    func: Option<String>,
    /// (param key, caller var key, caller var lives in a frame) —
    /// written back when the call returns.
    byref: Vec<(String, String, bool)>,
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
    /// Hoisted procedures keyed lower-case name.
    procs: HashMap<String, Proc>,
    /// Active procedure frame (locals shadowing), if any.
    frame: Option<Frame>,
    /// Function return value for the active call.
    ret: Option<Variant>,
    /// Current recursion depth.
    call_depth: usize,
}

/// A hoisted procedure: parameters plus a normalized body. Declaration
/// kind matters only for the `name = value` return convention.
#[derive(Debug, Clone)]
struct Proc {
    name: String,
    params: Vec<Param>,
    body: Vec<StmtN>,
    is_function: bool,
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

/// Control-flow outcome that escapes a statement.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Flow {
    /// Keep going.
    Normal,
    /// `Exit For` (true) / `Exit Do` (false).
    ExitLoop(bool),
    /// `Exit Sub` / `Exit Function`.
    ExitProc,
}

/// Normalized statement: what the executor runs. Deferred delimiters
/// are consumed by the normalization pass; non-structural statements
/// pass through in [`StmtN::Plain`].
#[derive(Debug, Clone)]
enum StmtN {
    Plain(Stmt),
    If {
        branches: Vec<(Expr, Vec<StmtN>)>,
        else_body: Option<Vec<StmtN>>,
    },
    For {
        var: String,
        start: Expr,
        end: Expr,
        step: Option<Expr>,
        body: Vec<StmtN>,
    },
    Do {
        cond: Option<Expr>,
        pre: bool,
        while_form: bool,
        body: Vec<StmtN>,
    },
}

/// Execute a parsed statement list to completion, hoisting procedures
/// and pairing deferred loop delimiters.
pub fn exec_block(stmts: &[Stmt], env: &mut ExecEnv) -> AspResult<()> {
    exec_block_loops(stmts, env)
}

/// Same as [`exec_block`]: the name survives from M1 where it ran the
/// deferred-delimiter stack machine; since M2 it is the normalizing
/// entry point that handles every construct.
pub fn exec_block_loops(stmts: &[Stmt], env: &mut ExecEnv) -> AspResult<()> {
    let (nested, procs) = normalize_block(stmts, true)?;
    for proc in procs {
        env.procs.insert(key(&proc.name), proc);
    }
    match exec_stream(&nested, env)? {
        Flow::Normal => Ok(()),
        Flow::ExitLoop(true) => Err(rt_error(1, "'Exit For' outside of a loop")),
        Flow::ExitLoop(false) => Err(rt_error(1, "'Exit Do' outside of a loop")),
        Flow::ExitProc => Err(rt_error(
            1,
            "'Exit Sub'/'Exit Function' outside of a procedure",
        )),
    }
}

/// Execute normalized statements in order, stopping at the first
/// control-flow escape.
fn exec_stream(stmts: &[StmtN], env: &mut ExecEnv) -> AspResult<Flow> {
    for stmt in stmts {
        if env.response.ended {
            return Ok(Flow::Normal);
        }
        let flow = exec_stmt_n(stmt, env)?;
        if !matches!(flow, Flow::Normal) {
            return Ok(flow);
        }
    }
    Ok(Flow::Normal)
}

fn exec_stmt_n(stmt: &StmtN, env: &mut ExecEnv) -> AspResult<Flow> {
    match stmt {
        StmtN::Plain(s) => exec_plain(s, env),
        StmtN::If {
            branches,
            else_body,
            ..
        } => {
            for (cond, body) in branches {
                if truthy(eval_expr(cond, env)?) {
                    return exec_stream(body, env);
                }
            }
            if let Some(body) = else_body {
                return exec_stream(body, env);
            }
            Ok(Flow::Normal)
        }
        StmtN::For {
            var,
            start,
            end,
            step,
            body,
        } => {
            let start_v = eval_expr(start, env)?;
            let end_v = eval_expr(end, env)?;
            let step_v = match step {
                Some(e) => eval_expr(e, env)?,
                None => Variant::Int(1),
            };
            let mut current = start_v.as_number(1)? as i64;
            let target = end_v.as_number(1)? as i64;
            let step_n = match &step_v {
                Variant::Empty => 1i64,
                v => v.as_number(1)? as i64,
            };
            if step_n == 0 {
                return Err(rt_error(1, "For loop Step cannot be zero"));
            }
            let mut iterations = 0u64;
            let mut flow = Flow::Normal;
            while (step_n > 0 && current <= target) || (step_n < 0 && current >= target) {
                iterations += 1;
                if iterations > MAX_LOOP_ITERATIONS {
                    return Err(rt_error(1, "For loop exceeded the iteration limit"));
                }
                assign_var_unchecked(env, var, Variant::Int(current));
                flow = exec_stream(body, env)?;
                if !matches!(flow, Flow::Normal) || env.response.ended {
                    break;
                }
                current += step_n;
            }
            match flow {
                Flow::ExitLoop(true) | Flow::Normal => Ok(Flow::Normal),
                other => Ok(other),
            }
        }
        StmtN::Do {
            cond,
            pre,
            while_form,
            body,
        } => {
            let mut iterations = 0u64;
            let mut flow = Flow::Normal;
            if *pre {
                while cond_holds(cond.as_ref(), *while_form, env)? {
                    iterations += 1;
                    if iterations > MAX_LOOP_ITERATIONS {
                        return Err(rt_error(1, "Do loop exceeded the iteration limit"));
                    }
                    flow = exec_stream(body, env)?;
                    if !matches!(flow, Flow::Normal) || env.response.ended {
                        break;
                    }
                }
            } else {
                loop {
                    iterations += 1;
                    if iterations > MAX_LOOP_ITERATIONS {
                        return Err(rt_error(1, "Do loop exceeded the iteration limit"));
                    }
                    flow = exec_stream(body, env)?;
                    if !matches!(flow, Flow::Normal) || env.response.ended {
                        break;
                    }
                    if !cond_holds(cond.as_ref(), *while_form, env)? {
                        break;
                    }
                }
            }
            match flow {
                Flow::ExitLoop(false) | Flow::Normal => Ok(Flow::Normal),
                other => Ok(other),
            }
        }
    }
}

/// Execute a non-structural statement.
fn exec_plain(stmt: &Stmt, env: &mut ExecEnv) -> AspResult<Flow> {
    match stmt {
        Stmt::Dim {
            name,
            size,
            extra_names,
        } => {
            declare_var(env, name, size.as_ref())?;
            for (n, s) in extra_names {
                declare_var(env, n, s.as_ref())?;
            }
            Ok(Flow::Normal)
        }
        Stmt::Const { name, value } => {
            env.consts.insert(key(name), value.clone());
            Ok(Flow::Normal)
        }
        Stmt::Assign { name, index, value } => {
            let k = key(name);
            if env.consts.contains_key(&k) {
                return Err(rt_error(1, format!("cannot assign to Const '{name}'")));
            }
            // Inside a Function, assigning to the function's own name
            // sets the return value (VBScript convention).
            if let Some(f) = env.frame.as_ref()
                && f.func.as_deref() == Some(k.as_str())
            {
                let v = eval_expr(value, env)?;
                env.ret = Some(v);
                return Ok(Flow::Normal);
            }
            let v = eval_expr(value, env)?;
            match index {
                Some(ix) => {
                    array_store(env, name, ix, v)?;
                    Ok(Flow::Normal)
                }
                None => {
                    assign_var(env, name, v);
                    Ok(Flow::Normal)
                }
            }
        }
        Stmt::SessionAssign { name, value } => {
            let v = eval_expr(value, env)?;
            env.session.insert(key(name), v);
            Ok(Flow::Normal)
        }
        Stmt::ResponseCall { verb, arg } => {
            exec_response_call(verb, arg.as_ref(), env)?;
            Ok(Flow::Normal)
        }
        Stmt::ExitLoop { for_form } => Ok(Flow::ExitLoop(*for_form)),
        Stmt::ExitProc { .. } => Ok(Flow::ExitProc),
        Stmt::CallStmt {
            name,
            args,
            wrapped,
            ..
        } => {
            call_procedure(name, args, env, *wrapped)?;
            Ok(Flow::Normal)
        }
        // Unreachable after normalization; kept for exhaustiveness.
        Stmt::ForLoopOpen { .. }
        | Stmt::Next
        | Stmt::DoOpen { .. }
        | Stmt::LoopClose
        | Stmt::DoClose { .. }
        | Stmt::ProcOpen { .. }
        | Stmt::ProcClose => Err(rt_error(
            1,
            "internal error: deferred statement escaped the nesting pass",
        )),
        Stmt::If { .. } => Err(rt_error(
            1,
            "internal error: structural If reached the plain executor",
        )),
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
        Variant::Date(_) => true,
        Variant::Arr(_) => true,
        Variant::ObjectRef(_) => true,
    }
}

/// Case-insensitive variable key.
fn key(name: &str) -> String {
    name.to_ascii_lowercase()
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

/// Declare a variable (`Dim`), sizing its array when a size is present.
/// Inside a procedure the declaration is a local.
fn declare_var(env: &mut ExecEnv, name: &str, size: Option<&Expr>) -> AspResult<()> {
    let k = key(name);
    match size {
        None => {
            if let Some(f) = env.frame.as_mut() {
                f.locals.entry(k).or_insert(Variant::Empty);
            } else {
                env.vars.entry(k).or_insert(Variant::Empty);
            }
            Ok(())
        }
        Some(expr) => {
            let n = eval_expr(expr, env)?.as_number(1)? as i64;
            if n < 0 {
                return Err(rt_error(1, format!("negative array size in Dim '{name}'")));
            }
            if n > 1_000_000 {
                return Err(rt_error(1, format!("array '{name}' is too large")));
            }
            // VBScript `Dim a(3)` has four slots, indexes 0..=3.
            let arr = Variant::Arr(vec![Variant::Empty; (n + 1) as usize]);
            if let Some(f) = env.frame.as_mut() {
                f.locals.insert(k, arr);
            } else {
                env.vars.insert(k, arr);
            }
            Ok(())
        }
    }
}

/// Store into an array element, finding the array in local or global scope.
fn array_store(env: &mut ExecEnv, name: &str, ix: &Expr, value: Variant) -> AspResult<()> {
    let i = eval_expr(ix, env)?.as_number(1)? as i64;
    let k = key(name);
    if let Some(f) = env.frame.as_mut() {
        if let Some(Variant::Arr(items)) = f.locals.get_mut(&k) {
            return store_at(items, i as usize, value, name);
        }
        if f.locals.contains_key(&k) {
            return Err(rt_error(1, format!("'{name}' is not an array")));
        }
    }
    match env.vars.get_mut(&k) {
        Some(Variant::Arr(items)) => store_at(items, i as usize, value, name),
        Some(_) => Err(rt_error(1, format!("'{name}' is not an array"))),
        None => Err(rt_error(1, format!("array '{name}' is not dimensioned"))),
    }
}

fn store_at(items: &mut [Variant], i: usize, value: Variant, name: &str) -> AspResult<()> {
    if i >= items.len() {
        return Err(rt_error(1, format!("array index out of range in '{name}'")));
    }
    items[i] = value;
    Ok(())
}

/// Assign to a plain variable: locals shadow globals.
fn assign_var(env: &mut ExecEnv, name: &str, value: Variant) {
    let k = key(name);
    if let Some(f) = env.frame.as_mut() {
        f.locals.insert(k, value);
    } else {
        env.vars.insert(k, value);
    }
}

/// Loop-variable assignment bypasses the frame: the loop's own counter
/// lives where the loop statement's scope puts it (globals at top
/// level, locals inside a procedure).
fn assign_var_unchecked(env: &mut ExecEnv, name: &str, value: Variant) {
    assign_var(env, name, value);
}

/// Read a variable: locals, then consts, then globals.
fn lookup_var(env: &ExecEnv, name: &str) -> Option<Variant> {
    let k = key(name);
    if let Some(f) = &env.frame
        && let Some(v) = f.locals.get(&k)
    {
        return Some(v.clone());
    }
    if let Some(c) = env.consts.get(&k) {
        return Some(c.clone());
    }
    env.vars.get(&k).cloned()
}

fn eval_expr(expr: &Expr, env: &mut ExecEnv) -> AspResult<Variant> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Variable(name) => {
            if let Some(v) = lookup_var(env, name) {
                return Ok(v);
            }
            // `x = Now` / `x = MyFunc`: parens-less zero-argument calls.
            let k = key(name);
            if let Some(p) = env.procs.get(&k) {
                if p.params.is_empty() {
                    let decl = p.clone();
                    return call_procedure_inner(&decl, &[], env, false, false);
                }
                return Err(rt_error(
                    1,
                    format!(
                        "'{}' expects {} argument(s); use parentheses when calling it",
                        name,
                        p.params.len()
                    ),
                ));
            }
            if matches!(k.as_str(), "now" | "date" | "time" | "timer") {
                return eval_builtin(&k, &[], 1, env);
            }
            Ok(Variant::Empty)
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
                Variant::Date(_) => Err(rt_error(1, "type mismatch: cannot negate a Date")),
                Variant::Arr(_) => Err(rt_error(1, "type mismatch: cannot negate an array")),
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
        Expr::Builtin(name, args, line) => {
            if let Some(p) = env.procs.get(&key(name)) {
                let decl = p.clone();
                return call_procedure_inner(&decl, args, env, false, false);
            }
            // Array element read: `a(i)` — parens serve call and index.
            if args.len() == 1 && matches!(lookup_var(env, name), Some(Variant::Arr(_))) {
                let Some(Variant::Arr(items)) = lookup_var(env, name) else {
                    unreachable!("checked directly above");
                };
                let i = eval_expr(&args[0], env)?.as_number(*line)? as i64;
                if i < 0 || i as usize >= items.len() {
                    return Err(rt_error(
                        *line,
                        format!("array index out of range in '{name}'"),
                    ));
                }
                return Ok(items[i as usize].clone());
            }
            eval_builtin(name, args, *line, env)
        }
    }
}

/// Call a procedure by name (used by `CallStmt`).
fn call_procedure(
    name: &str,
    args: &[Expr],
    env: &mut ExecEnv,
    wrapped: bool,
) -> AspResult<Variant> {
    call_procedure_inner_by_name(name, args, env, wrapped, false)
}

/// Invoke a hoisted procedure with evaluated arguments. `wrapped` marks
/// the parens form; `was_call_keyword` distinguishes `Call f(x)` (list
/// parens, ByRef kept) from the bare `f (x)` ByVal form.
fn call_procedure_inner_by_name(
    name: &str,
    args: &[Expr],
    env: &mut ExecEnv,
    wrapped: bool,
    was_call_keyword: bool,
) -> AspResult<Variant> {
    let proc = match env.procs.get(&key(name)) {
        Some(p) => p.clone(),
        None => {
            return Err(rt_error(1, format!("undefined procedure '{name}'")));
        }
    };
    call_procedure_inner(&proc, args, env, wrapped, was_call_keyword)
}

/// Shared invocation logic.
#[allow(clippy::too_many_arguments)]
fn call_procedure_inner(
    proc: &Proc,
    args: &[Expr],
    env: &mut ExecEnv,
    wrapped: bool,
    was_call_keyword: bool,
) -> AspResult<Variant> {
    if env.call_depth >= MAX_CALL_DEPTH {
        return Err(rt_error(
            1,
            "procedure call depth exceeded (possible runaway recursion)",
        ));
    }
    let kind = if proc.is_function { "Function" } else { "Sub" };
    if args.len() != proc.params.len() {
        return Err(rt_error(
            1,
            format!(
                "{kind} '{}' expects {} argument(s), got {}",
                proc.name,
                proc.params.len(),
                args.len()
            ),
        ));
    }
    let mut locals = HashMap::new();
    // (param key, caller var key, whether the caller var is a local)
    let mut byref: Vec<(String, String, bool)> = Vec::new();
    for (p, a) in proc.params.iter().zip(args) {
        // VBScript: a paren-wrapped single argument in a BARE call is
        // passed ByVal. `Call f(x)` parens are list syntax (ByRef kept).
        if p.by_val || (wrapped && args.len() == 1 && !was_call_keyword) {
            locals.insert(key(&p.name), eval_expr(a, env)?);
            continue;
        }
        if let Expr::Variable(vname) = a {
            // ByRef: bind to the caller's variable when the argument is
            // a plain name.
            let in_frame = env
                .frame
                .as_ref()
                .map(|f| f.locals.contains_key(&key(vname)))
                .unwrap_or(false);
            let in_globals = env.vars.contains_key(&key(vname));
            let initial = lookup_var(env, vname).unwrap_or(Variant::Empty);
            if !in_frame && !in_globals {
                env.vars.insert(key(vname), Variant::Empty);
            }
            byref.push((key(&p.name), key(vname), in_frame));
            locals.insert(key(&p.name), initial);
        } else {
            locals.insert(key(&p.name), eval_expr(a, env)?);
        }
    }
    let saved_frame = env.frame.take();
    let saved_ret = env.ret.take();
    env.frame = Some(Frame {
        locals,
        func: if proc.is_function {
            Some(key(&proc.name))
        } else {
            None
        },
        byref,
    });
    env.call_depth += 1;
    let flow = exec_stream(&proc.body, env)?;
    env.call_depth -= 1;
    let frame = env.frame.take();
    env.frame = saved_frame;
    let ret = env.ret.take();
    env.ret = saved_ret;
    // ByRef write-back into the caller's variable.
    if let Some(mut f) = frame {
        for (pk, ck, in_caller_frame) in f.byref.drain(..) {
            if let Some(val) = f.locals.get(&pk) {
                if in_caller_frame {
                    if let Some(cf) = env.frame.as_mut() {
                        cf.locals.insert(ck, val.clone());
                    }
                } else {
                    env.vars.insert(ck, val.clone());
                }
            }
        }
    }
    let _ = flow; // ExitProc is the normal return; ExitLoop propagates.
    Ok(ret.unwrap_or(Variant::Empty))
}

/// Binary operators with VBScript semantics: `+` is numeric addition
/// (string operands are coerced), `&` always concatenates, comparisons
/// compare strings lexically when both sides are strings, dates enter
/// arithmetic through the VB epoch number.
fn apply_binary(op: &str, lv: Variant, rv: Variant) -> AspResult<Variant> {
    match op {
        "&" => Ok(Variant::Str(format!("{}{}", lv.display(), rv.display()))),
        "+" => {
            if matches!(lv, Variant::Date(_)) || matches!(rv, Variant::Date(_)) {
                return date_arithmetic(op, &lv, &rv);
            }
            if matches!(lv, Variant::Str(_))
                && matches!(rv, Variant::Str(ref s) if !string_is_numeric(s))
            {
                // `+` between non-numeric strings concatenates in VBScript.
                return Ok(Variant::Str(format!("{}{}", lv.display(), rv.display())));
            }
            let n = lv.as_number(1)? + rv.as_number(1)?;
            Ok(exact_number(n))
        }
        "-" | "*" | "/" | "\\" | "mod" | "^" => {
            if matches!(lv, Variant::Date(_)) || matches!(rv, Variant::Date(_)) {
                return date_arithmetic(op, &lv, &rv);
            }
            match op {
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
                _ => Ok(Variant::Float(lv.as_number(1)?.powf(rv.as_number(1)?))),
            }
        }
        "=" | "<>" | "<" | ">" | "<=" | ">=" => Ok(Variant::Bool(compare(op, &lv, &rv)?)),
        "and" | "or" | "xor" => {
            if let (Variant::Bool(a), Variant::Bool(b)) = (&lv, &rv) {
                let n = match op {
                    "and" => *a && *b,
                    "or" => *a || *b,
                    _ => *a != *b,
                };
                return Ok(Variant::Bool(n));
            }
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

/// True when the string parses as a plain number (not a date).
fn string_is_numeric(s: &str) -> bool {
    s.parse::<f64>().is_ok() || parse_number(s).is_some()
}

/// Date arithmetic: `Date + n` (days), `Date - n`, and `Date - Date`
/// (a day difference). Other Date operand combinations are type errors.
fn date_arithmetic(op: &str, lv: &Variant, rv: &Variant) -> AspResult<Variant> {
    match (op, lv, rv) {
        ("-", Variant::Date(a), Variant::Date(b)) => Ok(Variant::Float(
            vb_datetime::to_number(*a) - vb_datetime::to_number(*b),
        )),
        ("+", Variant::Date(a), other) => Ok(Variant::Date(shift_date(*a, other.as_number(1)?))),
        ("+", other, Variant::Date(b)) => Ok(Variant::Date(shift_date(*b, other.as_number(1)?))),
        ("-", Variant::Date(a), other) => Ok(Variant::Date(shift_date(*a, -other.as_number(1)?))),
        _ => Err(rt_error(
            1,
            "type mismatch: this operator does not support Date operands",
        )),
    }
}

/// Shift a datetime by a fractional number of days.
fn shift_date(d: chrono::NaiveDateTime, days: f64) -> chrono::NaiveDateTime {
    let whole = days.trunc() as i64;
    let secs = ((days - days.trunc()) * 86_400.0).round() as i64;
    d + chrono::Duration::days(whole) + chrono::Duration::seconds(secs)
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
/// strings vs Empty compare lexically against "", everything else
/// compares numerically (a non-numeric string is a type mismatch).
fn compare(op: &str, lv: &Variant, rv: &Variant) -> AspResult<bool> {
    let ord = match (lv, rv) {
        (Variant::Str(a), Variant::Str(b)) => a.cmp(b),
        (Variant::Str(a), Variant::Empty) => a.cmp(&String::new()),
        (Variant::Empty, Variant::Str(b)) => String::new().cmp(b),
        _ => {
            let ln = lv.as_number(1)?;
            let rn = rv.as_number(1)?;
            ln.partial_cmp(&rn).unwrap_or(std::cmp::Ordering::Equal)
        }
    };
    Ok(match op {
        "=" => ord == std::cmp::Ordering::Equal,
        "<>" => ord != std::cmp::Ordering::Equal,
        "<" => ord == std::cmp::Ordering::Less,
        ">" => ord == std::cmp::Ordering::Greater,
        "<=" => ord != std::cmp::Ordering::Greater,
        _ => ord != std::cmp::Ordering::Less,
    })
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
            // Always buffered; accept and ignore the property value.
            if let Some(e) = arg {
                let _ = eval_expr(e, env)?;
            }
            Ok(())
        }
        "flush" | "clear" => {
            // Real Clear drops buffered output; supported faithfully.
            if verb == "clear" {
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

/// The builtin function set (M1 string tools, M2 conversions, array
/// helpers, and the Date/Time family). Each is small and total.
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
        ("strreverse", [v]) => Ok(Variant::Str(v.display().chars().rev().collect())),
        ("space", [n]) => Ok(Variant::Str(
            " ".repeat(n.as_number(line)?.max(0.0) as usize),
        )),
        ("string", [n, ch]) => {
            let n = n.as_number(line)?.max(0.0) as usize;
            let unit = ch.display().chars().next().unwrap_or(' ').to_string();
            Ok(Variant::Str(unit.repeat(n)))
        }
        ("replace", [hay, from, to]) => Ok(Variant::Str(
            hay.display().replace(&from.display(), &to.display()),
        )),
        ("split", [v, sep]) => {
            let sep = sep.display();
            Ok(Variant::Arr(
                v.display()
                    .split(&sep as &str)
                    .map(|p| Variant::Str(p.to_string()))
                    .collect(),
            ))
        }
        ("split", [v]) => Ok(Variant::Arr(
            v.display()
                .split(' ')
                .map(|p| Variant::Str(p.to_string()))
                .collect(),
        )),
        ("join", [arr]) => match arr {
            Variant::Arr(items) => Ok(Variant::Str(
                items
                    .iter()
                    .map(|v| v.display())
                    .collect::<Vec<_>>()
                    .join(" "),
            )),
            _ => Ok(Variant::Str(arr.display())),
        },
        ("join", [arr, sep]) => match arr {
            Variant::Arr(items) => Ok(Variant::Str(
                items
                    .iter()
                    .map(|v| v.display())
                    .collect::<Vec<_>>()
                    .join(&sep.display()),
            )),
            _ => Ok(Variant::Str(arr.display())),
        },
        ("ubound", [v]) => array_bound(v, line),
        ("ubound", [v, _]) => array_bound(v, line),
        ("lbound", [_v]) => Ok(Variant::Int(0)),
        ("array", elems) => Ok(Variant::Arr(elems.to_vec())),
        ("cstr", [v]) => Ok(Variant::Str(v.display())),
        ("cint", [v]) => to_int(v, line, true),
        ("clng", [v]) => to_int(v, line, false),
        ("cdbl", [v]) => Ok(Variant::Float(v.as_number(line)?)),
        ("cbool", [v]) => Ok(Variant::Bool(truthy(v.clone()))),
        ("cdate", [v]) => match v {
            Variant::Date(_) => Ok(v.clone()),
            Variant::Str(s) => match vb_datetime::parse(s) {
                Some(d) => Ok(Variant::Date(d)),
                None => Err(rt_error(
                    line,
                    format!("type mismatch: '{s}' is not a date"),
                )),
            },
            _ => Err(rt_error(line, "CDate requires a date string")),
        },
        ("datevalue", [v]) => match v {
            Variant::Date(d) => Ok(Variant::Date(d.date().and_time(chrono::NaiveTime::MIN))),
            Variant::Str(s) => match vb_datetime::parse(s) {
                Some(d) => Ok(Variant::Date(d.date().and_time(chrono::NaiveTime::MIN))),
                None => Err(rt_error(
                    line,
                    format!("type mismatch: '{s}' is not a date"),
                )),
            },
            _ => Err(rt_error(line, "DateValue requires a date")),
        },
        ("timevalue", [v]) => match v {
            Variant::Date(d) => Ok(Variant::Date(time_only(*d))),
            Variant::Str(s) => match vb_datetime::parse(s) {
                Some(d) => Ok(Variant::Date(time_only(d))),
                None => Err(rt_error(
                    line,
                    format!("type mismatch: '{s}' is not a time"),
                )),
            },
            _ => Err(rt_error(line, "TimeValue requires a time")),
        },
        ("isnull", [v]) => Ok(Variant::Bool(matches!(v, Variant::Null))),
        ("isempty", [v]) => Ok(Variant::Bool(matches!(v, Variant::Empty))),
        ("isarray", [v]) => Ok(Variant::Bool(matches!(v, Variant::Arr(_)))),
        ("isdate", [v]) => Ok(Variant::Bool(match v {
            Variant::Date(_) => true,
            Variant::Str(s) => vb_datetime::parse(s).is_some(),
            _ => false,
        })),
        ("isnumeric", [v]) => Ok(Variant::Bool(
            matches!(
                v,
                Variant::Int(_) | Variant::Float(_) | Variant::Bool(_) | Variant::Date(_)
            ) || string_is_numeric(&v.display()),
        )),
        ("strcomp", [a, b]) => {
            let equal = compare("=", a, b)?;
            Ok(Variant::Int(if equal { 0 } else { 1 }))
        }
        // Date/time family. `Weekday` follows VBScript's Sunday=1 default.
        ("year", [v]) => Ok(Variant::Int(as_datetime(v, line)?.year() as i64)),
        ("month", [v]) => Ok(Variant::Int(as_datetime(v, line)?.month() as i64)),
        ("day", [v]) => Ok(Variant::Int(as_datetime(v, line)?.day() as i64)),
        ("hour", [v]) => Ok(Variant::Int(as_datetime(v, line)?.hour() as i64)),
        ("minute", [v]) => Ok(Variant::Int(as_datetime(v, line)?.minute() as i64)),
        ("second", [v]) => Ok(Variant::Int(as_datetime(v, line)?.second() as i64)),
        ("weekday", [v]) => Ok(Variant::Int(
            as_datetime(v, line)?.weekday().num_days_from_sunday() as i64 + 1,
        )),
        ("dateserial", [y, m, d]) => {
            let y = y.as_number(line)? as i32;
            let m = m.as_number(line)? as i32;
            let d = d.as_number(line)? as i32;
            match vb_datetime::serial_date(y, m, d) {
                Some(date) => Ok(Variant::Date(date.and_time(chrono::NaiveTime::MIN))),
                None => Err(rt_error(line, "DateSerial arguments do not form a date")),
            }
        }
        ("dateadd", [iv, n, d]) => {
            let iv = iv.display().to_ascii_lowercase();
            let n = n.as_number(line)?;
            let base = as_datetime(d, line)?;
            let shifted = match iv.as_str() {
                "yyyy" => shift_months(base, n * 12.0),
                "q" => shift_months(base, n * 3.0),
                "m" => shift_months(base, n),
                "d" | "y" | "w" => shift_date(base, n),
                "h" => shift_date(base, n / 24.0),
                "n" => shift_date(base, n / 1_440.0),
                "s" => shift_date(base, n / 86_400.0),
                _ => {
                    return Err(unimplemented(line, format!("DateAdd interval '{iv}'")));
                }
            };
            Ok(Variant::Date(shifted))
        }
        ("datediff", [iv, a, b]) => {
            let iv = iv.display().to_ascii_lowercase();
            let (a, b) = (as_datetime(a, line)?, as_datetime(b, line)?);
            let days = vb_datetime::to_number(b) - vb_datetime::to_number(a);
            let diff = match iv.as_str() {
                "d" | "y" => days,
                "h" => days * 24.0,
                "n" => days * 1_440.0,
                "s" => days * 86_400.0,
                "m" => {
                    (((b.year() - a.year()) as i64) * 12 + (b.month() as i64 - a.month() as i64))
                        as f64
                }
                "yyyy" => (b.year() - a.year()) as f64,
                "q" => ((b.year() - a.year()) * 4) as f64,
                _ => {
                    return Err(unimplemented(line, format!("DateDiff interval '{iv}'")));
                }
            };
            Ok(exact_number(diff))
        }
        ("now", []) => Ok(Variant::Date(chrono::Local::now().naive_local())),
        ("date", []) => Ok(Variant::Date(
            chrono::Local::now()
                .naive_local()
                .date()
                .and_time(chrono::NaiveTime::MIN),
        )),
        ("time", []) => Ok(Variant::Date(time_only(chrono::Local::now().naive_local()))),
        ("timer", []) => {
            let secs = chrono::Local::now().time().num_seconds_from_midnight();
            Ok(Variant::Float(secs as f64))
        }
        (other, _) => Err(unimplemented(line, format!("function {other}"))),
    }
}

/// Coerce a value that should be a date (Variant or date string).
fn as_datetime(v: &Variant, line: usize) -> AspResult<chrono::NaiveDateTime> {
    match v {
        Variant::Date(d) => Ok(*d),
        Variant::Str(s) => vb_datetime::parse(s)
            .ok_or_else(|| rt_error(line, format!("type mismatch: '{s}' is not a date"))),
        _ => Err(rt_error(line, "type mismatch: value is not a date")),
    }
}

fn time_only(d: chrono::NaiveDateTime) -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(1899, 12, 30)
        .unwrap_or_else(|| d.date())
        .and_time(d.time())
}

/// Shift a datetime by whole months, clamping overflow days
/// (`DateAdd("m", 1, Jan 31)` → Feb 28).
fn shift_months(d: chrono::NaiveDateTime, months: f64) -> chrono::NaiveDateTime {
    let total_months = if months.fract() >= 0.5 {
        months.ceil() as i64
    } else {
        months.trunc() as i64
    };
    let years = total_months.div_euclid(12);
    let months_left = total_months.rem_euclid(12);
    let target = d.month() as i64 - 1 + months_left;
    let year = d.year() as i64 + years + target.div_euclid(12);
    let month = target.rem_euclid(12) as u32 + 1;
    let day = d.day().min(days_in_month(year as i32, month));
    let date = chrono::NaiveDate::from_ymd_opt(year as i32, month, day).unwrap_or_else(|| d.date());
    date.and_time(d.time())
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first =
        chrono::NaiveDate::from_ymd_opt(next_year, next_month, 1).unwrap_or(chrono::NaiveDate::MAX);
    let this = chrono::NaiveDate::from_ymd_opt(year, month, 1).unwrap_or(chrono::NaiveDate::MIN);
    (first - this).num_days() as u32
}

/// VBScript CInt/CLng: banker's rounding (.5 to even) with overflow
/// errors beyond the target integer size.
fn to_int(v: &Variant, line: usize, is_int: bool) -> AspResult<Variant> {
    let n = v.as_number(line)?;
    let bound: f64 = if is_int { 32_767.0 } else { 2_147_483_647.0 };
    if n.abs() > bound {
        return Err(rt_error(line, "overflow during conversion"));
    }
    Ok(Variant::Int(n.round_ties_even() as i64))
}

/// UBound: largest legal index; one dimension until multi-dim support.
fn array_bound(v: &Variant, line: usize) -> AspResult<Variant> {
    match v {
        Variant::Arr(items) => Ok(Variant::Int(items.len() as i64 - 1)),
        _ => Err(rt_error(line, "UBound requires an array")),
    }
}

/// Pair deferred delimiters into normalized statements and hoist
/// procedures. `allow_procs` is true only at top level and inside
/// `If` branch bodies — procedures cannot appear inside loops or
/// other procedures (matching VBScript).
fn normalize_block(stmts: &[Stmt], allow_procs: bool) -> AspResult<(Vec<StmtN>, Vec<Proc>)> {
    let mut out = Vec::new();
    let mut procs = Vec::new();
    let mut i = 0usize;
    while i < stmts.len() {
        match &stmts[i] {
            Stmt::If {
                branches,
                else_body,
                ..
            } => {
                let mut n_branches = Vec::new();
                for (cond, body) in branches {
                    let (b, p) = normalize_block(body, allow_procs)?;
                    procs.extend(p);
                    n_branches.push((cond.clone(), b));
                }
                let n_else = match else_body {
                    Some(body) => {
                        let (b, p) = normalize_block(body, allow_procs)?;
                        procs.extend(p);
                        Some(b)
                    }
                    None => None,
                };
                out.push(StmtN::If {
                    branches: n_branches,
                    else_body: n_else,
                });
                i += 1;
            }
            Stmt::ForLoopOpen {
                var,
                start,
                end,
                step,
            } => {
                let close = find_delimiter(stmts, i + 1, Delim::Next)?;
                let (body, inner) = normalize_block(&stmts[i + 1..close], false)?;
                procs.extend(inner);
                out.push(StmtN::For {
                    var: var.clone(),
                    start: start.clone(),
                    end: end.clone(),
                    step: step.clone(),
                    body,
                });
                i = close + 1;
            }
            Stmt::DoOpen {
                cond,
                pre,
                while_form,
            } => {
                let close = find_delimiter(stmts, i + 1, Delim::Loop)?;
                let (body, inner) = normalize_block(&stmts[i + 1..close], false)?;
                procs.extend(inner);
                // Trailing-condition form (`Loop While cond`) overrides.
                let (pre_now, cond_now, while_now) = match &stmts[close] {
                    Stmt::DoClose {
                        cond: trailing,
                        while_form: tw,
                    } => {
                        if cond.is_some() {
                            return Err(rt_error(
                                1,
                                "Do loop cannot have both a leading and a trailing condition",
                            ));
                        }
                        (false, trailing.clone(), *tw)
                    }
                    _ => (*pre, cond.clone(), *while_form),
                };
                out.push(StmtN::Do {
                    cond: cond_now,
                    pre: pre_now,
                    while_form: while_now,
                    body,
                });
                i = close + 1;
            }
            Stmt::ProcOpen {
                name,
                params,
                is_function,
            } => {
                if !allow_procs {
                    return Err(rt_error(
                        1,
                        "procedures cannot be defined inside a loop or another procedure",
                    ));
                }
                let close = find_delimiter(stmts, i + 1, Delim::Proc)?;
                let (body, inner) = normalize_block(&stmts[i + 1..close], false)?;
                if !inner.is_empty() {
                    return Err(rt_error(
                        1,
                        "procedures cannot be defined inside another procedure",
                    ));
                }
                procs.push(Proc {
                    name: name.clone(),
                    params: params.clone(),
                    body,
                    is_function: *is_function,
                });
                i = close + 1;
            }
            Stmt::Next => return Err(rt_error(1, "'Next' without a matching 'For'")),
            Stmt::LoopClose | Stmt::DoClose { .. } => {
                return Err(rt_error(1, "'Loop' without a matching 'Do'"));
            }
            Stmt::ProcClose => return Err(rt_error(1, "'End Sub' without a matching 'Sub'")),
            other => {
                out.push(StmtN::Plain(other.clone()));
                i += 1;
            }
        }
    }
    Ok((out, procs))
}

/// Which delimiter a scan is looking for.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Delim {
    Next,
    Loop,
    Proc,
}

/// Scan forward from `start` for the delimiter closing the opener that
/// pushed `want`, pairing nested openers of any kind. Returns the
/// index of the closing delimiter, with precise errors for missing or
/// mismatched closers.
fn find_delimiter(stmts: &[Stmt], start: usize, want: Delim) -> AspResult<usize> {
    // Open kinds on the scan stack: For waits for Next, Do waits for
    // Loop/DoClose, and Proc waits for ProcClose.
    #[derive(Clone, Copy, PartialEq)]
    enum Open {
        For,
        Do,
        Proc,
    }
    let mut stack: Vec<Open> = Vec::new();
    let mut j = start;
    while j < stmts.len() {
        let closes = match &stmts[j] {
            Stmt::ForLoopOpen { .. } => {
                stack.push(Open::For);
                None
            }
            Stmt::DoOpen { .. } => {
                stack.push(Open::Do);
                None
            }
            Stmt::ProcOpen { .. } => {
                stack.push(Open::Proc);
                None
            }
            Stmt::Next => {
                if stack.is_empty() {
                    Some(Delim::Next)
                } else {
                    match stack.pop() {
                        Some(Open::For) => None,
                        Some(Open::Do) => {
                            return Err(rt_error(1, "'Next' cannot close a 'Do' loop"));
                        }
                        Some(Open::Proc) => {
                            return Err(rt_error(1, "'Next' cannot close a procedure"));
                        }
                        None => None,
                    }
                }
            }
            Stmt::LoopClose | Stmt::DoClose { .. } => {
                if stack.is_empty() {
                    Some(Delim::Loop)
                } else {
                    match stack.pop() {
                        Some(Open::Do) => None,
                        Some(Open::For) => {
                            return Err(rt_error(1, "'Loop' cannot close a 'For' loop"));
                        }
                        Some(Open::Proc) => {
                            return Err(rt_error(1, "'Loop' cannot close a procedure"));
                        }
                        None => None,
                    }
                }
            }
            Stmt::ProcClose => {
                if stack.is_empty() {
                    Some(Delim::Proc)
                } else {
                    match stack.pop() {
                        Some(Open::Proc) => None,
                        Some(Open::For) => {
                            return Err(rt_error(1, "'End Sub' cannot close a 'For' loop"));
                        }
                        Some(Open::Do) => {
                            return Err(rt_error(1, "'End Sub' cannot close a 'Do' loop"));
                        }
                        None => None,
                    }
                }
            }
            _ => None,
        };
        if let Some(d) = closes
            && stack.is_empty()
        {
            if d != want {
                return Err(rt_error(
                    1,
                    match (d, want) {
                        (Delim::Loop, Delim::Next) => "missing 'Next' (found 'Loop')",
                        (Delim::Next, Delim::Loop) => "missing 'Loop' (found 'Next')",
                        _ => "missing a closing delimiter",
                    },
                ));
            }
            return Ok(j);
        }
        j += 1;
    }
    Err(rt_error(
        1,
        match want {
            Delim::Next => "unterminated loop: missing 'Next'",
            Delim::Loop => "unterminated loop: missing 'Loop'",
            Delim::Proc => "unterminated procedure: missing 'End Sub' or 'End Function'",
        },
    ))
}
