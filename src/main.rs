//! `weir` — single-binary CLI agent orchestrator.
//!
//! Entry point: parses CLI arguments with [`clap`] derive, dispatches to the
//! appropriate handler in the `cli::*` modules, and handles exit codes:
//!
//! | Code | Meaning                                          |
//! |------|--------------------------------------------------|
//! |  0   | Success                                          |
//! |  1   | User / config error (invalid args, bad TOML, …) |
//! |  2   | System / unexpected error (I/O, network, …)     |

use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};

use crate::backends::stdio_cli::StdioCliBackend;
use crate::backends::{Backend, ChatMessage, ChatRequest};
use crate::config::BackendKind;
use crate::error::{Result, WeirError};
use crate::exit::ExitKind;
use crate::observability::{Metrics, MetricsPersister};
use crate::resilience::ResilientBackend;

mod backends;
mod cli;
mod config;
mod engine;
mod error;
mod exit;
mod lease;
mod observability;
mod resilience;
mod task;
mod worker;

// ── top-level CLI ─────────────────────────────────────────────────────────────

/// weir — single-binary CLI agent orchestrator.
#[derive(Debug, Parser)]
#[command(
    name = "weir",
    version,
    about = "CLI agent orchestrator — compose, route and fan-out local AI agent CLIs",
    long_about = None,
)]
struct Cli {
    /// Path to the weir config file.
    ///
    /// Resolution order: this flag > `$WEIR_CONFIG` >
    /// `$XDG_CONFIG_HOME/weir/weir.toml` > `$HOME/.config/weir/weir.toml`.
    /// `./weir.toml` in the current directory is deliberately never searched.
    #[arg(short, long, value_name = "PATH", env = "WEIR_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// Emit machine-readable JSON on stdout for all commands.
    #[arg(long, global = true)]
    json: bool,

    /// Default log level (e.g. "info", "debug", "weir=debug,info").
    /// Overridden by the RUST_LOG env var when set.
    #[arg(long, value_name = "LEVEL", default_value = "info", global = true)]
    log_level: String,

    /// Log format: "pretty" for human-readable, "json" for structured JSON lines.
    #[arg(long, value_name = "FORMAT", default_value = "pretty", global = true)]
    log_format: LogFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, clap::ValueEnum)]
enum LogFormat {
    Pretty,
    Json,
}

// ── subcommands ───────────────────────────────────────────────────────────────

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate the config file and exit.
    ///
    /// Works for both the legacy (v0.5) schema and `version = 2`. With
    /// `--deep` (v2 configs only) also probes the environment: worker
    /// wrappers on PATH, a working `bwrap`, every `wt_roots` entry present in
    /// agent-jail's `WT_ROOTS`, and a creatable `state_dir`.
    Validate(ValidateArgs),

    /// Manage backends.
    #[command(subcommand)]
    Backend(BackendCommand),

    /// Manage workflows.
    #[command(subcommand)]
    Workflow(WorkflowCommand),

    /// Print a summary of the current configuration and backend metrics.
    Status,

    /// Print version and build information.
    Version,

    /// Print the JSON Schema for weir.toml to stdout.
    Schema,

    /// Send a prompt directly to a named backend and print the response.
    ///
    /// weir reads the config and calls the backend in-process. Ideal for
    /// scripts and skill invocations.
    ///
    /// Example:
    ///   weir chat agy "Summarise this file: $(cat notes.txt)"
    Chat(ChatArgs),

    /// GPU slot leases — run a command under an exclusive flock-based slot,
    /// or report which slots are held.
    ///
    /// Neither subcommand reads weir.toml. Only commands named `pi-worker` or
    /// `agy-worker` may take a lease; `{replica}` in the args is replaced by the
    /// replica of the slot that was acquired.
    ///
    /// Example:
    ///   weir lease run --slot gpu-a=a --slot gpu-b=b -- pi-worker -b {replica} wt notes.md
    #[command(subcommand)]
    Lease(LeaseCommand),

    /// Task lifecycle: run a worker in a fresh git worktree, then inspect or
    /// garbage-collect the recorded tasks.
    ///
    /// `run` admits the task (id, prompt file, worktree, GPU slot lease), runs
    /// one allow-listed worker wrapper in it, captures the diff and a patch
    /// file, runs the `[check.*]` entries under `agent-jail`, applies the
    /// cleanup policy and appends one `weir.task/1` line to the ledger. `show`,
    /// `list` and `clean` read that ledger; `clean` never removes a branch
    /// other than `wt/<id>` nor a path outside `paths.wt_roots`.
    ///
    /// Example:
    ///   weir task run --repo ~/repo --base main --worker pi \\
    ///       --prompt-file notes.md --check typecheck --json
    #[command(subcommand)]
    Task(task::cli::TaskCommand),

    /// Run one worker wrapper directly (precursor of `weir task run`).
    ///
    /// Reads the `version = 2` config, builds the wrapper argv
    /// (`<worker>-worker [-b REPLICA|-m MODEL] -t <secs> <WORKDIR> <PROMPT_FILE>`)
    /// and spawns it in its own process group with capped output and a hard
    /// deadline. It only ever execs `pi-worker` / `agy-worker` from PATH — the
    /// allow-list is hard-coded and re-checked at spawn time. No lease, no
    /// worktree, no checks: that is what `weir task run` will add.
    ///
    /// Example:
    ///   weir worker run --worker pi --replica a /wt/notes /tmp/prompt.md
    #[command(subcommand)]
    Worker(WorkerCommand),

    /// Inspect the configuration itself.
    #[command(subcommand)]
    Config(ConfigCommand),
}

