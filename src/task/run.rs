//! `weir task run` — one worker run in a fresh git worktree, then the checks.
//!
//! The whole command is two phases, and the boundary is the point:
//!
//! * **Phase A — validation** (this module's first half). Config, worker
//!   table, flag/worker compatibility, timeout, check names, repository, base
//!   revision, `wt_root`, prompt and task id. Anything rejected here exits 2
//!   and leaves *nothing* behind: no worktree, no branch, no `tasks/<id>`
//!   directory, no ledger line.
//! * **Phase B — side effects** (the second half). Prompt file, worktree,
//!   slot lease, worker, diff, checks, cleanup policy, ledger line, report.
//!   From here on every failure becomes a `weir.task/1` record with its own
//!   status, and the process exit code is that record's `exit`.
//!
//! weir itself execs nothing directly here: the worker goes through
//! [`run_worker`] (allow-list re-checked there), git through [`crate::task::git`]
//! and the checks through [`crate::task::check`].
//!
//! Output: `--json` prints exactly one line — the record, byte-identical to the
//! ledger line — and nothing else on stdout. Human mode prints the worker's
//! answer on stdout and one summary line on stderr.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::config::v2::{Check, ConfigV2};
use crate::exit::ExitKind;
use crate::lease::{acquire_slot, SlotLease};
use crate::task::check;
use crate::task::cli::{self, TaskRunArgs};
use crate::task::git;
use crate::task::id;
use crate::task::ledger::{self, Attempt, CheckResult, DiffInfo, TaskRecord};
use crate::worker::cli::WorkerKind;
use crate::worker::spawn::{
    check_program_allowed, expand_args, run_worker, WorkerSpec, MAX_TIMEOUT_SECS,
};

/// `answer` is capped at 64 KiB, on a char boundary.
const ANSWER_CAP: usize = 64 * 1024;
/// The value of `--replica`. `auto` lets the lease pick the concrete replica.
const REPLICA_AUTO: &str = "auto";
/// How much of the prompt the lease affinity hash is taken over (2 KiB), so
/// two tasks in the same repository land on the replica that already has the
/// prefix cached.
const AFFINITY_PROMPT_BYTES: usize = 2048;

// ── phase A: the validated plan ──────────────────────────────────────────────

/// Everything a legal invocation boils down to, decided before any side effect.
struct Plan {
    /// Where the state dir (`tasks/`, `ledger.jsonl`, `leases/`) lives.
    state: PathBuf,
    /// `state/tasks`: `tasks/<id>/prompt.md` and `patch.diff` go here.
    tasks_dir: PathBuf,
    /// Where worktrees are created (`paths.wt_roots[0]`), kept so a future
    /// reclaim pass can find the tree again.
    #[allow(dead_code)]
    wt_root: PathBuf,
    /// The repository, canonicalised — this is what the record reports.
    repo: PathBuf,
    /// `--base` exactly as the user typed it.
    base: String,
    /// `--base` resolved to a commit sha.
    base_commit: String,
    /// The prompt text, verbatim.
    prompt: String,
    /// Task id (explicit or allocated).
    id: String,
    /// `wt/<id>`.
    branch: String,
    /// `wt_root/<id>`, deliberately *not* canonicalised: this exact string is
    /// handed to `git`, to `agent-jail` and into the record.
    worktree: PathBuf,
    /// `pi` or `agy`.
    worker: &'static str,
    /// Requested replica for pi (`auto`, `a` or `b`); `None` for agy.
    replica: Option<String>,
    /// Model for agy (`--model` or `default_model`); `None` for pi.
    model: Option<String>,
    /// Wrapper timeout in seconds.
    timeout_secs: u64,
    /// Wrapper program (`pi-worker` / `agy-worker`).
    program: String,
    /// argv template for the pi replica (from `[worker.pi] replica_args`).
    replica_args: Vec<String>,
    /// Wrapper argv before `-t <secs>`, built with the *requested* replica.
    args_prefix: Vec<String>,
    /// Quota pattern for the exit-3 classification (agy only).
    quota_pattern: Option<String>,
    /// Lease candidates: `(slot name, replica)`, in preference order.
    candidates: Vec<(String, String)>,
    /// Check names with their config, in the order given.
    checks: Vec<(String, Check)>,
    /// When to remove the worktree.
    cleanup: Cleanup,
}

