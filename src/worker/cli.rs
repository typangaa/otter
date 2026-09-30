//! `weir worker run` — launch one allow-listed worker wrapper.
//!
//! This is the precursor of `weir task run`: it does exactly the spawn part of
//! a task (config → argv → [`run_worker`] → classify → report) without leases,
//! worktrees, checks or a ledger. It only ever execs `pi-worker` /
//! `agy-worker` from PATH — the config's `command` value is re-checked against
//! the allow-list here as well, so even a hand-edited config cannot point weir
//! at another program.
//!
//! Output:
//!
//! * human mode: worker stdout verbatim on stdout, then ONE summary line on
//!   stderr (`weir: worker=… replica=… model=… exit=… kind=… elapsed=…ms
//!   truncated=…`)
//! * `--json`: exactly one JSON object on stdout, nothing else
//!
//! Exit codes: the worker's [`ExitKind`] mapped through
//! [`ExitKind::to_process_code`], plus the spawn-failure conventions
//! (2 = refused by the allow-list / bad arguments, 127 = wrapper not on PATH,
//! 126 = wrapper found but not spawnable).

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Args;

use crate::config::v2::{load_any, AnyConfig, ConfigV2};
use crate::worker::spawn::{
    check_program_allowed, expand_args, run_worker, WorkerOutcome, WorkerSpec, MAX_TIMEOUT_SECS,
};

/// Returned when the wrapper was refused by the allow-list, or the arguments /
/// config are unusable (shell convention: `EX_USAGE`).
const EXIT_REFUSED: i32 = 2;
/// Returned when the wrapper executable is not on PATH.
const EXIT_NOT_FOUND: i32 = 127;
/// Returned when the wrapper exists but could not be spawned.
const EXIT_SPAWN_FAILED: i32 = 126;

/// Which worker to run. Mirrors the two `[worker.*]` tables of config v2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum WorkerKind {
    Pi,
    Agy,
}

impl WorkerKind {
    /// Stable name, matching the `[worker.<name>]` table.
    pub fn as_str(self) -> &'static str {
        match self {
            WorkerKind::Pi => "pi",
            WorkerKind::Agy => "agy",
        }
    }
}

/// `weir worker run` arguments.
#[derive(Debug, Clone, Args)]
pub struct WorkerRunArgs {
    /// Worker to run: `pi` (local llama.cpp wrapper) or `agy` (cloud wrapper).
    #[arg(long, value_name = "WORKER")]
    pub worker: WorkerKind,

    /// Pi replica (`a` or `b`), substituted into `replica_args`. Pi only.
    #[arg(long, value_name = "REPLICA")]
    pub replica: Option<String>,

    /// Model id, substituted into `model_args`. Agy only; defaults to
    /// `[worker.agy] default_model`.
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,

    /// Wrapper timeout in seconds. Default: the worker's `timeout`. Must be > 0.
    #[arg(long, value_name = "SECS")]
    pub timeout: Option<u64>,

    /// Emit one JSON object on stdout instead of text + summary line.
    #[arg(long)]
    pub json: bool,

    /// Working directory handed to the wrapper (must exist).
    pub workdir: PathBuf,

    /// Prompt file handed to the wrapper (must exist).
    pub prompt_file: PathBuf,
}

impl WorkerRunArgs {
    /// Whether the user asked for JSON (either `weir --json …` or `… --json`).
    pub fn json_flag(&self) -> bool {
        self.json
    }
}

