# RASP

> Experimental, vibe-coded. See [docs/asp-classic-interpreter-plan.md](docs/asp-classic-interpreter-plan.md) for the full project plan and current status.

## What it is

RASP runs existing Classic ASP applications from a single Rust binary (or a
Docker image) on Linux, macOS, Windows, or any platform that supports
containers — no Windows or IIS required.

## Status: Hello ASP (Milestone 1)

Workstreams and milestones are tracked in
[docs/asp-classic-interpreter-plan.md](docs/asp-classic-interpreter-plan.md).
The interpreter renders real pages: VBScript expressions, control flow
(`If`, `For`/`Next`, `Do`/`Loop` — including bodies that interleave
markup across `<% %>` blocks), `Response.Write`/`End`/`Clear`,
`Request.QueryString`, and `#include` directives, served over HTTP.

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

# Or via Docker
docker build -t rasp .
docker run --rm rasp version
```

`check` stays quiet when every page parses (exit 0); `run` prints the
rendered body to stdout.

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