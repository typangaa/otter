//! clap arguments and the read-only / garbage-collecting handlers of `weir task`.
//!
//! Four subcommands share this module:
//!
//! * `run` — the orchestrated worker run. Its arguments live here, its logic
//!   in [`crate::task::run`].
//! * `show` — one ledger record (or its patch file) by id.
//! * `list` — every ledger record, optionally filtered by age.
//! * `clean` — remove the worktree and branch of finished tasks, behind safety
//!   rules that never let weir touch a branch other than `wt/<id>` or a path
//!   outside the configured `paths.wt_roots`.
//!
//! `--json` exists per subcommand *and* as the global `weir --json`; the
//! dispatcher passes the disjunction down. In JSON mode stdout carries exactly
//! one machine-readable value and nothing else; prose goes to stderr.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, Subcommand};

use crate::config::v2::{parse_duration, ConfigV2};
use crate::task::git;
use crate::task::id;
use crate::task::ledger::{self, TaskRecord};
use crate::worker::cli::WorkerKind;

/// Exit code for a usage / argument error (shell convention `EX_USAGE`).
pub const EXIT_USAGE: i32 = 2;
/// Exit code for a failure after the arguments were accepted.
pub const EXIT_ERROR: i32 = 1;

/// `weir task …` subcommands.
#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    /// Run one worker in a fresh git worktree, then run the jailed checks.
    Run(TaskRunArgs),

    /// Print one task's ledger record (or its patch file).
    Show(TaskShowArgs),

    /// List the recorded tasks, oldest first.
    List(TaskListArgs),

    /// Remove the worktrees and branches of finished tasks.
    Clean(TaskCleanArgs),
}

/// `weir task run` arguments.
#[derive(Debug, Clone, Args)]
pub struct TaskRunArgs {
    /// Repository the base commit comes from and the worktree is added for.
    #[arg(long, value_name = "PATH")]
    pub repo: PathBuf,

    /// Base revision (branch, tag or commit), resolved to a commit sha.
    #[arg(long, value_name = "REV", default_value = "develop")]
    pub base: String,

    /// Explicit task id instead of a generated one.
    #[arg(long, value_name = "ID")]
    pub id: Option<String>,

    /// Worker to run: `pi` (local llama.cpp wrapper) or `agy` (cloud wrapper).
    /// Exactly one of `--worker` or `--ladder` is required.
    #[arg(
        long,
        value_name = "WORKER",
        conflicts_with = "ladder",
        required_unless_present = "ladder"
    )]
    pub worker: Option<WorkerKind>,

    /// Pi replica `auto|a|b`. Pi only; default `auto`.
    #[arg(long, value_name = "REPLICA", conflicts_with = "ladder")]
    pub replica: Option<String>,

    /// Model id. Agy only; defaults to `[worker.agy] default_model`.
    #[arg(long, value_name = "MODEL", conflicts_with = "ladder")]
    pub model: Option<String>,

    /// Run the steps of `[ladder.NAME]` in order, moving to the next step only
    /// on `timeout`, `empty` or `quota`. Each step gets a fresh worktree
    /// (`<id>-s<N>`) unless the ladder sets `fresh_worktree_per_step = false`.
    #[arg(long, value_name = "NAME", conflicts_with = "worker")]
    pub ladder: Option<String>,

    /// Wrapper timeout in seconds. Default: the worker's `timeout`. With
    /// `--ladder` it overrides the timeout of every step.
    #[arg(long, value_name = "SECS")]
    pub timeout: Option<u64>,

    /// Read the prompt from this file (relative to the process cwd).
    #[arg(long = "prompt-file", value_name = "F")]
    pub prompt_file: Option<PathBuf>,

    /// Read the whole prompt from stdin.
    #[arg(long)]
    pub prompt_stdin: bool,

    /// `[check.NAME]` to run afterwards, in the order given (repeatable).
    #[arg(long = "check", value_name = "NAME")]
    pub check: Vec<String>,

    /// When to remove the worktree and branch: `never`, `on-success`, `always`.
    #[arg(long, value_name = "POLICY", default_value = "never")]
    pub cleanup: String,

    /// Emit exactly one JSON record on stdout instead of text.
    #[arg(long)]
    pub json: bool,
}