/// `weir worker run …` — returns the process exit code.
///
/// `cfg_path` is the config resolved by the dispatcher; it must be a readable
/// `version = 2` file containing the requested `[worker.X]` table. A path that
/// does not exist is a usage error (the dispatcher normally catches this
/// earlier, but `WEIR_CONFIG` is only applied by clap in test invocations).
pub async fn run(cfg_path: &Path, args: &WorkerRunArgs, json: bool) -> i32 {
    if !cfg_path.is_file() {
        return usage_error(
            json,
            &format!("config file not found: {}", cfg_path.display()),
        );
    }
    let cfg = match load_any(cfg_path) {
        Ok(AnyConfig::V2(cfg)) => cfg,
        Ok(AnyConfig::Legacy(_)) => {
            return usage_error(
                json,
                &format!(
                    "worker run requires a version = 2 config ({})",
                    cfg_path.display()
                ),
            )
        }
        Err(e) => return usage_error(json, &format!("{e}")),
    };

    let plan = match build_plan(&cfg, args) {
        Ok(plan) => plan,
        Err(msg) => return usage_error(json, &msg),
    };

    let outcome = match run_worker(&plan.spec).await {
        Ok(outcome) => outcome,
        Err(e) => {
            let code = match e.kind() {
                // Refused by the allow-list (never even spawned).
                std::io::ErrorKind::InvalidInput => EXIT_REFUSED,
                // Wrapper not on PATH.
                std::io::ErrorKind::NotFound => EXIT_NOT_FOUND,
                // Exists but could not be spawned, or another I/O failure.
                _ => EXIT_SPAWN_FAILED,
            };
            eprintln!("weir worker: error: {e}");
            return code;
        }
    };

    if let Err(e) = emit(json, &plan, &outcome) {
        eprintln!("weir worker: error: {e}");
        return EXIT_SPAWN_FAILED;
    }

    outcome.kind.to_process_code()
}

/// The `[worker.*]` table selected for this run.
enum WorkerPlan<'a> {
    Pi(&'a crate::config::v2::PiWorker),
    Agy(&'a crate::config::v2::AgyWorker),
}

/// What a validated invocation boils down to: the spawn spec plus the fields
/// the report needs (replica / model are `None` when not applicable).
#[derive(Debug)]
struct Plan {
    spec: WorkerSpec,
    worker: &'static str,
    replica: Option<String>,
    model: Option<String>,
}

/// Validate the arguments against the config and build the [`WorkerSpec`].
///
/// Every rejection reason is a usage error (exit 2) and is returned before
/// anything is spawned.
fn build_plan(cfg: &ConfigV2, args: &WorkerRunArgs) -> Result<Plan, String> {
    match args.worker {
        WorkerKind::Pi if args.model.is_some() => {
            return Err(
                "worker run: --model does not apply to --worker pi (use --replica a|b)".to_string(),
            );
        }
        WorkerKind::Agy if args.replica.is_some() => {
            return Err(
                "worker run: --replica does not apply to --worker agy (use --model MODEL)"
                    .to_string(),
            );
        }
        _ => {}
    }

    // Both values are substituted into the wrapper's argv, where an empty
    // string or something shaped like a flag would either stall the wrapper or
    // be reinterpreted by it as an option.
    if let Some(r) = args.replica.as_deref() {
        check_substituted_value("--replica", r)?;
    }
    if let Some(m) = args.model.as_deref() {
        check_substituted_value("--model", m)?;
    }

    // A value coming from the config is checked by the same rule as one coming
    // from the command line: `[worker.agy] default_model` ends up in argv too.
    if let WorkerKind::Agy = args.worker {
        if let Some(agy) = cfg.worker.agy.as_ref() {
            if let Some(m) = agy.default_model.as_deref() {
                if let Err(e) = check_substituted_value("default_model", m) {
                    return Err(format!("{e} (from [worker.agy] default_model)"));
                }
            }
        }
    }

    // The requested worker table first: every later message can then name it.
    let worker = match args.worker {
        WorkerKind::Pi => match cfg.worker.pi.as_ref() {
            Some(pi) => {
                if let Err(e) = check_program_allowed(&pi.command) {
                    return Err(format!("worker run: {e}"));
                }
                WorkerPlan::Pi(pi)
            }
            None => return Err("worker run: no [worker.pi] table in this config".to_string()),
        },
        WorkerKind::Agy => match cfg.worker.agy.as_ref() {
            Some(agy) => {
                if let Err(e) = check_program_allowed(&agy.command) {
                    return Err(format!("worker run: {e}"));
                }
                WorkerPlan::Agy(agy)
            }
            None => return Err("worker run: no [worker.agy] table in this config".to_string()),
        },
    };

    let timeout = match args.timeout {
        Some(0) => return Err("worker run: --timeout must be > 0".to_string()),
        Some(secs) => secs,
        None => match &worker {
            WorkerPlan::Pi(pi) => pi.timeout,
            WorkerPlan::Agy(agy) => agy.timeout,
        },
    };
    if timeout == 0 {
        return Err(format!(
            "worker run: [worker.{}] timeout must be > 0",
            args.worker.as_str()
        ));
    }
    // Also enforced by `run_worker`, but a config-provided timeout is a usage
    // error here, and rejecting it before anything is spawned keeps exit code 2
    // (instead of a panicking runtime or an endless run).
    if timeout > MAX_TIMEOUT_SECS {
        let source = if args.timeout.is_some() {
            "--timeout".to_string()
        } else {
            format!("[worker.{}] timeout", args.worker.as_str())
        };
        return Err(format!(
            "worker run: timeout {timeout} exceeds the maximum of {MAX_TIMEOUT_SECS} seconds ({source})"
        ));
    }

    let workdir = existing_dir(&args.workdir)?;
    let prompt_file = existing_file(&args.prompt_file)?;

    match worker {
        WorkerPlan::Pi(pi) => {
            let replica = args.replica.clone();
            let args_prefix = match &replica {
                Some(r) => expand_args(&pi.replica_args, "replica", r),
                None => Vec::new(),
            };
            Ok(Plan {
                spec: WorkerSpec {
                    program: pi.command.clone(),
                    args_prefix,
                    timeout_secs: timeout,
                    workdir,
                    prompt_file,
                    // pi is a local server: exit 3 always means "empty".
                    quota_pattern: None,
                },
                worker: "pi",
                replica,
                model: None,
            })
        }
        WorkerPlan::Agy(agy) => {
            let model = args.model.clone().or_else(|| agy.default_model.clone());
            let args_prefix = match &model {
                Some(m) => expand_args(&agy.model_args, "model", m),
                // No --model and no default_model: no model arguments at all.
                None => Vec::new(),
            };
            Ok(Plan {
                spec: WorkerSpec {
                    program: agy.command.clone(),
                    args_prefix,
                    timeout_secs: timeout,
                    workdir,
                    prompt_file,
                    quota_pattern: Some(agy.quota_pattern.clone()),
                },
                worker: "agy",
                replica: None,
                model,
            })
        }
    }
}

/// Reject a `--replica` / `--model` value that cannot be a real identifier:
/// empty, or starting with `-` (which the wrapper would parse as its own flag,
/// or as the end-of-options marker).
fn check_substituted_value(flag: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("worker run: {flag} must not be empty"));
    }
    if value.starts_with('-') {
        return Err(format!(
            "worker run: {flag} value {value:?} must not start with '-'"
        ));
    }
    Ok(())
}

