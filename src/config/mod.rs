//! Configuration model and loading.
//!
//! The TOML file (`weir.toml`) is the single source of truth. It is parsed via
//! `serde` into [`Config`]. The CLI mutates it via `toml_edit` (comment-preserving)
//! — see the `editor` module (added in v0.2).

pub mod resolve;
pub mod validate;
// consumed by later phases
#[allow(dead_code)]
pub mod v2;

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::{Result, WeirError};

/// Top-level config. Mirrors `weir.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// `[[backend]]` entries.
    #[serde(default, rename = "backend")]
    pub backends: Vec<BackendConfig>,

    /// `[[workflow]]` entries.
    #[serde(default, rename = "workflow")]
    pub workflows: Vec<WorkflowConfig>,

    /// `[resilience]` block — global retry / circuit-breaker / rate-limit
    /// defaults. Absent block → documented defaults (see [`ResilienceConfig`]).
    #[serde(default)]
    pub resilience: ResilienceConfig,
}

/// A single backend definition. The `type` field selects the [`BackendKind`]
/// variant; its fields are flattened alongside `name` / `timeout_secs`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BackendConfig {
    pub name: String,
    #[serde(flatten)]
    pub kind: BackendKind,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Default model substituted for `{model}` in stdio-cli arg templates when
    /// the caller does not supply `--model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,

    // ── Per-backend resilience overrides (fall back to the global block) ──
    /// Override `[resilience].retry_attempts` for this backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_attempts: Option<u32>,
    /// Override `[resilience].failure_threshold` for this backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_threshold: Option<u32>,
    /// Override `[resilience].recovery_secs` for this backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_secs: Option<u64>,
    /// Override `[resilience].rate_limit_rps` for this backend (0.0 disables it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_rps: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum BackendKind {
    /// A local CLI agent invoked in oneshot mode (e.g. `hermes -z {prompt}`).
    /// This is the only backend type — weir orchestrates CLI agents and is
    /// neither an HTTP client nor an HTTP server.
    StdioCli {
        command: String,
        /// Argument template; the literal token `{prompt}` is replaced at call time.
        #[serde(default)]
        args: Vec<String>,
    },
}

/// Global resilience tuning. Each backend may override a subset of these via
/// the per-backend fields on [`BackendConfig`]; unspecified values fall back
/// here, and an absent `[resilience]` block falls back to these defaults.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResilienceConfig {
    /// Total attempts (first try + retries) for transient failures.
    #[serde(default = "default_retry_attempts")]
    pub retry_attempts: u32,
    /// Base backoff delay (ms) before the first retry.
    #[serde(default = "default_base_delay_ms")]
    pub base_delay_ms: u64,
    /// Upper cap (ms) on the computed backoff delay.
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,
    /// Consecutive failures before the circuit opens.
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
    /// Seconds the circuit stays open before probing recovery.
    #[serde(default = "default_recovery_secs")]
    pub recovery_secs: u64,
    /// Sustained requests/sec per backend (bucket = 2×). `0.0` disables the limiter.
    #[serde(default = "default_rate_limit_rps")]
    pub rate_limit_rps: f64,
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self {
            retry_attempts: default_retry_attempts(),
            base_delay_ms: default_base_delay_ms(),
            max_delay_ms: default_max_delay_ms(),
            failure_threshold: default_failure_threshold(),
            recovery_secs: default_recovery_secs(),
            rate_limit_rps: default_rate_limit_rps(),
        }
    }
}

/// Fully-resolved resilience settings for one backend (global ← per-backend
/// override merge already applied). Returned by [`Config::resilience_for`].
#[derive(Debug, Clone, Copy)]
pub struct ResolvedResilience {
    pub retry_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    pub failure_threshold: u32,
    pub recovery_secs: u64,
    pub rate_limit_rps: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkflowConfig {
    pub name: String,
    /// "fan-out" | "pipeline" | "router" | "eval-loop"
    pub pattern: String,

    // fan-out / router
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backends: Vec<String>,
    /// v1: only "all" is implemented for fan-out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregation: Option<String>,

    // pipeline
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<PipelineStep>,

    // eval-loop
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,

    // fusion
    /// Backend that analyses panel responses (required for fusion pattern).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<String>,
    /// Backend that produces the final synthesis (defaults to judge if absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesizer: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PipelineStep {
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_template: Option<String>,
}

fn default_timeout() -> u64 {
    60
}
fn default_retry_attempts() -> u32 {
    3
}
fn default_base_delay_ms() -> u64 {
    100
}
fn default_max_delay_ms() -> u64 {
    5000
}
fn default_failure_threshold() -> u32 {
    5
}
fn default_recovery_secs() -> u64 {
    30
}
fn default_rate_limit_rps() -> f64 {
    100.0
}

impl Config {
    /// Load and parse the config file (syntactic layer only; see
    /// [`validate`] for semantic + environmental checks).
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| WeirError::Config(format!("cannot read {}: {e}", path.display())))?;
        let cfg: Config = toml::from_str(&text)
            .map_err(|e| WeirError::Config(format!("parse {}: {e}", path.display())))?;
        Ok(cfg)
    }

    /// Resolve effective resilience settings for `backend_name` by merging the
    /// global `[resilience]` block with any per-backend overrides. Unknown
    /// backend names simply yield the global defaults.
    ///
    /// `base_delay_ms` / `max_delay_ms` are global-only (no per-backend override).
    pub fn resilience_for(&self, backend_name: &str) -> ResolvedResilience {
        let g = &self.resilience;
        let bc = self.backends.iter().find(|b| b.name == backend_name);
        ResolvedResilience {
            retry_attempts: bc
                .and_then(|b| b.retry_attempts)
                .unwrap_or(g.retry_attempts),
            base_delay_ms: g.base_delay_ms,
            max_delay_ms: g.max_delay_ms,
            failure_threshold: bc
                .and_then(|b| b.failure_threshold)
                .unwrap_or(g.failure_threshold),
            recovery_secs: bc.and_then(|b| b.recovery_secs).unwrap_or(g.recovery_secs),
            rate_limit_rps: bc
                .and_then(|b| b.rate_limit_rps)
                .unwrap_or(g.rate_limit_rps),
        }
    }
}