/// The `[worker.X]` facts phase A needs, flattened so the rest of the plan
/// does not care which worker was chosen.
#[derive(Clone)]
struct WorkerTable {
    /// Wrapper program name (`pi-worker` / `agy-worker`).
    command: String,
    /// argv template for the pi replica.
    replica_args: Vec<String>,
    /// argv template for the agy model.
    model_args: Vec<String>,
    /// `[worker.agy] default_model`.
    default_model: Option<String>,
    /// Quota pattern for exit-3 classification (agy only).
    quota_pattern: Option<String>,
    /// Configured timeout.
    timeout: u64,
    /// `[worker.pi] slots` replica -> slot name.
    slots: BTreeMap<String, String>,
}

/// `--cleanup` policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cleanup {
    Never,
    OnSuccess,
    Always,
}

impl Cleanup {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "never" => Ok(Cleanup::Never),
            "on-success" => Ok(Cleanup::OnSuccess),
            "always" => Ok(Cleanup::Always),
            other => Err(format!(
                "task run: --cleanup must be never, on-success or always, got {other:?}"
            )),
        }
    }
}

/// Validate the arguments against the config and the repository. Every
/// rejection is a usage error (exit 2) and happens before any side effect.
fn build_plan(cfg_path: &Path, args: &TaskRunArgs) -> Result<Plan, String> {
    // 1. A version = 2 config.
    let cfg = cli::load_v2(cfg_path)?;

    // 2. Worker flags must match the worker, before the table is consulted so
    //    that `--model` with `pi` never depends on pi being configured.
    let kind = args.worker;
    match kind {
        WorkerKind::Pi if args.model.is_some() => {
            return Err(
                "task run: --model does not apply to --worker pi (use --replica auto|a|b)"
                    .to_string(),
            );
        }
        WorkerKind::Agy if args.replica.is_some() => {
            return Err(
                "task run: --replica does not apply to --worker agy (use --model MODEL)"
                    .to_string(),
            );
        }
        _ => {}
    }
    let replica = match kind {
        WorkerKind::Pi => Some(
            args.replica
                .clone()
                .unwrap_or_else(|| REPLICA_AUTO.to_string()),
        ),
        WorkerKind::Agy => None,
    };
    if let Some(value) = replica.as_deref() {
        check_substituted("--replica", value)?;
        if value != REPLICA_AUTO && value != "a" && value != "b" {
            return Err(format!(
                "task run: --replica must be auto, a or b, got {value:?}"
            ));
        }
    }
    if let Some(value) = args.model.as_deref() {
        check_substituted("--model", value)?;
    }

    // 3. The `[worker.X]` table and the hard-coded exec allow-list.
    let table = load_worker(kind, &cfg)?;

    // 4. Timeout: `--timeout` wins, else the worker's; bounded either way.
    let timeout_secs = match args.timeout {
        Some(secs) => secs,
        None => table.timeout,
    };
    if timeout_secs == 0 {
        let source = if args.timeout.is_some() {
            "--timeout".to_string()
        } else {
            format!("[worker.{}] timeout", kind.as_str())
        };
        return Err(format!("task run: timeout must be > 0 ({source})"));
    }
    if timeout_secs > MAX_TIMEOUT_SECS {
        let source = if args.timeout.is_some() {
            "--timeout".to_string()
        } else {
            format!("[worker.{}] timeout", kind.as_str())
        };
        return Err(format!(
            "task run: timeout {timeout_secs} exceeds the maximum of {MAX_TIMEOUT_SECS} seconds ({source})"
        ));
    }

    // 5. Every --check must exist, in the order given.
    let mut checks = Vec::with_capacity(args.check.len());
    for name in &args.check {
        match cfg.check.get(name) {
            Some(def) => checks.push((name.clone(), def.clone())),
            None => {
                return Err(format!(
                    "task run: no [check.{name}] in this config (known: {})",
                    known_names(cfg.check.keys())
                ));
            }
        }
    }

    let cleanup = Cleanup::parse(&args.cleanup)?;

    // 6. Paths from the config, then the prompt (both needed for the id).
    let state = cfg
        .state_dir_expanded()
        .map_err(|e| format!("task run: {e}"))?;
    let tasks_dir = state.join("tasks");
    let wt_root = cfg
        .wt_roots_expanded()
        .map_err(|e| format!("task run: {e}"))?
        .into_iter()
        .next()
        .ok_or_else(|| "task run: paths.wt_roots is empty".to_string())?;
    if !wt_root.is_dir() {
        return Err(format!(
            "task run: wt_root {} is not an existing directory",
            wt_root.display()
        ));
    }

    let prompt = read_prompt(args)?;

    // 7. The repository and its base revision.
    if !args.repo.is_dir() {
        return Err(format!(
            "task run: repo {} is not an existing directory",
            args.repo.display()
        ));
    }
    if !git::is_git_repo(&args.repo) {
        return Err(format!(
            "task run: repo {} is not a git repository",
            args.repo.display()
        ));
    }
    let repo = std::fs::canonicalize(&args.repo)
        .map_err(|e| format!("task run: cannot resolve {}: {e}", args.repo.display()))?;
    let base_commit = git::resolve_commit(&repo, &args.base).ok_or_else(|| {
        format!(
            "task run: base revision {:?} does not resolve in {}",
            args.base,
            repo.display()
        )
    })?;

    // 8. The lease candidates, so an unusable slot map is also a phase A error.
    let candidates = match kind {
        WorkerKind::Pi => pi_candidates(replica.as_deref().unwrap_or(REPLICA_AUTO), &table.slots),
        // agy takes no lease in P4; an empty list never reaches `acquire_slot`.
        WorkerKind::Agy => Vec::new(),
    };

    let id = decide_id(args, &tasks_dir, &repo, &wt_root)?;
    let model = match kind {
        WorkerKind::Agy => args.model.clone().or_else(|| table.default_model.clone()),
        WorkerKind::Pi => None,
    };
    let args_prefix = match (&replica, &model) {
        (Some(value), _) => expand_args(&table.replica_args, "replica", value),
        (None, Some(value)) => expand_args(&table.model_args, "model", value),
        // agy without --model and without default_model: no model arguments.
        (None, None) => Vec::new(),
    };

    Ok(Plan {
        state,
        tasks_dir,
        wt_root: wt_root.clone(),
        repo,
        base: args.base.clone(),
        base_commit,
        prompt,
        branch: format!("wt/{id}"),
        worktree: wt_root.join(&id),
        id,
        worker: kind.as_str(),
        replica,
        model,
        timeout_secs,
        program: table.command,
        replica_args: table.replica_args.clone(),
        args_prefix,
        quota_pattern: table.quota_pattern,
        candidates,
        checks,
        cleanup,
    })
}

