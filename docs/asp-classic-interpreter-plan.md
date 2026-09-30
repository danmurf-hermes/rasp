# Classic ASP Interpreter — Implementation Plan

## Purpose

Build a portable, container-ready implementation of Classic ASP using Rust. The goal is to let someone run an existing ASP application from a single Docker image on Linux, macOS, Windows, or any other platform with a container runtime. The implementation should prefer a single Rust binary, predictable behavior, useful diagnostics, and compatibility with common Classic ASP patterns over trying to reproduce every Windows/IIS/COM-specific edge case immediately.

This document is a living roadmap and the project's handoff system. Every completed unit of work must update this plan in the same pull request as the implementation. A merged pull request should leave this plan ready for the next agent to choose the next piece of work without needing to inspect stale notes elsewhere.

Do not mark a feature **Built** unless the corresponding automated tests pass and the behavior is documented.

---

## 0. Plan update contract

This plan is part of the definition of done. Implementation code and its plan update must be merged together.

### Required in every pull request

Every pull request that changes implementation or project structure must also update this document so that, when merged, the plan describes the repository as it exists after the merge.

At minimum, the pull request must update:

1. The relevant **Workstream status** row.
2. The matching detailed workstream section.
3. The affected milestone status.
4. The **Immediate next steps** section.
5. `README.md`, if user-facing behavior or the supported feature set changed.

### Status rules

- Mark **Built** only when the feature is implemented, tested, and documented.
- If work is incomplete but useful, mark it **In progress** and list the exact remaining tasks.
- If a feature needs research first, mark it **Exploring** and record the question or prototype result.
- If a feature is deliberately deferred, mark it **Out of scope for now** and explain why.
- Do not mark **Built** because code was written. Tests and documentation are required.
- Do not leave old statuses unchanged because the work was small.
- Do not update the plan in a separate future pull request unless the implementation pull request is blocked or rejected.

### Handoff rules for the next agent

1. Start from this plan, not from unmerged local work or private notes.
2. Pick the earliest unfinished milestone that can be completed as a small, verifiable pull request.
3. Check the **Workstream status** table for the exact state of each area.
4. Do not skip ahead unless a dependency is genuinely blocked and the reason is recorded here.
5. Include the plan update in your working branch before requesting review.
6. Reviewers should reject an implementation-only pull request if this plan has not been updated.

### Recommended pull-request flow

1. Read the plan and select the next unfinished task.
2. Create a small branch for that task.
3. Implement the smallest useful slice.
4. Add or update tests and examples.
5. Update this plan in the same branch.
6. Update the README if the user-facing scope changed.
7. Run the quality commands listed in the maintenance checklist.
8. Open a pull request containing implementation, tests, examples, and plan updates together.
9. After merge, this plan should immediately identify the next task.

---

## 1. Guiding principles

1. **Compatibility in layers.** Start with a small, well-tested subset and expand. A clear error such as `Unsupported VBScript feature: ...` is better than a runtime crash.
2. **Single binary first.** The interpreter and HTTP server should compile into one executable. External configuration, optional adapters, and container packaging can be added without compromising this.
3. **Cross-platform, not Windows emulation.** Windows-only COM objects cannot be provided generically. Implement native Rust equivalents for common COM interfaces and map known ProgIDs to those implementations.
4. **Safety by default.** Use safe Rust where practical, enforce request timeouts and resource limits, and prevent file and path access from escaping the configured application root unless explicitly allowed.
5. **Testability before breadth.** Every language feature and ASP object should have unit and integration tests. Golden tests should compare HTTP status, headers, body, and side effects.
6. **Observability matters.** Structured errors, tracing, and deterministic test output are core features, not later polish.

---

## 2. What Classic ASP actually is

Classic ASP is not a single language. It is a server-page model that mixes markup and server-side script. A typical `.asp` page can contain:

