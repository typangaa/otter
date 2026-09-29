# weir

**weir** is a single-binary CLI agent orchestrator that drives a fleet of local
**CLI agents** through one unified command. It spawns CLIs — it is neither an HTTP
client nor an HTTP server, and it handles no API keys.

```
weir  ──spawns──▶  hermes   (local / OpenRouter)
               ──▶  claude   (Claude Code CLI)
               ──▶  gemini   (Gemini CLI)
               ──▶  ollama run  (local models)
```

You drive it from the shell (`weir chat`, `weir workflow run`) or from Claude
Code via the bundled `/weir` skill, which calls the same CLI.

## Why weir?

| | weir | Python wrappers | Go gateways |
|---|---|---|---|
| Distribution | **single binary, zero deps** | virtualenv / uv | binary + config |
| Binary size | **~1.6 MB** | ~50–200 MB | ~10–20 MB |
| LLM-native workflows | fan-out, pipeline, eval-loop, fusion | none | none |
| API keys in config | **none ever** (CLI agents own auth) | often inline | varies |
| Interface | **single CLI** (scriptable, skill-friendly) | varies | varies |

Every command reads the same `weir.toml` and supports all backends and workflows.

## Install

### Pre-built binary (coming soon)

```sh
curl -fsSL https://github.com/typangaa/otter/releases/latest/download/weir-linux-x86_64 \
  -o ~/.local/bin/weir && chmod +x ~/.local/bin/weir
```

### Build from source

```sh
git clone https://github.com/typangaa/otter
cd otter
cargo build --release
cp target/release/weir ~/.local/bin/weir   # or any directory in $PATH
```

Requires Rust 1.75+. No other system dependencies.

## Quick start

**1. Create a config:**

```sh
mkdir -p ~/.config/weir
cp weir.example.toml ~/.config/weir/weir.toml
```

**2. Make sure any CLI agents you reference are installed and logged in.**

weir handles no API keys and makes no network calls of its own. Every backend
is a CLI agent (hermes, claude, agy, gemini, `ollama run`, …) that you have
already installed and logged in — each owns its own auth and network access.

**3. Validate:**

```sh
weir validate --config ~/.config/weir/weir.toml --json
# {"backend_count":3,"status":"ok","workflow_count":2}
```

**4. Use as a Claude Code skill (optional):**

Copy the skill file:
```sh
mkdir -p ~/.claude/skills/weir
cp skill/SKILL.md ~/.claude/skills/weir/SKILL.md
```

Then invoke with `/weir` in Claude Code, or Claude will use it automatically
when you ask to "chat with agy", "ask hermes", "fan-out to all backends", etc.
The skill simply calls the `weir` CLI from the shell.

## CLI usage

The quickest way to call a backend or run a workflow:

```sh
# Chat with any backend
weir --config ~/.config/weir/weir.toml chat agy "Explain Rust's borrow checker"
weir --config ~/.config/weir/weir.toml chat hermes "Summarise: $(cat notes.txt)"

# Pipe from stdin
cat long_doc.txt | weir --config ~/.config/weir/weir.toml chat agy -

# With system message
weir --config ~/.config/weir/weir.toml chat agy \
  --system "You are a terse code reviewer." \
  "Review: $(cat src/main.rs)"

# Machine-readable JSON
weir --config ~/.config/weir/weir.toml --json chat agy "What is 2+2?"
# → {"backend":"agy","content":"4\n"}

# Run a fan-out workflow (parallel responses from multiple backends)
weir --config ~/.config/weir/weir.toml workflow run dual-review \
  "What are the trade-offs of async Rust?"

# Run a pipeline workflow (sequential, each step refines the previous)
weir --config ~/.config/weir/weir.toml workflow run draft-then-polish \
  "Write a README for a CLI tool"

# Run an eval-loop workflow (iterate until criteria met)
weir --config ~/.config/weir/weir.toml workflow run quality-loop \
  --criteria "Must be under 50 words and cite a specific Rust feature" \
  "Describe what makes Rust unique"

# JSON output for any workflow
weir --config ~/.config/weir/weir.toml --json workflow run dual-review "PROMPT"

# Override which backends a workflow uses, at call time (no flag = weir.toml default)
weir --config ~/.config/weir/weir.toml workflow run dual-review \
  --backend agy --backend hermes-local "..."          # replace the fan-out list
weir --config ~/.config/weir/weir.toml workflow run deep-review \
  --judge claude-code --synthesizer agy "..."         # swap fusion roles
```

