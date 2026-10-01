# weir v1.0: decision document

**Date:** 2026-09-29. **Scope:** `~/Documents/otterbridge` at `c2c4a26` plus uncommitted work in 6 files, the live `~/.config/weir/weir.toml`, and the wrappers in `~/.local/bin`. Nothing was modified.

## 0. Spot-check of the reviewers' key claims

| Claim | Result |
|---|---|
| `debug_args` slices `&prompt_text[..200]` by bytes and runs eagerly inside `debug!` | **Holds.** See `src/backends/stdio_cli.rs:79-81` and `:109-115`. A CJK or Vietnamese prompt longer than 200 bytes whose byte 200 falls inside a character will panic. The EchoMeo prompts make this very likely. |
| Retry treats every `WeirError::Backend` as transient | **Holds.** `src/resilience/retry.rs:36-38` does `matches!(err, WeirError::Backend(_))`, and the default is 3 attempts. Timeouts, non-zero exits and spawn failures are all retried. |
| No cwd, env or process-group control | **Holds.** Grepping `src` finds no `current_dir`, `.env(` or `process_group`. The only protection is `kill_on_drop` at `stdio_cli.rs:122`. |
| The WIP removes the 60 s default, but the schema still advertises it | **Holds.** The `config/mod.rs` diff drops `default_timeout()`, and `src/cli/status.rs:93-97` still says `"default": 60`. The live config sets no `timeout_secs` anywhere, so every live backend becomes unbounded. |
| The WIP falls back to `./weir.toml` from the current directory | **Holds.** `src/main.rs:312`. |
| The live `[server]` block is silently ignored | **Holds.** There is no `deny_unknown_fields` anywhere in `src`. |
| Fan-out output order is non-deterministic | **Holds.** `src/engine/fan_out.rs:62-83` pushes results in completion order. |
| The substitution order `{prompt}` → `{model}` rewrites a literal `{model}` inside prompts | **Holds.** `stdio_cli.rs:63-65`. |

**Claims I dropped or corrected:**

- **"Exit 4 = no final text"** (product-fit reviewer): dropped. No wrapper uses exit 4.
- **"Run verification outside the jail"** (product-fit reviewer): **rejected for safety.** The checks run code the agent just wrote. They must run under `agent-jail` too (see §3).

**New facts from reading the wrappers that change the design:**

- **`hermes-worker` has no `-b` flag.** Its `worker` profile hard-codes `base_url: http://localhost:8080/v1` (`~/.hermes/profiles/worker/config.yaml:5`), which is the nginx pool. weir cannot pin hermes to a replica until the wrapper can.
- **`agy-worker` returns exit 3 for both "empty" and quota exhaustion.** The only way to tell them apart is the stderr line `agy-worker: status=… error=…RESOURCE_EXHAUSTED…`.
- **`agy-worker` and `hermes-worker` still put the prompt into argv** via `"$(cat "$PF")"`. The 128 KiB per-argument limit therefore still applies inside those two wrappers. `pi-worker` feeds the prompt on stdin (`< "$pf"`).
- **Only `pi-worker` uses `timeout --kill-after`.** `agy-worker` and `hermes-worker` use a plain `timeout`. `agent-jail` `exec`s `bwrap --die-with-parent`, so grandchildren die when bwrap dies.

## 1. Verdict on code health and the WIP

**Code health: about 6.5/10 as a prompt router, about 2/10 for what the stack needs now.** The build is clean: the review target showed 88 tests passing and no clippy warnings. The module layout is readable. But weir's request model is `messages → argv → stdout`. It has no idea of a workdir, prompt file, typed exit code, replica, quota or worktree. Its retry layer is actively harmful for agents that edit files.

**WIP: rework. Do not commit it as-is, and never commit it straight to main.**