- HTML and literal text.
- `<% ... %>` server-side script blocks.
- `<%= expression %>` expression blocks that write the expression result to the response.
- Server-side `<!-- #include file="..." -->` and `<!-- #include virtual="..." -->` directives.
- One or more scripting languages, commonly VBScript and occasionally JScript.
- Built-in ASP objects such as `Request`, `Response`, `Server`, `Session`, and `Application`.
- Optional COM components, especially `ADODB` for database access and `Scripting.FileSystemObject` for filesystem work.
- Application-level events defined in `global.asa`.

A practical interpreter therefore needs several distinct layers:

1. HTTP request handling.
2. ASP page extraction and include resolution.
3. A scripting language parser and evaluator.
4. ASP runtime objects.
5. State management for sessions and applications.
6. Native replacements or mappings for common COM components.
7. Configuration, deployment, and security.

---

## 3. Recommended Rust workspace

Use a Cargo workspace so the project remains testable and maintainable:

```text
/
  Cargo.toml
  crates/
    asp-core/          # Shared AST, values, errors, page model, include model
    asp-vbscript/      # VBScript lexer, parser, evaluator, conformance tests
    asp-jscript/       # Optional JScript support through a JavaScript engine
    asp-runtime/       # ASP objects, request lifecycle, state, COM mappings
    asp-db/            # ADO-compatible database adapters
    asp-http/          # HTTP server and request-to-response integration
    asp-cli/           # Command-line executable
  docs/
    asp-classic-interpreter-plan.md
  examples/
    hello-world/
    form-handling/
    includes/
    session-state/
    database/
  tests/
    integration/
    golden/
```

Suggested initial dependency choices:

- **HTTP:** `axum` and `tokio`.
- **Parsing:** hand-written lexer/parser rather than a general grammar framework initially; Classic VBScript syntax has quirks that are easier to control explicitly.
- **JavaScript/JScript:** evaluate a compatible engine such as `boa` only when VBScript core support is stable.
- **Logging/tracing:** `tracing`.
- **CLI:** `clap`.
- **Serialization:** `serde` and `serde_json`.
- **Database:** start with SQL adapters such as `sqlx`; do not attempt a universal ADO implementation first.
- **Testing:** Rust built-in tests, `insta` for golden snapshots if useful, and `criterion` only when performance benchmarking begins.

Avoid making all crates depend on the HTTP crate. The language and runtime must be usable from tests and non-HTTP tools.

---

## 4. High-level architecture

```text
Browser
  |
HTTP request
  |
asp-http
  |- Build Request model
  |- Resolve requested ASP page safely
  |
asp-core
  |- Load page
  |- Resolve includes
  |- Split into literal text, expressions, and script segments
  |
asp-vbscript / asp-jscript
  |- Parse script
  |- Evaluate against runtime environment
  |
asp-runtime
  |- Request, Response, Server, Session, Application
  |- Optional File, Database, Email, XML mappings
  |
Buffered response
  |
HTTP response
```

Important design decisions:

- Parse ASP page structure separately from parsing script code.
- Represent script values as a `Value` or `Variant` type in `asp-core`, not as HTTP-specific types.
- Keep output buffered by default so `Response.Clear`, `Response.End`, and error handling can work predictably.
- Keep language evaluation synchronous at first; HTTP I/O can remain asynchronous around it.
- Centralize coercion rules (`String`, `Integer`, `Double`, `Date`, `Boolean`, `Null`, `Empty`, arrays, and objects). Classic ASP relies heavily on implicit conversion.
- Make unsupported constructs explicit and testable rather than silently approximating them.

---

## 5. Feature status legend

- **Not started** — no implementation exists.
- **Exploring** — research, prototype, or partial design exists.
- **In progress** — implementation exists but is incomplete or not fully tested.
- **Built** — implemented, tested, and documented.
- **Out of scope for now** — intentionally deferred with a documented reason.

Any status change should be accompanied by a short note in the relevant section and, where useful, a link to tests or examples.

---

## 6. Workstream status