/// `weir task show ID [--patch] [--json]` arguments.
#[derive(Debug, Clone, Args)]
pub struct TaskShowArgs {
    /// Task id to look up in the ledger.
    pub id: String,

    /// Print the task's patch file verbatim instead of the record.
    #[arg(long)]
    pub patch: bool,

    /// Emit the record as one JSON line instead of pretty-printed JSON.
    #[arg(long)]
    pub json: bool,
}

/// `weir task list [--since DUR] [--json]` arguments.
#[derive(Debug, Clone, Args)]
pub struct TaskListArgs {
    /// Only tasks created within this duration (`30m`, `1h`, `7d`).
    #[arg(long, value_name = "DUR")]
    pub since: Option<String>,

    /// Emit a JSON array of records instead of one line per task.
    #[arg(long)]
    pub json: bool,
}

/// `weir task clean (ID | --merged | --older-than DUR) [--json]` arguments.
///
/// The three selectors exclude each other and one is required, so clap rejects
/// both "no selector" and "two selectors" before any git call happens.
#[derive(Debug, Clone, Args)]
pub struct TaskCleanArgs {
    /// Clean exactly this task id.
    #[arg(
        value_name = "ID",
        required_unless_present_any = ["merged", "older_than"],
        conflicts_with_all = ["merged", "older_than"]
    )]
    pub id: Option<String>,

    /// Clean every recorded task whose branch is already merged into its base.
    #[arg(long, conflicts_with_all = ["older_than"])]
    pub merged: bool,

    /// Clean every recorded task older than this duration (`7d`, `24h`).
    #[arg(long, value_name = "DUR")]
    pub older_than: Option<String>,

    /// Emit `{"removed":[…],"skipped":[…]}` instead of one line per task.
    #[arg(long)]
    pub json: bool,
}

// ── shared helpers (also used by `task run`) ─────────────────────────────────

/// Unix seconds now; 0 when the clock is before the epoch.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Report a usage error (stderr, plus one JSON object in `--json` mode) and
/// hand back exit code 2.
pub fn usage_error(json: bool, msg: &str) -> i32 {
    eprintln!("weir task: error: {msg}");
    if json {
        println!(
            "{}",
            serde_json::json!({"schema": ledger::RECORD_SCHEMA, "status": "usage",
                              "exit": EXIT_USAGE, "error": msg})
        );
    }
    EXIT_USAGE
}

/// Report a failure that happened after the arguments were accepted.
pub fn runtime_error(json: bool, msg: &str, code: i32) -> i32 {
    eprintln!("weir task: error: {msg}");
    if json {
        println!(
            "{}",
            serde_json::json!({"schema": ledger::RECORD_SCHEMA, "status": "error",
                              "exit": code, "error": msg})
        );
    }
    code
}

/// Load the `version = 2` config every `task` subcommand needs. Every failure
/// is a usage error and carries the reason.
pub fn load_v2(cfg_path: &Path) -> Result<ConfigV2, String> {
    if !cfg_path.is_file() {
        return Err(format!("config file not found: {}", cfg_path.display()));
    }
    ConfigV2::load(cfg_path).map_err(|e| e.to_string())
}

/// Validate an id that names a path component (`tasks/<id>`, `wt/<id>`).
pub fn validate_id(value: &str) -> Result<(), String> {
    id::validate_explicit_id(value).map_err(|e| e.replacen("task run: --id", "task id", 1))
}

// ── task show ────────────────────────────────────────────────────────────────

/// `weir task show ID` — the record with any later `clean` event applied, or
/// the task's patch file with `--patch`.
pub fn show(cfg: &ConfigV2, args: &TaskShowArgs, json: bool) -> i32 {
    let state = match cfg.state_dir_expanded() {
        Ok(dir) => dir,
        Err(e) => return usage_error(json, &format!("task show: {e}")),
    };
    let Some(record) = ledger::find(&state, &args.id) else {
        return usage_error(
            json,
            &format!(
                "task show: no task {id} in the ledger ({path})",
                id = args.id,
                path = ledger::ledger_path(&state).display()
            ),
        );
    };

    if args.patch {
        return show_patch(&record, json);
    }
    let text = if json {
        // Byte-identical to the line `task run --json` printed and logged.
        record.to_line()
    } else {
        serde_json::to_string_pretty(&record)
    };
    match text {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(e) => runtime_error(
            json,
            &format!("task show: cannot encode record: {e}"),
            EXIT_ERROR,
        ),
    }
}

