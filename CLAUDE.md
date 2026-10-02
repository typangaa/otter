# CLAUDE.md

weir v1: a task runner and GPU slot scheduler for jailed CLI agent workers
(single Rust binary, no HTTP/MCP/server/API keys). See `DESIGN.md` and
`docs/design-v1.md`.

## Build and test

Always build to a target dir outside the repo (the repo may live on a slow or
synced filesystem):

```bash
source $HOME/.cargo/env
export CARGO_TARGET_DIR=$HOME/.cache/weir-target
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked          # binary: $CARGO_TARGET_DIR/release/weir
```

Style: default rustfmt, zero clippy warnings, English only in code, docs,
commits and branch names.

## Smoke tests

```bash
W=$CARGO_TARGET_DIR/release/weir
$W --version
$W validate --config examples/weir.v2.example.toml --json
$W schema
$W lease status --json
$W task list --json
```

Live smoke (needs the real `pi-worker`, `agent-jail` and a llama.cpp replica):

```bash
R=/tmp/pilot/weir-smoke-repo
rm -rf $R && mkdir -p $R && git -C $R init -q -b main
echo '# smoke' > $R/README.md && git -C $R add . && git -C $R commit -qm init
echo 'Create hello.txt containing the line: hello. Do nothing else.' > /tmp/pilot/smoke.md
$W task run --repo $R --base main --worker pi --timeout 400 \
  --prompt-file /tmp/pilot/smoke.md --json     # expect exit 0, diff.files == 1
$W task clean <id>
```

## Hard rules

- No HTTP client or server, no MCP, no daemon, no API keys.
- Only exec `pi-worker`, `agy-worker` (allow-list hard-coded), `agent-jail`, `git`.
- `stdin` is null on every spawn except the prompt pipe.
- weir never commits, merges or pushes; the caller does.
- Unknown config keys are an error; never search `./weir.toml`.
- Do not rename env vars (`WEIR_CONFIG`), config paths (`~/.config/weir/weir.toml`),
  state dirs or the ledger schema id (`weir.task/1`) without a dedicated change.

## Architecture

Config v2 (`version = 2`) declares paths, slots, workers, ladders and checks.
`task run` = admit, worktree, lease, spawn wrapper in a process group, classify
exit code, optional ladder, diff, jailed checks, ledger line, cleanup.

| Path | Role |
|---|---|
| `src/main.rs` | clap dispatch, config resolution, exit codes |
| `src/config/{resolve,v2}.rs` | path resolution; v2 schema and validation |
| `src/cli/{validate,status,deep}.rs` | validate, `--deep` probes, status, schema |
| `src/exit.rs` | `ExitKind` classification of wrapper exits |
| `src/lease.rs` | flock leases, `lease run/status` |
| `src/worker/{spawn,cli}.rs` | process-group spawn, caps, deadlines; `worker run` |
| `src/task/run.rs` | task lifecycle and ladder |
| `src/task/{git,check,id,cooldown,ledger,cli}.rs` | worktrees/diffs, jailed checks, ids, quota cooldown, JSONL ledger, CLI |
| `src/observability/` | tracing setup |
| `tests/` | `assert_cmd` integration tests: `cli`, `config_v2`, `task_run`, `worker_spawn` |
| `docs/archive/` | superseded v0 plans (history only) |

## Release process

1. Branch, bump `version` in `Cargo.toml`, refresh `Cargo.lock`
   (`cargo build --locked` must pass), add a `## [X.Y.Z] - YYYY-MM-DD` section to
   `CHANGELOG.md`.
2. Open a PR and merge it to `main` (never push to main directly).
3. Tag the merge commit on `main`: `git tag vX.Y.Z && git push origin vX.Y.Z`.
4. `.github/workflows/release.yml` verifies the tag matches `Cargo.toml`, runs
   fmt/clippy/test, builds the Linux binary and publishes the tarball plus
   checksum with notes extracted from `CHANGELOG.md`.