Per-pattern override flags on `workflow run` (each flag fully replaces that slot;
referenced backends must exist in `weir.toml`):

| Flag | Overrides | Pattern(s) |
|------|-----------|------------|
| `--backend NAME` (repeat) | backend list | fan-out / router / fusion panel |
| `--step BACKEND[:TEMPLATE]` (repeat) | pipeline steps | pipeline |
| `--generator` / `--evaluator` | loop roles | eval-loop |
| `--judge` / `--synthesizer` | fusion roles | fusion |

Fan-out JSON output:
```json
{
  "workflow": "dual-review",
  "pattern": "fan-out",
  "results": [
    {"backend": "agy",    "content": "..."},
    {"backend": "hermes", "content": "..."}
  ]
}
```

## Configuration (`weir.toml`)

TOML is the single source of truth. See [`weir.example.toml`](weir.example.toml) for a
fully annotated example.

### `[[backend]]` — stdio CLI (the only backend type)

```toml
[[backend]]
name         = "hermes"
type         = "stdio-cli"
command      = "hermes"
args         = ["-z", "{prompt}"]   # {prompt} is replaced at call time
timeout_secs = 180
```

The process is spawned, stdout captured as the response, then exits. Works with
any CLI agent that supports a oneshot / headless mode (hermes `-z`, agy `-p`,
llamafile `--oneshot`, etc.).

### `[[workflow]]` — fan-out

```toml
[[workflow]]
name        = "multi-review"
pattern     = "fan-out"
backends    = ["ollama", "hermes"]
aggregation = "all"
```

All backends called in parallel. Returns an array of responses.

### `[[workflow]]` — pipeline

```toml
[[workflow]]
name    = "draft-then-polish"
pattern = "pipeline"

[[workflow.steps]]
backend = "ollama"
role    = "drafter"

[[workflow.steps]]
backend          = "hermes"
role             = "polisher"
prompt_template  = "Refine this draft:\n\n{{step.output}}"
```

Each step receives the previous step's output. Use `{{step.output}}` in
`prompt_template` to inject it.

### `[[workflow]]` — eval-loop

```toml
[[workflow]]
name           = "quality-loop"
pattern        = "eval-loop"
generator      = "ollama"
evaluator      = "hermes"
max_iterations = 5
```

Generator produces a response; evaluator scores it against caller-supplied
criteria. Loops until evaluator says `PASS` or `max_iterations` is reached.

### `[[workflow]]` — router

```toml
[[workflow]]
name     = "fast-path"
pattern  = "router"
backends = ["ollama"]
```

Explicit single-backend dispatch. Useful for aliasing backends by role.

## Full CLI reference

All commands accept `--json` for machine-readable output and
`--config PATH` to override the default `weir.toml`.

```
weir [--config PATH] [--json] [--log-level LEVEL] [--log-format pretty|json] <COMMAND>

Inference commands:
  chat <BACKEND> <PROMPT>           Call a backend directly, print response
  chat <BACKEND> -                  Read prompt from stdin
  chat <BACKEND> [--system MSG]     Prepend a system message
  chat <BACKEND> [--max-tokens N]   Cap generation length
  workflow run <NAME> <PROMPT>      Run any workflow (fan-out/pipeline/router/eval-loop/fusion)
  workflow run <NAME> --criteria C  Criteria for eval-loop workflows
  workflow run <NAME> --backend B   Override the backend list (repeat); also
                                    --step/--generator/--evaluator/--judge/--synthesizer

GPU slot leases (no weir.toml needed):
  lease run --slot NAME=REPLICA \
    [--slot NAME=REPLICA]...        Acquire a free slot, run CMD under the lease
    [--affinity KEY]
    [--wait-timeout SECS]
    -- <CMD> [ARGS]...
  lease status [--json]             List every known slot: held / free (+ record)

Config management:
  validate                          Validate weir.toml and exit
  backend list                      List configured backends
  backend test <NAME>               Check backend connectivity
  backend add cli <NAME> \
    --command CMD [--arg ARG]...    Add a stdio-CLI backend
  backend remove <NAME>             Remove a backend
  workflow list                     List configured workflows
  workflow add fanout <NAME> \
    --backend B... [--aggregation all]
  workflow add pipeline <NAME> \
    --step B[:TEMPLATE]...
  workflow remove <NAME>
  status                            Print config summary
  version                           Print version info
  schema                            Print JSON Schema for weir.toml
```

