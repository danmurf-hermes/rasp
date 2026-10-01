# RASP

> Experimental, vibe-coded. See [docs/asp-classic-interpreter-plan.md](docs/asp-classic-interpreter-plan.md) for the full project plan and current status.

## What it is

RASP runs existing Classic ASP applications from a single Rust binary (or a
Docker image) on Linux, macOS, Windows, or any platform that supports
containers — no Windows or IIS required.

## Status: Objects and filesystem (Milestone 5)

Workstreams and milestones are tracked in
[docs/asp-classic-interpreter-plan.md](docs/asp-classic-interpreter-plan.md).
The interpreter renders real pages: VBScript expressions, control flow
(`If`, `For`/`Next`, `Do`/`Loop` — including bodies that interleave
markup across `<% %>` blocks, nested loops, `Exit For`/`Exit Do`),
fixed-size arrays and `Array()`/`UBound`/`Split`/`Join`,
`Sub`/`Function` procedures with `Call` and ByRef/ByVal parameters,
conversions (`CInt`/`CLng` with banker's rounding, `CDbl`, `CBool`,
`CDate`, `Is*`), Date/Time functions (`DateSerial`, `DateAdd`,
`DateDiff`, `Year`/`Month`/`Day`/…), `Response.Write`/`End`/`Clear`
plus `Response.Cookies`, `Request.QueryString`/`Form`/`Cookies`
plus `Request.ServerVariables`, `#include` directives,
`<SCRIPT RUNAT=Server>` blocks, and `global.asa` events
(`Session_OnEnd` fires on abandon/timeout with the dying session's
values) — served over HTTP with session state: `Session` values
(`Contents`, `SessionID`, `Timeout`, `Abandon`) behind an HMAC-signed
`ASPSESSIONID` cookie and shared `Application` values with
`Lock`/`UnLock`. Native objects live behind
`Server.CreateObject`: a sandboxed `Scripting.FileSystemObject` (paths
stay inside the application root), `Scripting.Dictionary`,
`Server.MapPath`/`Execute`/`Transfer`, and the M6 database subset —
`ADODB.Connection` (Open/Close/Execute/transactions), `ADODB.Command`
with parameterised queries (`CreateParameter`/`Parameters.Append`),
and `ADODB.Recordset` (`MoveNext`, `BOF`/`EOF`/`RecordCount`, live
`Fields`/`Field` reads). SQLite is bundled; PostgreSQL is compiled in
for host/database connection strings. See
[examples/database/](examples/database/) for the Docker Compose
example (credentials come from the environment, never the repo).

## Quick start

```bash
# From source (requires Rust stable)
cargo build -p asp-cli
./target/debug/rasp version

# Run a single page against a synthetic request
./target/debug/rasp run --root examples/hello-app hello.asp

# Syntax-check every .asp file in an application
./target/debug/rasp check --root examples/hello-app

# Serve the example app over HTTP
./target/debug/rasp serve --root examples/hello-app --port 8080
curl http://127.0.0.1:8080/loop.asp

# Session + Application state demo (send the cookie back to count up)
curl -c /tmp/jar.txt http://127.0.0.1:8080/state.asp
curl -b /tmp/jar.txt http://127.0.0.1:8080/state.asp

# Native objects: a Dictionary, and the sandboxed folder listing
curl http://127.0.0.1:8080/dict.asp
curl http://127.0.0.1:8080/files.asp

# Or via Docker
docker build -t rasp .
docker run --rm rasp version
```

`check` stays quiet when every page parses (exit 0); `run` prints the
rendered body to stdout.

## Development

Quality gates (fmt → clippy → tests) run as a pre-commit hook; enable them
once after cloning:

```bash
git config core.hooksPath .githooks
```

## Repository layout

```text
crates/asp-core      Shared AST, values, errors, page/include model, database trait
crates/asp-vbscript  VBScript lexer, parser, evaluator, ADO object states
crates/asp-runtime   ASP objects (Request, Response, Server, Session, Application, hosts)
crates/asp-db        Database adapters (SQLite bundled, PostgreSQL)
crates/asp-http      HTTP server and request-to-response integration
crates/asp-cli       The `rasp` executable
docs/                Project plan and documentation
examples/            ASP application examples (added from Milestone 1)
tests/               Integration and golden tests (added from Milestone 2)
```