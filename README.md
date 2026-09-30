# RASP

> Experimental, vibe-coded. See [docs/asp-classic-interpreter-plan.md](docs/asp-classic-interpreter-plan.md) for the full project plan and current status.

## What it is

RASP runs existing Classic ASP applications from a single Rust binary (or a
Docker image) on Linux, macOS, Windows, or any platform that supports
containers — no Windows or IIS required.

## Status: planning-only scaffold (Milestone 0)

Workstreams and milestones are tracked in
[docs/asp-classic-interpreter-plan.md](docs/asp-classic-interpreter-plan.md).
Nothing interprets ASP yet.

## Quick start

```bash
# From source (requires Rust stable)
cargo build -p asp-cli
./target/debug/rasp version

# Or via Docker
docker build -t rasp .
docker run --rm rasp version
```

`serve`, `run`, and `check` subcommands exist as placeholders and report that
they are not implemented yet.

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