/// `weir worker …` subcommands.
#[derive(Debug, Subcommand)]
enum WorkerCommand {
    /// Run one allow-listed worker wrapper and report its result.
    Run(worker::cli::WorkerRunArgs),
}

#[derive(Debug, Args)]
struct ValidateArgs {
    /// Also probe the environment (v2 configs only): worker wrappers on PATH,
    /// a working bwrap, agent-jail WT_ROOTS, and a creatable state_dir.
    #[arg(long)]
    deep: bool,
}

/// `weir config …` subcommands.
#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the resolved config file path and exit.
    Path,
}

/// `weir lease …` subcommands.
#[derive(Debug, Subcommand)]
enum LeaseCommand {
    /// Acquire a slot lease and run the command under it.
    Run(lease::LeaseRunArgs),

    /// Show which slots are held and which are free.
    Status(lease::LeaseStatusArgs),
}

// ── backend subcommands ───────────────────────────────────────────────────────

#[derive(Debug, Subcommand)]
enum BackendCommand {
    /// List all configured backends.
    List,

    /// Test connectivity / health of a named backend.
    Test {
        /// Name of the backend to test.
        name: String,
    },

    /// Add a new backend.
    #[command(subcommand)]
    Add(BackendAddCommand),

    /// Remove a backend by name.
    Remove {
        /// Name of the backend to remove.
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum BackendAddCommand {
    /// Add a local stdio CLI agent (e.g. hermes, claude, agy, gemini).
    Cli(BackendAddCli),
}

#[derive(Debug, Args)]
struct BackendAddCli {
    /// Unique name for this backend.
    name: String,

    /// Executable to invoke.
    #[arg(long, value_name = "CMD")]
    command: String,

    /// Argument(s) to pass. Use {prompt} as the placeholder for the user message.
    #[arg(long = "arg", value_name = "ARG")]
    args: Vec<String>,
}

// ── workflow subcommands ──────────────────────────────────────────────────────

#[derive(Debug, Subcommand)]
enum WorkflowCommand {
    /// List all configured workflows.
    List,

    /// Add a new workflow.
    #[command(subcommand)]
    Add(WorkflowAddCommand),

    /// Remove a workflow by name.
    Remove {
        /// Name of the workflow to remove.
        name: String,
    },

    /// Run a workflow directly from the CLI and print the result.
    ///
    /// Works for all four patterns: fan-out, pipeline, router, eval-loop.
    ///
    /// Examples:
    ///   weir workflow run dual-review "Review this PR: ..."
    ///   weir workflow run quality-loop --criteria "Must be under 100 words" "Write a summary"
    Run(WorkflowRunArgs),
}

#[derive(Debug, Subcommand)]
enum WorkflowAddCommand {
    /// Add a fan-out workflow (dispatch to multiple backends in parallel).
    Fanout(WorkflowAddFanout),

    /// Add a pipeline workflow (chain backends sequentially).
    Pipeline(WorkflowAddPipeline),
}

#[derive(Debug, Args)]
struct WorkflowAddFanout {
    /// Unique name for this workflow.
    name: String,

    /// Backend(s) to fan-out to (specify multiple times).
    #[arg(long = "backend", value_name = "BACKEND", required = true)]
    backends: Vec<String>,

    /// Aggregation strategy for combining responses.
    #[arg(long, value_name = "STRATEGY", default_value = "all")]
    aggregation: String,
}

#[derive(Debug, Args)]
struct WorkflowAddPipeline {
    /// Unique name for this workflow.
    name: String,

    /// Pipeline step(s) in order: BACKEND or BACKEND:TEMPLATE.
    /// Use {{step.output}} in the template to inject the previous step's output.
    /// Specify multiple times to add steps.
    #[arg(long = "step", value_name = "BACKEND[:TEMPLATE]", required = true)]
    steps: Vec<String>,
}

// ── chat / workflow run args ──────────────────────────────────────────────────

#[derive(Debug, Args)]
struct ChatArgs {
    /// Name of the backend to use (must exist in weir.toml).
    backend: String,

    /// The prompt to send. Pass `-` to read from stdin.
    prompt: String,

    /// Optional system message prepended before the user prompt.
    #[arg(long, value_name = "MSG")]
    system: Option<String>,

    /// Maximum tokens to generate.
    #[arg(long, value_name = "N")]
    max_tokens: Option<u32>,

    /// Sampling temperature (0.0–1.0).
    #[arg(long, value_name = "F")]
    temperature: Option<f32>,

    /// Model name to pass to the backend via `{model}` arg substitution.
    /// For stdio-cli backends (e.g. hermes-openrouter), substituted into the
    /// args template. Omitting it drops any `-m {model}` pair from the args.
    #[arg(long, value_name = "MODEL")]
    model: Option<String>,
}

#[derive(Debug, Args)]
struct WorkflowRunArgs {
    /// Name of the workflow to run (must exist in weir.toml).
    name: String,

    /// The prompt to pass to the workflow.
    prompt: String,

    /// Criteria for eval-loop workflows (ignored for other patterns).
    #[arg(long, value_name = "TEXT")]
    criteria: Option<String>,

    // ── call-time backend overrides (empty/None ⇒ use the weir.toml default) ──
    /// Override the backend list (fan-out / router / fusion panel). Repeat for
    /// multiple; replaces the configured list entirely. Names must exist in weir.toml.
    #[arg(long = "backend", value_name = "BACKEND")]
    backends: Vec<String>,

    /// Override the pipeline steps as BACKEND[:TEMPLATE]. Repeat for multiple;
    /// replaces the configured steps. Use {{step.output}} in the template.
    #[arg(long = "step", value_name = "BACKEND[:TEMPLATE]")]
    steps: Vec<String>,

    /// Override the eval-loop generator backend.
    #[arg(long, value_name = "BACKEND")]
    generator: Option<String>,

    /// Override the eval-loop evaluator backend.
    #[arg(long, value_name = "BACKEND")]
    evaluator: Option<String>,

    /// Override the fusion judge backend.
    #[arg(long, value_name = "BACKEND")]
    judge: Option<String>,

    /// Override the fusion synthesizer backend.
    #[arg(long, value_name = "BACKEND")]
    synthesizer: Option<String>,
}

// ── entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    let explicit_config = cli.config.clone();
    let log_level = cli.log_level.clone();
    let json_log = matches!(cli.log_format, LogFormat::Json);

    let exit_code = dispatch(cli, explicit_config.as_deref(), json, json_log, &log_level).await;
    process::exit(exit_code);
}

/// Resolve the config file for a command that needs one.
///
/// On failure the message goes to stderr (plus a JSON object on stdout in
/// `--json` mode) and the caller returns [`ExitKind::Usage`]'s process code
/// (2) straight away.
fn resolve_config_or_exit(
    explicit: Option<&Path>,
    json: bool,
) -> std::result::Result<PathBuf, i32> {
    match config::resolve::resolve(explicit) {
        Ok(path) => {
            if explicit.is_some() && !path.is_file() {
                eprintln!("error: {}", config::resolve::missing_explicit(&path));
                return Err(ExitKind::Usage.to_process_code());
            }
            Ok(path)
        }
        Err(e) => {
            cli::validate::report_unresolved(&e, json);
            Err(ExitKind::Usage.to_process_code())
        }
    }
}

async fn dispatch(
    cli: Cli,
    explicit_config: Option<&Path>,
    json: bool,
    json_log: bool,
    log_level: &str,
) -> i32 {
    // All commands are short-lived CLI invocations; wire tracing up front so
    // `--log-level` / `--log-format` take effect. Logs go to stderr, leaving
    // stdout clean for command output (and `--json`).
    observability::init_tracing(json_log, log_level);

    // Commands that read the config resolve it lazily: `lease`, `version`,
    // `schema` and `config path` must keep working with no config at all.
    let needs_config = !matches!(
        cli.command,
        Command::Version | Command::Schema | Command::Lease(_) | Command::Config(_)
    );
    let resolved = if needs_config {
        match resolve_config_or_exit(explicit_config, json) {
            Ok(p) => Some(p),
            Err(code) => return code,
        }
    } else {
        None
    };
    let config_path: &Path = resolved
        .as_deref()
        .unwrap_or_else(|| Path::new("weir.toml"));

    match cli.command {
        // ── validate ──────────────────────────────────────────────────────────
        Command::Validate(args) => {
            match cli::validate::validate_config(config_path, args.deep, json) {
                cli::validate::ValidateStatus::Ok => 0,
                cli::validate::ValidateStatus::Invalid => 1,
                cli::validate::ValidateStatus::Usage => ExitKind::Usage.to_process_code(),
            }
        }

        // ── backend list ──────────────────────────────────────────────────────
        Command::Backend(BackendCommand::List) => match config::Config::load(config_path) {
            Ok(cfg) => {
                cli::backend::list_backends(&cfg, json);
                0
            }
            Err(e) => {
                eprintln!("error: {e}");
                exit_code_for(&e)
            }
        },

        // ── backend test ──────────────────────────────────────────────────────
        Command::Backend(BackendCommand::Test { name }) => {
            match config::Config::load(config_path) {
                Ok(cfg) => match cli::backend::test_backend(&cfg, &name, json).await {
                    Ok(()) => 0,
                    Err(e) => exit_code_for(&e),
                },
                Err(e) => {
                    eprintln!("error: {e}");
                    exit_code_for(&e)
                }
            }
        }

        // ── backend add cli ───────────────────────────────────────────────────
        Command::Backend(BackendCommand::Add(BackendAddCommand::Cli(args))) => {
            match cli::backend::add_backend_cli(config_path, &args.name, &args.command, &args.args)
            {
                Ok(()) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({"status":"ok","action":"added","name":args.name})
                        );
                    } else {
                        println!("Added stdio-cli backend '{}'.", args.name);
                    }
                    0
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    exit_code_for(&e)
                }
            }
        }

        // ── backend remove ────────────────────────────────────────────────────
        Command::Backend(BackendCommand::Remove { name }) => {
            match cli::backend::remove_backend(config_path, &name) {
                Ok(()) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({"status":"ok","action":"removed","name":name})
                        );
                    } else {
                        println!("Removed backend '{name}'.");
                    }
                    0
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    exit_code_for(&e)
                }
            }
        }

        // ── workflow list ─────────────────────────────────────────────────────
        Command::Workflow(WorkflowCommand::List) => match config::Config::load(config_path) {
            Ok(cfg) => {
                cli::workflow::list_workflows(&cfg, json);
                0
            }
            Err(e) => {
                eprintln!("error: {e}");
                exit_code_for(&e)
            }
        },

        // ── workflow add fanout ───────────────────────────────────────────────
        Command::Workflow(WorkflowCommand::Add(WorkflowAddCommand::Fanout(args))) => {
            match cli::workflow::add_fanout_workflow(
                config_path,
                &args.name,
                &args.backends,
                &args.aggregation,
            ) {
                Ok(()) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({"status":"ok","action":"added","name":args.name,"pattern":"fan-out"})
                        );
                    } else {
                        println!("Added fan-out workflow '{}'.", args.name);
                    }
                    0
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    exit_code_for(&e)
                }
            }
        }

        // ── workflow add pipeline ─────────────────────────────────────────────
        Command::Workflow(WorkflowCommand::Add(WorkflowAddCommand::Pipeline(args))) => {
            // Parse BACKEND[:TEMPLATE] tokens.
            let steps: Vec<(String, Option<String>)> =
                args.steps.iter().map(|s| parse_step_spec(s)).collect();

            match cli::workflow::add_pipeline_workflow(config_path, &args.name, &steps) {
                Ok(()) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({"status":"ok","action":"added","name":args.name,"pattern":"pipeline"})
                        );
                    } else {
                        println!("Added pipeline workflow '{}'.", args.name);
                    }
                    0
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    exit_code_for(&e)
                }
            }
        }

        // ── workflow remove ───────────────────────────────────────────────────
        Command::Workflow(WorkflowCommand::Remove { name }) => {
            match cli::workflow::remove_workflow(config_path, &name) {
                Ok(()) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({"status":"ok","action":"removed","name":name})
                        );
                    } else {
                        println!("Removed workflow '{name}'.");
                    }
                    0
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    exit_code_for(&e)
                }
            }
        }

        // ── chat ─────────────────────────────────────────────────────────────
        Command::Chat(args) => match run_chat(config_path, args, json).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e}");
                exit_code_for(&e)
            }
        },

        // ── workflow run ──────────────────────────────────────────────────────
        Command::Workflow(WorkflowCommand::Run(args)) => {
            match run_workflow(config_path, args, json).await {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("error: {e}");
                    exit_code_for(&e)
                }
            }
        }

        // ── status ────────────────────────────────────────────────────────────
        Command::Status => match config::Config::load(config_path) {
            Ok(cfg) => {
                let metrics_file = observability::metrics_path();
                let metrics_snap = observability::load_snapshot(&metrics_file);

                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "backend_count":  cfg.backends.len(),
                            "workflow_count": cfg.workflows.len(),
                            "backends": cfg.backends.iter().map(|b| &b.name).collect::<Vec<_>>(),
                            "workflows": cfg.workflows.iter().map(|w| &w.name).collect::<Vec<_>>(),
                            "metrics": metrics_snap,
                        })
                    );
                } else {
                    println!("Backends:  {} configured", cfg.backends.len());
                    let per_backend = metrics_snap
                        .as_ref()
                        .and_then(|m| m.get("backends"))
                        .and_then(|b| b.as_object());
                    for b in &cfg.backends {
                        match per_backend.and_then(|m| m.get(&b.name)) {
                            Some(stat) => {
                                let req =
                                    stat.get("requests").and_then(|v| v.as_u64()).unwrap_or(0);
                                let err = stat.get("errors").and_then(|v| v.as_u64()).unwrap_or(0);
                                let avg = stat
                                    .get("avg_latency_ms")
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                let circuit = stat
                                    .get("circuit")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("closed");
                                println!(
                                    "  - {:<16} {:>5} req, {:>3} err, avg {:>7.1} ms  [{}]",
                                    b.name,
                                    req,
                                    err,
                                    avg,
                                    circuit.to_uppercase()
                                );
                            }
                            None => println!("  - {}", b.name),
                        }
                    }
                    println!("Workflows: {} configured", cfg.workflows.len());
                    for w in &cfg.workflows {
                        println!("  - {} ({})", w.name, w.pattern);
                    }
                    if let Some(snap) = &metrics_snap {
                        if let Some(updated) = snap.get("updated_unix").and_then(|v| v.as_u64()) {
                            println!(
                                "(metrics from {}, updated_unix={})",
                                metrics_file.display(),
                                updated
                            );
                        }
                    } else {
                        println!("(no metrics recorded yet at {})", metrics_file.display());
                    }
                }
                0
            }
            Err(e) => {
                eprintln!("error: {e}");
                exit_code_for(&e)
            }
        },

        // ── task run / show / list / clean (need a v2 config) ─────────────
        Command::Task(sub) => match sub {
            task::cli::TaskCommand::Run(args) => {
                task::run::run(config_path, &args, json || args.json).await
            }
            task::cli::TaskCommand::Show(args) => match task::cli::load_v2(config_path) {
                Ok(cfg) => task::cli::show(&cfg, &args, json || args.json),
                Err(e) => task::cli::usage_error(json, &e),
            },
            task::cli::TaskCommand::List(args) => match task::cli::load_v2(config_path) {
                Ok(cfg) => task::cli::list(&cfg, &args, json || args.json),
                Err(e) => task::cli::usage_error(json, &e),
            },
            task::cli::TaskCommand::Clean(args) => match task::cli::load_v2(config_path) {
                Ok(cfg) => task::cli::clean(&cfg, &args, json || args.json),
                Err(e) => task::cli::usage_error(json, &e),
            },
        },

        // ── worker run (needs a v2 config) ────────────────────────────────────
        Command::Worker(WorkerCommand::Run(args)) => {
            worker::cli::run(config_path, &args, json || args.json_flag()).await
        }

        // ── lease (never touches weir.toml) ───────────────────────────────────
        Command::Lease(LeaseCommand::Run(args)) => lease::run(args).await,
        Command::Lease(LeaseCommand::Status(args)) => lease::status(json || args.json_flag()),

        // ── config path ──────────────────────────────────────────────────────
        Command::Config(ConfigCommand::Path) => match config::resolve::resolve(explicit_config) {
            Ok(path) => {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({"status": "ok", "path": path.display().to_string()})
                    );
                } else {
                    println!("{}", path.display());
                }
                0
            }
            Err(e) => {
                cli::validate::report_unresolved(&e, json);
                ExitKind::Usage.to_process_code()
            }
        },

        // ── version ───────────────────────────────────────────────────────────
        Command::Version => {
            cli::status::show_version(json);
            0
        }

        // ── schema ────────────────────────────────────────────────────────────
        Command::Schema => {
            cli::status::show_schema(json);
            0
        }
    }
}

