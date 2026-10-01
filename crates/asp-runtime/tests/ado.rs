//! ADO integration tests (M6): real SQLite through the render path.

use asp_core::AppRoot;
use asp_runtime::render_page;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// A fresh temp app root per test (SQLite files are created inside).
fn app_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rasp-ado-test-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Render one page body string, catching errors into the result so
/// assertions can match against error text.
fn page_body(tag: &str, asp: &str) -> String {
    let dir = app_root(tag);
    let page_name = "p.asp";
    fs::write(dir.join(page_name), asp).unwrap();
    let app = AppRoot::new(&dir);
    let out = render_page(&app, page_name, HashMap::new())
        .map(|o| o.body)
        .unwrap_or_else(|e| format!("ERR: {e}"));
    fs::remove_dir_all(&dir).unwrap();
    out
}

#[test]
fn ado_connection_execute_and_records_affected() {
    let asp = concat!(
        "<%\n",
        "Set conn = Server.CreateObject(\"ADODB.Connection\")\n",
        "conn.Open \"app.db\"\n",
        "conn.Execute \"CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)\"\n",
        "conn.Execute \"INSERT INTO t (name) VALUES ('Ada')\"\n",
        "Response.Write \"affected=\" & conn.RecordsAffected\n",
        "conn.Close\n",
        "%>"
    );
    assert_eq!(page_body("aff", asp), "affected=1");
}

#[test]
fn ado_command_parameterised_select_and_recordset_walk() {
    let asp = concat!(
        "<%\n",
        "Set conn = Server.CreateObject(\"ADODB.Connection\")\n",
        "conn.Open \"app.db\"\n",
        "conn.Execute \"CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)\"\n",
        "conn.Execute \"INSERT INTO t (name) VALUES ('Ada')\"\n",
        "conn.Execute \"INSERT INTO t (name) VALUES ('Grace')\"\n",
        "Set cmd = Server.CreateObject(\"ADODB.Command\")\n",
        "cmd.ActiveConnection = conn\n",
        "cmd.CommandText = \"SELECT id, name FROM t WHERE name = ?\"\n",
        "Set param = cmd.CreateParameter(\"name\", 200, 1)\n",
        "param.Value = \"Grace\"\n",
        "Set ps = cmd.Parameters\n",
        "ps.Append param\n",
        "Set rs = cmd.Execute\n",
        "Do While Not rs.EOF\n",
        "    Response.Write rs.Fields(\"id\").Value & \"=\" & rs.Fields(\"name\").Value & \";\"\n",
        "    rs.MoveNext\n",
        "Loop\n",
        "Response.Write rs.RecordCount\n",
        "conn.Close\n",
        "%>"
    );
    assert_eq!(page_body("cmd", asp), "2=Grace;1");
}

#[test]
fn ado_unsupported_recordset_open_is_clear() {
    let asp = concat!(
        "<%\n",
        "Set conn = Server.CreateObject(\"ADODB.Connection\")\n",
        "conn.Open \"app.db\"\n",
        "Set rs = Server.CreateObject(\"ADODB.Recordset\")\n",
        "rs.Open \"SELECT * FROM t\", conn\n",
        "%>"
    );
    let body = page_body("openx", asp);
    assert!(
        body.starts_with("ERR:") && body.contains("'Recordset.open' is not supported"),
        "{body}"
    );
}

#[test]
fn ado_connection_string_errors_surface() {
    let asp = concat!(
        "<%\n",
        "Set conn = Server.CreateObject(\"ADODB.Connection\")\n",
        "conn.Open \"mysql://u:p@host/db\"\n",
        "%>"
    );
    let body = page_body("mysqlx", asp);
    assert!(body.contains("Connection.Open:"), "{body}");
    assert!(body.contains("MySQL"), "{body}");
}

#[test]
fn ado_traversal_is_rejected() {
    let asp = concat!(
        "<%\n",
        "Set conn = Server.CreateObject(\"ADODB.Connection\")\n",
        "conn.Open \"../evil.db\"\n",
        "%>"
    );
    let body = page_body("trav", asp);
    assert!(body.contains("Connection.Open:"), "{body}");
    assert!(body.contains("escape"), "{body}");
}

#[test]
fn ado_connection_execute_query_returns_recordset() {
    let asp = concat!(
        "<%\n",
        "Set conn = Server.CreateObject(\"ADODB.Connection\")\n",
        "conn.Open \"app.db\"\n",
        "conn.Execute \"CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)\"\n",
        "conn.Execute \"INSERT INTO t (name) VALUES ('Ada')\"\n",
        "Set rs = conn.Execute(\"SELECT name FROM t\")\n",
        "Do While Not rs.EOF\n",
        "    Response.Write rs.Fields(\"name\").Value\n",
        "    rs.MoveNext\n",
        "Loop\n",
        "conn.Close\n",
        "%>"
    );
    assert_eq!(page_body("connq", asp), "Ada");
}

#[test]
fn ado_empty_recordset_is_eof_without_error() {
    let asp = concat!(
        "<%\n",
        "Set conn = Server.CreateObject(\"ADODB.Connection\")\n",
        "conn.Open \"app.db\"\n",
        "conn.Execute \"CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)\"\n",
        "Set rs = conn.Execute(\"SELECT name FROM t\")\n",
        "n = 0\n",
        "Do While Not rs.EOF\n",
        "    n = n + 1\n",
        "    rs.MoveNext\n",
        "Loop\n",
        "Response.Write \"walked=\" & n & \" bof=\" & rs.BOF & \" eof=\" & rs.EOF\n",
        "conn.Close\n",
        "%>"
    );
    assert_eq!(page_body("empty", asp), "walked=0 bof=True eof=True");
}