- **Salvage:** the clap `env` feature and `WEIR_CONFIG` / XDG / `$HOME` config resolution. Delete the `./weir.toml` fallback at `main.rs:312`: a repo-controlled `weir.toml` would decide which binaries weir executes. Error out instead, listing the paths searched.
- **Discard:** `timeout_secs: Option<u64>`. In v1 the wall-clock limit moves to the wrapper's mandatory `-t`, plus a weir-side hard deadline. An "unlimited by omission" mode must not exist.
- **Discard:** the fusion judge prompt rewrite. The fusion engine is being deleted (§2). The new prompt also asks for JSON that nothing parses (`fusion.rs:113-123`).
- **Hotfix now, on a branch against 0.5.x:** the `debug_args` byte-slice panic. It is a live crash for the current skill users.

## 2. Role of weir: rewrite, yes

**Yes, rewrite, as v1.0 "task runner and slot scheduler".** It is not a from-scratch rewrite: roughly 35% of the code survives.

**Why.** The Claude Code Workflow tool already does fan-out, sequencing and judge/synthesize better than weir's engines, with subagents that verify and commit. The wrappers already handle jail entry, timeout, empty/denied detection and the summary line. weir currently erases the wrappers' exit codes (`stdio_cli.rs:172-181` folds them into a string) and re-runs agents up to 3 times.

weir is worth keeping only for what no single wrapper and no single Workflow Bash call can do. That work is **shared state across concurrent short-lived processes**:

1. **Lease the single llama.cpp slot per replica**, pinning a worker to `a` or `b` so the prompt-prefix cache stays warm. Time spent waiting for a slot is not charged against the task's timeout.
2. **Track agy quota cooldown** across invocations and fall back along a ladder.
3. **Run one worktree task lifecycle:** create the worktree, run the jailed worker, capture the diff, run jailed checks, emit one JSON record, then apply a cleanup policy.
4. **Keep an append-only JSONL task ledger.** It doubles as resume state and as cost and latency metrics.

**Keep, by module:**

- `src/config/` (the loader and `validate.rs` framework; the schema is replaced).
- `src/observability/tracing_setup.rs`.
- `src/cli/validate.rs` and the `schema` and `status` command scaffolding.
- `error.rs`, as the base for a typed taxonomy.
- The `tests/cli.rs` assert_cmd harness.
- CI (`.github/workflows/ci.yml`), plus `--locked` on every job.
- The hard constraints: no HTTP, no MCP, no server, no API keys, and `stdin=null` on every spawn except the prompt pipe.

**Drop:**

- All of `src/engine/`: `fan_out`, `pipeline`, `eval_loop`, `fusion` and `router`. Their replacements are Workflow scripts calling `weir ask` and `weir task run`, with leases providing the serialization.
- `resilience/{retry,rate_limit,circuit_breaker,resilient_backend}.rs`. They are replaced by the escalation ladder and persisted cooldowns.
- `observability/{metrics,persist}.rs` (about 575 lines). The ledger replaces them.
- The TOML write-back commands (`backend add/remove`, `workflow add/remove`).
- `legacy/`, the untracked `skill/`, and the MCP-era body of `DESIGN.md` (lines 18 onward). Archive it or delete it.

## 3. Target architecture

### 3.1 CLI surface

```
weir task run   --repo PATH [--base develop] [--id ID]
                --worker pi|opencode|hermes|agy [--replica auto|a|b|pool] [--model M]
                [--timeout SECS] [--deadline SECS]
                (--prompt-file F | --prompt-stdin)
                [--check NAME]... [--ladder NAME]
                [--cleanup never|on-success|always]   # default: never (Claude commits)
                [--json]
weir task show ID [--patch]        # ledger record, optionally the full diff
weir task list [--since 24h] [--json]
weir task clean ID|--merged|--older-than 7d   # git worktree remove + branch -D
weir ask        --worker W [--replica ...] [--model M] [--timeout S] (--prompt-file F|--prompt-stdin)
                # read-only Q&A in a fresh /tmp/pilot/<id> scratch dir (the jail's scratch mode)
weir lease status [--json]         # holder pid/task per slot, agy cooldown until; --json: {slots:[...], cooldowns:{agy:{model:unix}}}
weir validate [--deep]             # --deep: wrappers on PATH, wt_root == agent-jail WT_ROOT, bwrap works, agy model ids
weir config path | schema
```