// ── run helpers ──────────────────────────────────────────────────────────────

/// Build a resilient [`Backend`] instance from config. The raw backend is
/// wrapped in a [`ResilientBackend`] so every CLI call gets retry +
/// circuit-breaking + rate-limiting + metrics.
async fn build_backend(
    cfg: &config::Config,
    name: &str,
    metrics: &Arc<Metrics>,
) -> Result<Arc<dyn Backend>> {
    let bc = cfg
        .backends
        .iter()
        .find(|b| b.name == name)
        .ok_or_else(|| WeirError::BackendNotFound(name.to_owned()))?;

    let inner: Arc<dyn Backend> = match &bc.kind {
        BackendKind::StdioCli { .. } => Arc::new(StdioCliBackend::new(bc)?),
    };

    let resolved = cfg.resilience_for(name);
    let bm = metrics.get_or_create(name).await;
    Ok(Arc::new(ResilientBackend::new(inner, &resolved, bm)))
}

/// Build several resilient backends by name, in order.
async fn build_backends(
    cfg: &config::Config,
    names: impl IntoIterator<Item = String>,
    metrics: &Arc<Metrics>,
) -> Result<Vec<Arc<dyn Backend>>> {
    let mut out = Vec::new();
    for n in names {
        out.push(build_backend(cfg, &n, metrics).await?);
    }
    Ok(out)
}

