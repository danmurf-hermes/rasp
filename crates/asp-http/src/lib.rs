//! HTTP server and request-to-response integration for RASP.
//!
//! M1 wraps `tiny_http` with the ASP request pipeline: parse the request
//! URL, decode query/form/cookies into `Request` data, render the page
//! through `asp-runtime`, and emit status + body. Static assets are not
//! served in M1 — only `.asp` pages.

use asp_core::AppRoot;
use asp_runtime::{RenderOutput, build_request_data, render_page};
use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

/// Server configuration for one application host.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Hostname or IP to bind (e.g. `127.0.0.1`).
    pub host: String,
    /// TCP port to listen on.
    pub port: u16,
    /// Default document when a path resolves to a directory.
    pub default_document: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8174,
            default_document: "default.asp".to_string(),
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
}

impl HttpRequest {
    /// Combine the request's collections into the runtime's key map.
    pub fn request_data(&self) -> HashMap<String, String> {
        build_request_data(&self.query, &self.form, &self.cookies)
    }
}

/// The outcome of attempting to serve one request.
#[derive(Debug, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    pub content_type: String,
}

/// Map a URL path to a page inside `app`, enforcing the default
/// document and the `.asp`-only rule for M1.
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
/// (the integration point used by tests and the `run` CLI).
pub fn handle_request(app: &AppRoot, root_relative: &str, request: &HttpRequest) -> HttpResponse {
    match try_render(app, root_relative, request) {
        Ok(out) => to_response(out),
        Err(asp_core::AspError::PageNotFound(_)) => HttpResponse {
            status: 404,
            body: "<html><body><h1>404 Not Found</h1></body></html>\n".to_string(),
            content_type: "text/html".to_string(),
        },
        Err(err) => error_response(500, &err.to_string()),
    }
}

fn try_render(
    app: &AppRoot,
    root_relative: &str,
    request: &HttpRequest,
) -> asp_core::AspResult<RenderOutput> {
    let out = render_page(app, root_relative, request.request_data())?;
    Ok(out)
}

fn to_response(out: RenderOutput) -> HttpResponse {
    HttpResponse {
        status: out.status.unwrap_or(200),
        body: out.body,
        content_type: out.content_type.unwrap_or_else(|| "text/html".to_string()),
    }
}

fn error_response(status: u16, message: &str) -> HttpResponse {
    let body = format!(
        "<html><body><h1>RASP error</h1><pre>{}</pre></body></html>\n",
        html_escape(message)
    );
    HttpResponse {
        status,
        body,
        content_type: "text/html".to_string(),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Run the blocking HTTP server loop until the process is stopped.
/// Returns an error only when the listener cannot be created.
pub fn serve(app: &AppRoot, config: &ServerConfig) -> asp_core::AspResult<()> {
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
        // M1 is sequential: correctness over concurrency (tokio/threads in M2).
        let method = request.method().to_string();
        let url = request.url().to_string();
        let (path, query) = split_url(&url);
        let cookies = header_value(&request_headers(&request), "cookie");
        let form = read_form(&mut request);
        let http_request = HttpRequest {
            method,
            path,
            query,
            form,
            cookies,
        };
        let root_relative = resolve_request_path(app, &config, &http_request.path);
        let response = handle_request(app, &root_relative, &http_request);
        let header =
            tiny_http::Header::from_bytes(&b"Content-Type"[..], response.content_type.as_bytes())
                .unwrap_or_else(|_| {
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html"[..]).unwrap()
                });
        let mut http_response = tiny_http::Response::from_data(response.body.into_bytes())
            .with_status_code(response.status);
        http_response.add_header(header);
        let _ = request.respond(http_response);
    }
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

/// Read the request body as a form string (`k=v&k2=v2`), up to a limit.
fn read_form(request: &mut tiny_http::Request) -> String {
    let mut body = String::new();
    let length = request.body_length().unwrap_or(0).min(1_048_576) as u64;
    if length > 0 {
        let _ = request.as_reader().take(length).read_to_string(&mut body);
    }
    body
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
}