/// Canonicalise a path that must be an existing directory.
fn existing_dir(path: &Path) -> Result<PathBuf, String> {
    if !path.is_dir() {
        return Err(format!(
            "worker run: workdir {} is not an existing directory",
            path.display()
        ));
    }
    std::fs::canonicalize(path)
        .map_err(|e| format!("worker run: cannot resolve workdir {}: {e}", path.display()))
}

/// Canonicalise a path that must be an existing file.
fn existing_file(path: &Path) -> Result<PathBuf, String> {
    if !path.is_file() {
        return Err(format!(
            "worker run: prompt file {} is not an existing file",
            path.display()
        ));
    }
    std::fs::canonicalize(path).map_err(|e| {
        format!(
            "worker run: cannot resolve prompt file {}: {e}",
            path.display()
        )
    })
}

/// Report one outcome: JSON object on stdout, or verbatim stdout + summary
/// line on stderr.
fn emit(json: bool, plan: &Plan, outcome: &WorkerOutcome) -> std::io::Result<()> {
    if json {
        let obj = serde_json::json!({
            "worker": plan.worker,
            "replica": plan.replica,
            "model": plan.model,
            "exit": outcome.exit_code,
            "kind": outcome.kind.as_str(),
            "elapsed_ms": outcome.elapsed_ms,
            "stdout": outcome.stdout,
            "stderr_tail": outcome.stderr_tail,
            "truncated": outcome.truncated,
        });
        // A single line keeps `--json` output pipe-friendly.
        writeln!(std::io::stdout(), "{obj}")?;
        return std::io::stdout().flush();
    }

    let mut out = std::io::stdout().lock();
    out.write_all(outcome.stdout.as_bytes())?;
    out.flush()?;
    eprintln!(
        "weir: worker={} replica={} model={} exit={} kind={} elapsed={}ms truncated={}",
        plan.worker,
        plan.replica.clone().unwrap_or_else(|| "-".to_string()),
        plan.model.clone().unwrap_or_else(|| "-".to_string()),
        outcome.exit_code,
        outcome.kind.as_str(),
        outcome.elapsed_ms,
        outcome.truncated
    );
    Ok(())
}

