//! End-to-end golden tests: render the example app pages and compare
//! exact bodies, statuses, and error handling. These are the fixtures
//! the plan doc's "golden tests" row refers to.

// Shared through dev-dependencies on the workspace crates via
// `tests/`-scoped integration: this file uses asp_runtime + asp_core.
use asp_core::AppRoot;
use asp_http::{HttpRequest, ServerConfig, handle_request, resolve_request_path};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Locate the repository root from CARGO_MANIFEST_DIR of the crate.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("repo root must exist")
}

fn example_app() -> &'static PathBuf {
    static APP: OnceLock<PathBuf> = OnceLock::new();
    APP.get_or_init(|| workspace_root().join("examples").join("hello-app"))
}

fn app() -> AppRoot {
    AppRoot::new(example_app())
}

fn get(path: &str, query: &str) -> (HttpResponseLike, String) {
    let _ = query;
    let config = ServerConfig::default();
    let root_relative = resolve_request_path(&app(), &config, path);
    let request = HttpRequest {
        method: "GET".to_string(),
        path: path.to_string(),
        query: query.to_string(),
        form: String::new(),
        cookies: String::new(),
    };
    let response = handle_request(&app(), &root_relative, &request);
    (
        HttpResponseLike {
            status: response.status,
            body: response.body.clone(),
        },
        root_relative,
    )
}

#[derive(Debug)]
struct HttpResponseLike {
    status: u16,
    body: String,
}

#[test]
fn golden_hello_page() {
    let (response, _) = get("/hello.asp", "");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.body,
        "<!DOCTYPE html>\n\
         <html>\n\
         <head><title>Hello from RASP</title></head>\n\
         <body>\n\
         <h1>Hello from RASP</h1>\n\
         \n\
         <p>Forty-two is 42.</p>\n\
         <p>Greeting: Hi there</p>\n\
         <div class=\"footer\">rendered by RASP</div>\n\
         </body>\n\
         </html>"
    );
}

#[test]
fn golden_loop_page() {
    let (response, _) = get("/loop.asp", "");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.body,
        "\n<span></span><span>1</span><span>2</span><span>3</span>"
    );
}

#[test]
fn golden_default_document_at_root() {
    let (response, rel) = get("/", "");
    assert_eq!(rel, "default.asp");
    assert_eq!(response.status, 404);
}

#[test]
fn golden_missing_page_is_404() {
    let (response, _) = get("/gone.asp", "");
    assert_eq!(response.status, 404);
}

#[test]
fn golden_error_page_is_500() {
    let root = example_app();
    let bad = root.join("boom.asp");
    std::fs::write(&bad, "<% q = 1 / 0 %>").unwrap();
    let (response, _) = get("/boom.asp", "");
    let _ = std::fs::remove_file(&bad);
    assert_eq!(response.status, 500);
    assert!(response.body.contains("division by zero"));
}

#[test]
fn golden_querystring_page() {
    // Build a page on the fly that echoes a querystring value.
    let root: &Path = example_app();
    std::fs::write(
        root.join("q.asp"),
        "<%= \"Hello \" & Request.QueryString(\"who\") %>",
    )
    .unwrap();
    let config = ServerConfig::default();
    let request = HttpRequest {
        method: "GET".to_string(),
        path: "/q.asp".to_string(),
        query: "who=Dan".to_string(),
        form: String::new(),
        cookies: String::new(),
    };
    let root_relative = resolve_request_path(&app(), &config, "/q.asp");
    let response = handle_request(&app(), &root_relative, &request);
    let _ = std::fs::remove_file(root.join("q.asp"));
    assert_eq!(response.body, "Hello Dan");
    let _ = HashMap::<String, String>::new();
}
