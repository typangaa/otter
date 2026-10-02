# weir

A task runner and GPU slot scheduler for jailed CLI agent workers. One static
binary, no daemon.

A coding agent (for example a Claude Code subagent) hands a bounded job to a
cheaper worker. weir gives that job a fresh git worktree, a lease on a single
GPU slot, a hard deadline, jailed verification checks and one JSON record, and
then gets out of the way. The caller reviews the patch and commits.

- No HTTP, no MCP, no server, no API keys. Workers own their own auth.
- weir only execs `pi-worker`, `agy-worker`, `agent-jail` and `git`.
- weir never commits, merges or pushes.
- `stdin` is null on every spawn except the prompt pipe.

Design: [DESIGN.md](DESIGN.md). Decision record: [docs/design-v1.md](docs/design-v1.md).
History: [CHANGELOG.md](CHANGELOG.md).

## Install

```bash
cargo install --path . --root ~/.local          # installs ~/.local/bin/weir
```

Or download `weir-<version>-x86_64-unknown-linux-gnu.tar.gz` from the GitHub
release, verify the `.sha256` next to it, and put `weir` on your PATH.

Runtime requirements: Linux, `git`, the worker wrappers you configure
(`pi-worker`, `agy-worker`) on PATH, and `agent-jail` (with `bwrap`) for checks.

## Quick start

```bash
mkdir -p ~/.config/weir
cp examples/weir.v2.example.toml ~/.config/weir/weir.toml   # edit paths and checks
weir validate --deep          # wrappers, bwrap, agent-jail WT_ROOTS, state_dir
weir status
weir task run --repo ~/code/myrepo --base main --worker pi \
  --prompt-file notes.md --check web-typecheck --json
```

Config lookup: `--config` > `$WEIR_CONFIG` > `$XDG_CONFIG_HOME/weir/weir.toml` >
`~/.config/weir/weir.toml`. `./weir.toml` is deliberately never read. The file
must contain `version = 2`; `weir schema` prints its JSON Schema.

## Commands

All commands accept `--json`, `-c/--config`, `--log-level` and `--log-format`.

### task

```bash
# One worker, two checks, keep the worktree for review (default cleanup=never)
weir task run --repo ~/code/myrepo --base main --worker pi --replica auto \
  --prompt-file notes.md --check web-typecheck --check web-lint --json

# Walk an escalation ladder: pi, then cloud models, on timeout/empty/quota
weir task run --repo ~/code/myrepo --ladder default --prompt-stdin --json < notes.md

weir task list --since 24h
weir task show 20261001-fix-tones-3fa2            # ledger record
weir task show 20261001-fix-tones-3fa2 --patch    # the diff
weir task clean --merged                          # or: <ID>, --older-than 7d
```

Each run creates `wt/<id>` under one of `paths.wt_roots`, runs the wrapper,
captures `patch.diff`, runs the jailed `[check.*]` entries, appends one
`weir.task/1` line to `<state_dir>/ledger.jsonl` and prints it with `--json`.
`--cleanup never|on-success|always` controls worktree removal.

### lease

```bash
# Run a wrapper under an exclusive slot; {replica} is filled with the winner
weir lease run --slot gpu-a=a --slot gpu-b=b --wait-timeout 600 -- \
  pi-worker -b {replica} /wt/notes /tmp/prompt.md
weir lease status --json     # {"slots":[...],"cooldowns":{"agy":{"<model>":<unix>}}}
```

Leases are `flock` files, so a crashed holder frees its slot automatically.
`--affinity KEY` prefers the slot that last served that key (warm prefix cache).
`lease` never reads the config; only `pi-worker` and `agy-worker` are accepted.

### worker

```bash
weir worker run --worker pi --replica a /wt/notes /tmp/prompt.md
```

Runs one wrapper in its own process group with capped output and a hard
deadline. No lease, worktree or checks.

### validate, status, schema, config

```bash
weir validate [--deep]    # exit 0 valid, 1 invalid
weir status               # workers, slots, ladders, checks, active agy cooldowns
weir schema               # JSON Schema of the v2 config
weir config path          # resolved config file
weir version
```

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success (task: worker ok and all checks passed) |
| 1 | Config or runtime error |
| 2 | Usage error |
| 3 | Worker produced empty output |
| 5 | Jail refused |
| 6 | Worker denied actions |
| 7 | Quota exhausted (ladder exhausted too) |
| 10 | Worker ok, a check failed |
| 124 | Timeout |
| 75 | `lease run`: all slots busy until `--wait-timeout` |
| 126 / 127 | `lease run`: worker could not be spawned / not found |

`lease run` otherwise returns the child's own exit code. A calling script can
branch on these without parsing JSON.

## Wrapper contract

weir drives wrappers as `<worker>-worker [-b REPLICA | -m MODEL] -t <secs>
<WORKDIR> <PROMPT_FILE>` and expects:

| Wrapper exit | Meaning | weir kind |
|---|---|---|
| 0 | Success | `ok` |
| 124 | Wrapper timeout | `timeout` |
| 3 | Empty output, or quota if stderr contains `quota_pattern` | `empty` / `quota` |
| 6 | Denied actions | `denied` |
| 5 | Jail refused | `jail_refused` |
| 2 | Usage | `usage` |
| other | Failure | `error` |

- `pi-worker`: local llama.cpp worker, pinned with `-b a|b`; slots `gpu-a`/`gpu-b`.
- `agy-worker`: cloud worker, model chosen with `-m`; a quota hit writes a
  per-model cooldown to `<state_dir>/cooldown.json`.
- `agent-jail`: sandbox (bubblewrap) that confines a worker, and every check, to
  one worktree under `WT_ROOTS`. weir never runs worker code outside it.
  `weir validate --deep` verifies that `paths.wt_roots` matches it.

Ladders advance only on `timeout`, `empty` or `quota`, each step in a fresh
worktree. They never advance on `denied`, `jail_refused` or a failed check.

## Development

```bash
export CARGO_TARGET_DIR=$HOME/.cache/weir-target
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
```

## License

Apache-2.0.