**Exit codes:**

| Code | Meaning |
|---|---|
| 0 | Worker succeeded and all checks passed |
| 10 | Worker succeeded but a check failed |
| 124 | Timeout |
| 3 | Empty output |
| 6 | Denied actions |
| 5 | Jail refused |
| 7 | Quota exhausted and the ladder is exhausted |
| 2 | Usage or config error |

These mirror the wrapper contract, so a Workflow script can branch on them without parsing JSON.

### 3.2 Config schema (`~/.config/weir/weir.toml`, v2)

```toml
version = 2

[paths]
wt_root    = "~/Documents/echomeo-wt"      # must equal agent-jail's WT_ROOT (validate --deep greps it)
state_dir  = "~/.local/state/weir"         # leases/, cooldown.json, ledger.jsonl, tasks/<id>/
scratch    = "/tmp/pilot"

[slots]                                     # capacity-1 leases = one llama.cpp slot each
gpu-a = { capacity = 1 }                    # :18080
gpu-b = { capacity = 1 }                    # :18081
agy   = { capacity = 2 }                    # concurrency cap on the cloud quota

[worker.pi]
command  = "pi-worker"                      # must match ^[a-z]+-worker$ (enforced)
replica  = ["-b", "{replica}"]              # how to pin: a|b|pool
timeout  = 900
slots    = { a = "gpu-a", b = "gpu-b" }     # pool => weir picks a free replica and pins it

[worker.opencode]
command  = "opencode-worker"
replica  = ["-b", "{replica}"]
timeout  = 900
slots    = { a = "gpu-a", b = "gpu-b" }

[worker.hermes]
command  = "hermes-worker"
timeout  = 900
env      = { HERMES_WORKER_PROFILE = "worker-{replica}" }   # needs worker-a/worker-b profiles (see risks)
slots    = { a = "gpu-a", b = "gpu-b" }

[worker.agy]
command  = "agy-worker"
model    = ["-m", "{model}"]
default_model = "gemini-3.8-flash-high"
timeout  = 3600
slots    = { any = "agy" }
quota_pattern = "RESOURCE_EXHAUSTED"        # exit 3 + this in stderr => kind=quota
quota_cooldown = "30m"

[ladder.default]                            # used on timeout|empty|quota, never on denied/jail/check-fail
steps = ["pi:auto", "opencode:auto", "agy:gemini-3.8-flash-high", "agy:claude-opus-4-6-thinking"]
fresh_worktree_per_step = true              # never re-run on a dirty tree

[check.web-typecheck]
cwd = "frontend"
cmd = ["npx", "tsc", "--noEmit", "-p", "tsconfig.app.json"]
timeout = 300
[check.web-lint]
cwd = "frontend"
cmd = ["npm", "run", "lint"]
timeout = 300
[check.backend-tests]
cwd = "backend"
cmd = ["pytest", "-q", "-x"]
timeout = 600
```

Unknown keys are a hard error (`deny_unknown_fields`). `validate` rejects the following:

- Any `command` that is not a `*-worker` name resolving under `~/.local/bin`.
- `timeout = 0`.
- `wt_root` values that differ from `agent-jail`.

### 3.3 Data flow for one task