/// Print a usage error (stderr, plus a JSON object on stdout in `--json` mode)
/// and hand back exit code 2.
fn usage_error(json: bool, msg: &str) -> i32 {
    eprintln!("error: {msg}");
    if json {
        println!("{}", serde_json::json!({"status": "error", "error": msg}));
    }
    EXIT_REFUSED
}

#[cfg(test)]
mod tests {
    use super::*;

    const PI_ONLY: &str = "version = 2\n[worker.pi]\ncommand = \"pi-worker\"\ntimeout = 900\nslots = { a = \"gpu-a\" }\n[slots.gpu-a]\ncapacity = 1\n";
    const AGY_ONLY: &str = "version = 2\n[worker.agy]\ncommand = \"agy-worker\"\ntimeout = 3600\ndefault_model = \"m-default\"\n";

    fn v2_cfg(text: &str) -> ConfigV2 {
        let c = ConfigV2::parse(text).unwrap();
        c.validate().unwrap();
        c
    }

    fn args(worker: WorkerKind, workdir: &Path, prompt: &Path) -> WorkerRunArgs {
        WorkerRunArgs {
            worker,
            replica: None,
            model: None,
            timeout: None,
            json: false,
            workdir: workdir.to_path_buf(),
            prompt_file: prompt.to_path_buf(),
        }
    }