/// Best-effort flush of this process's metrics delta to the on-disk file.
/// Failures are logged at debug level and never surfaced to the user.
async fn flush_metrics(metrics: &Arc<Metrics>) {
    if let Err(e) = MetricsPersister::at_default_path().flush(metrics).await {
        tracing::debug!(error = %e, "metrics flush failed");
    }
}

/// `weir chat BACKEND PROMPT` — oneshot call, prints response to stdout.
async fn run_chat(path: &Path, args: ChatArgs, json: bool) -> Result<()> {
    let cfg = config::Config::load(path)?;

    let prompt = if args.prompt == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(WeirError::Io)?;
        buf.trim_end().to_owned()
    } else {
        args.prompt.clone()
    };

    let metrics = Arc::new(Metrics::new());
    let backend = build_backend(&cfg, &args.backend, &metrics).await?;

    let mut messages = Vec::new();
    if let Some(sys) = &args.system {
        messages.push(ChatMessage::system(sys));
    }
    messages.push(ChatMessage::user(&prompt));

    let req = ChatRequest {
        messages,
        max_tokens: args.max_tokens,
        temperature: args.temperature,
        model: args.model,
    };
    let result = backend.chat(req).await;
    flush_metrics(&metrics).await;
    let resp = result?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "backend": resp.backend_name,
                "content": resp.content,
            })
        );
    } else {
        print!("{}", resp.content);
        // ensure trailing newline if content doesn't have one
        if !resp.content.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}