## GPU slot leases

`weir lease` gates commands that need an exclusive GPU (or one replica of a
model server). It is independent of `weir.toml` — no config file is read.

```sh
weir lease run --slot gpu-a=a --slot gpu-b=b -- pi-worker -b {replica} -t 900 <worktree> <prompt.md>
weir lease status          # gpu-a  held  pid=123 replica=a command=pi-worker -b a …
weir lease status --json   # [{"slot":"gpu-a","state":"held","record":{…}}, …]
```

- **Slots.** Each `--slot NAME=REPLICA` is one mutually-exclusive slot. weir
  tries them in the order given (`--affinity KEY` first prefers the slot that
  last served `KEY`), taking a non-blocking `flock(2)`; if every slot is busy it
  polls every 250 ms. `--wait-timeout SECS` bounds that wait — on expiry weir
  prints to stderr and exits **75** (`EX_TEMPFAIL`); the default is to wait
  forever.
- **`{replica}`** is replaced in every argument with the replica of the slot that
  won, so one command line fans out over N replicas.
- **Allowed commands.** Only an executable whose basename is exactly `pi-worker`
  or `agy-worker` may take a lease. Anything else fails with exit **2** before a
  lock file is even created. This list is hard-coded, not configurable. The
  check inspects the command **basename only**, so callers must ensure the
  resolved executable comes from a trusted location (i.e. via `PATH`).
- **State.** Lock files live in
  `${XDG_STATE_HOME:-$HOME/.local/state}/weir/leases/<NAME>.lock` (a slot shows
  up in `lease status` once its file exists, i.e. after its first `lease run`);
  affinity state in `.../weir/affinity.json`. Set `WEIR_STATE_DIR` to relocate
  the whole state root (`$WEIR_STATE_DIR/leases`, `$WEIR_STATE_DIR/affinity.json`).
  Each lock file holds a JSON record of the current holder
  (`slot`, `replica`, `pid`, `command`, `started_at`).
- **Cleanup.** The child runs in its own process group; `flock` releases the slot
  even if weir is killed, and a SIGINT/SIGTERM sent to weir is forwarded to the
  child's process group (SIGKILL after 10 s if it survives). weir exits with the
  child's exit code (`128+signal` if it died on a signal) and prints exactly one
  summary line to stderr:
  `weir lease: slot=gpu-a replica=a queue_wait_ms=0 elapsed_ms=90412 exit=0`.

## Observability

**Logging** — structured logs to stderr (stdout stays clean for command output).
Pretty format by default; JSON for machine parsing.

```sh
weir --log-format json --log-level debug chat agy "..."   # JSON logs
RUST_LOG=weir=debug weir chat agy "..."                   # filter to weir only
```

**Metrics** — per-backend counters persisted across invocations:

```sh
weir status --json
```

## Security

- **weir handles no API keys.** There is no key/auth field of any kind in
  `weir.toml`. Every backend is a `stdio-cli` agent that owns its own
  credentials. weir never reads, stores, or forwards a secret.
- **No network surface.** weir is not an HTTP client (it spawns CLIs, it does not
  call `/v1/chat/completions`) and not an HTTP server (it opens no port and
  listens on no socket).

## Architecture

```
weir.toml (TOML source of truth)
    │
    └─── CLI (clap) ──▶ weir chat / weir workflow run / weir backend / …
              │
              ▼  one-shot Config::load → validate (syntactic → semantic → resilience)
              │
              ▼
         Engine ──▶ Backend::chat()  (wrapped by ResilientBackend:
              └── StdioCliBackend         retry → rate-limit → breaker)
                  (tokio::process oneshot, stdin=Stdio::null())

Engines:
  fan_out   → tokio JoinSet (parallel)
  pipeline  → sequential chain with {{step.output}} template substitution
  router    → explicit single backend
  eval_loop → generator ↔ evaluator loop until PASS / max_iterations
  fusion    → panel fan-out → judge JSON analysis → synthesizer
```