/// Print the patch file verbatim: stdout carries its bytes and nothing else.
fn show_patch(record: &TaskRecord, json: bool) -> i32 {
    let Some(diff) = record.diff.as_ref() else {
        return usage_error(json, &format!("task show: task {} has no diff", record.id));
    };
    let path = Path::new(&diff.patch);
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) => {
            return usage_error(
                json,
                &format!(
                    "task show: cannot read patch {path}: {e}",
                    path = path.display()
                ),
            )
        }
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match std::io::copy(&mut std::io::BufReader::new(file), &mut out) {
        Ok(_) => 0,
        Err(e) => runtime_error(
            json,
            &format!(
                "task show: cannot read patch {path}: {e}",
                path = path.display()
            ),
            EXIT_ERROR,
        ),
    }
}

// ── task list ────────────────────────────────────────────────────────────────

/// `weir task list` — the latest record per id, oldest first, optionally
/// filtered to `created_at >= now - --since`.
pub fn list(cfg: &ConfigV2, args: &TaskListArgs, json: bool) -> i32 {
    let state = match cfg.state_dir_expanded() {
        Ok(dir) => dir,
        Err(e) => return usage_error(json, &format!("task list: {e}")),
    };
    let cutoff = match args.since.as_deref() {
        Some(raw) => match parse_duration(raw) {
            Ok(span) => Some(now_unix() - i64::try_from(span.as_secs()).unwrap_or(i64::MAX)),
            Err(e) => return usage_error(json, &format!("task list: {e}")),
        },
        None => None,
    };

    let rows: Vec<TaskRecord> = ledger::list_sorted(&state)
        .into_iter()
        .filter(|row| cutoff.is_none_or(|from| row.created_at >= from))
        .collect();

    if json {
        return match serde_json::to_string(&rows) {
            Ok(line) => {
                println!("{line}");
                0
            }
            Err(e) => runtime_error(
                json,
                &format!("task list: cannot encode records: {e}"),
                EXIT_ERROR,
            ),
        };
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for row in &rows {
        if let Err(e) = writeln!(
            out,
            "{id}  {status}  exit={exit}  {worker}  files={files}  {cleanup}",
            id = row.id,
            status = row.status,
            exit = row.exit,
            worker = worker_label(row),
            files = row.diff.as_ref().map(|d| d.files).unwrap_or(0),
            cleanup = row.cleanup
        ) {
            return runtime_error(json, &format!("task list: {e}"), EXIT_ERROR);
        }
    }
    0
}

/// `worker`, `worker:replica` or `worker:model` for the human list line.
fn worker_label(row: &TaskRecord) -> String {
    if let Some(replica) = row.replica.as_deref() {
        return format!("{}:{}", row.worker, replica);
    }
    if let Some(model) = row.model.as_deref() {
        return format!("{}:{}", row.worker, model);
    }
    row.worker.clone()
}

// ── task clean ───────────────────────────────────────────────────────────────

/// Which selector was used. Clap already rejects combinations and omissions.
#[derive(Debug)]
enum Selector {
    /// Clean this exact id (a refusal is then an error, not a skip).
    Id(String),
    /// Clean every non-removed record whose branch is merged into its base.
    Merged,
    /// Clean every non-removed record older than these seconds.
    Older(u64),
}

/// One candidate, decided: either removable (`refusal == None`) or skipped
/// with the reason to report.
struct Plan {
    id: String,
    repo: PathBuf,
    worktree: PathBuf,
    branch: String,
    refusal: Option<String>,
}

/// `weir task clean` — remove worktrees and branches; never a prompt or patch.
pub fn clean(cfg: &ConfigV2, args: &TaskCleanArgs, json: bool) -> i32 {
    let state = match cfg.state_dir_expanded() {
        Ok(dir) => dir,
        Err(e) => return usage_error(json, &format!("task clean: {e}")),
    };
    let wt_roots = match cfg.wt_roots_expanded() {
        Ok(roots) => roots,
        Err(e) => return usage_error(json, &format!("task clean: {e}")),
    };

    let selector = match selector_of(args) {
        Ok(selector) => selector,
        Err(e) => return usage_error(json, &format!("task clean: {e}")),
    };

    // Candidates: the explicit id, or the ledger filtered by the bulk rule.
    let records: Vec<TaskRecord> = match &selector {
        Selector::Id(id) => match ledger::find(&state, id) {
            Some(record) => vec![record],
            None => {
                return usage_error(
                    json,
                    &format!(
                        "task clean: no task {id} in the ledger ({path})",
                        path = ledger::ledger_path(&state).display()
                    ),
                )
            }
        },
        Selector::Merged => ledger::list_sorted(&state)
            .into_iter()
            .filter(|row| row.cleanup != "removed")
            .collect(),
        Selector::Older(seconds) => {
            let before = now_unix() - i64::try_from(*seconds).unwrap_or(i64::MAX);
            ledger::list_sorted(&state)
                .into_iter()
                .filter(|row| row.cleanup != "removed" && row.created_at <= before)
                .collect()
        }
    };

    // Phase 1: decide everything without touching git.
    let plans: Vec<Plan> = records
        .iter()
        .map(|row| plan_for(row, &selector, &wt_roots))
        .collect();

    // Phase 2: remove, and log one clean event per removal.
    let mut removed: Vec<String> = Vec::new();
    let mut skipped: Vec<serde_json::Value> = Vec::new();
    let mut removal_failed = false;
    for plan in plans {
        if let Some(reason) = plan.refusal {
            skipped.push(skipped_row(&plan.id, &reason));
            continue;
        }
        let r = records.iter().find(|r| r.id == plan.id).unwrap();
        if let Err(err) = remove_task(&plan, r) {
            removal_failed = true;
            skipped.push(skipped_row(&plan.id, &err));
            continue;
        }
        match ledger::append_clean(&state, &plan.id, now_unix()) {
            Ok(()) => removed.push(plan.id.clone()),
            Err(e) => {
                removal_failed = true;
                skipped.push(skipped_row(
                    &plan.id,
                    &format!("worktree removed, but the clean event was not logged: {e}"),
                ));
            }
        }
    }

    if json {
        let payload = serde_json::json!({"removed": removed, "skipped": skipped});
        match serde_json::to_string(&payload) {
            Ok(line) => println!("{line}"),
            Err(e) => return runtime_error(json, &format!("task clean: {e}"), EXIT_ERROR),
        }
    } else {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for id in &removed {
            let _ = writeln!(out, "removed {id}");
        }
        for row in &skipped {
            let _ = writeln!(
                out,
                "skipped {id}: {reason}",
                id = row["id"].as_str().unwrap_or(""),
                reason = row["reason"].as_str().unwrap_or("")
            );
        }
    }

    // An explicit id that ended up not cleaned is a usage error (2); under a
    // bulk selector a skipped record is normal, but a git failure is not.
    if matches!(selector, Selector::Id(_)) && removed.is_empty() {
        return EXIT_USAGE;
    }
    if removal_failed {
        return EXIT_ERROR;
    }
    0
}

/// The `skipped` entry of the JSON report.
fn skipped_row(id: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({"id": id, "reason": reason})
}

/// The single selector in effect.
fn selector_of(args: &TaskCleanArgs) -> Result<Selector, String> {
    if let Some(id) = &args.id {
        validate_id(id)?;
        return Ok(Selector::Id(id.clone()));
    }
    if args.merged {
        return Ok(Selector::Merged);
    }
    match args.older_than.as_deref() {
        Some(raw) => {
            let span = parse_duration(raw).map_err(|e| e.to_string())?;
            Ok(Selector::Older(span.as_secs()))
        }
        None => Err("one of ID, --merged or --older-than is required".to_string()),
    }
}

/// Decide one candidate without touching git or the filesystem (beyond the
/// existence probes of the safety check).
fn plan_for(record: &TaskRecord, selector: &Selector, wt_roots: &[PathBuf]) -> Plan {
    let mut plan = Plan {
        id: record.id.clone(),
        repo: PathBuf::from(&record.repo),
        worktree: PathBuf::from(&record.worktree),
        branch: record.branch.clone(),
        refusal: None,
    };

    if let Err(reason) = safety_check(record, &plan.worktree, wt_roots) {
        plan.refusal = Some(reason);
        return plan;
    }

    if matches!(selector, Selector::Merged) {
        if plan.repo.as_os_str().is_empty() || !git::is_git_repo(&plan.repo) {
            plan.refusal = Some(format!(
                "repository {} is not a git repository",
                record.repo
            ));
            return plan;
        }
        if !git::branch_merged(&plan.repo, &record.branch, &record.base) {
            plan.refusal = Some(format!(
                "branch {} is not merged into {}",
                record.branch, record.base
            ));
        } else if git::resolve_commit(&plan.repo, &format!("refs/heads/{}", record.branch))
            .is_none_or(|tip| tip == record.base_commit)
        {
            // Jailed workers cannot commit, so a branch still at its base
            // commit is "an ancestor of base" without being merged: its output
            // lives only in the worktree.
            plan.refusal = Some(format!(
                "branch {} has no commits of its own; its work is not merged",
                record.branch
            ));
        } else if git::worktree_dirty(&plan.worktree) {
            plan.refusal = Some("the worktree has uncommitted changes".to_string());
        }
    }

    plan
}

/// The rules that must hold before anything is removed, for the record's own
/// tree and every ladder sibling in `extra_*`: each branch is `wt/<id>` or
/// `wt/<id>-s<N>` and not a protected name, and each worktree lies strictly
/// inside a configured `wt_root`.
fn safety_check(record: &TaskRecord, worktree: &Path, wt_roots: &[PathBuf]) -> Result<(), String> {
    check_tree(&record.id, &record.branch, worktree, wt_roots)?;
    let branches = record.extra_branches.as_deref().unwrap_or_default();
    let worktrees = record.extra_worktrees.as_deref().unwrap_or_default();
    if branches.len() != worktrees.len() {
        return Err("the record's extra_branches and extra_worktrees do not pair up".to_string());
    }
    for (branch, extra) in branches.iter().zip(worktrees) {
        check_tree(&record.id, branch, Path::new(extra), wt_roots)?;
    }
    Ok(())
}

/// One `(branch, worktree)` pair of a task: see [`safety_check`].
fn check_tree(id: &str, branch: &str, worktree: &Path, wt_roots: &[PathBuf]) -> Result<(), String> {
    if !is_task_branch(id, branch) {
        return Err(format!(
            "branch {branch} is not exactly wt/{id} or a wt/{id}-s<N> ladder step"
        ));
    }
    // A recorded ref may be spelled as a full refname; strip it before the
    // protected-name test so `refs/heads/main` cannot sneak through.
    let name = branch
        .strip_prefix("refs/heads/")
        .unwrap_or(branch)
        .strip_prefix("wt/")
        .unwrap_or("");
    if name.is_empty() || name.contains('/') {
        return Err(format!("branch {branch} is not a plain task branch"));
    }
    if matches!(name, "main" | "master" | "develop") {
        return Err(format!("branch {branch} is protected"));
    }
    if worktree.as_os_str().is_empty() {
        return Err("the record has no worktree path".to_string());
    }
    if inside_wt_root(worktree, wt_roots).is_some() {
        Ok(())
    } else {
        Err(format!(
            "worktree {} is not strictly inside a configured wt_root",
            worktree.display()
        ))
    }
}

/// `wt/<id>`, or `wt/<id>-s<N>` with `N >= 2` for a later ladder step.
fn is_task_branch(id: &str, branch: &str) -> bool {
    let Some(rest) = branch.strip_prefix("wt/").and_then(|b| b.strip_prefix(id)) else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    rest.strip_prefix("-s")
        .filter(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
        .and_then(|n| n.parse::<u32>().ok())
        .is_some_and(|n| n >= 2)
}

/// The canonical `wt_root` that strictly contains `worktree`.
///
/// When the worktree directory is already gone, its canonicalized parent plus
/// its own file name are tested instead, so cleaning an already-deleted
/// directory still removes the branch. Both sides are canonicalized: a `wt_root`
/// reachable only through a symlink still matches, and `..`-laden paths cannot
/// escape it.
fn inside_wt_root(worktree: &Path, wt_roots: &[PathBuf]) -> Option<PathBuf> {
    // An existing path must be strictly below a root; a vanished one is
    // rebuilt as canonicalize(parent).join(file_name) and judged the same way.
    let probe = match worktree.canonicalize() {
        Ok(path) => path,
        Err(_) => {
            // The directory is already gone: rebuild the path from its parent.
            // Canonicalizing only the parent would wrongly test the parent
            // itself, which IS the wt_root for a direct child worktree.
            let name = worktree.file_name()?;
            if name == ".." || name == "." {
                return None;
            }
            worktree.parent()?.canonicalize().ok()?.join(name)
        }
    };
    wt_roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .find(|root| root != &probe && probe.starts_with(root))
}

/// `git worktree remove` + `git branch -D` + `git worktree prune`.
///
/// The `prune` last, because a git >= 2.32 worktree registers itself in the
/// parent's ref store as `refs/worktree/<name>`: `branch -D` succeeds but that
/// registration survives it, and only `prune` clears it — which is what makes
/// `git branch --list` stop reporting the branch.
fn remove_task(plan: &Plan, record: &TaskRecord) -> Result<(), String> {
    let mut worktrees = vec![plan.worktree.clone()];
    let mut branches = vec![plan.branch.clone()];
    if let Some(extras) = &record.extra_worktrees {
        for w in extras {
            worktrees.push(std::path::PathBuf::from(w));
        }
    }
    if let Some(extras) = &record.extra_branches {
        for b in extras {
            branches.push(b.clone());
        }
    }

    // Try every tree before reporting, so one missing `-sN` sibling does not
    // leave the others behind; the first error is returned at the end.
    let mut first_err: Option<String> = None;
    for worktree in &worktrees {
        if let Err(e) = git::worktree_remove(&plan.repo, worktree) {
            first_err.get_or_insert(e);
        }
    }
    // A worktree directory that was already deleted leaves its registration
    // behind, and git refuses `branch -D` on a branch it still lists as checked
    // out: prune before the delete, and once more after it.
    git::worktree_prune(&plan.repo)?;
    for branch in &branches {
        if let Err(e) = git::branch_delete(&plan.repo, branch) {
            first_err.get_or_insert(e);
        }
    }
    git::worktree_prune(&plan.repo)?;
    first_err.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::{inside_wt_root, is_task_branch};

    #[test]
    fn task_branch_accepts_the_id_and_its_ladder_steps_only() {
        let id = "20261001-fix-3fa2";
        assert!(is_task_branch(id, "wt/20261001-fix-3fa2"));
        assert!(is_task_branch(id, "wt/20261001-fix-3fa2-s2"));
        assert!(is_task_branch(id, "wt/20261001-fix-3fa2-s10"));
        for bad in [
            "wt/20261001-fix-3fa2-s1",
            "wt/20261001-fix-3fa2-s",
            "wt/20261001-fix-3fa2-sx",
            "wt/20261001-fix-3fa2x",
            "wt/20261001-fix-3fa2/s2",
            "wt/other",
            "main",
            "refs/heads/wt/20261001-fix-3fa2",
        ] {
            assert!(!is_task_branch(id, bad), "{bad} must be refused");
        }
    }

    #[test]
    fn wt_root_containment_is_strict_but_accepts_a_vanished_child() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("wt");
        std::fs::create_dir_all(root.join("alive")).expect("mkdir");
        let roots = vec![root.clone()];

        assert!(inside_wt_root(&root.join("alive"), &roots).is_some());
        // The root itself is never "inside" itself.
        assert!(inside_wt_root(&root, &roots).is_none());
        // A directory that is already gone is judged by its parent.
        assert!(inside_wt_root(&root.join("gone"), &roots).is_some());
        // Escapes and unrelated paths are refused.
        assert!(inside_wt_root(&root.join("../outside"), &roots).is_none());
        assert!(inside_wt_root(&tmp.path().join("other"), &roots).is_none());
    }
}
