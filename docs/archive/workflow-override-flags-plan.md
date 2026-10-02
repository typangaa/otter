# weir — call-time workflow backend overrides (plan-first)

> Status: **DRAFT — awaiting review.** No code written yet.
> Target release: **v0.5.0** (new capability, backward-compatible — no flag means
> the `weir.toml` workflow definition is used unchanged).

## 1. Goal

Let the caller (a human, or Claude via the `/weir` skill) choose which backend
CLIs a workflow uses **at call time**, without editing `weir.toml`. Today the
backend membership of every workflow is fixed in config; the only call-time knobs
are `--criteria` (eval-loop) and, for `chat`, `--model`.

Contract: **no flag → use the TOML default; flag present → replace that slot.**

## 2. Decisions (from review Q&A)

- **D1 Scope:** all patterns / all roles are overridable.
- **D2 Semantics:** a given flag **fully replaces** the corresponding TOML value
  (not append). Slots with no flag keep their TOML value.
- **D3 Validation:** every backend named by a flag **must already exist** as a
  `[[backend]]` in `weir.toml`; otherwise fail fast with a clear error *before*
  the workflow runs.
- **D4 (defaulted, see §7):** passing a flag that the target pattern does not use
  is an **error** (fail-fast), not a silent no-op.

## 3. CLI surface

New flags on `weir workflow run NAME PROMPT [...]` (all optional, mirror the
existing `workflow add` flag names for consistency):

| Flag (repeatable?) | Overrides | Applies to pattern(s) |
|---|---|---|
| `--backend NAME` (repeat) | the `backends` list | fan-out, router (uses first), fusion (panel) |
| `--step "BACKEND[:TEMPLATE]"` (repeat) | the pipeline `steps` | pipeline |
| `--generator NAME` | `generator` | eval-loop |
| `--evaluator NAME` | `evaluator` | eval-loop |
| `--judge NAME` | `judge` | fusion |
| `--synthesizer NAME` | `synthesizer` | fusion |

Notes:
- `--step` reuses the exact `BACKEND[:TEMPLATE]` parsing already used by
  `weir workflow add pipeline` (extract it into a shared helper). `{{step.output}}`
  is the template token (per v0.4.1).
- `--criteria` already exists and is unchanged.
- `router` takes `backends.first()`, so `--backend a --backend b` on a router
  picks `a` (consistent with current behaviour).

### Examples
```bash
# fan-out to a different pair this one time
weir workflow run dual-review --backend agy --backend hermes-local "..."

# swap the fusion judge/synth without touching the panel
weir workflow run deep-review --judge claude-code --synthesizer agy "..."

# eval-loop with a different generator
weir workflow run quality-loop --generator hermes-local --criteria "..." "..."
```

## 4. Behaviour per pattern (effective config)

For each run: start from the TOML `WorkflowConfig`, apply whichever flags are
present (replace), then dispatch exactly as today. Concretely:

| Pattern | Reads (after override) |
|---|---|
| fan-out | `backends` |
| router  | `backends.first()` |
| pipeline | `steps` |
| eval-loop | `generator`, `evaluator` (+ `max_iterations`, `--criteria`) |
| fusion | `backends` (panel), `judge`, `synthesizer` (defaults to judge) |

## 5. Implementation

- **`src/main.rs`**
  - Extend `WorkflowRunArgs` with the six new optional fields (`backends:
    Vec<String>`, `steps: Vec<String>`, `generator/evaluator/judge/synthesizer:
    Option<String>`).
  - New helper `fn effective_workflow(wf: &WorkflowConfig, args: &WorkflowRunArgs)
    -> Result<WorkflowConfig>`: clone `wf`, replace fields from any present flags
    (parsing `--step` via the shared helper), and run §6 validation. Returns the
    effective config; `run_workflow_inner` then uses it in place of the looked-up
    `wf`.
  - Extract the `BACKEND[:TEMPLATE]` → `PipelineStep` parse currently inline in
    the `workflow add pipeline` dispatch into a reusable `fn parse_step(&str) ->
    PipelineStep` (used by both add and run).
- **No engine changes.** Engines already take resolved backends/steps; we only
  change what config they receive.
- **No `config/mod.rs` schema change** — overrides are call-time only, never
  written to disk.

## 6. Validation (fail-fast, before running)

After computing the effective config:
1. **Existence (D3):** every referenced backend name (`backends`, every
   `steps[].backend`, `generator`, `evaluator`, `judge`, `synthesizer`) must be a
   defined `[[backend]]`. Else `WeirError::Validation("backend 'X' not found …")`.
2. **Applicability (D4):** if a flag is set that the pattern does not consume
   (e.g. `--judge` on a fan-out, `--step` on a fusion), error with a message
   naming the flag and the pattern. (Reuse a small per-pattern allowed-flags set.)
3. **Required-after-override:** the pattern's required slots must still be
   non-empty (e.g. eval-loop must end up with a generator and evaluator). Existing
   dispatch already errors on missing generator/evaluator/judge; keep that.

## 7. Defaulted design choice (open for review)

**D4 — inapplicable flags = error (recommended).** Rationale: consistent with the
fail-fast validation you chose (D3); silent no-ops would let a skill quietly send
`--judge` to a fan-out and wonder why nothing changed. Alternative is to ignore
irrelevant flags. Flip this at review if you prefer lenient behaviour.

## 8. Tests

- **Integration (`tests/cli.rs`, deterministic POSIX backends):**
  - fan-out: `--backend echoer` (single) → results length 1, only echoer.
  - fan-out: replace semantics — TOML `dual` is [echoer, fixed]; run with
    `--backend fixed` → only `fixed` (proves replace, not append).
  - fusion: `--judge`/`--synthesizer` swap changes `synthesis` output.
  - eval-loop: `--generator`/`--evaluator` override drives the run.
  - pipeline: `--step "echoer" --step "echoer:prev: {{step.output}}"` → resolves.
  - validation: `--backend ghost` → non-zero exit, "not found".
  - applicability: `--judge x` on a fan-out workflow → non-zero exit.
  - **regression:** running each workflow with NO flags still uses TOML default
    (already covered by existing tests; keep them green).
- **Unit:** `effective_workflow` + `parse_step` direct tests (replace, untouched
  slots preserved, applicability errors).

## 9. Docs / skill updates

- `~/.claude/skills/weir/SKILL.md`: document the override flags + an example so
  Claude knows it can pick CLIs per call (this is the main motivation).
- `README.md` CLI reference + `CLAUDE.md` if it lists `workflow run` flags.
- Help text on the new args (clap doc comments).

## 10. Commit sequence

1. `feat(workflow): call-time backend overrides for workflow run` — args +
   `effective_workflow` + `parse_step` extraction + validation (§5–6).
2. `test(cli): cover workflow backend overrides` — §8.
3. `docs: document workflow override flags` — skill + README + CLAUDE (§9).
4. `chore(release): v0.5.0` — Cargo.toml bump + README roadmap.

Each commit: `cargo fmt --all && clippy -D warnings && cargo test` green. Push
only when asked.

## 11. Non-goals (this change)

- No fully ad-hoc/nameless workflows (`--pattern fan-out` with no TOML entry) —
  the workflow must still exist in `weir.toml`; flags only override its slots.
- No new backend *definitions* at call time — overrides reference existing
  `[[backend]]`s only (D3).
- No change to `chat` (it already selects the backend positionally).
