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
        headers: Vec::new(),
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
        // The M2 executor no longer runs the body once before the
        // first iteration (an M1 stack-machine artifact that emitted a
        // spurious empty span; real VBScript never does this).
        "\n<span>1</span><span>2</span><span>3</span>"
    );
}

#[test]
fn golden_arrays_page() {
    let (response, _) = get("/arrays.asp", "");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.body,
        "\n\n<ul>\n<li>ann: 10</li><li>bob: 20</li><li>cid: 30</li>\n</ul>\n"
    );
}

#[test]
fn golden_procedures_page() {
    let (response, _) = get("/procs.asp", "");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.body,
        "\n\n<p>Hello, Dan!</p>\n<p>high (150)</p>\n<p>score 42</p>\n<p>2</p>"
    );
}

#[test]
fn golden_exit_and_conversions_page() {
    let (response, _) = get("/exit.asp", "");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.body,
        "\n\n<p>loop1=123</p>\n<p>loop2=3</p>\n<p>round=24</p>\n\
         <p>date=2026-9-30</p>\n<p>eom=2026-03-01</p>"
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
        headers: Vec::new(),
    };
    let root_relative = resolve_request_path(&app(), &config, "/q.asp");
    let response = handle_request(&app(), &root_relative, &request);
    let _ = std::fs::remove_file(root.join("q.asp"));
    assert_eq!(response.body, "Hello Dan");
    let _ = HashMap::<String, String>::new();
}

/// A stateful GET through a fresh server holder (M4 goldens run the
/// real cross-request state path).
fn stateful_get(
    holder: &asp_http::ServerHolder,
    path: &str,
    cookie: Option<&str>,
) -> asp_http::HttpResponse {
    let config = ServerConfig::default();
    let root_relative = resolve_request_path(&app(), &config, path);
    let request = HttpRequest {
        method: "GET".to_string(),
        path: path.to_string(),
        query: String::new(),
        form: String::new(),
        cookies: cookie
            .map(|c| format!("ASPSESSIONID={c}"))
            .unwrap_or_default(),
        headers: Vec::new(),
    };
    asp_http::handle_request_with_state(holder, &app(), &root_relative, &request, true).0
}

fn session_cookie(response: &asp_http::HttpResponse) -> String {
    response
        .set_cookies
        .iter()
        .find(|c| c.to_ascii_lowercase().starts_with("aspsessionid"))
        .map(|c| c.split_once('=').unwrap().1.to_string())
        .expect("new visit must set the session cookie")
}

#[test]
fn golden_serverscript_include_page() {
    assert_eq!(
        stateful_get(
            &asp_http::ServerHolder::for_app_root(&app()).unwrap(),
            "/serverscript.asp",
            None
        )
        .body,
        "\n\nSHOUT IT OUT"
    );
}

#[test]
fn golden_state_page_session_round_trip() {
    let holder = asp_http::ServerHolder::for_app_root(&app()).unwrap();
    let first = stateful_get(&holder, "/state.asp", None);
    let cookie = session_cookie(&first);
    assert_eq!(
        first.body,
        "\n\n<p>Your views: 1</p>\n<p>All views: 1</p>\n<p>Your session id: 1</p>\n<p>App booted: yes</p>"
    );
    let second = stateful_get(&holder, "/state.asp", Some(&cookie));
    assert_eq!(
        second.body,
        "\n\n<p>Your views: 2</p>\n<p>All views: 2</p>\n<p>Your session id: 1</p>\n<p>App booted: yes</p>"
    );
    // An established session sends no new cookie.
    assert!(session_cookie_or_none(&second).is_none());
}

fn session_cookie_or_none(response: &asp_http::HttpResponse) -> Option<String> {
    response
        .set_cookies
        .iter()
        .find(|c| c.to_ascii_lowercase().starts_with("aspsessionid"))
        .map(|c| c.split_once('=').unwrap().1.to_string())
}

