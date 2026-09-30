//! HTTP server and request-to-response integration for RASP.
//!
//! M3 wraps `tiny_http` with the full ASP request pipeline: parse the
//! request URL, decode query/form/cookies into `Request` data, populate
//! `Request.ServerVariables` from the real request, render the page
//! through `asp-runtime`, and emit status + headers + cookies + body.
//! Requests enforce a body-size limit and a URL-length limit. Runaway
//! pages are bounded by the interpreter's loop/recursion caps. Static
//! assets are not served — only `.asp` pages.

use asp_core::AppRoot;
use asp_runtime::{
    GlobalAsa, RenderOutput, SESSION_COOKIE_NAME, SessionManager, StateStores, build_request_data,
    fire_global_asa_events, render_page_stores, session::session_cookie_from_header,
};
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};

/// Maximum accepted request body (form posts), bytes.
const MAX_BODY_BYTES: usize = 1_048_576;

/// Maximum accepted request-URL length, bytes.
const MAX_URL_LENGTH: usize = 2048;

/// Server configuration for one application host.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Hostname or IP to bind (e.g. `127.0.0.1`).
    pub host: String,
    /// TCP port to listen on.
    pub port: u16,
    /// Default document when a path resolves to a directory.
    pub default_document: String,
    /// Development mode: error pages carry full diagnostics.
    /// Production returns a generic 500 without internal detail.
    pub dev_errors: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8174,
            default_document: "default.asp".to_string(),
            dev_errors: true,
        }
    }
}

/// One HTTP request reduced to what ASP pages can see.
#[derive(Debug)]
pub struct HttpRequest {
    pub method: String,
    /// Decoded URL path (no query string).
    pub path: String,
    pub query: String,
    pub form: String,
    pub cookies: String,
    /// Raw request headers (lower-cased names) for ServerVariables.
    pub headers: Vec<(String, String)>,
}

impl HttpRequest {
    /// Combine the request's collections into the runtime's key map.
    pub fn request_data(&self) -> HashMap<String, String> {
        build_request_data(&self.query, &self.form, &self.cookies)
    }

    /// `Request.ServerVariables` values. Keys are stored lowercase so
    /// the evaluator's case-insensitive lookups find them (IIS names
    /// like `REQUEST_METHOD` are accepted in any case from pages).
    pub fn server_variables(&self) -> HashMap<String, String> {
        let mut vars = HashMap::new();
        vars.insert("request_method".to_string(), self.method.clone());
        vars.insert("script_name".to_string(), self.path.clone());
        vars.insert("query_string".to_string(), self.query.clone());
        vars.insert("http_cookie".to_string(), self.cookies.clone());
        vars.insert("http_method".to_string(), self.method.clone());
        for (name, value) in &self.headers {
            let lower = format!("http_{}", name.to_ascii_lowercase().replace('-', "_"));
            vars.insert(lower, value.clone());
        }
        vars
    }
}

/// The outcome of attempting to serve one request.
#[derive(Debug, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    pub content_type: String,
    /// Extra headers in emission order (Content-Type excluded).
    pub headers: Vec<(String, String)>,
    /// Outbound Set-Cookie headers.
    pub set_cookies: Vec<String>,
}

impl HttpResponse {
    fn new(status: u16, body: String, content_type: String) -> Self {
        Self {
            status,
            body,
            content_type,
            headers: Vec::new(),
            set_cookies: Vec::new(),
        }
    }
}

/// Map a URL path to a page inside `app`, enforcing the default
/// document and the `.asp`-only rule.
pub fn resolve_request_path(app: &AppRoot, config: &ServerConfig, path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    let mut root_relative = if trimmed.is_empty() {
        config.default_document.clone()
    } else {
        trimmed.to_string()
    };
    // Directory URLs fall back to the default document, IIS-style.
    if app.path().join(&root_relative).is_dir() {
        if root_relative.ends_with('/') {
            root_relative.push_str(&config.default_document);
        } else {
            root_relative.push('/');
            root_relative.push_str(&config.default_document);
        }
    }
    root_relative
}

