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
`<SCRIPT RUNAT=Server>` blocks, and `global.asa` events — served over
HTTP with session state: `Session` values (`Contents`, `SessionID`,
`Timeout`, `Abandon`) behind an `ASPSESSIONID` cookie and shared
`Application` values with `Lock`/`UnLock`. Native objects come next to
`Server.CreateObject`: a sandboxed `Scripting.FileSystemObject` (paths
stay inside the application root) and `Scripting.Dictionary`, plus
`Server.MapPath`/`Execute`/`Transfer`.

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
crates/asp-core      Shared AST, values, errors, page and include model
crates/asp-vbscript  VBScript lexer, parser, evaluator
crates/asp-runtime   ASP objects (Request, Response, Server, Session, Application)
crates/asp-http      HTTP server and request-to-response integration
crates/asp-cli       The `rasp` executable
docs/                Project plan and documentation
examples/            ASP application examples (added from Milestone 1)
tests/               Integration and golden tests (added from Milestone 2)
```