1. **Admit.** Allocate the ID (`<yyyymmdd>-<slug>-<4hex>`). Write the prompt to `state_dir/tasks/ID/prompt.md` with mode 0600. The prompt never goes into weir's own argv.
2. **Worktree.** Run `git -C <repo> worktree add -b wt/<ID> <wt_root>/<ID> <base>`. This runs outside the jail, as the user, and the branch is never main or develop.
3. **Lease.** For `auto` or `pool`, try `flock -n` on `leases/gpu-a` and then `gpu-b`. If both are held, block on the one the ledger says is least recently used. Record `queue_wait_ms`. Pass the concrete `-b a|b` to the worker, never `pool`, so weir owns placement. **Affinity:** `--affinity KEY` (default: the repo plus the first 2 KB of the prompt, hashed) prefers the replica that last served that key, for prefix-cache warmth. For agy, weir checks `cooldown.json` first; if that **model** is cooling down, the step is recorded as `skipped_cooldown` without spawning and the ladder moves on. `cooldown.json` lives in the lease state root (the same directory as `leases/`, so `weir lease status` can report it without a config) and maps each agy model to the unix second its cooldown ends: `{"agy":{"<model>":1759300000}}`.
4. **Run.** Spawn `<worker> [-b r] [-m M] -t <timeout> <wt> <prompt.md>` with the following settings:
   - `process_group(0)`
   - stdout and stderr piped, each capped at 8 MiB
   - env: the worker's `env` entries layered on weir's environment; there is no allowlist in v1
   - `stdin=null`

   The weir-side hard deadline is `timeout + 60 s`. When it expires, weir sends `killpg(SIGTERM)`, waits 10 s, then sends `SIGKILL`. SIGINT and SIGTERM to weir trigger the same kill. Once the worker exits, weir releases the lease.
5. **Classify** the wrapper exit code: 0 ok, 124 timeout, 3 empty (or quota, if `quota_pattern` appears in stderr), 6 denied, 5 jail refused, 2 usage, anything else error. **weir never retries in place.** On a ladder-eligible kind (timeout, empty, quota), the next step gets a fresh worktree, `<ID>-s2`. One record covers the whole ladder: `attempts` lists every step, `branch`/`worktree`/`diff`/`checks` describe the last tree, and earlier trees are listed in `extra_branches`/`extra_worktrees` so cleanup and `task clean` remove them too. `--timeout` overrides every step; `--replica`/`--model` do not apply to a ladder.
6. **Diff.** Run `git -C wt add -N . && git diff --stat <base>` and `git diff --binary <base> > tasks/ID/patch.diff`. The per-worktree index is writable by design.
7. **Checks.** Each check runs as `agent-jail <wt> -- <cmd>` with its own timeout. It is sandboxed because it executes code the agent wrote. weir records the exit code and the last 60 lines of output.
8. **Emit.** Append one ledger line and print one JSON record to stdout.
9. **Cleanup.** The default is `never`: the Claude subagent reviews, commits in the worktree and merges. `weir task clean --merged` garbage-collects later.

### 3.4 Result schema (`weir task run --json`)

```json
{"schema":"weir.task/1","id":"20260929-fix-tones-3fa2","status":"ok|check_failed|timeout|empty|quota|denied|jail_refused|error",
 "exit":0,"repo":"/home/typangaa/Documents/EchoMeo-Vietnamese","base":"develop",
 "branch":"wt/20260929-fix-tones-3fa2","worktree":"/home/typangaa/Documents/echomeo-wt/20260929-fix-tones-3fa2",
 "attempts":[{"worker":"pi","replica":"a","model":null,"exit":0,"kind":"ok","queue_wait_ms":41200,"elapsed_ms":312000,
              "summary":"pi-worker: backend=a elapsed=312s exit=0","stderr_tail":"..."}],
 "answer":"<worker stdout, capped 64 KiB>",
 "diff":{"files":4,"insertions":37,"deletions":9,"patch":"state/tasks/<id>/patch.diff"},
 "checks":[{"name":"web-typecheck","exit":0,"elapsed_ms":48000,"tail":"..."}],
 "cleanup":"kept"}
```

### 3.5 How Claude Code workflows call it

A Workflow step (Bash inside a subagent) runs:

```
weir task run --repo ~/Documents/EchoMeo-Vietnamese --worker pi --replica auto \
  --prompt-file /tmp/claude-.../task1.md --check web-typecheck --check web-lint --ladder default --json
```

The subagent parses the JSON. On `ok` it reads `patch.diff`, verifies, commits in `worktree` and merges. On `check_failed` it either fixes the problem itself or starts a new `task run` with the check tail appended to the prompt. Parallel Workflow branches simply start more `weir` processes. The flock leases queue them with at most one per GPU slot. There is no daemon, and waiting in the queue does not eat into the task's timeout.

