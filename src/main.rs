//! `weir` — task runner and GPU slot scheduler for jailed CLI agent workers.
//!
//! Entry point: parses CLI arguments with [`clap`] derive and dispatches to the
//! `cli`, `lease`, `task` and `worker` modules. Exit codes are defined in
//! [`exit::ExitKind`] and documented in `DESIGN.md`.

use std::path::{Path, PathBuf};
use std::process;

use clap::{Args, Parser, Subcommand};

use crate::config::v2::ConfigV2;

use crate::error::WeirError;
use crate::exit::ExitKind;

mod cli;
mod config;
mod error;
mod exit;
mod lease;
mod observability;
mod task;
mod worker;

// ── top-level CLI ─────────────────────────────────────────────────────────────

/// weir — single-binary CLI agent orchestrator.
#[derive(Debug, Parser)]
#[command(
    name = "weir",
    version,
    about = "Task runner and GPU slot scheduler for jailed CLI agent workers",
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
    /// The file must be a `version = 2` config. With `--deep` also probes the
    /// environment: worker wrappers on PATH, a working `bwrap`, every
    /// `wt_roots` entry present in agent-jail's `WT_ROOTS`, and a creatable
    /// `state_dir`.
    Validate(ValidateArgs),

    /// Print a summary of the v2 configuration: workers, slots, ladders, checks
    /// and active quota cooldowns.
    Status,

    /// Print version and build information.
    Version,

    /// Print the JSON Schema (v2) for weir.toml to stdout.
    Schema,

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
    /// Also probe the environment: worker wrappers on PATH,
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
            }
        }

        // ── status ────────────────────────────────────────────────────────────
        Command::Status => match ConfigV2::load(config_path) {
            Ok(cfg) => {
                cli::status::show_status(&cfg, config_path, json);
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

// ── exit code mapping ─────────────────────────────────────────────────────────

/// Map a [`WeirError`] to an exit code.
///
/// * `1` — user / config error (bad TOML, validation failure)
/// * `2` — system / unexpected error (I/O, JSON decode)
fn exit_code_for(e: &WeirError) -> i32 {
    match e {
        WeirError::Config(_) | WeirError::Validation(_) => 1,
        WeirError::Io(_) | WeirError::Json(_) => 2,
    }
}