/// Render one request into an HTTP response without a server socket
/// (the integration point used by tests and the `run` CLI). Error
/// bodies carry full diagnostics (development mode).
pub fn handle_request(app: &AppRoot, root_relative: &str, request: &HttpRequest) -> HttpResponse {
    handle_request_for_server(app, root_relative, request, true)
}

/// Render one request for the server. With `dev_errors` off, render
/// failures return a generic 500 with no internal detail (production).
///
/// Runaway pages are stopped by the interpreter's own caps
/// (`MAX_LOOP_ITERATIONS`, `MAX_CALL_DEPTH`) — they surface as 500s,
/// not hangs; a wall-clock deadline cannot interrupt an in-flight
/// tree-walk anyway. Cross-request state (sessions, application,
/// `global.asa`) lives in a [`ServerHolder`]; this entry point is the
/// stateless compatibility shim over a throwaway holder.
pub fn handle_request_for_server(
    app: &AppRoot,
    root_relative: &str,
    request: &HttpRequest,
    dev_errors: bool,
) -> HttpResponse {
    let holder = ServerHolder::for_app_root(app).expect("holder builds when the app root exists");
    handle_request_with_state(&holder, app, root_relative, request, dev_errors).0
}

/// Cross-request server state: the session manager (session store +
/// application state) and the parsed `global.asa` (when the app root
/// ships one). Shared by all requests of one `serve` process.
pub struct ServerHolder {
    manager: Mutex<SessionManager>,
    global_asa: Option<GlobalAsa>,
}

impl ServerHolder {
    /// Build from the app root. A `global.asa` that fails to parse is a
    /// startup error (IIS refuses to start a broken application too).
    pub fn for_app_root(app: &AppRoot) -> asp_core::AspResult<Self> {
        let global_asa = match app.read_page("global.asa") {
            Ok(source) => Some(GlobalAsa::parse(&source)?),
            Err(asp_core::AspError::PageNotFound(_)) => None,
            Err(err) => return Err(err),
        };
        Ok(Self {
            manager: Mutex::new(SessionManager::new()),
            global_asa,
        })
    }
}

/// Render one request THROUGH the cross-request state: resolve the
/// session from the request's cookies, fire `global.asa` start events,
/// render, and commit the mutated stores back. Returns the response
/// plus a flag telling whether the session is new (for tests/drivers).
pub fn handle_request_with_state(
    holder: &ServerHolder,
    app: &AppRoot,
    root_relative: &str,
    request: &HttpRequest,
    dev_errors: bool,
) -> (HttpResponse, bool) {
    // URL-length guard mirrors the server's pre-render limit.
    if request.path.len() + request.query.len() > MAX_URL_LENGTH {
        return (error_page(414, "URI Too Long", None), false);
    }
    let cookie_value = session_cookie_from_header(&request.cookies);
    let (store, session_cookie, is_new, application) = {
        let mut manager = lock_manager(holder);
        manager.resolve_request_state(cookie_value.as_deref())
    };
    let mut stores = StateStores {
        session: store,
        application,
    };
    if let Some(asa) = &holder.global_asa
        && let Err(err) = fire_global_asa_events(asa, is_new, &mut stores)
    {
        // Event writes so far are committed even on event failure.
        commit(holder, &session_cookie, &stores);
        return (server_error_response(err, dev_errors), is_new);
    }
    let mut vars = request.server_variables();
    // IIS reports SCRIPT_NAME as the executing script (after default
    // document resolution), not the raw URL path.
    vars.insert("script_name".to_string(), format!("/{root_relative}"));
    match render_page_stores(app, root_relative, request.request_data(), vars, &stores) {
        Ok((out, exit_stores)) => {
            commit(holder, &session_cookie, &exit_stores);
            let mut response = to_response(out);
            if is_new {
                response
                    .set_cookies
                    .push(format!("{SESSION_COOKIE_NAME}={session_cookie}"));
            }
            (response, is_new)
        }
        Err(asp_core::AspError::PageNotFound(_)) => {
            commit(holder, &session_cookie, &stores);
            (error_page(404, "Not Found", None), is_new)
        }
        Err(err) => {
            // State writes survive a page error (session values set
            // before the error stay set), so commit what we have.
            commit(holder, &session_cookie, &stores);
            (server_error_response(err, dev_errors), is_new)
        }
    }
}