/// Read the `[worker.X]` table for the requested kind and re-check its command
/// against the hard-coded allow-list (a hand-edited config is refused here).
fn load_worker(kind: WorkerKind, cfg: &ConfigV2) -> Result<WorkerTable, String> {
    match kind {
        WorkerKind::Pi => {
            let pi = cfg
                .worker
                .pi
                .as_ref()
                .ok_or_else(|| "task run: no [worker.pi] table in this config".to_string())?;
            Ok(WorkerTable {
                command: pi.command.clone(),
                replica_args: pi.replica_args.clone(),
                model_args: Vec::new(),
                default_model: None,
                quota_pattern: None,
                timeout: pi.timeout,
                slots: pi.slots.clone(),
            })
        }
        WorkerKind::Agy => {
            let agy = cfg
                .worker
                .agy
                .as_ref()
                .ok_or_else(|| "task run: no [worker.agy] table in this config".to_string())?;
            // `default_model` lands in argv too, so it gets the same rule as
            // `--model`.
            if let Some(model) = agy.default_model.as_deref() {
                check_substituted("[worker.agy] default_model", model)?;
            }
            Ok(WorkerTable {
                command: agy.command.clone(),
                replica_args: Vec::new(),
                model_args: agy.model_args.clone(),
                default_model: agy.default_model.clone(),
                quota_pattern: Some(agy.quota_pattern.clone()),
                timeout: agy.timeout,
                slots: BTreeMap::new(),
            })
        }
    }
    .and_then(|table| {
        check_program_allowed(&table.command).map_err(|e| format!("task run: {e}"))?;
        Ok(table)
    })
}

/// Comma-separated list of the names the config does define.
fn known_names<'a>(names: impl Iterator<Item = &'a String>) -> String {
    let list: Vec<&str> = names.map(String::as_str).collect();
    if list.is_empty() {
        "none".to_string()
    } else {
        list.join(", ")
    }
}

