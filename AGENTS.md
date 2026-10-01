# Agent guide

Classic ASP interpreter in Rust. Keep changes small; one milestone per PR.

## Gates (run before every push, in order)

Commits run them automatically via the pre-commit hook — enable once with
`git config core.hooksPath .githooks`. To run them by hand:

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --all
```

## Plan discipline

- `docs/asp-classic-interpreter-plan.md` is the contract: every PR updates its status rows, workstream sections, and next steps in the same PR.
- Mark **Built** only when tested and documented; unsupported constructs raise clear "not supported in milestone N" diagnostics.

## Rust conventions

- Edition 2024; let chains available. No `#[allow(dead_code)]` or stub pub items for future milestones — add code when its milestone lands.
- One type alias, not two; no "marker alias kept from earlier drafts".
- Error paths use `AspError` variants with `Diagnostic`; never panic on user input.
- Parse once: don't call `Page::parse` / `parse_block` for validation and then again for use.
- Use plain text in byte/string literals (`b"-->"`, not escapes like `\x3e`).
- Golden tests assert exact bodies/statuses; explain any golden diff before updating fixtures.
- Keep path confinement (`fs::confined_join`) and include-cycle detection intact — they are security gates, not polish.

## Architecture invariants

- Cross-block loops flatten into one statement stream via `flatten_steps`; the parser emits **deferred delimiters** (`ForLoopOpen`/`Next`, `DoOpen`/`LoopClose`/`DoClose`, `ProcOpen`/`ProcClose`) and `exec_block_loops` runs a normalization pass that pairs them (rejecting mismatched closers), hoists `Sub`/`Function` declarations, and executes the resulting nested statement tree — that pass is what keeps nested loops and hoisted procedures correct.
- `Response.Write <expr>` takes a full expression; a leading `(` is a parenthesised operand.
- Statements separate on `Tok::LineEnd`; `consume_loop_tail` eats only the optional `Next i` name, never `While`/`Until`/`Loop`/`For`.
- Session cookies are HMAC-signed (`asp-core::cookie_sign`, per-process key owned by `SessionManager`); decoding verifies the signature, so a tampered cookie is just "no session". `Session_OnEnd` fires on abandon and idle expiry (resolve-time drops park in a pending queue and report at commit); `Application_OnEnd` is intentionally NOT fired — IIS fires it on application recycle, a process-lifetime event. End events see only `Session` + `Application`, never `Request`/`Response`/`Server`.
- Ordinary HTML comments pass through; only `<!-- #include ... -->` is consumed.
- Goldens may change only with an explained diff (e.g. the M2 fix that stopped the loop body running once before the first iteration).