/// `weir workflow run NAME PROMPT` — runs any workflow pattern, prints results.
async fn run_workflow(path: &Path, args: WorkflowRunArgs, json: bool) -> Result<()> {
    let cfg = config::Config::load(path)?;

    let wf = cfg
        .workflows
        .iter()
        .find(|w| w.name == args.name)
        .ok_or_else(|| WeirError::WorkflowNotFound(args.name.clone()))?
        .clone();

    // Apply any call-time backend overrides (no flag ⇒ TOML default).
    let wf = effective_workflow(&cfg, &wf, &args)?;

    let metrics = Arc::new(Metrics::new());

    let outcome = run_workflow_inner(&cfg, &wf, &args, json, &metrics).await;
    flush_metrics(&metrics).await;
    outcome
}

/// Split a `BACKEND[:TEMPLATE]` step spec. Shared by `workflow add pipeline` and
/// `workflow run --step`. The first `:` separates the backend from the template;
/// a missing template yields `None`.
fn parse_step_spec(s: &str) -> (String, Option<String>) {
    match s.split_once(':') {
        Some((backend, tmpl)) => (backend.to_owned(), Some(tmpl.to_owned())),
        None => (s.to_owned(), None),
    }
}

/// Apply call-time overrides from `args` onto a copy of the configured workflow.
///
/// Each supplied flag fully replaces that slot; absent flags keep the TOML
/// value. Fails fast (before any backend runs) if a flag is set that the
/// pattern does not consume, or if any referenced backend is not defined in
/// `weir.toml`.
fn effective_workflow(
    cfg: &config::Config,
    wf: &config::WorkflowConfig,
    args: &WorkflowRunArgs,
) -> Result<config::WorkflowConfig> {
    // Which override flags were supplied, by their CLI spelling.
    let supplied: Vec<&str> = [
        ("--backend", !args.backends.is_empty()),
        ("--step", !args.steps.is_empty()),
        ("--generator", args.generator.is_some()),
        ("--evaluator", args.evaluator.is_some()),
        ("--judge", args.judge.is_some()),
        ("--synthesizer", args.synthesizer.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, set)| set.then_some(name))
    .collect();

    // Applicability: reject flags the pattern does not use (fail-fast).
    let applicable: &[&str] = match wf.pattern.as_str() {
        "fan-out" | "router" => &["--backend"],
        "pipeline" => &["--step"],
        "eval-loop" => &["--generator", "--evaluator"],
        "fusion" => &["--backend", "--judge", "--synthesizer"],
        other => {
            return Err(WeirError::Validation(format!(
                "unknown workflow pattern: {other}"
            )));
        }
    };
    for flag in &supplied {
        if !applicable.contains(flag) {
            return Err(WeirError::Validation(format!(
                "workflow '{}' (pattern '{}'): flag {flag} does not apply to this pattern",
                wf.name, wf.pattern
            )));
        }
    }

    // Apply overrides onto a clone.
    let mut eff = wf.clone();
    if !args.backends.is_empty() {
        eff.backends = args.backends.clone();
    }
    if !args.steps.is_empty() {
        eff.steps = args
            .steps
            .iter()
            .map(|s| {
                let (backend, prompt_template) = parse_step_spec(s);
                config::PipelineStep {
                    backend,
                    role: None,
                    prompt_template,
                }
            })
            .collect();
    }
    if let Some(g) = &args.generator {
        eff.generator = Some(g.clone());
    }
    if let Some(e) = &args.evaluator {
        eff.evaluator = Some(e.clone());
    }
    if let Some(j) = &args.judge {
        eff.judge = Some(j.clone());
    }
    if let Some(s) = &args.synthesizer {
        eff.synthesizer = Some(s.clone());
    }

    // Existence: every referenced backend must be defined in weir.toml.
    let mut referenced: Vec<&str> = Vec::new();
    referenced.extend(eff.backends.iter().map(String::as_str));
    referenced.extend(eff.steps.iter().map(|s| s.backend.as_str()));
    referenced.extend(eff.generator.as_deref());
    referenced.extend(eff.evaluator.as_deref());
    referenced.extend(eff.judge.as_deref());
    referenced.extend(eff.synthesizer.as_deref());
    for name in referenced {
        if !cfg.backends.iter().any(|b| b.name == name) {
            return Err(WeirError::BackendNotFound(name.to_string()));
        }
    }

    Ok(eff)
}

