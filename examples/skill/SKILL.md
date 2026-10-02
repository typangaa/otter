---
name: weir
description: Hand a bounded coding job to a jailed worker (local GPU or cloud) with the weir CLI, then review and commit the result. Use when delegating edits to pi-worker or agy-worker, checking GPU slot occupancy, or reading past task records.
---

# Driving weir v1

weir runs one jailed worker per task in a fresh git worktree, under a lease on a
GPU slot, then runs jailed checks and prints one JSON record. It never commits.
The subagent commits, weir never commits.

## When to use

- A self-contained edit that a cheaper worker can do (write a prompt file first).
- Several parallel jobs that must not overload one local GPU slot: start more
  `weir task run` processes; leases queue them.
- Inspecting what ran: `weir task list`, `weir task show`.

Do not use it for work that needs the full conversation context or needs network
access beyond what the worker's jail allows.

## Run a task

```bash
weir task run --repo "$REPO" --base main --worker pi --replica auto \
  --prompt-file /tmp/task1.md --check typecheck --check lint --json
```

Use `--ladder default` instead of `--worker` to escalate on timeout, empty
output or quota. Use `--prompt-stdin` to pipe the prompt. Default cleanup keeps
the worktree so you can review it.

## Read the result

Stdout is exactly one JSON record (`schema: "weir.task/1"`):

- `status`: ok, check_failed, timeout, empty, quota, denied, jail_refused, error
- `exit`: weir's exit code (same as the process exit code)
- `branch`, `worktree`, `base_commit`
- `attempts[]`: `worker`, `replica`, `model`, `exit`, `kind`, `queue_wait_ms`,
  `elapsed_ms`, `stderr_tail`
- `answer`: worker stdout (capped)
- `diff`: `files`, `insertions`, `deletions`, `patch` (path to the patch file)
- `checks[]`: `name`, `exit`, `timed_out`, `tail`

## Branch on the exit code

| Exit | Do |
|---|---|
| 0 | Read the patch, verify it, commit inside `worktree`, merge |
| 10 | Fix it yourself, or rerun with the failing check's `tail` appended to the prompt |
| 124, 3 | Retry with a ladder or a stronger worker; do not retry in the same dirty tree |
| 7 | Quota exhausted; wait for the cooldown (`weir lease status`) or do the work yourself |
| 6, 5 | Denied or jail refused: do not retry blindly; inspect `stderr_tail` |
| 2, 1 | Fix the invocation or config (`weir validate --deep`) |

## Inspect

```bash
weir lease status --json      # slots held/free, agy cooldowns
weir task list --since 24h --json
weir task show <ID> --patch   # the diff
weir task clean --merged      # remove worktrees of merged tasks
```

## Lease a wrapper directly

```bash
weir lease run --slot gpu-a=a --slot gpu-b=b --wait-timeout 600 -- \
  pi-worker -b {replica} "$WORKTREE" /tmp/task1.md
```

Exit 75 means every slot stayed busy; otherwise the exit code is the wrapper's.

## Rules

- The subagent commits, weir never commits. Review `patch.diff` before merging.
- Write prompts to a file; do not put prompt text on the command line.
- One task per worktree; never reuse a dirty tree for a retry.