| Area | Status | Current notes |
|---|---|---|
| Project scaffolding | Built | Cargo workspace, five focused crates, placeholder `rasp` CLI (`version` works), README quick start. |
| Cargo workspace and CI | Built | GitHub Actions quality job (fmt, clippy, test on Ubuntu and macOS) plus a Docker build/run smoke test. Pre-commit hook in `.githooks/` (enable via `git config core.hooksPath .githooks`) runs the same gates before every commit. `AGENTS.md` records the conventions for future agents. |
| ASP page parser | Built | Text, `<% %>`, `<%= %>`, `<%@ Language %>`, and `<!-- #include -->` directives parsed per page; ordinary comments pass through. |
| Include resolution | Built | `file` resolves against the containing page's directory, `virtual` against the app root; path-confinement checks, 32-depth cycle detection. |
| VBScript lexer | Built | Case-insensitive keywords, strings with `""` escapes, `'` comments, `_` continuations, hex/octal literals, statement line-end tracking. |
| VBScript parser and AST | Built | Expressions with full precedence, Dim/Const, assignment, If/ElseIf/Else (block and single-line), For/Next and Do/Loop (deferred openers for cross-block bodies), Response/Request/Session targets. |
| VBScript evaluator | Built | Deterministic tree-walking evaluator: variables, arithmetic, `&` concat, loops with iteration caps, builtins (Len/UCase/Left/Mid/InStr/CStr/...), per-request `ExecEnv` + buffered Response. |
| Variant/value semantics | Not started | Needs careful coercion and equality rules. |
| ASP intrinsic objects | In progress | `Response.Write/End/Clear` and the buffer passthrough work; `Request.QueryString` reads query/form/cookie data; Response controls (Redirect, ContentType, cookies) parse but their effects land in M3. |
| Request lifecycle | In progress | Output buffers per request and ends early on `Response.End`; errors produce diagnostic pages/500s; timeouts and size limits still to come. |
| Session state | Not started | Begin with signed cookie + in-process store; add Redis later if needed. |
| Application state | Not started | Begin with per-process state. |
| `global.asa` support | Not started | Start with application/session events; defer COM/library registration. |
| Filesystem object mapping | Not started | Map `Scripting.FileSystemObject` to a sandboxed Rust implementation. |
| Database/ADO support | Not started | Start with a small SQL adapter before trying broad ADO compatibility. |
| JScript support | Not started | Optional; prioritize VBScript first. |
| HTTP server integration | Built | `tiny_http`-backed sequential server maps GET/POST URLs to `.asp` pages, applies the default document, decodes query/form/cookie data, and returns rendered bodies with 404/500 handling. |
| Configuration | Not started | App root, port, timeouts, limits, logging, session settings. |
| Security model | Not started | Path confinement, request limits, timeouts, non-root container. |
| Golden and integration tests | Built | Crate-level unit tests across parser/lexer/evaluator/runtime plus end-to-end golden tests rendering `examples/hello-app` over the HTTP request path (exact bodies, 404/500 paths, querystring data). |
| Docker image | Not started | Multi-stage Rust build; minimal runtime image; non-root user. |
| Documentation and migration guide | In progress | `README.md` documents the Milestone 0 scaffold, quick start, and repository layout; migration and configuration documentation remain pending. |

---

## 7. Detailed workstreams

### 7.1 Project scaffolding and quality gates

**Status:** Built

Current state:

- Root `Cargo.toml` workspace with crates `asp-core`, `asp-vbscript`,
  `asp-runtime`, `asp-http`, and `asp-cli` (binary `rasp`).
- Each placeholder crate compiles with a unit test; the CLI reports its
  version and rejects not-yet-implemented subcommands with exit code 2.
- CI quality job runs `cargo fmt --check`, clippy, and `cargo test --all` on
  Ubuntu and macOS, plus a Docker build/run smoke test.
