//! CLI subcommand: `weir validate` — load `weir.toml` and run all checks.
//!
//! The file must be a `version = 2` config ([`ConfigV2::load`] rejects
//! anything else with a migration hint). `--deep` additionally runs the
//! environment probes (see [`crate::cli::deep`]).

use std::path::Path;

use serde_json::json;

use crate::config::v2::ConfigV2;
use crate::error::WeirError;

/// How `weir validate` finished, mapped to a process exit code by `main`.
pub enum ValidateStatus {
    /// The config is valid (text/JSON output already printed).
    Ok,
    /// The config is broken — exit code 1.
    Invalid,
}

// ── validate ──────────────────────────────────────────────────────────────────

/// Load and fully validate `weir.toml`.
///
/// On success prints `{"status":"ok",...}` (json mode) or a plain success
/// line. On failure the error is printed (stdout in json mode, stderr
/// otherwise) and [`ValidateStatus::Invalid`] is returned.
pub fn validate_config(path: &Path, deep: bool, json: bool) -> ValidateStatus {
    match ConfigV2::load(path) {
        Ok(cfg) => validate_v2(&cfg, path, deep, json),
        Err(e) => {
            print_config_error(path, &e.to_string(), json);
            ValidateStatus::Invalid
        }
    }
}

/// Report success, plus the environment probes when `deep`.
fn validate_v2(cfg: &ConfigV2, path: &Path, deep: bool, json: bool) -> ValidateStatus {
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
