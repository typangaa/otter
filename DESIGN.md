# weir v1 design

weir is a task runner and GPU slot scheduler for jailed CLI agent workers. The
decision record (why v0 was dropped, trade-offs, phased plan) is in
[docs/design-v1.md](docs/design-v1.md). This file describes what the code does.

## Purpose

A coding agent (for example a Claude Code subagent) needs to hand a bounded job
to a cheaper worker, safely and concurrently. weir owns only the state that
short-lived processes must share:

1. Leases on single-slot GPU replicas, so concurrent callers queue instead of
   thrashing one llama.cpp slot.
2. A persisted cloud quota cooldown and an escalation ladder.
3. One task lifecycle: worktree, jailed worker, diff, jailed checks, one JSON
   record, cleanup policy.
4. An append-only JSONL ledger of every task.

## Hard constraints

- No HTTP client or server, no MCP, no daemon, no API keys. Workers own their auth.
- weir only execs three kinds of program, hard-coded: the allow-listed wrappers
  `pi-worker` and `agy-worker`, `agent-jail` (checks) and `git` (bookkeeping).
- `stdin` is null on every spawn except the prompt pipe.
- weir never commits, merges or pushes. The caller reviews the patch and commits.
- Workers run in their own process group with capped output and a hard deadline.
- Unknown config keys are a hard error. `./weir.toml` is never searched.

## Command surface

```
weir validate [--deep]          check the v2 config; --deep probes the environment
weir status                     config summary and active agy cooldowns
weir version | schema           build info | JSON Schema of the v2 config
weir config path                resolved config path
weir lease run --slot N=R... -- <worker> ...   run a wrapper under a flock slot
weir lease status [--json]      which slots are held, cooldowns
weir worker run                 run one wrapper directly (no lease/worktree/checks)
weir task run | show | list | clean
```

Global flags: `-c/--config`, `--json`, `--log-level`, `--log-format`.
Config resolution: `--config` > `$WEIR_CONFIG` > `$XDG_CONFIG_HOME/weir/weir.toml`
> `$HOME/.config/weir/weir.toml`.

## Exit codes

| Code | Source | Meaning |
|---|---|---|
| 0 | all | Success (task: worker ok and all checks passed) |
| 1 | all | Config or runtime error (`validate`, `status`, `task`) |
| 2 | all | Usage error (`worker` also reports a pre-v2 config as 2) |
| 3 | task | Worker produced empty output |
| 5 | task | Jail refused |
| 6 | task | Worker denied actions |
| 7 | task | Quota exhausted (and the ladder, if any, is exhausted) |
| 10 | task | Worker ok but a check failed |
| 124 | task | Timeout |
| 75 | lease run | All slots busy until `--wait-timeout` |
| 126 / 127 | lease run | Worker could not be spawned / not found |
| N | lease run | The child's own exit code (`128+signal` on a signal) |

`weir task run` re-uses the wrapper codes (`src/exit.rs`): wrapper 3 becomes 7
when stderr contains the worker's `quota_pattern`. weir never retries in place.

## Config v2

`version = 2` is required; a file without it is rejected. See
`examples/weir.v2.example.toml` (`weir schema` prints the JSON Schema).

- `[paths]`: `wt_roots` (must match agent-jail's `WT_ROOTS`), `state_dir`
  (default `${XDG_STATE_HOME:-~/.local/state}/weir`), `scratch`.
- `[slots]`: named capacities, for example `gpu-a`, `gpu-b`, `agy`.
- `[worker.pi]`, `[worker.agy]`: `command` must be `<name>-worker`; `timeout`,
  `slots`, replica/model args, agy `quota_pattern` and `quota_cooldown`.
- `[ladder.NAME]`: ordered `steps` such as `pi:auto`, `agy:<model>`.
- `[check.NAME]`: `cwd`, `cmd`, `timeout`; run under `agent-jail`.

## Leases

`flock(2)` on files under `<state_dir>/leases/`. No daemon: the kernel drops the
lock when the holder dies. `--slot NAME=REPLICA` is tried in order; `--affinity`
prefers the slot that last served the same key (warm prompt-prefix cache). Time
spent queueing is recorded as `queue_wait_ms` and is not charged to the worker
timeout. Only `pi-worker` and `agy-worker` may take a lease.

## Ladder and cooldown

A ladder advances only on `timeout`, `empty` or `quota`; never on `denied`,
`jail_refused` or a failed check. Each step gets a fresh worktree `<id>-sN`
(unless `fresh_worktree_per_step = false`). On `quota`, the agy model is written
to `<state_dir>/cooldown.json` (`{"agy":{"<model>":<unix>}}`) for
`quota_cooldown`; later steps on a cooling model are recorded as
`skipped_cooldown` without spawning.

## Task lifecycle and ledger

1. Admit: id, prompt file (0600) in `tasks/<id>/`, base resolved to a sha.
2. `git worktree add -b wt/<id> <wt_root>/<id> <base>`.
3. Lease a slot (if the worker has slots).
4. Spawn the wrapper (`<worker>-worker [-b R|-m M] -t <secs> <wt> <prompt>`),
   hard deadline `timeout + 60s`, SIGTERM to the group then SIGKILL after 10s.
5. Classify the exit code; walk the ladder if configured.
6. Diff against the base into `tasks/<id>/patch.diff`.
7. Run `[check.*]` entries under `agent-jail`.
8. Append one `weir.task/1` line to `ledger.jsonl` and print it with `--json`.
9. Apply the cleanup policy (`never` default, `on-success`, `always`).

`task clean` only removes branches named `wt/<id>` and paths under `wt_roots`.

## Module map

| Path | Role |
|---|---|
| `src/main.rs` | clap dispatch, config resolution, exit codes |
| `src/config/{resolve,v2}.rs` | config path resolution; v2 schema and validation |
| `src/cli/{validate,status,deep}.rs` | `validate`, `--deep` probes, `status`, `schema` |
| `src/exit.rs` | wrapper exit classification (`ExitKind`) |
| `src/lease.rs` | flock leases, `lease run`, `lease status` |
| `src/worker/{spawn,cli}.rs` | process-group spawn, caps, deadlines; `worker run` |
| `src/task/run.rs` | task lifecycle and ladder |
| `src/task/{git,check,id,cooldown,ledger}.rs` | worktrees and diffs, jailed checks, ids, quota cooldown, JSONL ledger |
| `src/task/cli.rs` | `task run/show/list/clean` argument handling |
| `src/observability/` | tracing setup |
