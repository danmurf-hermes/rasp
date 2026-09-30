//! ASP intrinsic objects and the page render loop for RASP.
//!
//! M1 covers the `Response` object (Write/End/Clear/buffer passthrough),
//! assembly of preprocessed pages (includes resolved, cycles detected),
//! and one-shot rendering of a parsed page into a full HTTP response.

use asp_core::error::Diagnostic;
use asp_core::parser::{Block, Page};
use asp_core::{AppRoot, AspError, AspResult};
use asp_vbscript::{ExecEnv, parse_block};
use std::collections::HashMap;

/// An assembled page: script blocks with their statement lists plus
/// literal text, ready to render.
#[derive(Debug)]
pub struct AssembledPage {
    /// Render steps in source order.
    pub steps: Vec<RenderStep>,
}

/// One step of rendering: emit literal text or run a script block.
#[derive(Debug)]
pub enum RenderStep {
    Text(String),
    /// Pre-parsed statements of one `<%` block, with its start line.
    Script(Vec<asp_vbscript::Stmt>),
    /// `<%= expr %>` with the pre-parsed expression.
    Output(asp_vbscript::Expr),
}

/// Maximum include depth before a cycle is assumed (IIS nests 32 deep).
const MAX_INCLUDE_DEPTH: usize = 32;

/// Resolve includes and parse every script block of a page.
///
/// `root_relative` names the page inside the app root; `seen` tracks the
/// active include chain by path for cycle detection.
pub fn assemble(
    app: &AppRoot,
    root_relative: &str,
    seen: &mut Vec<String>,
) -> AspResult<AssembledPage> {
    if seen.len() > MAX_INCLUDE_DEPTH {
        return Err(AspError::IncludeCycle(root_relative.to_string()));
    }
    let source = app.read_page(root_relative)?;
    let page = Page::parse(&source)?;
    let parent_dir = app
        .path()
        .join(root_relative.trim_start_matches('/'))
        .parent()
        .map(|p| p.to_path_buf());
    assemble_from_source(app, &page, parent_dir.as_deref(), root_relative, seen)
}

/// Assemble from an already-parsed page (unit-test entry point).
pub fn assemble_from_source(
    app: &AppRoot,
    page: &Page,
    parent_dir: Option<&std::path::Path>,
    label: &str,
    seen: &mut Vec<String>,
) -> AspResult<AssembledPage> {
    let key = label.trim_start_matches('/').to_ascii_lowercase();
    if seen.contains(&key) {
        return Err(AspError::IncludeCycle(label.to_string()));
    }
    seen.push(key);
    let _ = parent_dir;

    match &page.language {
        asp_core::parser::LanguageSetting::Explicit(name)
            if name.eq_ignore_ascii_case("vbscript") => {}
        asp_core::parser::LanguageSetting::Explicit(name) => {
            return Err(AspError::UnsupportedLanguage(name.clone()));
        }
    }

    let mut steps = Vec::new();
    for block in &page.blocks {
        match block {
            Block::Text(text) => steps.push(RenderStep::Text(text.clone())),
            Block::Script { body } => {
                let stmts = parse_block(body, 1)?;
                let _ = stmts.len();
                steps.push(RenderStep::Script(parse_block(body, 1)?));
            }
            Block::Output { expression } => {
                let expr = parse_expr_standalone(expression, 0)?;
                steps.push(RenderStep::Output(expr));
            }
            Block::Include { path } => {
                let (kind, rel) = path
                    .split_once(':')
                    .ok_or_else(|| AspError::Io(format!("malformed include: {path}")))?;
                let full = match kind {
                    "virtual" => {
                        // Root-relative but stored without a leading
                        // slash, which `read_page` would read as an
                        // absolute path escape.
                        rel.trim_start_matches('/').to_string()
                    }
                    _ => {
                        // Sibling-relative: resolve against the parent dir.
                        match parent_dir {
                            Some(dir) => {
                                let joined = dir.join(rel.trim_start_matches('/'));
                                joined
                                    .strip_prefix(app.path())
                                    .map_err(|_| AspError::PathEscape(rel.to_string()))?
                                    .to_string_lossy()
                                    .into_owned()
                            }
                            None => rel.to_string(),
                        }
                    }
                };
                let child = assemble(app, &full, seen)?;
                steps.extend(child.steps);
            }
        }
    }
    seen.pop();
    Ok(AssembledPage { steps })
}

/// Parse one `<%= %>` expression by reusing the statement grammar: the
/// source is temporarily promoted to a Response.Write call and the
/// resulting expression is lifted back out.
fn parse_expr_standalone(expression: &str, line: usize) -> AspResult<asp_vbscript::Expr> {
    let src = format!("response.write {expression}");
    match parse_block(&src, line)?.as_slice() {
        [
            asp_vbscript::Stmt::ResponseCall {
                arg: Some(expr), ..
            },
        ] => Ok(expr.clone()),
        [asp_vbscript::Stmt::ResponseCall { arg: None, .. }] => Err(AspError::Syntax(
            Diagnostic::new(line, "<%= %> requires an expression"),
        )),
        other => Err(AspError::Syntax(Diagnostic::new(
            line,
            format!(
                "<%= %> must contain one expression (got {} statements)",
                other.len()
            ),
        ))),
    }
}