/// Reject a value that ends up in the wrapper's argv but cannot be a real
/// identifier: empty, or starting with `-` (which the wrapper would read as its
/// own option, or as end-of-options).
fn check_substituted(flag: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("task run: {flag} must not be empty"));
    }
    if value.starts_with('-') {
        return Err(format!(
            "task run: {flag} value {value:?} must not start with '-'"
        ));
    }
    Ok(())
}

/// The prompt: `--prompt-file` (relative to the process cwd) or all of stdin.
fn read_prompt(args: &TaskRunArgs) -> Result<String, String> {
    let text = match (&args.prompt_file, args.prompt_stdin) {
        (Some(path), _) => {
            // A relative path is resolved by the process, not by the wrapper.
            let resolved = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()
                    .map_err(|e| format!("task run: cannot read the cwd: {e}"))?
                    .join(path)
            };
            std::fs::read_to_string(&resolved)
                .map_err(|e| format!("task run: cannot read prompt {}: {e}", resolved.display()))?
        }
        (None, true) => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| format!("task run: cannot read the prompt from stdin: {e}"))?;
            buf
        }
        (None, false) => {
            return Err(
                "task run: one of --prompt-file F or --prompt-stdin is required".to_string(),
            )
        }
    };
    if text.trim().is_empty() {
        return Err("task run: the prompt is empty".to_string());
    }
    Ok(text)
}

/// The task id: `--id` validated and checked for collisions, otherwise a
/// generated one (date + prompt-file slug + entropy).
fn decide_id(
    args: &TaskRunArgs,
    tasks_dir: &Path,
    repo: &Path,
    wt_root: &Path,
) -> Result<String, String> {
    match &args.id {
        Some(explicit) => {
            cli::validate_id(explicit)?;
            // `wt/main` is a branch `task clean` refuses as protected, so such a
            // task could never be cleaned.
            if matches!(explicit.as_str(), "main" | "master" | "develop") {
                return Err(format!(
                    "task run: --id {explicit:?} is a protected branch name"
                ));
            }
            match id::Collision::check(explicit, tasks_dir, repo, wt_root) {
                None => Ok(explicit.clone()),
                Some(hit) => Err(format!(
                    "task run: task id {explicit} is taken: {}",
                    hit.reason
                )),
            }
        }
        None => {
            // The slug comes from the prompt *file*; stdin has no file stem.
            let slug = id::slug_for_prompt(args.prompt_file.as_deref());
            id::Collision::allocate(&slug, tasks_dir, repo, wt_root)
                .map_err(|hit| format!("task run: cannot allocate a task id: {}", hit.reason))
        }
    }
}

/// Lease candidates for pi. A pinned replica yields exactly one candidate, so
/// a busy pinned slot blocks instead of silently switching GPU.
fn pi_candidates(replica: &str, slot_map: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let slot_for = |name: &str| {
        slot_map
            .get(name)
            .cloned()
            .unwrap_or_else(|| format!("gpu-{name}"))
    };
    match replica {
        REPLICA_AUTO => vec![
            (slot_for("a"), "a".to_string()),
            (slot_for("b"), "b".to_string()),
        ],
        other => vec![(slot_for(other), other.to_string())],
    }
}

// ── phase B: side effects ────────────────────────────────────────────────────

/// `weir task run …` — returns the process exit code, which is the record's
/// `exit`.
pub async fn run(cfg_path: &Path, args: &TaskRunArgs, json: bool) -> i32 {
    let plan = match build_plan(cfg_path, args) {
        Ok(plan) => plan,
        Err(msg) => return cli::usage_error(json, &msg),
    };
    execute(&plan, json).await
}

