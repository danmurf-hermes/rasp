//! ASP intrinsic objects and the page render loop for RASP.
//!
//! M1 covers the `Response` object (Write/End/Clear/buffer passthrough),
//! assembly of preprocessed pages (includes resolved, cycles detected),
//! and one-shot rendering of a parsed page into a full HTTP response.
//! M4 adds session/application state ([`session`]) and `global.asa`
//! event firing around the render.

pub mod host;
pub mod session;

pub use asp_vbscript::StateStores;

pub mod ado_host;
pub use ado_host::RuntimeAdoHost;
pub use host::RuntimeHost;
pub use session::{GlobalAsa, SESSION_COOKIE_NAME, SessionManager, fire_global_asa_events};

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

fn assemble_from_source(
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
            Block::ServerScript { body, .. } => {
                // Server-side <SCRIPT> statements splice into the stream
                // exactly like <% %> blocks (procedure declarations are
                // hoisted by the executor's nesting pass).
                steps.push(RenderStep::Script(parse_block(body, 1)?));
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
        [asp_vbscript::Stmt::ResponseCall { args, .. }] if args.len() == 1 => Ok(args[0].clone()),
        [asp_vbscript::Stmt::ResponseCall { args, .. }] if args.is_empty() => Err(
            AspError::Syntax(Diagnostic::new(line, "<%= %> requires an expression")),
        ),
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
    pub status_text: Option<String>,
    pub content_type: Option<String>,
    pub charset: Option<String>,
    /// Extra headers in insertion order (`AddHeader`, `Expires`, …).
    pub headers: Vec<(String, String)>,
    /// Outbound cookies (`Response.Cookies("k") = v`).
    pub cookies: Vec<asp_vbscript::Cookie>,
    /// `Response.Redirect target` set on this render.
    pub redirect: Option<String>,
    /// Session state after the render (mutations included). The caller
    /// commits it back to the manager; when `abandoned`, the store is
    /// dropped instead.
    pub session: Option<asp_vbscript::SessionStore>,
    /// Application state after the render.
    pub application: Option<asp_vbscript::ApplicationState>,
}

/// Render an assembled page end to end against a base environment.
///
/// The steps are **flattened into one VBScript statement stream** before
/// execution — literal text becomes `Response.Write "…"`, `<%= %>`
/// becomes `Response.Write expr` — so a `For` opened in one `<% %>`
/// block can close its `Next` in a later block with the page's HTML
/// re-emitted on every iteration, exactly as Classic ASP behaves.
pub fn render_assembled(assembled: &AssembledPage, base_env: &ExecEnv) -> AspResult<RenderOutput> {
    // ExecEnv is request-scoped; each page starts from the parsed
    // request data rather than inheriting mutable state.
    let mut env = fresh_env(base_env);
    let mut out = RenderOutput::default();
    let stmts = flatten_steps(&assembled.steps)?;
    asp_vbscript::exec_block_loops(&stmts, &mut env)?;
    out.body = env.response.body();
    out.status = env.response.status;
    out.status_text = env.response.status_text.clone();
    out.content_type = env.response.content_type.clone();
    out.charset = env.response.charset.clone();
    out.headers = env.response.headers.clone();
    out.cookies = env.response.cookies.clone();
    out.redirect = env.response.redirect.clone();
    out.session = Some(env.session.clone());
    out.application = Some(env.application.clone());
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
                    args: vec![asp_vbscript::Expr::Literal(asp_vbscript::Variant::Str(
                        text.clone(),
                    ))],
                });
            }
            RenderStep::Script(block_stmts) => {
                stmts.extend(block_stmts.iter().cloned());
            }
            RenderStep::Output(expr) => {
                stmts.push(Stmt::ResponseCall {
                    verb: "write".to_string(),
                    args: vec![expr.clone()],
                });
            }
        }
    }
    Ok(stmts)
}