/// Inner workflow dispatch (wrapped so metrics are flushed regardless of outcome).
async fn run_workflow_inner(
    cfg: &config::Config,
    wf: &config::WorkflowConfig,
    args: &WorkflowRunArgs,
    json: bool,
    metrics: &Arc<Metrics>,
) -> Result<()> {
    match wf.pattern.as_str() {
        "fan-out" => {
            let backends = build_backends(cfg, wf.backends.iter().cloned(), metrics).await?;

            let req = ChatRequest {
                messages: vec![ChatMessage::user(&args.prompt)],
                max_tokens: None,
                temperature: None,
                model: None,
            };
            let responses = engine::fan_out::run(&backends, req, 8).await?;

            if json {
                let items: Vec<serde_json::Value> = responses
                    .iter()
                    .map(|r| serde_json::json!({"backend": r.backend_name, "content": r.content}))
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({"workflow": wf.name, "pattern": "fan-out", "results": items})
                );
            } else {
                for r in &responses {
                    println!("=== {} ===\n{}", r.backend_name, r.content.trim_end());
                    println!();
                }
            }
        }

        "pipeline" => {
            let backends =
                build_backends(cfg, wf.steps.iter().map(|s| s.backend.clone()), metrics).await?;

            let resp = engine::pipeline::run(&backends, &wf.steps, &args.prompt).await?;

            if json {
                println!(
                    "{}",
                    serde_json::json!({"workflow": wf.name, "pattern": "pipeline", "content": resp.content})
                );
            } else {
                print!("{}", resp.content);
                if !resp.content.ends_with('\n') {
                    println!();
                }
            }
        }

        "router" => {
            let backend_name = wf.backends.first().ok_or_else(|| {
                WeirError::Validation(format!("workflow '{}': no backend", wf.name))
            })?;
            let backend = build_backend(cfg, backend_name, metrics).await?;

            let req = ChatRequest {
                messages: vec![ChatMessage::user(&args.prompt)],
                max_tokens: None,
                temperature: None,
                model: None,
            };
            let resp = engine::router::run(backend, req).await?;

            if json {
                println!(
                    "{}",
                    serde_json::json!({"workflow": wf.name, "pattern": "router", "content": resp.content})
                );
            } else {
                print!("{}", resp.content);
                if !resp.content.ends_with('\n') {
                    println!();
                }
            }
        }

        "eval-loop" => {
            let gen_name = wf.generator.as_deref().ok_or_else(|| {
                WeirError::Validation(format!("workflow '{}': missing generator", wf.name))
            })?;
            let eval_name = wf.evaluator.as_deref().ok_or_else(|| {
                WeirError::Validation(format!("workflow '{}': missing evaluator", wf.name))
            })?;

            let generator = build_backend(cfg, gen_name, metrics).await?;
            let evaluator = build_backend(cfg, eval_name, metrics).await?;

            let criteria = args
                .criteria
                .as_deref()
                .unwrap_or("The response should be accurate, helpful, and complete.");
            let max_iter = wf.max_iterations.unwrap_or(5);

            let result =
                engine::eval_loop::run(generator, evaluator, &args.prompt, criteria, max_iter)
                    .await?;

            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "workflow":   wf.name,
                        "pattern":    "eval-loop",
                        "content":    result.response.content,
                        "iterations": result.iterations,
                        "passed":     result.passed,
                    })
                );
            } else {
                println!(
                    "(iterations: {}, passed: {})",
                    result.iterations, result.passed
                );
                print!("{}", result.response.content);
                if !result.response.content.ends_with('\n') {
                    println!();
                }
            }
        }

        "fusion" => {
            let panel = build_backends(cfg, wf.backends.iter().cloned(), metrics).await?;

            let judge_name = wf.judge.as_deref().ok_or_else(|| {
                WeirError::Validation(format!("workflow '{}': missing judge", wf.name))
            })?;
            let judge = build_backend(cfg, judge_name, metrics).await?;

            let synthesizer_name = wf.synthesizer.as_deref().unwrap_or(judge_name);
            let synthesizer = build_backend(cfg, synthesizer_name, metrics).await?;

            let result = engine::fusion::run(&panel, judge, synthesizer, &args.prompt, 8).await?;

            if json {
                let panel_items: Vec<serde_json::Value> = result
                    .panel_responses
                    .iter()
                    .map(|r| serde_json::json!({"backend": r.backend_name, "content": r.content}))
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({
                        "workflow":       wf.name,
                        "pattern":        "fusion",
                        "panel":          panel_items,
                        "judge_analysis": result.judge_analysis,
                        "synthesis":      result.synthesis.content,
                    })
                );
            } else {
                println!("=== Panel responses ===");
                for r in &result.panel_responses {
                    println!("\n--- {} ---\n{}", r.backend_name, r.content.trim_end());
                }
                println!(
                    "\n=== Judge analysis ===\n{}",
                    result.judge_analysis.trim_end()
                );
                println!(
                    "\n=== Synthesis ===\n{}",
                    result.synthesis.content.trim_end()
                );
                if !result.synthesis.content.ends_with('\n') {
                    println!();
                }
            }
        }

        other => {
            return Err(WeirError::Validation(format!(
                "unknown workflow pattern: {other}"
            )))
        }
    }

    Ok(())
}

// ── exit code mapping ─────────────────────────────────────────────────────────

/// Map a [`WeirError`] to an exit code.
///
/// * `1` — user / config error (bad TOML, missing backend, validation failure)
/// * `2` — system / unexpected error (I/O, HTTP, JSON decode)
fn exit_code_for(e: &WeirError) -> i32 {
    match e {
        WeirError::Config(_)
        | WeirError::BackendNotFound(_)
        | WeirError::WorkflowNotFound(_)
        | WeirError::Validation(_) => 1,
        WeirError::Backend(_)
        | WeirError::CircuitOpen(_)
        | WeirError::RateLimited(_)
        | WeirError::Io(_)
        | WeirError::Json(_) => 2,
    }
}
