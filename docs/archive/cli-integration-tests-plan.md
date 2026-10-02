# weir — CLI integration tests (plan-first)

> Status: **APPROVED — decisions made (D1=C, D2=add deps).**
> Scope: add end-to-end tests that drive the built `weir` binary, plus fix a
> pipeline template bug those tests would have caught.

## 1. Motivation

The engines have unit tests (via `src/engine/test_support.rs` MockBackend), but
nothing exercises the actual binary: arg parsing, config load → validate, real
`StdioCliBackend` process spawning, and the JSON output contracts the `/weir`
skill depends on. The original v0.4 hardening list called this "the one remaining
coverage gap." (The earlier "serve + MCP e2e" item is obsolete — that server was
removed in v0.4.)

## 2. Prerequisite bug fix — pipeline template token (HIGH)

`engine/pipeline.rs:56` substitutes **`{{step.output}}`**, but every piece of
user-facing surface — `main.rs:211` help, `status.rs:174` schema, all 7 example
TOMLs, README, CLAUDE.md, the `/weir` skill — documents **`{input}`**. The two
never match, so **every shipped example pipeline is broken**: the engine fails to
find `{{step.output}}`, leaves the template verbatim, and forwards the literal
text `...{input}` to the next backend instead of the previous step's output.

This must be fixed first; the pipeline integration test (4.3) then locks it in.

**Decision (D1 = option C):** keep the engine token `{{step.output}}` as the one
canonical form (engine code unchanged) and correct every doc/example/help line to
match. So the "fix" is documentation-only — the pipeline already works today if
you write `{{step.output}}`; the bug was that nothing told users that.

Files to change (`{input}` → `{{step.output}}`): `src/main.rs:211` (help),
`src/cli/status.rs:174` (schema desc), all 7 example TOMLs (`weir.example.toml`
×3 + `examples/providers/{claude-code,gemini,openrouter}.toml` +
`examples/use-cases/{local-only,full-stack,free-cloud}.toml`), `README.md` ×3,
`CLAUDE.md:68`, and the out-of-repo `~/.claude/skills/weir/SKILL.md` ×2. Engine
and its unit test are untouched.

## 3. Test harness

- **New dev-deps:** `assert_cmd = "2"` (run the built binary, assert exit code +
  stdout/stderr) and `predicates = "3"` (matchers). Add to `[dev-dependencies]`.
- **Layout:** new `tests/cli.rs` (Cargo treats each file under `tests/` as its
  own integration crate, compiled against the built `weir` binary).
- **Deterministic backends without installing agy/hermes:** define a throwaway
  `weir.toml` per test in a `tempfile::TempDir`, whose backends are plain POSIX
  tools that always exist on the CI runner:
  | test backend | command / args | behaviour |
  |---|---|---|
  | `echoer` | `echo` `["{prompt}"]` | echoes the prompt back (tests `{prompt}` plumbing + capture) |
  | `fixed` | `printf` `["fixed-output"]` | constant output (deterministic asserts) |
  | `failer` | `sh` `["-c","exit 3"]` | non-zero exit → error path |
  | `slow` | `sh` `["-c","sleep 5"]` + `timeout_secs=1` | timeout path |
- **Portability:** gate the suite with `#![cfg(unix)]` (CI is ubuntu; POSIX shell
  assumed). Documented in the file header.

## 4. Test cases

### 4.1 `weir chat`
- `chat echoer "hello"` → exit 0, stdout contains `hello`.
- `--json chat fixed "x"` → stdout parses as JSON, `.backend == "fixed"`,
  `.content == "fixed-output"`.
- `chat failer "x"` → non-zero exit, stderr mentions the backend.
- `chat nonexistent "x"` → non-zero exit, "backend not found"-style message.

### 4.2 fan-out workflow
- `--json workflow run dual "x"` (fan-out: echoer + fixed) → `.pattern ==
  "fan-out"`, `.results` length 2, each has `backend`/`content`.

### 4.3 pipeline workflow (the regression guard)
- pipeline `[echoer, second]` where step 2 has
  `prompt_template = "prev: {{step.output}}"` and backend `echo` `["{prompt}"]`.
- Assert step 2's echoed output contains `prev: hello` — i.e. `{{step.output}}`
  really resolved to step 1's output. Guards the token contract going forward.

### 4.4 config-management commands
- `validate --json` on a good config → exit 0, `.valid == true` (or equivalent).
- `validate` on a malformed TOML → non-zero exit.
- `backend list --json` → lists the configured backend names.
- `--version` → prints the crate version.

## 5. CI impact

`cargo test --all-targets` (already the CI `test` job) auto-discovers `tests/`,
so no workflow change. Slowest test is the 1s timeout case; total suite ≈ a few
seconds. `assert_cmd` builds the binary once and caches it.

## 6. Commit sequence

1. `docs: align pipeline template token on {{step.output}}` — §2 doc/example
   sweep (engine unchanged).
2. `test(cli): end-to-end integration tests for chat/workflow/validate` — add
   `assert_cmd` + `predicates` dev-deps + `tests/cli.rs` (§3–4).

Each commit: `cargo fmt --all && clippy -D warnings && cargo test` green before
committing. Push only when asked.

## 7. Decisions (resolved)

- **D1 — pipeline token: option C.** `{{step.output}}` stays the single canonical
  token; engine untouched; all docs/examples/help/schema/skill corrected to match.
- **D2 — dev-deps: yes.** Add `assert_cmd = "2"` + `predicates = "3"`.