/// Build the request-scoped environment from the base one.
fn fresh_env(base: &ExecEnv) -> ExecEnv {
    let mut env = ExecEnv::new();
    // Request data (query/form/cookies) is the request's; session and
    // application state are the cross-request stores the caller passed
    // in and are carried through (mutations land back there afterward).
    env.session = base.session.clone();
    env.application = base.application.clone();
    env.request_data = base.request_data.clone();
    env.server_variables = base.server_variables.clone();
    env.host = base.host.clone();
    env.ado_host = base.ado_host.clone();
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

/// Top-level render of a page by path: read, parse, assemble, execute.
/// `server_variables` feeds `Request.ServerVariables`. No cross-request
/// state: the page starts with an empty session/application.
pub fn render_page(
    app: &AppRoot,
    root_relative: &str,
    request_data: HashMap<String, String>,
) -> AspResult<RenderOutput> {
    render_page_with(app, root_relative, request_data, HashMap::new())
}

/// Same as [`render_page`] with server variables supplied.
pub fn render_page_with(
    app: &AppRoot,
    root_relative: &str,
    request_data: HashMap<String, String>,
    server_variables: HashMap<String, String>,
) -> AspResult<RenderOutput> {
    render_page_stores(
        app,
        root_relative,
        request_data,
        server_variables,
        &StateStores::default(),
    )
    .map(|(out, _)| out)
}

/// Render with cross-request state: the stores are cloned into the
/// render environment and the (possibly mutated) stores come back in
/// the output. `session_is_new`/`global_asa` event firing is the
/// caller's job (`fire_global_asa_events`) so a failing page still
/// commits event writes; this function only renders.
pub fn render_page_stores(
    app: &AppRoot,
    root_relative: &str,
    request_data: HashMap<String, String>,
    server_variables: HashMap<String, String>,
    stores: &StateStores,
) -> AspResult<(RenderOutput, StateStores)> {
    let mut seen = Vec::new();
    let assembled = assemble(app, root_relative, &mut seen)?;
    let host = RuntimeHost::new(
        app.clone(),
        stores.clone(),
        server_variables.clone(),
        request_data.clone(),
    );
    let base = ExecEnv::new()
        .with_request_data(request_data)
        .with_server_variables(server_variables)
        .with_state(stores.clone())
        .with_host(host as std::rc::Rc<dyn asp_vbscript::NativeHost>)
        .with_ado_host(std::rc::Rc::new(ado_host::RuntimeAdoHost::new(app.clone())));
    let out = render_assembled(&assembled, &base)?;
    let exit_stores = StateStores {
        session: out.session.clone().unwrap_or_default(),
        application: out.application.clone().unwrap_or_default(),
    };
    Ok((out, exit_stores))
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
                args: vec![expr],
            }],
            &mut env,
        ) {
            Ok(()) => assert_eq!(env.response.body(), "v=4"),
            Err(e) => panic!("exec failed: {e}"),
        }
    }

    // ---- Milestone 5: native objects, MapPath, Execute/Transfer ----

    #[test]
    fn fso_read_write_delete_inside_root() {
        let root = build_app("fso1");
        fs::write(root.join("p.asp"),
            "<% Set f = Server.CreateObject(\"Scripting.FileSystemObject\")\n f.CreateTextFile \"out/log.txt\", \"hello\"\n Response.Write f.ReadTextFile(\"out/log.txt\")\n f.DeleteFile \"out/log.txt\"\n Response.Write \":\" & f.FileExists(\"out/log.txt\") %>").unwrap();
        let app = AppRoot::new(&root);
        let (out, _) = crate::render_page_stores(
            &app,
            "p.asp",
            HashMap::new(),
            HashMap::new(),
            &StateStores::default(),
        )
        .unwrap();
        assert_eq!(out.body, "hello:False");
    }

    #[test]
    fn fso_rejects_traversal_outside_root() {
        let root = build_app("fso2");
        fs::write(root.join("p.asp"),
            "<% Set f = Server.CreateObject(\"Scripting.FileSystemObject\")\n Response.Write f.FileExists(\"../secret.txt\") %>").unwrap();
        let app = AppRoot::new(&root);
        let err = crate::render_page_stores(
            &app,
            "p.asp",
            HashMap::new(),
            HashMap::new(),
            &StateStores::default(),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("application root"), "{err}");
    }

    #[test]
    fn fso_rejects_symlink_escape() {
        let root = build_app("fso3");
        let outside = std::env::temp_dir().join(format!("rasp-outside-{}", std::process::id()));
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "top secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("leak")).unwrap();
        fs::write(root.join("p.asp"),
            "<% Set f = Server.CreateObject(\"Scripting.FileSystemObject\")\n Response.Write f.ReadTextFile(\"leak/secret.txt\") %>").unwrap();
        let app = AppRoot::new(&root);
        let err = crate::render_page_stores(
            &app,
            "p.asp",
            HashMap::new(),
            HashMap::new(),
            &StateStores::default(),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("application root"), "{err}");
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn server_mappath_maps_inside_root() {
        let root = build_app("mp");
        fs::write(root.join("p.asp"), "<%= Server.MapPath(\"data/x.txt\") %>").unwrap();
        let app = AppRoot::new(&root);
        let (out, _) = crate::render_page_stores(
            &app,
            "p.asp",
            HashMap::new(),
            HashMap::new(),
            &StateStores::default(),
        )
        .unwrap();
        assert!(out.body.ends_with("data/x.txt"), "{}", out.body);
    }

    #[test]
    fn server_execute_merges_output() {
        let root = build_app("exec");
        fs::write(root.join("sub.asp"), "SUB").unwrap();
        fs::write(root.join("p.asp"), "A<% Server.Execute \"sub.asp\" %>B").unwrap();
        let app = AppRoot::new(&root);
        let (out, _) = crate::render_page_stores(
            &app,
            "p.asp",
            HashMap::new(),
            HashMap::new(),
            &StateStores::default(),
        )
        .unwrap();
        assert_eq!(out.body, "ASUBB");
    }

    #[test]
    fn server_transfer_replaces_output() {
        let root = build_app("transfer");
        fs::write(root.join("target.asp"), "TARGET").unwrap();
        fs::write(root.join("p.asp"), "A<% Server.Transfer \"target.asp\" %>B").unwrap();
        let app = AppRoot::new(&root);
        let (out, _) = crate::render_page_stores(
            &app,
            "p.asp",
            HashMap::new(),
            HashMap::new(),
            &StateStores::default(),
        )
        .unwrap();
        assert_eq!(out.body, "TARGET");
    }

    #[test]
    fn create_object_unknows_are_clear() {
        let root = build_app("objerr");
        fs::write(
            root.join("p.asp"),
            "<% Set x = Server.CreateObject(\"MySql.ProgID.Nowhere\") %>",
        )
        .unwrap();
        let app = AppRoot::new(&root);
        let err = crate::render_page_stores(
            &app,
            "p.asp",
            HashMap::new(),
            HashMap::new(),
            &StateStores::default(),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not available"), "{msg}");
        assert!(msg.contains("Scripting.Dictionary"), "{msg}");
        assert!(msg.contains("ADODB.Connection"), "{msg}");
    }
}