    /// A real dir + file so the path checks pass and we exercise argv building.
    fn paths() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("wt")).unwrap();
        std::fs::write(tmp.path().join("prompt.md"), "hi").unwrap();
        tmp
    }

    #[test]
    fn pi_flags_and_defaults() {
        let tmp = paths();
        let cfg = v2_cfg(PI_ONLY);
        let wd = tmp.path().join("wt");
        let pf = tmp.path().join("prompt.md");

        let plan = build_plan(&cfg, &args(WorkerKind::Pi, &wd, &pf)).unwrap();
        assert_eq!(plan.spec.program, "pi-worker");
        assert!(plan.spec.args_prefix.is_empty());
        assert_eq!(plan.spec.timeout_secs, 900);
        assert!(plan.spec.quota_pattern.is_none());
        assert_eq!(plan.worker, "pi");
        assert!(plan.replica.is_none());

        let mut a = args(WorkerKind::Pi, &wd, &pf);
        a.replica = Some("b".to_string());
        a.timeout = Some(42);
        let plan = build_plan(&cfg, &a).unwrap();
        assert_eq!(plan.spec.args_prefix, ["-b", "b"]);
        assert_eq!(plan.spec.timeout_secs, 42);
        assert_eq!(plan.replica.as_deref(), Some("b"));
        // Paths are handed to the wrapper canonicalised.
        assert!(plan.spec.workdir.is_absolute());
        assert!(plan.spec.prompt_file.is_absolute());
    }

    #[test]
    fn agy_model_resolution() {
        let tmp = paths();
        let cfg = v2_cfg(AGY_ONLY);
        let wd = tmp.path().join("wt");
        let pf = tmp.path().join("prompt.md");

        // default_model applies.
        let plan = build_plan(&cfg, &args(WorkerKind::Agy, &wd, &pf)).unwrap();
        assert_eq!(plan.spec.args_prefix, ["-m", "m-default"]);
        assert_eq!(plan.model.as_deref(), Some("m-default"));
        assert_eq!(plan.spec.timeout_secs, 3600);
        assert_eq!(
            plan.spec.quota_pattern.as_deref(),
            Some("RESOURCE_EXHAUSTED")
        );

        // --model wins.
        let mut a = args(WorkerKind::Agy, &wd, &pf);
        a.model = Some("other".to_string());
        let plan = build_plan(&cfg, &a).unwrap();
        assert_eq!(plan.spec.args_prefix, ["-m", "other"]);
        assert_eq!(plan.model.as_deref(), Some("other"));
    }

    #[test]
    fn agy_without_any_model_drops_the_args() {
        let tmp = paths();
        let cfg = v2_cfg("version = 2\n[worker.agy]\ncommand = \"agy-worker\"\ntimeout = 60\n");
        let mut a = args(
            WorkerKind::Agy,
            &tmp.path().join("wt"),
            &tmp.path().join("prompt.md"),
        );
        std::fs::create_dir_all(tmp.path().join("wt")).unwrap();
        std::fs::write(tmp.path().join("prompt.md"), "hi").unwrap();
        let plan = build_plan(&cfg, &a).unwrap();
        assert!(plan.spec.args_prefix.is_empty());
        assert!(plan.model.is_none());

        a.model = Some("m1".to_string());
        let plan = build_plan(&cfg, &a).unwrap();
        assert_eq!(plan.spec.args_prefix, ["-m", "m1"]);
    }

    #[test]
    fn rejects_flags_that_do_not_match_the_worker() {
        let tmp = paths();
        let cfg = v2_cfg(&format!(
            "{PI_ONLY}{}\n",
            "[worker.agy]\ncommand = \"agy-worker\"\ntimeout = 60\n"
        ));
        let wd = tmp.path().join("wt");
        let pf = tmp.path().join("prompt.md");

        let mut a = args(WorkerKind::Pi, &wd, &pf);
        a.model = Some("x".to_string());
        let err = build_plan(&cfg, &a).unwrap_err();
        assert!(
            err.contains("--model does not apply to --worker pi"),
            "{err}"
        );

        let mut a = args(WorkerKind::Agy, &wd, &pf);
        a.replica = Some("a".to_string());
        let err = build_plan(&cfg, &a).unwrap_err();
        assert!(
            err.contains("--replica does not apply to --worker agy"),
            "{err}"
        );
    }

    #[test]
    fn rejects_missing_worker_table_and_bad_paths() {
        let tmp = paths();
        let wd = tmp.path().join("wt");
        let pf = tmp.path().join("prompt.md");

        // No [worker.agy] in a pi-only config.
        let cfg = v2_cfg(PI_ONLY);
        let err = build_plan(&cfg, &args(WorkerKind::Agy, &wd, &pf)).unwrap_err();
        assert!(err.contains("no [worker.agy] table"), "{err}");

        // Missing workdir / prompt file.
        let cfg = v2_cfg(PI_ONLY);
        let mut a = args(WorkerKind::Pi, &wd, &pf);
        a.workdir = tmp.path().join("nope");
        let err = build_plan(&cfg, &a).unwrap_err();
        assert!(err.contains("is not an existing directory"), "{err}");

        let mut a = args(WorkerKind::Pi, &wd, &pf);
        a.prompt_file = tmp.path().join("nope.md");
        let err = build_plan(&cfg, &a).unwrap_err();
        assert!(err.contains("is not an existing file"), "{err}");

        // A directory as prompt file, a file as workdir.
        let mut a = args(WorkerKind::Pi, &wd, &pf);
        a.prompt_file = wd.clone();
        assert!(build_plan(&cfg, &a)
            .unwrap_err()
            .contains("is not an existing file"));
    }

    #[test]
    fn rejects_zero_timeout() {
        let tmp = paths();
        let cfg = v2_cfg(PI_ONLY);
        let mut a = args(
            WorkerKind::Pi,
            &tmp.path().join("wt"),
            &tmp.path().join("prompt.md"),
        );
        std::fs::create_dir_all(tmp.path().join("wt")).unwrap();
        std::fs::write(tmp.path().join("prompt.md"), "hi").unwrap();
        a.timeout = Some(0);
        assert!(build_plan(&cfg, &a).unwrap_err().contains("--timeout"));
    }

    #[test]
    fn rejects_timeout_above_the_maximum() {
        let tmp = paths();
        let cfg = v2_cfg(PI_ONLY);
        let wd = tmp.path().join("wt");
        let pf = tmp.path().join("prompt.md");

        // u64::MAX used to panic inside tokio after the spawn.
        let mut a = args(WorkerKind::Pi, &wd, &pf);
        a.timeout = Some(u64::MAX);
        let err = build_plan(&cfg, &a).unwrap_err();
        assert!(
            err.contains(&format!(
                "timeout {} exceeds the maximum of {MAX_TIMEOUT_SECS} seconds",
                u64::MAX
            )),
            "{err}"
        );

        // The boundary itself is legal, one second above is not.
        let mut a = args(WorkerKind::Pi, &wd, &pf);
        a.timeout = Some(MAX_TIMEOUT_SECS);
        assert_eq!(
            build_plan(&cfg, &a).unwrap().spec.timeout_secs,
            MAX_TIMEOUT_SECS
        );
        let mut a = args(WorkerKind::Pi, &wd, &pf);
        a.timeout = Some(MAX_TIMEOUT_SECS + 1);
        assert!(build_plan(&cfg, &a)
            .unwrap_err()
            .contains("exceeds the maximum"));
    }

    #[test]
    fn rejects_a_config_timeout_above_the_maximum() {
        let tmp = paths();
        let cfg = v2_cfg(&format!(
            "version = 2\n[worker.pi]\ncommand = \"pi-worker\"\ntimeout = {}\n",
            MAX_TIMEOUT_SECS + 1
        ));
        let err = build_plan(
            &cfg,
            &args(
                WorkerKind::Pi,
                &tmp.path().join("wt"),
                &tmp.path().join("prompt.md"),
            ),
        )
        .unwrap_err();
        assert!(err.contains("exceeds the maximum"), "{err}");
        assert!(err.contains("[worker.pi] timeout"), "{err}");
    }

    #[test]
    fn rejects_empty_or_dash_leading_replica_and_model() {
        let tmp = paths();
        let wd = tmp.path().join("wt");
        let pf = tmp.path().join("prompt.md");

        let cfg = v2_cfg(PI_ONLY);
        for bad in ["", "-", "--help", "-c", "-m"] {
            let mut a = args(WorkerKind::Pi, &wd, &pf);
            a.replica = Some(bad.to_string());
            let err = build_plan(&cfg, &a).unwrap_err();
            assert!(
                err.contains("--replica")
                    && (err.contains("must not be empty")
                        || err.contains("must not start with '-'")),
                "replica {bad:?}: {err}"
            );
        }

        let cfg = v2_cfg(AGY_ONLY);
        for bad in ["", "-", "--version", "-x"] {
            let mut a = args(WorkerKind::Agy, &wd, &pf);
            a.model = Some(bad.to_string());
            let err = build_plan(&cfg, &a).unwrap_err();
            assert!(
                err.contains("--model")
                    && (err.contains("must not be empty")
                        || err.contains("must not start with '-'")),
                "model {bad:?}: {err}"
            );
        }

        // Ordinary values still work, including ones containing a dash.
        let mut a = args(WorkerKind::Agy, &wd, &pf);
        a.model = Some("gpt-4o-mini".to_string());
        assert_eq!(
            build_plan(&cfg, &a).unwrap().model.as_deref(),
            Some("gpt-4o-mini")
        );
        let mut a = args(WorkerKind::Pi, &wd, &pf);
        a.replica = Some("b".to_string());
        let cfg_pi = v2_cfg(PI_ONLY);
        assert_eq!(
            build_plan(&cfg_pi, &a).unwrap().replica.as_deref(),
            Some("b")
        );
    }

    #[test]
    fn worker_kind_names() {
        assert_eq!(WorkerKind::Pi.as_str(), "pi");
        assert_eq!(WorkerKind::Agy.as_str(), "agy");
    }

    #[test]
    fn expand_helper_is_the_one_from_spawn() {
        // `expand_args` is re-exported from the spawn core, not re-implemented.
        assert_eq!(
            expand_args(&["-b".to_string(), "{replica}".to_string()], "replica", "a"),
            ["-b", "a"]
        );
        assert_eq!(expand_args(&["{model}".to_string()], "model", "m"), ["m"]);
    }
}