- `Dockerfile` builds the release binary into a non-root `debian` runtime
  image (numeric `USER 1000`, because `debian:stable-slim` ships no
  `adduser`); `cargo fmt` and `cargo clippy` are the effective formatting and
  lint configuration (no `rustfmt.toml` needed).

Create:

- Root `Cargo.toml` workspace.
- Empty crates with focused responsibilities.
- `rustfmt.toml` only if the project needs a non-default setting.
- CI workflow that runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all`.
- A release build recipe and Dockerfile.
- A `README.md` with quick start instructions.

Acceptance criteria:

- All commands run successfully on Linux and macOS.
- The binary starts and reports a clear version and configuration.
- CI fails on formatting, lint, or test regressions.

---

### 7.2 ASP page model

**Status:** In progress

Responsibilities:

- Load `.asp` files safely from the configured application root.
- Detect script language defaults and `<%@ Language=... %>` style settings.
- Separate literal text from script.
- Parse `<%= expression %>` as an implicit `Response.Write`.
- Recognize `<SCRIPT RUNAT=SERVER LANGUAGE=...>...</SCRIPT>`.
- Resolve includes before script evaluation, matching Classic ASP's preprocessing behavior.
- Detect include cycles and report the include chain.

Initial support:

- VBScript pages.
- `<% %>`, `<%= %>`.
- `<!-- #include file="..." -->`.
- `<!-- #include virtual="..." -->`.
- UTF-8 and Latin-1 input, with configurable default code page.

Deferred:

- Multiple nested languages in one page.
- Codepage and locale edge cases beyond common applications.

Acceptance criteria:

- Golden tests verify exact output for mixed HTML and script.
- Include path traversal attempts fail safely.
- Include cycles produce a clear diagnostic.

Current state:

- All "Initial support" items are implemented and tested except `<SCRIPT RUNAT=SERVER>` recognition.
- Includes resolve with path confinement and depth-capped cycle detection.
- Script is executed via a flattened statement stream so `For`/`Do` bodies can interleave literal markup across `<% %>` blocks (matching Classic ASP behavior); block `If` bodies must still live inside one `<% %>` block for now.

---

### 7.3 VBScript language engine

**Status:** In progress

This is the largest workstream. Build in phases rather than attempting a complete parser immediately.

#### Phase 1: expressions and simple statements

- Variables and assignments.
- Numeric, string, and boolean literals.
- Arithmetic operators.
- String concatenation with `&`.
- Comparison and logical operators.
- `If ... Then ... ElseIf ... Else ... End If`.
- `For ... Next`.
- `Do While ... Loop` and `Do Until ... Loop`.
- `Dim`, `ReDim`, `Const`.
- Procedure calls.
- `Response.Write`.

#### Phase 2: procedures and arrays

- `Sub ... End Sub`.
- `Function ... End Function`.
- `ByRef` and `ByVal`.
- `Exit Sub`, `Exit Function`, `Exit For`, `Exit Do`.
- Fixed-size and dynamic arrays.
- `Array`, `UBound`, `LBound`, `Split`, `Join`.
- Common conversion functions such as `CStr`, `CInt`, `CLng`, `CDbl`, `CBool`, `IsArray`, `IsNull`, `IsEmpty`.

#### Phase 3: error handling and objects

- `On Error Resume Next`.
- `On Error GoTo 0`.
- `Err.Number`, `Err.Description`, `Err.Source`, `Err.Raise`, `Err.Clear`.
- `Set object = ...`.
- Property and method calls on runtime objects.
- `Class ... End Class`, `New`, `Property Let/Set/Get`.
- `With ... End With`.

#### Phase 4: broader runtime functions

- Date and time functions.
- String functions.
- Math functions.
- Collections and dictionaries.
- Regular expressions.
- Formatting and locale-sensitive functions where practical.

Compatibility notes:

- VBScript is generally case-insensitive for identifiers and keywords.
- Statements are usually line-oriented.
- Implicit conversion semantics are central and must be tested extensively.
- `ByRef` behavior matters and should not be guessed.