/// Commit the stores back to the manager under its lock.
fn commit(holder: &ServerHolder, session_cookie: &str, stores: &StateStores) {
    let mut manager = lock_manager(holder);
    manager.commit_request_state(
        session_cookie,
        stores.session.clone(),
        stores.application.clone(),
    );
}

/// Lock the manager, recovering from poisoning (a request that panicked
/// mid-render must not brick every later request).
fn lock_manager(holder: &ServerHolder) -> std::sync::MutexGuard<'_, SessionManager> {
    match holder.manager.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Produce the 500 response for a render failure (dev vs production).
fn server_error_response(err: asp_core::AspError, dev_errors: bool) -> HttpResponse {
    if dev_errors {
        error_page(500, "Server error", Some(err.to_string()))
    } else {
        error_page(500, "Server error", None)
    }
}

fn to_response(out: RenderOutput) -> HttpResponse {
    // Redirects: Classic ASP sends a 302 with a Location header and an
    // object-moved body; the buffered body is discarded.
    if let Some(target) = out.redirect {
        let mut response = HttpResponse::new(
            302,
            format!(
                "<html><body><p>Object moved</p><p><a href=\"{target}\">here</a>.</p></body></html>"
            ),
            "text/html".to_string(),
        );
        response.headers.push(("Location".to_string(), target));
        return response;
    }
    let mut response = HttpResponse::new(
        out.status.unwrap_or(200),
        out.body,
        out.content_type
            .clone()
            .unwrap_or_else(|| "text/html".to_string()),
    );
    if let Some(charset) = out.charset {
        response.content_type = format!("{}; charset={}", response.content_type, charset);
    }
    if let Some(text) = out.status_text {
        response
            .headers
            .push(("X-Rasp-Status-Text".to_string(), text));
    }
    response.headers.extend(out.headers);
    for cookie in &out.cookies {
        response
            .set_cookies
            .push(format!("{}={}", cookie.name, cookie.value));
    }
    response
}

/// Error body: a heading plus optional dev-only diagnostic detail.
fn error_page(status: u16, heading: &str, detail: Option<String>) -> HttpResponse {
    let body = match detail {
        Some(message) => format!(
            "<html><body><h1>{heading}</h1><pre>{}</pre></body></html>\n",
            html_escape(&message)
        ),
        None => format!("<html><body><h1>{heading}</h1></body></html>\n"),
    };
    HttpResponse::new(status, body, "text/html".to_string())
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Run the blocking HTTP server loop until the process is stopped.
/// Returns an error only when the listener or `global.asa` cannot be
/// initialised.
pub fn serve(app: &AppRoot, config: &ServerConfig) -> asp_core::AspResult<()> {
    let holder = Arc::new(ServerHolder::for_app_root(app)?);
    let addr = format!("{}:{}", config.host, config.port);
    let server = tiny_http::Server::http(&addr)
        .map_err(|e| asp_core::AspError::Io(format!("cannot bind {addr}: {e}")))?;
    let server = Arc::new(server);
    loop {
        let mut request = match server.recv() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("rasp: recv failed: {e}");
                continue;
            }
        };
        let config = config.clone();
        let method = request.method().to_string();
        let url = request.url().to_string();
        if url.len() > MAX_URL_LENGTH {
            let _ = request.respond(tiny_response(error_page(414, "URI Too Long", None)));
            continue;
        }
        let (path, query) = split_url(&url);
        let headers = request_headers(&request);
        let cookies = header_value(&headers, "cookie");
        let form_result = read_form(&mut request);
        let form = match form_result {
            Ok(f) => f,
            Err(message) => {
                let _ = request.respond(tiny_response(error_page(413, &message, None)));
                continue;
            }
        };
        let http_request = HttpRequest {
            method,
            path,
            query,
            form,
            cookies,
            headers,
        };
        let root_relative = resolve_request_path(app, &config, &http_request.path);
        let (response, _) = handle_request_with_state(
            &holder,
            app,
            &root_relative,
            &http_request,
            config.dev_errors,
        );
        let _ = request.respond(tiny_response(response));
    }
}