## Codebase layout

```
src/
├── main.rs                  # clap CLI, dispatch, run_chat, run_workflow
├── error.rs                 # WeirError + Result<T>
├── config/
│   ├── mod.rs               # Config, BackendConfig, WorkflowConfig (serde)
│   └── validate.rs          # 3-layer validation (syntactic → semantic → resilience)
├── backends/
│   ├── mod.rs               # Backend trait, ChatRequest/Response
│   └── stdio_cli.rs         # tokio::process oneshot (stdin=Stdio::null()) — only backend
├── engine/
│   ├── fan_out.rs           # parallel JoinSet
│   ├── pipeline.rs          # sequential + template substitution
│   ├── router.rs            # explicit dispatch
│   ├── eval_loop.rs         # gen ↔ eval loop
│   └── fusion.rs            # panel → judge → synthesizer
├── resilience/
│   ├── circuit_breaker.rs   # half-open state machine (wired v0.2)
│   ├── retry.rs             # exp backoff + deterministic jitter (wired v0.2)
│   ├── rate_limit.rs        # token bucket (wired v0.2)
│   └── resilient_backend.rs # decorator wrapping every backend call
├── cli/
│   ├── backend.rs           # backend subcommands (toml_edit write-back)
│   ├── workflow.rs          # workflow subcommands
│   ├── validate.rs          # `weir validate`
│   └── status.rs            # version, schema
└── observability/
    ├── metrics.rs           # per-backend AtomicU64 counters (wired v0.2)
    ├── persist.rs           # metrics snapshot → ~/.local/state/weir/metrics.json
    └── tracing_setup.rs     # tracing-subscriber init
```

## Development

```sh
cargo test                                          # run all 71 tests
cargo fmt --all --check                            # formatting (default rustfmt)
cargo clippy --all-targets -- -D warnings          # lint (zero-warning policy)
cargo build --release                              # ~1.6 MB binary
./target/release/weir validate --config weir.example.toml --json
```

All four gates (fmt / clippy / test / release build) run in CI on every push
and PR — see [`.github/workflows/ci.yml`](.github/workflows/ci.yml).

## Roadmap

- [x] v0.1 — Core backends + fan-out / pipeline / router / eval-loop; `weir chat` /
  `weir workflow run`; `backend`/`workflow` write-back; Claude Code skill
- [x] v0.2 — Resilience (retry + circuit breaker + rate limiter via `ResilientBackend`);
  per-backend metrics persisted to disk + `weir status`
- [x] v0.3 — Narrowed to a pure stdio-cli orchestrator: removed the openai-compat HTTP
  client (and the `reqwest`/TLS deps → ~2.5 MB binary) and all API-key handling
- [x] v0.4 — Removed the MCP server and the hot-reload config layer (dropped
  rmcp/schemars/arc-swap/notify → ~1.6 MB binary); weir is now a focused
  single-binary CLI agent orchestrator (engine unit tests + CI added)
- [x] v0.4.1 — Hardening: end-to-end CLI integration tests (`tests/cli.rs`),
  pipeline template token aligned on `{{step.output}}`, and stale
  openai-compat/MCP references and dead code removed
- [x] v0.5.0 — Call-time backend overrides for `workflow run`
  (`--backend`/`--step`/`--generator`/`--evaluator`/`--judge`/`--synthesizer`):
  choose which CLIs a workflow uses per call without editing `weir.toml`
- [ ] v1.0 — Stable config schema; backwards-compatibility guarantee

**Non-goals:** weir will not become an HTTP client (`/v1/chat/completions`), an
HTTP server, or an MCP server. It spawns local agent CLIs and nothing else. Wrap
HTTP-only model servers in a CLI (e.g. `ollama run`) instead.

## Legacy Python v1

The original Python FastMCP server is preserved in [`legacy/`](legacy/).

## License

Apache-2.0. See [LICENSE](LICENSE).

## Contributing

Issues and pull requests welcome at
[github.com/typangaa/otter](https://github.com/typangaa/otter).

One feature or fix per PR. All new code must include unit tests.
Run `cargo test` and `cargo clippy` before opening a PR.