/// The result of rendering: body plus response properties.
#[derive(Debug, Default, PartialEq)]
pub struct RenderOutput {
    pub body: String,
    pub status: Option<u16>,
    pub content_type: Option<String>,
}

/// Render an assembled page end to end against a base environment.
///
/// The steps are **flattened into one VBScript statement stream** before
/// execution — literal text becomes `Response.Write "…"`, `<%= %>`
/// becomes `Response.Write expr` — so a `For` opened in one `<% %>`
/// block can close its `Next` in a later block with the page's HTML
/// re-emitted on every iteration, exactly as Classic ASP behaves.
pub fn render_assembled(assembled: &AssembledPage, base_env: &ExecEnv) -> AspResult<RenderOutput> {
    // ExecEnv is request-scoped; M1 starts each page from the parsed
    // request data rather than inheriting mutable state.
    let mut env = fresh_env(base_env);
    let mut out = RenderOutput::default();
    let stmts = flatten_steps(&assembled.steps)?;
    asp_vbscript::exec_block_loops(&stmts, &mut env)?;
    out.body = env.response.body();
    out.status = env.response.status;
    out.content_type = env.response.content_type.clone();
    Ok(out)
}

/// Flatten render steps into one statement stream.
///
/// Text becomes a `Response.Write` literal; `<%= %>` becomes a
/// `Response.Write` of the parsed expression; script steps splice in
/// their pre-parsed statements. Newline-delimited `LineEnd`s keep the
/// per-block statement separation intact.
fn flatten_steps(steps: &[RenderStep]) -> AspResult<Vec<asp_vbscript::Stmt>> {
    use asp_vbscript::Stmt;
    let mut stmts: Vec<Stmt> = Vec::new();
    for step in steps {
        match step {
            RenderStep::Text(text) => {
                if text.is_empty() {
                    continue;
                }
                stmts.push(Stmt::ResponseCall {
                    verb: "write".to_string(),
                    arg: Some(asp_vbscript::Expr::Literal(asp_vbscript::Variant::Str(
                        text.clone(),
                    ))),
                });
            }
            RenderStep::Script(block_stmts) => {
                stmts.extend(block_stmts.iter().cloned());
            }
            RenderStep::Output(expr) => {
                stmts.push(Stmt::ResponseCall {
                    verb: "write".to_string(),
                    arg: Some(expr.clone()),
                });
            }
        }
    }
    Ok(stmts)
}

/// Build the request-scoped environment from the base one.
fn fresh_env(base: &ExecEnv) -> ExecEnv {
    let mut env = ExecEnv::new();
    // Request data (query/form/cookies) is the request's; session state
    // sharing across pages arrives with Session support (M4) but the
    // values themselves are carried through for the same request.
    env.session = base.session.clone();
    env.request_data = base.request_data.clone();
    env
}

/// Read Request data for one request: query string, form, cookies.
pub fn build_request_data(query: &str, form: &str, cookies: &str) -> HashMap<String, String> {
    let mut data = HashMap::new();
    for (collection, raw) in [("querystring", query), ("form", form), ("cookies", cookies)] {
        for pair in raw.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let k = url_decode(k);
            let v = url_decode(v);
            data.insert(format!("{collection}\u{0}{}", k.to_ascii_lowercase()), v);
        }
    }
    data
}

/// Decode `%xx` escapes and `+` (form encoding) into UTF-8.
pub fn url_decode(input: &str) -> String {
    let replaced = input.replace('+', " ");
    percent_encoding::percent_decode_str(&replaced)
        .decode_utf8_lossy()
        .into_owned()
}