### 3.6 The classifier rule

The existing `autoMode.allow` rule allows **only** the `*-worker` wrappers. A Bash call to `weir task run` is a different command, and routing agent launches through weir to bypass that rule would be wrong. The integration is therefore:

- **(a)** weir's code can only ever exec `*-worker` wrappers (for tasks) or `agent-jail` (for checks). The regex is hard-coded in the source, not configurable, and `validate` rejects anything else.
- **(b)** The **user** decides whether to add an explicit allow rule for `weir task run` and `weir ask`. The rationale is that weir only ever executes jailed wrappers.

Until the user does that, subagents can call the wrappers directly, and weir's leases still apply through a tiny mode: `weir lease run gpu-a -- pi-worker -b a …`. That mode is the same allowlisted wrapper with a prefix. Adding `weir lease run` to the allow rule is also the user's call.

## 4. Migration

### 4.1 Live `~/.config/weir/weir.toml`, entry by entry

| Entry | Action |
|---|---|
| `[server]` block (lines 1-5) | **Delete.** Stale, and silently ignored today. |
| `agy` (`agy -p`, unjailed) | **Replace** with `[worker.agy]`, default model `gemini-3.8-flash-high`. |
| `agy-gemini` ("Gemini 3.5 Flash (Medium)", which no longer exists) | **Delete.** Use `weir ask --worker agy --model gemini-3.8-flash-medium`. |
| `agy-sonnet` | **Delete.** Use `--model` with the Claude Sonnet 4.6 id from `agy models`. |
| `agy-gemini-pro` | **Delete.** Use `--model gemini-3.1-pro-high`. |
| `agy-inline`, `agy-gemini-inline`, `agy-sonnet-inline` | **Delete.** Also delete the three `~/.local/bin/agy-*-inline` shims. `agy-worker --output-format json` makes them redundant. |
| `claude-code` (`claude --print -p`, unjailed, full permissions) | **Delete.** Claude is the orchestrator, not a worker. |
| `hermes-local` (`hermes -z`, default profile, unjailed) | **Replace** with `[worker.hermes]`. |
| `hermes-gemma` (`complex -z`, stale "Gemma on :8081") | **Delete.** |
| `hermes-openrouter` (unjailed, free-tier logging risk, rotating model ids) | **Delete.** No workflow uses it. |
| New | **Add** `[worker.pi]` and `[worker.opencode]`, both absent today. |
| Workflows `dual-local`, `dual-review`, `multi-review`, `draft-then-polish`, `fast-then-agy`, `refine-loop`, `fast-refine-loop`, `local-fusion-agy`, `deep-review` | **Delete all nine.** They reference deleted backends. Reproduce the one worth keeping (`deep-review`) as a Workflow script that fans out `weir ask` over pi:a, pi:b and agy, then asks agy to judge. |
| New | **Add** `[ladder.default]` and the `[check.*]` blocks for EchoMeo. |

### 4.2 The weir Claude skill

- Rewrite `~/.claude/skills/weir/SKILL.md` around `task run`, `ask`, `lease status`, the JSON schema, the exit-code table and the "subagent commits, weir never commits" rule.
- Remove all mentions of Qwen3.6, Gemma, hermes-openrouter, fusion, pipeline, eval-loop and `--config`.
- Ship a generic copy with no personal paths at `examples/skill/SKILL.md` in the repo, and delete the untracked `skill/` directory.
- Update the `hermes-delegate` skill to point at `hermes-worker`.

## 5. Phased plan

Each phase is one branch and one PR against main. Nothing is pushed to main directly. Conventional commit prefixes throughout.

- **P0: `fix/v0.5-hotfix` (0.5.1).** Fix the char-boundary truncation in `debug_args`, evaluated only when debug is enabled. Substitute placeholders in a single pass. Update the live config: drop `[server]` and move the 3.5 model name to a 3.8 id.
  *Accept:* a unit test with a 300-character CJK prompt, and a test that a prompt containing `{model}` is left untouched.