Acceptance criteria:

- Parser has unit tests for syntax and diagnostics.
- Evaluator has unit and property tests for coercion and equality.
- Unsupported syntax produces an explicit `UnsupportedFeature` error.
- Golden tests cover representative programs.

---

### 7.4 Variant and runtime value semantics

**Status:** Not started

Implement a central value model covering:

- `Empty`
- `Null`
- `Boolean`
- `Integer` / `Long`
- `Double`
- `String`
- `Date` / `DateTime`
- Arrays
- Objects
- Byte or binary data where required by `Request.BinaryRead` and `Response.BinaryWrite`

Required behavior:

- Define conversion rules in one place.
- Test equality and ordering, including `Null` and `Empty`.
- Implement string formatting consistently.
- Avoid leaking HTTP-specific types into the language engine.
- Keep operations deterministic and independent of host locale unless explicitly configured.

Acceptance criteria:

- Conversion table is documented.
- Tests cover common edge cases.
- Runtime errors identify the offending operation and source location.

---

### 7.5 ASP intrinsic objects

**Status:** Not started

#### `Request`

Initial support:

- `QueryString`
- `Form`
- `Cookies`
- `ServerVariables`
- `TotalBytes`
- `BinaryRead`

Consider:

- Charset and URL decoding rules.
- Repeated form keys.
- File upload handling through a later adapter.
- Request body size limits.

#### `Response`

Initial support:

- `Write`
- `BinaryWrite`
- `End`
- `Clear`
- `Flush`
- `Redirect`
- `Status`
- `ContentType`
- `Charset`
- `Expires`
- `CacheControl`
- `Cookies`
- `AddHeader`
- `AppendToLog`

Behavior:

- Default to buffered output.
- Prevent headers from being modified after output is flushed.
- Ensure `Response.End` stops evaluation immediately without losing the current output buffer.

#### `Server`

Initial support:

- `HTMLEncode`
- `URLEncode`
- `MapPath`
- `CreateObject` with mappings to native implementations
- `ScriptTimeout`
- `Execute`
- `Transfer`

#### `Session`

Initial support:

- Cookie-based session identity.
- `SessionID`.
- `Timeout`.
- `Abandon`.
- Named values.
- `Contents`.

Storage phases:

1. In-memory per process for local use and development.
2. Pluggable distributed storage for multi-instance containers.

#### `Application`

Initial support:

- Named values.
- `Lock` and `UnLock`.
- `Contents`.
- Per-process state.

#### `ObjectContext`

- Research and map relevant transaction semantics only after the core runtime is stable.

Acceptance criteria:

- Each object has unit and integration tests.
- HTTP integration tests verify status, headers, cookies, and body.
- Session and application state are isolated between requests and sessions.

---

### 7.6 Request lifecycle

**Status:** Not started

A request should pass through:

1. Receive and parse HTTP request.
2. Apply body and header limits.
3. Create ASP runtime context.
4. Resolve the requested page.
5. Load and preprocess the page.
6. Evaluate script and buffer output.
7. Execute response termination logic.
8. Serialize status, headers, cookies, and body.
9. Record metrics and logs.
10. Release request resources.

Error policy:

- Syntax errors should become a 500 response in production and a detailed diagnostic in development.
- Unhandled runtime errors should not leak filesystem paths or secrets.
- Timeouts must stop the request and release resources.
- Error pages should be configurable.

Acceptance criteria:

- A simple page returns the expected body.
- A runtime error returns a controlled 500 response.
- Request timeout is enforced.
- Large request and response limits work predictably.

---

### 7.7 Sessions and `global.asa`

**Status:** Not started

Start with:

- `Session_OnStart`
- `Session_OnEnd`
- `Application_OnStart`
- `Application_OnEnd`

Session requirements:

- Signed session cookies.
- Configurable cookie name, path, domain, secure and HTTP-only flags.
- Session expiration.
- Serializable values where possible.
- Clear diagnostics when storing unsupported values.

