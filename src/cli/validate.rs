//! CLI subcommand: `weir validate` — load `weir.toml` and run all checks.
//!
//! The file may be a legacy (v0.5, no `version` key) config or a
//! `version = 2` config; [`crate::config::v2::load_any`] dispatches on the
//! top-level key. `--deep` runs the environment probes for a v2 config (see
//! [`crate::cli::deep`]).

use std::path::Path;

use serde_json::json;

use crate::config;
use crate::config::v2::{AnyConfig, ConfigV2};
use crate::config::{validate, Config};
use crate::error::{Result, WeirError};

/// How `weir validate` finished, mapped to a process exit code by `main`.
pub enum ValidateStatus {
    /// The config is valid (text/JSON output already printed).
    Ok,
    /// `--deep` was requested for a non-v2 config — a usage error.
    ///
    /// The message is on stderr already; exit code 2.
    Usage,
    /// The config is broken — exit code 1.
    Invalid,
}

// ── validate ──────────────────────────────────────────────────────────────────

/// Load and fully validate `weir.toml` (syntactic → semantic → resilience).
///
/// On success prints `{"status":"ok","path":"…"}` (json mode) or a plain
/// success line. On failure the error is printed (stdout in json mode, stderr
/// otherwise) and [`ValidateStatus::Invalid`] is returned.
pub fn validate_config(path: &Path, deep: bool, json: bool) -> ValidateStatus {
    match config::v2::load_any(path) {
        Ok(AnyConfig::Legacy(cfg)) => {
            if deep {
                let msg = "--deep requires a version = 2 config";
                if json {
                    println!(
                        "{}",
                        json!({"status": "error", "path": path.display().to_string(), "error": msg})
                    );
                } else {
                    eprintln!("error: {msg}");
                }
                return ValidateStatus::Usage;
            }
            match validate_legacy(&cfg, path, json) {
                Ok(()) => ValidateStatus::Ok,
                Err(_) => ValidateStatus::Invalid,
            }
        }
        Ok(AnyConfig::V2(cfg)) => validate_v2(&cfg, path, deep, json),
        Err(e) => {
            print_config_error(path, &e.to_string(), json);
            ValidateStatus::Invalid
        }
    }
}

/// Legacy path: the 3-layer validator, unchanged behaviour and output.
fn validate_legacy(cfg: &Config, path: &Path, json: bool) -> Result<()> {
    let path_str = path.display().to_string();

    match validate::validate(cfg) {
        Ok(()) => {
            let backend_count = cfg.backends.len();
            let workflow_count = cfg.workflows.len();

            if json {
                println!(
                    "{}",
                    json!({
                        "status":          "ok",
                        "path":            path_str,
                        "backend_count":   backend_count,
                        "workflow_count":  workflow_count,
                    })
                );
            } else {
                println!(
                    "Config valid: {path_str}  ({backend_count} backend(s), {workflow_count} workflow(s))"
                );
            }
            Ok(())
        }
        Err(e) => {
            print_config_error(path, &e.to_string(), json);
            Err(e)
        }
    }
}

/// V2 path: structural validation, plus the environment probes when `deep`.
fn validate_v2(cfg: &ConfigV2, path: &Path, deep: bool, json: bool) -> ValidateStatus {
    if let Err(e) = cfg.validate() {
        print_config_error(path, &e.to_string(), json);
        return ValidateStatus::Invalid;
    }

    if !deep {
        if json {
            println!(
                "{}",
                json!({
                    "status": "ok",
                    "version": 2,
                    "path": path.display().to_string(),
                    "workers": cfg.worker.defined_count(),
                    "ladders": cfg.ladder.len(),
                    "checks": cfg.check.len(),
                })
            );
        } else {
            println!(
                "Config valid (v2): {}  ({} worker(s), {} ladder(s), {} check(s))",
                path.display(),
                cfg.worker.defined_count(),
                cfg.ladder.len(),
                cfg.check.len()
            );
        }
        return ValidateStatus::Ok;
    }

    // --deep: the structure is good, now probe the environment.
    let env = crate::cli::deep::DeepEnv::from_env();
    if crate::cli::deep::run(cfg, path, &env, json) {
        ValidateStatus::Ok
    } else {
        ValidateStatus::Invalid
    }
}

/// Print a config/validation error the way `weir validate` always has.
fn print_config_error(path: &Path, msg: &str, json: bool) {
    let path_str = path.display().to_string();
    if json {
        println!(
            "{}",
            json!({"status": "error", "path": path_str, "error": msg})
        );
    } else {
        eprintln!("Config error in {path_str}: {msg}");
    }
}

/// The message for a config path that could not be resolved at all.
pub fn report_unresolved(err: &WeirError, json: bool) {
    let msg = err.to_string();
    if json {
        println!("{}", json!({"status": "error", "error": msg}));
    }
    eprintln!("error: {msg}");
}
