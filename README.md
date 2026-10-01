# RASP

> Experimental, vibe-coded.

A Classic ASP interpreter in Rust. RASP runs existing Classic ASP
applications from a single Rust binary (or a Docker image) on Linux,
macOS, Windows, or any platform that supports containers — no Windows
or IIS required.

## Features

| Feature | Status |
|---|---|
| ASP page model (text, `<% %>`, `<%= %>`, `<%@ Language %>`) | ✅ |
| `#include` directives (`file=`, `virtual=`) with path confinement | ✅ |
| `<SCRIPT RUNAT=Server LANGUAGE=VBScript>` blocks | ✅ |
| VBScript expressions with full precedence | ✅ |
| `If`/`ElseIf`/`Else` (block and single-line) | ✅ |
| Loops: `For`/`Next`, `Do`/`Loop`, nested, HTML interleave | ✅ |
| `Exit For`/`Exit Do`/`Exit Sub`/`Exit Function` | ✅ |
| `Sub`/`Function` with `Call`, ByRef/ByVal | ✅ |
| Fixed-size arrays; `Array()`, `UBound`, `Split`, `Join` | ✅ |
| Conversions (`CInt`, `CLng`, `CDbl`, `CBool`, `CDate`, `Is*`) | ✅ |
| Date/Time functions (`DateSerial`, `DateAdd`, `DateDiff`, parts) | ✅ |
| `Request.QueryString` / `Form` / `Cookies` / `ServerVariables` | ✅ |
| `Response.Write` / `End` / `Clear` / `Redirect` / `ContentType` | ✅ |
| `Response.Cookies` and response headers/status | ✅ |
| `Session` values, `Contents`, `SessionID`, `Timeout`, `Abandon` | ✅ |
| HMAC-signed `ASPSESSIONID` cookie | ✅ |
| `global.asa`: `Application_OnStart`, `Session_OnStart`/`OnEnd` | ✅ |
| `Application` values, `Contents`, `Lock`/`UnLock` | ✅ (lock is a flag; no concurrent requests yet) |
| `Scripting.Dictionary` (case-sensitive, insertion-ordered) | ✅ |
| `Scripting.FileSystemObject` sandboxed basics | ✅ |
| `Server.MapPath` / `Execute` / `Transfer` / `CreateObject` | ✅ |
| `ADODB.Connection` (Open/Close/Execute/transactions) | ✅ |
| `ADODB.Command` with parameterised queries | ✅ |
| `ADODB.Recordset` (`MoveNext`, `BOF`/`EOF`, live `Fields`) | ✅ |
| SQLite (bundled) and PostgreSQL backends | ✅ |
| `Folder`/`File`/`TextStream` object model | ❌ planned |
| Multi-dimensional arrays, `Byte`/binary data | ❌ planned |
| `Application_OnEnd` | ❌ planned (IIS fires it on app recycle) |
| MySQL / MariaDB | ❌ planned |
| Configuration file (port, limits, logging) | ❌ planned |
| JScript | ❌ not started |

Unsupported syntax fails with a clear "not supported" message. It never
silently produces wrong output.

## How to run it

Build from source (needs Rust stable):

```bash
cargo build -p asp-cli
./target/debug/rasp version
```

Run a single page against a synthetic request:

```bash
./target/debug/rasp run --root examples/hello-app hello.asp
```

Syntax-check every `.asp` file in an application:

```bash
./target/debug/rasp check examples/hello-app
```

Serve an application over HTTP:

```bash
./target/debug/rasp serve --root examples/hello-app --port 8080
curl http://127.0.0.1:8080/hello.asp
```

The server maps URLs to `.asp` pages, applies a default document, and
keeps session state across requests. Try the demos:

```bash
# Session + Application counters (send the cookie back to count up)
curl -c /tmp/jar.txt http://127.0.0.1:8080/state.asp
curl -b /tmp/jar.txt http://127.0.0.1:8080/state.asp

# Native objects: a Dictionary and the sandboxed folder listing
curl http://127.0.0.1:8080/dict.asp
curl http://127.0.0.1:8080/files.asp
```

Or via Docker:

```bash
docker build -t rasp .
docker run --rm rasp version
```

A database example with Docker Compose (SQLite and PostgreSQL) lives in
[examples/database/](examples/database/). Credentials come from the
environment, never from the repo.

## Development

The quality gates (format, clippy, tests) run as a pre-commit hook.
Enable them once after cloning:

```bash
git config core.hooksPath .githooks
```

Run the gates by hand:

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --all
```

## Repository layout

```text
crates/asp-core      Shared AST, values, errors, page model, cookie signing
crates/asp-vbscript  VBScript lexer, parser, evaluator, ADO object states
crates/asp-runtime   ASP objects (Request, Response, Server, Session, Application, hosts)
crates/asp-db        Database adapters (SQLite bundled, PostgreSQL)
crates/asp-http      HTTP server and request-to-response integration
crates/asp-cli       The `rasp` executable
docs/                Documentation
examples/            ASP application examples
tests/               Integration and golden tests
```