- **P1: `feat/config-v2`.** New schema with `deny_unknown_fields`. Config resolution via `WEIR_CONFIG`, XDG or `$HOME`, erroring on miss. `worker-name` regex enforcement. `validate --deep`. Typed `ExitKind`.
  *Accept:* CLI tests covering resolution order, rejection of a `command = "agy"` worker, rejection of `timeout = 0`, and detection of a `wt_root` mismatch against a fixture `agent-jail`.
- **P2: `feat/spawn-core`.** Worker spawn with `process_group(0)`, capped output, a hard deadline, a SIGTERM/SIGINT handler that kills the process group, and exit classification including the quota pattern.
  *Accept:* a fake wrapper that forks a sleeping grandchild; after the deadline, `pgrep` finds nothing. A fake wrapper exiting 3 with `RESOURCE_EXHAUSTED` classifies as `quota`.
- **P3: `feat/leases`.** Flock leases, `weir lease run|status`, affinity, `queue_wait_ms`, `weir ask` in scratch mode.
  *Accept:* four concurrent `weir ask --replica auto` calls against a fake wrapper that sleeps 2 s. Wall time is about 4 s, at most one holder per slot is ever observed, and the queue wait is excluded from the timeout.
- **P4: `feat/task-run`.** Worktree creation, diff, jailed checks, ledger, JSON output, `task show|list|clean`.
  *Accept:* against a temp git repo with a fake `agent-jail`, the fake worker edits a file, the JSON has `diff.files == 1`, a failing check yields exit 10, and the ledger line round-trips through `task show`.
- **P5: `feat/ladder`.** Escalation ladder with a fresh worktree per step, plus persisted `cooldown.json`.
  *Accept:* quota at step 1 moves to step 2 and sets a cooldown, and a second run skips agy. `denied` and `check_failed` never escalate.
- **P6: `chore!: remove v0 engines`.** Delete `engine/`, `resilience/`, `metrics` and `persist`, the write-back commands and `legacy/`. Rewrite `DESIGN.md` (about 100 lines), add a CHANGELOG, update CLAUDE.md and the skill, and add a tag-triggered release workflow. Tag v1.0.0.
  *Accept:* the test suite is green, the binary is under 2 MB, and the CLAUDE.md smoke test runs `weir task run` against `/tmp/pilot` with the real `pi-worker`.

**Test strategy.** All unit and CLI tests use fixture wrappers in `tests/fixtures/bin/` placed first on PATH, and a fake `agent-jail` that checks its arguments and then `exec`s. The real jail and GPUs appear only in one manual smoke script, `scripts/smoke-live.sh`, which uses `/tmp/pilot` and one `echomeo-wt` task. Builds go to a non-repo target dir; the repo's `target/` is 5.8 GB and should be cleaned.

**Risks.**

1. **Classifier rule.** weir-mediated launches need the user to extend `autoMode.allow` explicitly. Until then, use direct wrappers, optionally prefixed with `weir lease run`.
2. **Hermes pinning** needs `worker-a` and `worker-b` profiles, or a `-b` flag in `hermes-worker`, pointing at `:18080` and `:18081`. Today every hermes run goes through the pool, so its lease is advisory only.
3. **argv limit.** `agy-worker` and `hermes-worker` still expand the prompt into argv. Change both to stdin or a file, or have weir reject prompts over 100 KiB for those workers.
4. **Direct pool traffic.** Leases only coordinate weir processes. Any direct `-b pool` caller, including nginx users outside weir, can still contend for a slot. weir itself should always pin.
5. **Worktree root is duplicated** between the weir config and `agent-jail`. `validate --deep` guards it; the better fix is to export `WT_ROOT` from one sourced file.
6. **Checks run agent-written code.** They run under the jail, which has no network restriction; add `--unshare-net` to a check-mode jail later.
7. **Quota detection** depends on the stderr text of `agy-worker`. Pin it with a fixture test and keep the pattern configurable.