/// Everything from the prompt file onwards. Always produces a record.
async fn execute(plan: &Plan, json: bool) -> i32 {
    let started = Instant::now();
    let mut record = new_record(plan);

    // 1. The prompt file. The worker only ever sees this path, never the text.
    let prompt_path = plan.tasks_dir.join(&plan.id).join("prompt.md");
    // Claim the id atomically: `create_dir` (not `create_dir_all`) fails when a
    // concurrent run with the same explicit --id got here first, and that must
    // neither truncate its prompt nor shadow its record with an error record.
    let claim = std::fs::create_dir_all(&plan.tasks_dir)
        .and_then(|()| std::fs::create_dir(plan.tasks_dir.join(&plan.id)));
    if let Err(e) = claim {
        let msg = if e.kind() == std::io::ErrorKind::AlreadyExists {
            format!("task run: id {} is already in use", plan.id)
        } else {
            format!("task run: cannot create the task dir: {e}")
        };
        return cli::usage_error(json, &msg);
    }
    if let Err(e) = git::write_private_file(&prompt_path, plan.prompt.as_bytes()) {
        return finish(
            plan,
            &mut record,
            fail(
                ExitKind::Error,
                Some(format!(
                    "cannot write prompt {}: {e}",
                    prompt_path.display()
                )),
            ),
            started,
            json,
        );
    }

    // 2. The worktree.
    if let Err(err) = git::worktree_add(&plan.repo, &plan.branch, &plan.worktree, &plan.base_commit)
    {
        return finish(
            plan,
            &mut record,
            fail(ExitKind::Error, Some(err)),
            started,
            json,
        );
    }

    // 3. The slot lease (pi only). Held until the worker returns.
    let (lease, queue_wait_ms) = match take_lease(plan).await {
        Ok(guard) => guard,
        Err(err) => {
            return finish(
                plan,
                &mut record,
                fail(ExitKind::Error, Some(err)),
                started,
                json,
            )
        }
    };
    // The concrete replica the lease handed us, not the request (`auto`).
    let replica = lease
        .as_ref()
        .map(|slot| slot.replica.clone())
        .or_else(|| plan.replica.clone());
    record.replica = replica.clone();

    // 4. The worker. The lease drops the moment it returns.
    let spec = WorkerSpec {
        program: plan.program.clone(),
        args_prefix: argv_for(plan, replica.as_deref()),
        timeout_secs: plan.timeout_secs,
        workdir: plan.worktree.clone(),
        prompt_file: prompt_path.clone(),
        quota_pattern: plan.quota_pattern.clone(),
    };
    let outcome = run_worker(&spec).await;
    drop(lease);

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(e) => {
            record.attempts.push(Attempt {
                worker: plan.worker.to_string(),
                replica: replica.clone(),
                model: plan.model.clone(),
                // Nothing was spawned, so there is no wrapper code to report.
                exit: -1,
                kind: ExitKind::Error.as_str().to_string(),
                queue_wait_ms,
                elapsed_ms: 0,
                stderr_tail: e.to_string(),
                truncated: false,
            });
            return finish(
                plan,
                &mut record,
                fail(
                    ExitKind::Error,
                    Some(format!("cannot start the worker: {e}")),
                ),
                started,
                json,
            );
        }
    };

    record.attempts.push(Attempt {
        worker: plan.worker.to_string(),
        replica: replica.clone(),
        model: plan.model.clone(),
        exit: outcome.exit_code,
        kind: outcome.kind.as_str().to_string(),
        queue_wait_ms,
        elapsed_ms: outcome.elapsed_ms,
        stderr_tail: outcome.stderr_tail.clone(),
        truncated: outcome.truncated,
    });
    record.queue_wait_ms = queue_wait_ms;
    record.answer = cap_answer(&outcome.stdout);

    // 5. Diff — whenever a worktree exists, whatever the worker did.
    record.diff = capture_diff(plan, &prompt_path);

    // 6. Classify. Only a happy worker gets to the checks.
    let outcome_kind = match outcome.kind {
        // A wrapper usage error means weir built a bad argv: our bug, so 1,
        // never the wrapper's 2 (which means "your command line was wrong").
        ExitKind::Usage => ExitKind::Error,
        other => other,
    };
    if outcome_kind != ExitKind::Ok {
        let mut error = None;
        if outcome_kind == ExitKind::Error {
            error = Some(if outcome.stderr_tail.trim().is_empty() {
                format!("{} exited with {}", spec.program, outcome.exit_code)
            } else {
                outcome.stderr_tail.clone()
            });
        }
        return finish(plan, &mut record, fail(outcome_kind, error), started, json);
    }

    // 7. Checks, in the order given, all of them even after a failure.
    let mut checks: Vec<CheckResult> = Vec::with_capacity(plan.checks.len());
    let mut interrupted: Option<i32> = None;
    if !plan.checks.is_empty() {
        let mut interrupts = check::Interrupts::install();
        for (name, def) in &plan.checks {
            match interrupts.as_mut() {
                Some(listener) => {
                    let (row, sig) =
                        check::run_check_interruptible(name, def, &plan.worktree, listener).await;
                    checks.push(row);
                    if sig.is_some() {
                        interrupted = sig;
                        break;
                    }
                }
                None => checks.push(check::run_check(name, def, &plan.worktree).await),
            }
        }
    }
    record.checks = checks;
    let failed = record.checks.iter().find(|row| row.exit != 0);
    let classified = if let Some(sig) = interrupted {
        // The remaining checks were not run; the record is still finished and
        // logged so the interrupted run is not lost.
        fail(
            ExitKind::Error,
            Some(format!(
                "interrupted by signal {sig} during check {}; remaining checks not run",
                record.checks.last().map_or("?", |row| row.name.as_str())
            )),
        )
    } else {
        match failed {
            Some(row) => fail(
                ExitKind::CheckFailed,
                Some(format!("check {} exited with {}", row.name, row.exit)),
            ),
            None => (ExitKind::Ok, None),
        }
    };
    finish(plan, &mut record, classified, started, json)
}