#[test]
fn golden_global_asa_app_values_are_shared() {
    // state.asp's global.asa Application_OnStart ran in the previous
    // test with its own holder; here a separate holder proves events
    // fire per-process and the app counter carries across sessions.
    let holder = asp_http::ServerHolder::for_app_root(&app()).unwrap();
    let cookie_a = session_cookie(&stateful_get(&holder, "/state.asp", None));
    let cookie_b = session_cookie(&stateful_get(&holder, "/state.asp", None));
    assert_ne!(cookie_a, cookie_b);
    let a2 = stateful_get(&holder, "/state.asp", Some(&cookie_a));
    assert!(a2.body.contains("Your views: 2"), "{}", a2.body);
    assert!(a2.body.contains("All views: 3"), "{}", a2.body);
}

#[test]
fn golden_dictionary_page() {
    let response = stateful_get(
        &asp_http::ServerHolder::for_app_root(&app()).unwrap(),
        "/dict.asp",
        None,
    );
    assert_eq!(
        response.body,
        "\n\n<ul>\n<li>tea: 1.20</li><li>coffee: 1.40</li><li>cake: 2.10</li>\n</ul>\n<p>3 items</p>"
    );
}

#[test]
fn golden_files_page_lists_sandboxed_folder() {
    let response = stateful_get(
        &asp_http::ServerHolder::for_app_root(&app()).unwrap(),
        "/files.asp",
        None,
    );
    // The sandbox lists the example app's own files (sorted).
    for name in ["arrays.asp", "dict.asp", "global.asa", "state.asp"] {
        assert!(
            response.body.contains(&format!("<li>{name}</li>")),
            "{}",
            response.body
        );
    }
}

#[test]
fn golden_execute_and_transfer_pages() {
    // Write the pages this test needs; each holder is a fresh process
    // state so Execute/Transfer behaviours are deterministic.
    let root = example_app();
    std::fs::write(root.join("sub.asp"), "SUB").unwrap();
    std::fs::write(root.join("exec.asp"), "A<% Server.Execute \"sub.asp\" %>B").unwrap();
    std::fs::write(
        root.join("transfer.asp"),
        "A<% Server.Transfer \"sub.asp\" %>B",
    )
    .unwrap();
    let holder = asp_http::ServerHolder::for_app_root(&app()).unwrap();
    assert_eq!(stateful_get(&holder, "/exec.asp", None).body, "ASUBB");
    assert_eq!(stateful_get(&holder, "/transfer.asp", None).body, "SUB");
    let _ = std::fs::remove_file(root.join("sub.asp"));
    let _ = std::fs::remove_file(root.join("exec.asp"));
    let _ = std::fs::remove_file(root.join("transfer.asp"));
}
#[test]
fn golden_guestbook_database_page() {
    // The guestbook seeds its SQLite database inside the example app's
    // data/ folder; the file is test-local and removed after.
    let root = example_app();
    let db_rel = "data/guestbook.db";
    let db_path = root.join(db_rel);
    let _ = std::fs::remove_file(&db_path);
    let get_with_query = |query: &str| -> asp_http::HttpResponse {
        let config = ServerConfig::default();
        let root_relative = resolve_request_path(&app(), &config, "/guestbook.asp");
        let request = HttpRequest {
            method: "GET".to_string(),
            path: "/guestbook.asp".to_string(),
            query: query.to_string(),
            form: String::new(),
            cookies: String::new(),
            headers: Vec::new(),
        };
        asp_http::handle_request(&app(), &root_relative, &request)
    };
    let response = get_with_query("name=Grace");
    assert_eq!(response.status, 200);
    assert!(
        response.body.contains("<li>Grace</li>"),
        "{}",
        response.body
    );
    // Second visit accumulates (the DB file persists request to request).
    let response2 = get_with_query("name=Ada");
    assert!(
        response2.body.contains("<li>Grace</li><li>Ada</li>"),
        "{}",
        response2.body
    );
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_dir(root.join("data"));
}