fn tiny_response(response: HttpResponse) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let mut out = tiny_http::Response::from_data(response.body.into_bytes())
        .with_status_code(response.status);
    let content_type =
        tiny_http::Header::from_bytes(&b"Content-Type"[..], response.content_type.as_bytes())
            .unwrap_or_else(|_| {
                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html"[..]).unwrap()
            });
    out.add_header(content_type);
    for (name, value) in &response.headers {
        if let Ok(h) = tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            out.add_header(h);
        }
    }
    for cookie in &response.set_cookies {
        if let Ok(h) = tiny_http::Header::from_bytes(&b"Set-Cookie"[..], cookie.as_bytes()) {
            out.add_header(h);
        }
    }
    out
}

fn split_url(url: &str) -> (String, String) {
    match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.to_string(), String::new()),
    }
}

fn request_headers(request: &tiny_http::Request) -> Vec<(String, String)> {
    request
        .headers()
        .iter()
        .map(|h| {
            (
                h.field.as_str().as_str().to_ascii_lowercase(),
                h.value.as_str().to_string(),
            )
        })
        .collect()
}

fn header_value(headers: &[(String, String)], name: &str) -> String {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

/// Read the request body as a form string (`k=v&k2=v2`), up to the
/// body limit; oversize bodies are a 413.
fn read_form(request: &mut tiny_http::Request) -> Result<String, String> {
    let length = request.body_length().unwrap_or(0);
    if length > MAX_BODY_BYTES {
        return Err("request body too large".to_string());
    }
    let mut body = String::new();
    if length > 0 {
        let _ = request
            .as_reader()
            .take(length as u64)
            .read_to_string(&mut body)
            .map_err(|e| format!("cannot read request body: {e}"))?;
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn build_app(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir()
            .join("rasp-http-tests")
            .join(format!("{tag}-{}", std::process::id()));
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("hello.asp"), "hello <%= 6 * 7 %>").unwrap();
        fs::write(root.join("sub").join("default.asp"), "sub default").unwrap();
        root
    }

    fn request(path: &str, query: &str) -> HttpRequest {
        HttpRequest {
            method: "GET".to_string(),
            path: path.to_string(),
            query: query.to_string(),
            form: String::new(),
            cookies: String::new(),
            headers: Vec::new(),
        }
    }

    #[test]
    fn renders_page_over_handle_request() {
        let app = AppRoot::new(build_app("handle"));
        let config = ServerConfig::default();
        let rel = resolve_request_path(&app, &config, "/hello.asp");
        let response = handle_request(&app, &rel, &request("/hello.asp", ""));
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "hello 42");
        assert_eq!(response.content_type, "text/html");
    }

    #[test]
    fn root_maps_to_default_document() {
        let app = AppRoot::new(build_app("root"));
        let config = ServerConfig::default();
        let rel = resolve_request_path(&app, &config, "/");
        assert_eq!(rel, "default.asp");
        let rel = resolve_request_path(&app, &config, "/sub");
        assert_eq!(rel, "sub/default.asp");
    }

    #[test]
    fn missing_pages_are_404() {
        let app = AppRoot::new(build_app("404"));
        let config = ServerConfig::default();
        let rel = resolve_request_path(&app, &config, "/nope.asp");
        let response = handle_request(&app, &rel, &request("/nope.asp", ""));
        assert_eq!(response.status, 404);
    }

    #[test]
    fn page_errors_are_500_with_message() {
        let root = build_app("500");
        fs::write(root.join("bad.asp"), "<% z = 1 / 0 %>").unwrap();
        let app = AppRoot::new(&root);
        let config = ServerConfig::default();
        let rel = resolve_request_path(&app, &config, "/bad.asp");
        let response = handle_request(&app, &rel, &request("/bad.asp", ""));
        assert_eq!(response.status, 500);
        assert!(response.body.contains("division by zero"));
        assert!(!response.body.contains("<%"));
    }

    #[test]
    fn querystring_flows_through() {
        let root = build_app("query");
        fs::write(root.join("q.asp"), "<%= Request.QueryString(\"who\") %>").unwrap();
        let app = AppRoot::new(&root);
        let config = ServerConfig::default();
        let rel = resolve_request_path(&app, &config, "/q.asp");
        let response = handle_request(&app, &rel, &request("/q.asp", "who=Rasp"));
        assert_eq!(response.body, "Rasp");
    }

    #[test]
    fn url_decoder_round_trip() {
        assert_eq!(asp_runtime::url_decode("a+b%21"), "a b!");
    }

    #[test]
    fn form_data_flows_through() {
        let root = build_app("form");
        fs::write(
            root.join("f.asp"),
            "<%= Request.Form(\"user\") & \"/\" & Request.QueryString(\"x\") %>",
        )
        .unwrap();
        let app = AppRoot::new(&root);
        let config = ServerConfig::default();
        let rel = resolve_request_path(&app, &config, "/f.asp");
        let mut req = request("/f.asp", "x=9");
        req.method = "POST".to_string();
        req.form = "user=dan".to_string();
        let response = handle_request(&app, &rel, &req);
        assert_eq!(response.body, "dan/9");
    }

    #[test]
    fn cookies_flows_through() {
        let root = build_app("cookie");
        fs::write(root.join("c.asp"), "<%= Request.Cookies(\"sid\") %>").unwrap();
        let app = AppRoot::new(&root);
        let config = ServerConfig::default();
        let rel = resolve_request_path(&app, &config, "/c.asp");
        let mut req = request("/c.asp", "");
        req.cookies = "sid=abc123".to_string();
        let response = handle_request(&app, &rel, &req);
        assert_eq!(response.body, "abc123");
    }

    #[test]
    fn server_variables_expose_method_and_path() {
        let root = build_app("sv");
        fs::write(
            root.join("s.asp"),
            "<%= Request.ServerVariables(\"REQUEST_METHOD\") & \":\" & Request.ServerVariables(\"SCRIPT_NAME\") %>",
        )
        .unwrap();
        let app = AppRoot::new(&root);
        let config = ServerConfig::default();
        let rel = resolve_request_path(&app, &config, "/s.asp");
        let mut req = request("/s.asp", "");
        req.method = "PUT".to_string();
        let response = handle_request(&app, &rel, &req);
        assert_eq!(response.body, "PUT:/s.asp");
    }

    #[test]
    fn redirect_sets_302_and_location() {
        let root = build_app("redir");
        fs::write(
            root.join("r.asp"),
            "<% Response.Redirect \"/target.asp\" %>never",
        )
        .unwrap();
        let app = AppRoot::new(&root);
        let config = ServerConfig::default();
        let rel = resolve_request_path(&app, &config, "/r.asp");
        let response = handle_request(&app, &rel, &request("/r.asp", ""));
        assert_eq!(response.status, 302);
        assert!(
            response
                .headers
                .contains(&("Location".to_string(), "/target.asp".to_string()))
        );
        assert!(!response.body.contains("never"));
    }

    #[test]
    fn status_and_content_type_and_headers() {
        let root = build_app("resp");
        fs::write(
            root.join("p.asp"),
            "<% Response.Status = \"404 Not Found\"\nResponse.ContentType = \"text/plain\"\nResponse.AddHeader \"X-Flag\", \"on\"\nResponse.Charset = \"utf-8\"\n%>body",
        )
        .unwrap();
        let app = AppRoot::new(&root);
        let config = ServerConfig::default();
        let rel = resolve_request_path(&app, &config, "/p.asp");
        let response = handle_request(&app, &rel, &request("/p.asp", ""));
        assert_eq!(response.status, 404);
        assert_eq!(response.content_type, "text/plain; charset=utf-8");
        assert!(
            response
                .headers
                .contains(&("X-Flag".to_string(), "on".to_string()))
        );
        assert_eq!(response.body, "body");
    }

    #[test]
    fn cookies_are_set_on_response() {
        let root = build_app("set-cookie");
        fs::write(
            root.join("sc.asp"),
            "<% Response.Cookies(\"pref\") = \"dark\" %><%= Request.QueryString(\"x\") %>",
        )
        .unwrap();
        let app = AppRoot::new(&root);
        let config = ServerConfig::default();
        let rel = resolve_request_path(&app, &config, "/sc.asp");
        let response = handle_request(&app, &rel, &request("/sc.asp", "x=1"));
        // The page's own cookie first, then the session cookie every new
        // visitor gets (IIS behaviour).
        assert_eq!(
            response.set_cookies,
            vec![
                "pref=dark".to_string(),
                "ASPSESSIONID=rasp00000001".to_string()
            ]
        );
        assert_eq!(response.body, "1");
    }

    #[test]
    fn oversize_url_is_414() {
        let app = AppRoot::new(build_app("414"));
        let long = format!("/hello.asp?{}", "a=1&".repeat(2000));
        let config = ServerConfig::default();
        let rel = resolve_request_path(&app, &config, &long);
        let response = handle_request(&app, &rel, &request(&long, ""));
        assert_eq!(response.status, 414);
    }

    #[test]
    fn production_errors_hide_detail() {
        let root = build_app("prod");
        fs::write(root.join("x.asp"), "<%= 1 / 0 %>").unwrap();
        let app = AppRoot::new(&root);
        let config = ServerConfig::default();
        let rel = resolve_request_path(&app, &config, "/x.asp");
        let response = handle_request_for_server(&app, &rel, &request("/x.asp", ""), false);
        assert_eq!(response.status, 500);
        assert!(!response.body.contains("division by zero"));
        assert!(response.body.contains("Server error"));
        // Dev mode shows the diagnostic instead.
        let dev = handle_request_for_server(&app, &rel, &request("/x.asp", ""), true);
        assert!(dev.body.contains("division by zero"));
    }

    // ---- Milestone 4: cross-request state ----

    /// One holder over one app root, and a request with optional cookie.
    fn stateful(
        holder: &ServerHolder,
        app: &AppRoot,
        path: &str,
        query: &str,
        cookies: &str,
        method: &str,
        form: &str,
    ) -> HttpResponse {
        let config = ServerConfig::default();
        let rel = resolve_request_path(app, &config, path);
        let req = HttpRequest {
            method: method.to_string(),
            path: path.to_string(),
            query: query.to_string(),
            form: form.to_string(),
            cookies: cookies.to_string(),
            headers: Vec::new(),
        };
        handle_request_with_state(holder, app, &rel, &req, true).0
    }

    fn set_cookie_value(response: &HttpResponse) -> Option<String> {
        response
            .set_cookies
            .iter()
            .find(|c| c.to_ascii_lowercase().starts_with("aspsessionid"))
            .map(|c| c.split_once('=').unwrap().1.to_string())
    }

    #[test]
    fn session_persists_across_requests_with_cookie() {
        let root = build_app("sess1");
        fs::write(root.join("set.asp"), "<% Session(\"user\") = \"dan\" %>set").unwrap();
        fs::write(root.join("get.asp"), "<%= Session(\"user\") %>").unwrap();
        let app = AppRoot::new(&root);
        let holder = ServerHolder::for_app_root(&app).unwrap();
        let first = stateful(&holder, &app, "/set.asp", "", "", "GET", "");
        let cookie = set_cookie_value(&first).expect("session cookie on first visit");
        let second = stateful(
            &holder,
            &app,
            "/get.asp",
            "",
            &format!("ASPSESSIONID={cookie}"),
            "GET",
            "",
        );
        assert_eq!(second.body, "dan");
    }

    #[test]
    fn session_needs_the_cookie_otherwise_new() {
        let root = build_app("sess2");
        fs::write(root.join("set.asp"), "<% Session(\"u\") = \"dan\" %>set").unwrap();
        fs::write(root.join("get.asp"), "<%= Session(\"u\") %>").unwrap();
        let app = AppRoot::new(&root);
        let holder = ServerHolder::for_app_root(&app).unwrap();
        let first = stateful(&holder, &app, "/set.asp", "", "", "GET", "");
        assert_eq!(first.body, "set");
        // Without sending the cookie back, a NEW session reads empty.
        let second = stateful(&holder, &app, "/get.asp", "", "", "GET", "");
        assert_eq!(second.body, "");
    }

    #[test]
    fn sessions_are_isolated_per_visitor() {
        let root = build_app("sess3");
        fs::write(
            root.join("p.asp"),
            "<% If Session(\"who\") = Empty Then\nSession(\"who\") = Request.QueryString(\"as\")\nEnd If\n%><%= Session(\"who\") %>",
        )
        .unwrap();
        let app = AppRoot::new(&root);
        let holder = ServerHolder::for_app_root(&app).unwrap();
        let a = set_cookie_value(&stateful(
            &holder, &app, "/p.asp", "as=alice", "", "GET", "",
        ))
        .unwrap();
        let b =
            set_cookie_value(&stateful(&holder, &app, "/p.asp", "as=bob", "", "GET", "")).unwrap();
        assert_ne!(a, b);
        let a2 = stateful(
            &holder,
            &app,
            "/p.asp",
            "as=zoe",
            &format!("ASPSESSIONID={a}"),
            "GET",
            "",
        );
        let b2 = stateful(
            &holder,
            &app,
            "/p.asp",
            "as=zoe",
            &format!("ASPSESSIONID={b}"),
            "GET",
            "",
        );
        assert_eq!(a2.body, "alice");
        assert_eq!(b2.body, "bob");
    }

    #[test]
    fn session_abandon_drops_values_for_next_request() {
        let root = build_app("sess4");
        fs::write(
            root.join("p.asp"),
            "<% Session(\"n\") = Session(\"n\") + 1 %><%= Session(\"n\") %>",
        )
        .unwrap();
        fs::write(root.join("bye.asp"), "<% Session.Abandon %>bye").unwrap();
        let app = AppRoot::new(&root);
        let holder = ServerHolder::for_app_root(&app).unwrap();
        let first = stateful(&holder, &app, "/p.asp", "", "", "GET", "");
        let cookie = set_cookie_value(&first).unwrap();
        assert_eq!(first.body, "1");
        let second = stateful(
            &holder,
            &app,
            "/p.asp",
            "",
            &format!("ASPSESSIONID={cookie}"),
            "GET",
            "",
        );
        assert_eq!(second.body, "2");
        let bye = stateful(
            &holder,
            &app,
            "/bye.asp",
            "",
            &format!("ASPSESSIONID={cookie}"),
            "GET",
            "",
        );
        assert_eq!(bye.body, "bye");
        let after = stateful(
            &holder,
            &app,
            "/p.asp",
            "",
            &format!("ASPSESSIONID={cookie}"),
            "GET",
            "",
        );
        assert_eq!(after.body, "1");
    }

    #[test]
    fn application_shared_across_sessions() {
        let root = build_app("app1");
        fs::write(
            root.join("hit.asp"),
            "<% Application.Lock\nApplication(\"hits\") = Application(\"hits\") + 1\nApplication.UnLock\n%><%= Application(\"hits\") %>",
        )
        .unwrap();
        let app = AppRoot::new(&root);
        let holder = ServerHolder::for_app_root(&app).unwrap();
        let a = set_cookie_value(&stateful(&holder, &app, "/hit.asp", "", "", "GET", "")).unwrap();
        let b = set_cookie_value(&stateful(&holder, &app, "/hit.asp", "", "", "GET", "")).unwrap();
        assert_ne!(a, b);
        let a2 = stateful(
            &holder,
            &app,
            "/hit.asp",
            "",
            &format!("ASPSESSIONID={a}"),
            "GET",
            "",
        );
        let b2 = stateful(
            &holder,
            &app,
            "/hit.asp",
            "",
            &format!("ASPSESSIONID={b}"),
            "GET",
            "",
        );
        // Two different sessions incremented the same shared counter;
        // the first request was hit #1, then each follow-up adds one.
        assert_eq!(a2.body, "3");
        assert_eq!(b2.body, "4");
    }

    #[test]
    fn global_asa_events_fire_once_per_process_and_per_session() {
        let root = build_app("asa");
        fs::write(
            root.join("global.asa"),
            "<SCRIPT RUNAT=Server LANGUAGE=VBScript>\nSub Application_OnStart\nApplication(\"boot\") = \"on\"\nEnd Sub\n</SCRIPT>\n<SCRIPT RUNAT=Server LANGUAGE=VBScript>\nSub Session_OnStart\nSession(\"visits\") = 1\nEnd Sub\n</SCRIPT>",
        )
        .unwrap();
        fs::write(
            root.join("p.asp"),
            "<%= Application(\"boot\") & \":\" & Session(\"visits\") %>",
        )
        .unwrap();
        let app = AppRoot::new(&root);
        let holder = ServerHolder::for_app_root(&app).unwrap();
        let a = set_cookie_value(&stateful(&holder, &app, "/p.asp", "", "", "GET", "")).unwrap();
        let first = stateful(&holder, &app, "/p.asp", "", "", "GET", "");
        assert_eq!(first.body, "on:1");
        let second = stateful(
            &holder,
            &app,
            "/p.asp",
            "",
            &format!("ASPSESSIONID={a}"),
            "GET",
            "",
        );
        // Session_OnStart must NOT fire again; app values persist.
        assert_eq!(second.body, "on:1");
    }

    #[test]
    fn server_script_procedures_are_visible_to_pages() {
        let root = build_app("srvscript");
        fs::write(
            root.join("lib.inc"),
            "<SCRIPT RUNAT=Server LANGUAGE=\"VBScript\">\nSub shout(t)\nResponse.Write UCase(t)\nEnd Sub\n</SCRIPT>",
        )
        .unwrap();
        fs::write(
            root.join("p.asp"),
            "<!-- #include file=\"lib.inc\" --><% shout \"hey\" %>",
        )
        .unwrap();
        let app = AppRoot::new(&root);
        let holder = ServerHolder::for_app_root(&app).unwrap();
        let response = stateful(&holder, &app, "/p.asp", "", "", "GET", "");
        assert_eq!(response.body, "HEY");
    }

    #[test]
    fn session_timeout_persists_in_the_store() {
        let root = build_app("timeout");
        fs::write(root.join("p.asp"), "<% Session.Timeout = 45 %>").unwrap();
        fs::write(
            root.join("q.asp"),
            "<% If Session(\"t\") = Empty Then\nSession(\"t\") = 1\nEnd If\n%>ok",
        )
        .unwrap();
        let app = AppRoot::new(&root);
        let holder = ServerHolder::for_app_root(&app).unwrap();
        let first = stateful(&holder, &app, "/p.asp", "", "", "GET", "");
        let cookie = set_cookie_value(&first).unwrap();
        // Timeout itself is not directly observable from script; the
        // store round-trip (no expiry on next request) is the gate.
        let second = stateful(
            &holder,
            &app,
            "/q.asp",
            "",
            &format!("ASPSESSIONID={cookie}"),
            "GET",
            "",
        );
        assert_eq!(second.body, "ok");
        assert_eq!(set_cookie_value(&second), None);
    }
}