/// The argv before `-t <secs>`. For pi it is rebuilt from the template with
/// the replica the lease actually granted (`auto` resolves at that point);
/// agy uses the model argv built in phase A.
fn argv_for(plan: &Plan, replica: Option<&str>) -> Vec<String> {
    match replica {
        Some(value) => expand_args(&plan.replica_args, "replica", value),
        None => plan.args_prefix.clone(),
    }
}

/// Length of the longest prefix of `text` of at most [`AFFINITY_PROMPT_BYTES`]
/// bytes that ends on a char boundary.
fn prefix_len(text: &str) -> usize {
    if text.len() <= AFFINITY_PROMPT_BYTES {
        return text.len();
    }
    let mut end = AFFINITY_PROMPT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Acquire the slot lease. Returns `(guard, queue_wait_ms)`; the guard is
/// `None` for a worker that takes no lease.
async fn take_lease(plan: &Plan) -> Result<(Option<SlotLease>, u64), String> {
    if plan.candidates.is_empty() {
        return Ok((None, 0));
    }
    // The same root `weir lease run` locks under, so both queueing paths share
    // one lock file per slot whatever `paths.state_dir` says.
    let root = crate::lease::state_root().unwrap_or_else(|| plan.state.clone());
    let candidates = plan.candidates.clone();
    let affinity = Some(affinity_key(plan));
    // `holder` is only written into the lock file for `weir lease status`.
    let holder = vec![
        "weir".to_string(),
        "task".to_string(),
        "run".to_string(),
        plan.id.clone(),
    ];
    let acquired = tokio::task::spawn_blocking(move || {
        acquire_slot(&root, &candidates, affinity.as_deref(), &holder)
    })
    .await
    .map_err(|e| format!("task run: the lease wait failed: {e}"))?;
    match acquired {
        Ok(lease) => {
            let waited = lease.queue_wait_ms;
            Ok((Some(lease), waited))
        }
        Err(e) => Err(format!("task run: cannot acquire a worker slot: {e}")),
    }
}

/// The lease affinity key: hex of a hash over the canonical repo path plus the
/// first 2 KiB of the prompt, so the same work keeps the same replica warm.
fn affinity_key(plan: &Plan) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    plan.repo.hash(&mut hasher);
    // `str::hash` is defined as hashing the bytes, so hashing a prefix is the
    // same operation on the first 2 KiB.
    plan.prompt[..prefix_len(&plan.prompt)].hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// `git diff` counts plus the patch file, or `None` when there is no worktree.
fn capture_diff(plan: &Plan, prompt_path: &Path) -> Option<DiffInfo> {
    if !plan.worktree.exists() {
        return None;
    }
    let (files, insertions, deletions) = git::diff_stat(&plan.worktree, &plan.base_commit);
    let patch_path = prompt_path
        .parent()
        .map(|dir| dir.join("patch.diff"))
        .unwrap_or_else(|| plan.tasks_dir.join(&plan.id).join("patch.diff"));
    match git::write_patch(&plan.worktree, &plan.base_commit, &patch_path) {
        Ok(_) => Some(DiffInfo {
            files,
            insertions,
            deletions,
            // Absolute, so a reader does not need to know the state dir.
            patch: patch_path.display().to_string(),
        }),
        Err(_) => None,
    }
}

/// A fresh record with everything phase A already knows.
fn new_record(plan: &Plan) -> TaskRecord {
    let mut record = TaskRecord::new(&plan.id);
    record.repo = plan.repo.display().to_string();
    record.base = plan.base.clone();
    record.base_commit = plan.base_commit.clone();
    record.branch = plan.branch.clone();
    record.worktree = plan.worktree.display().to_string();
    record.worker = plan.worker.to_string();
    record.replica = plan.replica.clone();
    record.model = plan.model.clone();
    record.created_at = cli::now_unix();
    record
}

/// `(status, error)` for a non-OK outcome.
fn fail(kind: ExitKind, error: Option<String>) -> (ExitKind, Option<String>) {
    (kind, error)
}

/// Apply the cleanup policy, log the record, report it, and return the exit
/// code (`record.exit`).
fn finish(
    plan: &Plan,
    record: &mut TaskRecord,
    outcome: (ExitKind, Option<String>),
    started: Instant,
    json: bool,
) -> i32 {
    let (kind, error) = outcome;
    record.status = kind.as_str().to_string();
    record.exit = kind.to_process_code();
    record.error = error;

    // 8. Cleanup. `on-success` only removes an `ok` run; the patch and the
    //    prompt file are never touched.
    let should_remove = match plan.cleanup {
        Cleanup::Never => false,
        Cleanup::Always => true,
        Cleanup::OnSuccess => kind == ExitKind::Ok,
    };
    record.cleanup = if should_remove {
        match remove_worktree(plan) {
            Ok(()) => "removed".to_string(),
            Err(err) => {
                // The tree is still there: report it as kept, with the reason.
                record.error = Some(match record.error.take() {
                    Some(existing) => format!("{existing}; cleanup failed: {err}"),
                    None => format!("cleanup failed: {err}"),
                });
                "kept".to_string()
            }
        }
    } else {
        "kept".to_string()
    };

    record.elapsed_ms = started.elapsed().as_millis() as u64;

    // 9. Ledger first, then stdout, so a record we printed is always logged.
    if let Err(e) = ledger::append_record(&plan.state, record) {
        eprintln!("weir task: error: cannot append to the ledger: {e}");
    }
    report(json, record);
    record.exit
}

/// Remove the worktree and then its branch, and only prune the stale worktree
/// bookkeeping afterwards.
///
/// The order is deliberate. `prune` has to come last, because it is what clears
/// the `refs/worktree/<name>` registration a git >= 2.32 worktree keeps in the
/// parent's ref store, and that registration is what makes a branch show up in
/// `git branch --list` as still in use. Pruning earlier would leave a live
/// registration pointing at a directory that is about to disappear.
///
/// The three calls are sequential and never interleaved with another task's git
/// calls in the same repository, which a shared lock would guarantee but which
/// also follows from the fact that a single run owns its own `wt/<id>` branch.
fn remove_worktree(plan: &Plan) -> Result<(), String> {
    git::worktree_remove(&plan.repo, &plan.worktree)?;
    // A worktree directory that was already deleted leaves its registration
    // behind, and git refuses `branch -D` on a branch it still lists as checked
    // out: prune before the delete, and once more after it.
    git::worktree_prune(&plan.repo)?;
    git::branch_delete(&plan.repo, &plan.branch)?;
    git::worktree_prune(&plan.repo)
}

/// Print the record: one JSON line and nothing else, or the answer plus one
/// summary line on stderr.
fn report(json: bool, record: &TaskRecord) {
    if json {
        match record.to_line() {
            Ok(line) => println!("{line}"),
            Err(e) => eprintln!("weir task: error: cannot encode the record: {e}"),
        }
        return;
    }
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(record.answer.as_bytes());
    let _ = out.flush();
    let diff = record.diff.as_ref();
    let passed = record.checks.iter().filter(|row| row.exit == 0).count();
    eprintln!(
        "weir task: id={id} status={status} exit={exit} files={files} +{insertions} -{deletions} checks={passed}/{total} worktree={worktree}",
        id = record.id,
        status = record.status,
        exit = record.exit,
        files = diff.map(|d| d.files).unwrap_or(0),
        insertions = diff.map(|d| d.insertions).unwrap_or(0),
        deletions = diff.map(|d| d.deletions).unwrap_or(0),
        passed = passed,
        total = record.checks.len(),
        worktree = record.worktree,
    );
}

/// Cap the answer at [`ANSWER_CAP`] bytes, never splitting a UTF-8 char.
fn cap_answer(text: &str) -> String {
    if text.len() <= ANSWER_CAP {
        return text.to_string();
    }
    let mut end = ANSWER_CAP;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}