Deferred:

- Full IIS metabase and COM library registration behavior.
- Process/session affinity beyond documented defaults.

---

### 7.8 Native COM mappings

**Status:** Not started

Generic COM cannot be recreated cross-platform. Instead, define a native object registry.

Priority order:

1. `Scripting.FileSystemObject`
2. `ADODB.Connection` and `ADODB.Recordset`
3. `Scripting.Dictionary`
4. `MSXML2.DOMDocument`
5. `CDO.Message` or similar email components
6. Common third-party components discovered in real applications

Recommended implementation:

- A `ProgID` registry maps COM identifiers to native Rust implementations.
- Native objects implement the same runtime object interface as script-defined classes.
- Unsupported ProgIDs return a clear configuration error.
- The registry can be extended without changing the language engine.
- Dangerous capabilities are disabled by default.

`Scripting.FileSystemObject` should honor the ASP application root and an optional allowlist. It must not expose arbitrary host filesystem paths unless explicitly configured.

Acceptance criteria:

- Known objects can be created through `Server.CreateObject`.
- Unsupported objects produce a clear error.
- File operations are tested against path traversal and symlink escapes.

---

### 7.9 Database and ADO compatibility

**Status:** Not started

Do not begin by attempting a universal ADO clone. Instead:

1. Define a minimal connection and recordset interface.
2. Implement one practical database driver first, preferably SQLite for examples and local testing.
3. Add PostgreSQL and MySQL adapters once the interface is stable.
4. Map `ADODB.Connection`, `ADODB.Recordset`, `ADODB.Command`, and connection-string semantics gradually.
5. Implement transaction and parameter behavior explicitly.

Compatibility considerations:

- ADO connection strings are often provider-specific.
- Cursor types, lock types, and recordset pagination may have semantic differences.
- Parameter types and nullability require careful tests.
- Existing applications may rely on nonstandard provider behavior.

Acceptance criteria:

- A sample ASP page can query a configured database.
- Connection failures and SQL errors are surfaced as ASP errors.
- Credentials come from environment or configuration, never hardcoded examples.

---

### 7.10 JScript support

**Status:** Not started

JScript support should be optional and secondary. Most Classic ASP applications use VBScript.

Plan:

1. Keep ASP object access abstract from the JavaScript engine.
2. Evaluate embedded JavaScript with a Rust-compatible engine such as Boa.
3. Implement JScript-to-runtime value conversion.
4. Add explicit tests for ASP object calls.
5. Document unsupported JScript/COM behavior.

Deferred until VBScript Phase 1–3 are stable.

---

### 7.11 HTTP server and CLI

**Status:** In progress

The executable should support:

- `serve`: run the HTTP server.
- `run FILE`: run a single ASP page against a synthetic request and print the response.
- `check`: syntax-check all ASP files.
- `version`: print build and feature information.
- `config`: print the active configuration.

Essential server settings:

- Host and port.
- Application root.
- Default document.
- Request timeout.
- Body size limit.
- Response buffer size.
- Logging format and level.
- Development or production mode.
- Session settings.
- Path traversal policy.
- Optional static-file serving.

Acceptance criteria:

- The server can run a page from a mounted directory.
- The CLI can execute a single page for fast feedback.
- Configuration can come from CLI flags and environment variables.
- Invalid configuration fails before binding the server.

---

### 7.12 Security and resource controls

**Status:** Not started

Required controls:

- Restrict page and include resolution to the application root.
- Canonicalize paths and reject symlink escapes.
- Limit request headers, body size, URL length, and query parameters.
- Enforce per-request execution timeout.
- Limit output buffer size.
- Sign and validate session cookies.
- Run the container as a non-root user.
- Redact secrets from errors and logs.
- Avoid unsafe Rust unless a compelling, reviewed case exists.
- Prevent native filesystem objects from accessing arbitrary host paths by default.

Additional recommendations:

- Deny by default for any optional host access.
- Record security-sensitive configuration decisions in the documentation.
- Add tests for traversal, oversized requests, invalid encodings, and timeout behavior.

---

### 7.13 Testing strategy

**Status:** Not started

Use multiple test layers:

1. **Unit tests** for lexer, parser, values, coercions, and individual ASP methods.
2. **Integration tests** for complete pages and request lifecycles.
3. **Golden tests** for expected HTTP status, headers, cookies, and body.
4. **Compatibility tests** for language semantics and intrinsic objects.
5. **Error tests** for unsupported features, invalid input, and limits.
6. **Security tests** for path traversal, encoding, and request limits.
7. **Performance smoke tests** once behavior is stable.

Test fixture progression:

- Literal HTML.
- `<%= %>` expression.
- Variables.
- Conditions and loops.
- Includes.
- Forms and query strings.
- Cookies.
- Sessions.
- Application state.
- `global.asa`.
- Filesystem object.
- Database.
- Error handling.
- Realistic application excerpt.

Acceptance criteria:

- Tests can run without network access except for explicitly marked database tests.
- Every feature status update references at least one relevant test or example.
- CI produces a clear failure even when only one golden output changes.

---

### 7.14 Docker and deployment

**Status:** Not started

Recommended image:

- Multi-stage build.
- Rust builder with cached dependencies.
- Minimal runtime base such as `debian:stable-slim` or a distroless image.
- Copy only the release binary and required runtime files.
- Run as a dedicated non-root user.
- Expose one configurable HTTP port.
- Support mounting the ASP application directory as a volume.
- Provide a healthcheck endpoint or command.
- Use environment variables for configuration.
- Include an optional Docker Compose example with a database.

Suggested first `docker run` experience:

```bash
docker run \
  --rm \
  -p 8080:8080 \
  -v "$PWD/app:/app" \
  asp-classic:latest
```

Deployment considerations:

- Document volume permissions on macOS and Linux.
- Make session state pluggable before recommending multiple replicas.
- Keep the image independent of specific cloud providers.
- Publish image metadata and a small example application.
- Produce SBOM and provenance metadata before any public release.

Acceptance criteria:

- The image builds reproducibly.
- The container starts on macOS without requiring Windows.
- A mounted example app serves correctly.
- The process does not run as root.
- The healthcheck works.

---

## 8. Suggested milestone roadmap

### Milestone 0 — Skeleton

**Status:** Built

Deliverables:

- Cargo workspace.
- Empty focused crates.
- CLI that starts and prints version.
- Formatting, lint, and test CI.
- Docker build of a placeholder binary.

Definition of done:

- A contributor can clone the repository and run `cargo test`.
- CI passes on Linux and macOS.
- The placeholder image runs.

### Milestone 1 — Hello ASP

**Status:** Built

Deliverables:

- ASP page parser for text and `<% %>` blocks.
- Minimal VBScript expression evaluator.
- `Response.Write`.
- HTTP server returns a rendered page.
- `run` and `check` CLI commands.

Definition of done:

- An example page containing HTML and `<%= %>` returns the expected body.
- Syntax and runtime errors are visible and testable.

### Milestone 2 — Core language subset

**Status:** Not started

Deliverables:

- Variables, arithmetic, string concatenation, conditions, loops, arrays, procedures, and conversions.
- Golden tests for each feature.
- Clear unsupported-feature diagnostics.

Definition of done:

- Small self-contained pages run without database, filesystem, or COM dependencies.

### Milestone 3 — Request and response model

**Status:** Not started

Deliverables:

- `Request.QueryString`.
- `Request.Form`.
- `Request.Cookies`.
- `Request.ServerVariables`.
- `Response` controls and cookies.
- Request timeout and size limits.
- Error responses.

Definition of done:

- Form and query-string example applications work end to end.

### Milestone 4 — Includes, sessions, and application state

**Status:** Not started

Deliverables:

- File and virtual includes.
- Session cookie and in-process store.
- `Application` and `Session` objects.
- Basic `global.asa` event handling.

Definition of done:

- A small multi-page stateful application runs.

### Milestone 5 — Filesystem and object mappings

**Status:** Not started

Deliverables:

- Native object interface.
- `Scripting.FileSystemObject`.
- `Scripting.Dictionary`.
- Path sandbox and traversal tests.
- `Server.MapPath`, `Execute`, and `Transfer`.

Definition of done:

- File-backed ASP examples run safely inside the configured app root.

### Milestone 6 — Database support

**Status:** Not started

Deliverables:

- Minimal ADO-compatible interfaces.
- SQLite adapter and example.
- PostgreSQL or MySQL adapter.
- Parameterized queries and error handling.

Definition of done:

- A database-backed ASP example runs in Docker Compose without hardcoded credentials.

### Milestone 7 — Real-application compatibility

**Status:** Not started

Deliverables:

- JScript support if needed by target applications.
- Broader VBScript functions.
- Additional COM mappings.
- Migration tooling and compatibility report.
- Performance profiling.

Definition of done:

- At least one nontrivial real ASP application runs, or its unsupported features are enumerated in a compatibility report.

---

## 9. Compatibility policy

The project should publish a compatibility matrix with three levels:

1. **Supported** — implemented and covered by tests.
2. **Partial** — common behavior works; documented limitations exist.
3. **Unsupported** — emits a clear diagnostic.

Compatibility should not be claimed based on implementation alone. A feature is compatible only when its observable behavior is tested and documented.

Known major compatibility risks:

- Exact VBScript coercion and equality behavior.
- Locale-sensitive formatting.
- IIS-specific `ServerVariables`.
- COM object behavior and side effects.
- ADO cursor, locking, and transaction semantics.
- Codepage and character encoding.
- Error handling and `On Error Resume Next`.
- Session state in multi-instance deployments.
- Windows path and registry behavior.

---

## 10. Open technical questions

Answer these before or during the relevant milestone:

1. Which default codepage and locale behavior should be used for the first release?
2. Should the initial HTTP layer use `axum`, or is a simpler server sufficient?
3. Should sessions initially use signed stateless cookies or an in-memory server-side store keyed by a signed cookie?
4. Which database should be the first supported adapter?
5. Which COM ProgIDs are required by the target applications?
6. How should binary response data and streaming be represented?
7. Should static files be served by the Rust process or delegated to a reverse proxy?
8. What response header and cookie behavior must be preserved for legacy clients?
9. What minimum Rust version will be supported?
10. What are the first realistic ASP fixtures for compatibility testing?

---

## 11. Maintenance checklist for future agents

This checklist applies before every pull request is marked ready for review.

1. Update the relevant **Workstream status** row.
2. Update the matching detailed workstream section.
3. Update the affected milestone status.
4. Update **Immediate next steps** so the merged plan points to the next task.
5. Add or update tests and examples.
6. Run:

   ```bash
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   cargo test --all
   ```

   If the repository has no Rust workspace yet, state which quality checks were possible and which remain pending.
7. Update `README.md` when user-facing behavior changes.
8. Do not expand scope silently; prefer small, verifiable pull requests.
9. Record unsupported behavior explicitly rather than leaving it undocumented.
10. For security-sensitive features, add adversarial tests before marking the feature built.
11. Confirm that the pull request contains both implementation and plan updates. Implementation-only pull requests are incomplete.

---

## 12. Immediate next steps

1. Milestone 2 — core language subset: arrays, procedures (`Sub`/`Function`/`Call`), conversions, `Date`/`Time` functions, and `Exit For`/`Exit Do`.
2. Golden tests for each new language feature as it lands.
3. Milestone 3 prep: finish the Response model (`Redirect`, `ContentType`, cookies) and request timeouts/size limits.
4. Keep `AGENTS.md` in step with new conventions as they emerge.