/// URL-encode for redirects / cookies (used by M3+; kept alongside
/// `url_decode` for symmetry).
#[allow(dead_code)]
pub fn url_encode(input: &str) -> String {
    let mut out = String::new();
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Top-level render of a page by path: read, parse, assemble, execute.
pub fn render_page(
    app: &AppRoot,
    root_relative: &str,
    request_data: HashMap<String, String>,
) -> AspResult<RenderOutput> {
    let mut seen = Vec::new();
    let assembled = assemble(app, root_relative, &mut seen)?;
    let base = ExecEnv::new().with_request_data(request_data);
    render_assembled(&assembled, &base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn build_app(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir()
            .join("rasp-runtime-tests")
            .join(format!("{tag}-{}", std::process::id()));
        fs::create_dir_all(root.join("inc")).unwrap();
        root
    }

    #[test]
    fn renders_hello_page() {
        let root = build_app("hello");
        fs::write(
            root.join("hello.asp"),
            "<h1>Hello</h1><%= \" \" & (1 + 1) %>",
        )
        .unwrap();
        let app = AppRoot::new(&root);
        let out = render_page(&app, "hello.asp", HashMap::new()).unwrap();
        assert_eq!(out.body, "<h1>Hello</h1> 2");
    }

    #[test]
    fn renders_script_blocks_and_variables() {
        let root = build_app("script");
        fs::write(
            root.join("page.asp"),
            "<% Dim n: n = 3 %><p>n is <%= n * 2 %></p>",
        )
        .unwrap();
        let app = AppRoot::new(&root);
        let out = render_page(&app, "page.asp", HashMap::new()).unwrap();
        assert_eq!(out.body, "<p>n is 6</p>");
    }

    #[test]
    fn file_include_inlines_content() {
        let root = build_app("file");
        fs::create_dir_all(root.join("inner")).unwrap();
        fs::write(root.join("inner").join("child.asp"), "CHILD").unwrap();
        fs::write(
            root.join("inner").join("parent.asp"),
            "A<!-- #include file=\"child.asp\" -->B",
        )
        .unwrap();
        let app = AppRoot::new(&root);
        let out = render_page(&app, "inner/parent.asp", HashMap::new()).unwrap();
        assert_eq!(out.body, "ACHILDB");
    }

    #[test]
    fn virtual_include_inlines_content() {
        let root = build_app("virtual");
        fs::write(root.join("inc").join("footer.asp"), "<hr>").unwrap();
        fs::write(
            root.join("page.asp"),
            "Top<!-- #include virtual=\"/inc/footer.asp\" -->End",
        )
        .unwrap();
        let app = AppRoot::new(&root);
        let out = render_page(&app, "page.asp", HashMap::new()).unwrap();
        assert_eq!(out.body, "Top<hr>End");
    }

    #[test]
    fn include_cycle_is_detected() {
        let root = build_app("cycle");
        fs::write(root.join("a.asp"), "a<!-- #include file=\"a.asp\" -->").unwrap();
        let app = AppRoot::new(&root);
        let err = render_page(&app, "a.asp", HashMap::new()).unwrap_err();
        assert!(matches!(err, AspError::IncludeCycle(_)));
    }

    #[test]
    fn include_not_found_is_clear() {
        let root = build_app("missing");
        fs::write(root.join("page.asp"), "<!-- #include file=\"gone.asp\" -->").unwrap();
        let app = AppRoot::new(&root);
        let err = render_page(&app, "page.asp", HashMap::new()).unwrap_err();
        assert!(matches!(
            err,
            AspError::IncludeNotFound(_) | AspError::PageNotFound(_)
        ));
    }

    #[test]
    fn unsupported_language_is_rejected() {
        let root = build_app("lang");
        fs::write(root.join("j.asp"), "<%@ Language=JScript %><%= 1 %>").unwrap();
        let app = AppRoot::new(&root);
        let err = render_page(&app, "j.asp", HashMap::new()).unwrap_err();
        assert!(
            matches!(err, AspError::UnsupportedLanguage(l) if l.eq_ignore_ascii_case("jscript"))
        );
    }

    #[test]
    fn response_end_stops_rendering() {
        let root = build_app("end");
        fs::write(root.join("e.asp"), "one<% Response.End %>two").unwrap();
        let app = AppRoot::new(&root);
        let out = render_page(&app, "e.asp", HashMap::new()).unwrap();
        assert_eq!(out.body, "one");
    }

    #[test]
    fn querystring_data_reaches_page() {
        let root = build_app("query");
        fs::write(
            root.join("q.asp"),
            "<%= \"Hello \" & Request.QueryString(\"name\") %>",
        )
        .unwrap();
        let app = AppRoot::new(&root);
        let data = build_request_data("name=Dan%21", "", "");
        let out = render_page(&app, "q.asp", data).unwrap();
        assert_eq!(out.body, "Hello Dan!");
    }

    #[test]
    fn url_decode_handles_escapes_and_plus() {
        assert_eq!(url_decode("a%20b+c"), "a b c");
        assert_eq!(url_decode("100%25"), "100%");
        assert_eq!(url_encode("a b"), "a%20b");
    }

    #[test]
    fn syntax_error_pages_fail_with_line() {
        let root = build_app("syntax");
        fs::write(root.join("bad.asp"), "text\n<% x = %>").unwrap();
        let app = AppRoot::new(&root);
        let err = render_page(&app, "bad.asp", HashMap::new()).unwrap_err();
        assert!(matches!(err, AspError::Syntax(_)));
    }

    #[test]
    fn runtime_error_pages_fail_with_line() {
        let root = build_app("runtime");
        fs::write(root.join("r.asp"), "<% y = 1 / 0 %>").unwrap();
        let app = AppRoot::new(&root);
        let err = render_page(&app, "r.asp", HashMap::new()).unwrap_err();
        assert!(matches!(err, AspError::Runtime(d) if d.message.contains("division by zero")));
    }

    #[test]
    fn asp_expr_rewrites_output_expression() {
        let expr = parse_expr_standalone("\"v=\" & (2 + 2)", 1).unwrap();
        let mut env = ExecEnv::new();
        match asp_vbscript::exec_block(
            &[asp_vbscript::Stmt::ResponseCall {
                verb: "write".to_string(),
                arg: Some(expr),
            }],
            &mut env,
        ) {
            Ok(()) => assert_eq!(env.response.body(), "v=4"),
            Err(e) => panic!("exec failed: {e}"),
        }
    }
}
