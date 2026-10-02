//! Config schema v2 (`version = 2`).
//!
//! This module defines the v2 TOML schema with strict parsing
//! (`deny_unknown_fields` on every struct), validation, path expansion and
//! small helpers (`config_version`, `parse_duration`). [`ConfigV2::load`]
//! dispatches on the top-level `version` key and rejects pre-v2 files.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::error::{Result, WeirError};

/// The only worker commands weir is allowed to exec (hard-coded; no regex crate).
pub const ALLOWED_WORKER_COMMANDS: [&str; 2] = ["pi-worker", "agy-worker"];

/// Default `paths.wt_roots` when the `[paths]` table is absent.
const DEFAULT_WT_ROOTS: [&str; 2] = ["~/Documents/echomeo-wt", "~/Documents/otter-wt"];
/// Default `paths.scratch` when absent.
const DEFAULT_SCRATCH: &str = "/tmp/pilot";

// ── schema ────────────────────────────────────────────────────────────────────

/// Top-level v2 config.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigV2 {
    /// Must be `2`.
    pub version: i64,
    #[serde(default)]
    pub paths: Paths,
    #[serde(default)]
    pub slots: BTreeMap<String, SlotDef>,
    #[serde(default)]
    pub worker: Workers,
    #[serde(default)]
    pub ladder: BTreeMap<String, Ladder>,
    #[serde(default)]
    pub check: BTreeMap<String, Check>,
}

/// `[paths]` — all optional.
///
/// Each field carries its own `default = "..."` rather than relying on
/// `#[serde(default)]` on the field: serde applies the struct-level `Default`
/// implementation to the *whole* table as soon as any one field is missing, so
/// an explicit `[paths]` block that sets only `scratch` would otherwise lose
/// the `wt_roots` / `state_dir` defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    #[serde(default = "default_wt_roots")]
    pub wt_roots: Vec<String>,
    #[serde(default)]
    pub state_dir: Option<String>,
    #[serde(default = "default_scratch")]
    pub scratch: Option<String>,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            wt_roots: default_wt_roots(),
            state_dir: None,
            scratch: default_scratch(),
        }
    }
}

fn default_wt_roots() -> Vec<String> {
    DEFAULT_WT_ROOTS.iter().map(|s| s.to_string()).collect()
}

fn default_scratch() -> Option<String> {
    Some(DEFAULT_SCRATCH.to_string())
}

/// `[slots.NAME]` — a lease slot with a positive capacity.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotDef {
    pub capacity: u32,
}

/// `[worker.*]` — only `pi` and `agy` are permitted worker names.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Workers {
    #[serde(default)]
    pub pi: Option<PiWorker>,
    #[serde(default)]
    pub agy: Option<AgyWorker>,
}

impl Workers {
    /// Names of the defined workers, in a stable order.
    fn defined(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.pi.is_some() {
            v.push("pi");
        }
        if self.agy.is_some() {
            v.push("agy");
        }
        v
    }

    fn is_defined(&self, name: &str) -> bool {
        match name {
            "pi" => self.pi.is_some(),
            "agy" => self.agy.is_some(),
            _ => false,
        }
    }
}

/// `[worker.pi]`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PiWorker {
    #[serde(default = "default_pi_command")]
    pub command: String,
    #[serde(default = "default_replica_args")]
    pub replica_args: Vec<String>,
    pub timeout: u64,
    #[serde(default)]
    pub slots: BTreeMap<String, String>,
}

/// `[worker.agy]`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgyWorker {
    #[serde(default = "default_agy_command")]
    pub command: String,
    #[serde(default = "default_model_args")]
    pub model_args: Vec<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    pub timeout: u64,
    #[serde(default)]
    pub slots: BTreeMap<String, String>,
    #[serde(default = "default_quota_pattern")]
    pub quota_pattern: String,
    #[serde(default = "default_quota_cooldown")]
    pub quota_cooldown: String,
}

fn default_pi_command() -> String {
    "pi-worker".to_string()
}
fn default_agy_command() -> String {
    "agy-worker".to_string()
}
fn default_replica_args() -> Vec<String> {
    vec!["-b".to_string(), "{replica}".to_string()]
}
fn default_model_args() -> Vec<String> {
    vec!["-m".to_string(), "{model}".to_string()]
}
fn default_quota_pattern() -> String {
    "RESOURCE_EXHAUSTED".to_string()
}
fn default_quota_cooldown() -> String {
    "30m".to_string()
}

/// `[ladder.NAME]` — escalation steps.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ladder {
    pub steps: Vec<String>,
    #[serde(default = "default_fresh_worktree")]
    pub fresh_worktree_per_step: bool,
}

fn default_fresh_worktree() -> bool {
    true
}

/// `[check.NAME]` — a jailed verification command.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    #[serde(default)]
    pub cwd: Option<String>,
    pub cmd: Vec<String>,
    pub timeout: u64,
}

// ── loading / parsing ─────────────────────────────────────────────────────────

impl ConfigV2 {
    /// Parse only (no validation). Errors map to [`WeirError::Config`].
    pub fn parse(text: &str) -> Result<ConfigV2> {
        toml::from_str(text).map_err(|e| WeirError::Config(format!("parse v2 config: {e}")))
    }

    /// Read a file, check its `version` key, parse and validate it.
    ///
    /// A file without `version = 2` (the retired v0.5 schema) is rejected with
    /// a message pointing at the migration example.
    pub fn load(path: &Path) -> Result<ConfigV2> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| WeirError::Config(format!("cannot read {}: {e}", path.display())))?;
        match config_version(&text)? {
            None => {
                return Err(WeirError::Config(format!(
                    "{}: missing 'version = 2'. Only the v2 config schema is supported; \
                     migrate the file (see examples/weir.v2.example.toml)",
                    path.display()
                )))
            }
            Some(2) => {}
            Some(n) => {
                return Err(WeirError::Config(format!(
                    "unsupported config version {n}; expected version = 2 \
                     (see examples/weir.v2.example.toml)"
                )))
            }
        }
        let cfg = Self::parse(&text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Semantic validation. Returns [`WeirError::Validation`] on the first problem.
    pub fn validate(&self) -> Result<()> {
        if self.version != 2 {
            return Err(WeirError::Validation(format!(
                "version must be 2, got {}",
                self.version
            )));
        }

        // paths
        if self.paths.wt_roots.is_empty() {
            return Err(WeirError::Validation(
                "paths.wt_roots must not be empty".to_string(),
            ));
        }
        for (i, root) in self.paths.wt_roots.iter().enumerate() {
            if root.trim().is_empty() {
                return Err(WeirError::Validation(format!(
                    "paths.wt_roots[{i}] must not be empty"
                )));
            }
            if !(root.starts_with('/') || root == "~" || root.starts_with("~/")) {
                return Err(WeirError::Validation(format!(
                    "paths.wt_roots[{i}] must be an absolute path (or start with '~'): {root}"
                )));
            }
        }
        if self
            .paths
            .scratch
            .as_deref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
        {
            return Err(WeirError::Validation(
                "paths.scratch must not be empty".to_string(),
            ));
        }

        // slots
        for (name, slot) in &self.slots {
            if slot.capacity == 0 {
                return Err(WeirError::Validation(format!(
                    "[slots.{name}] capacity must be > 0"
                )));
            }
        }

        // at least one worker must be defined
        if self.worker.pi.is_none() && self.worker.agy.is_none() {
            return Err(WeirError::Validation(
                "at least one worker must be defined under [worker.*]".to_string(),
            ));
        }

        // workers detail
        if let Some(pi) = &self.worker.pi {
            validate_command("pi", &pi.command)?;
            if pi.timeout == 0 {
                return Err(WeirError::Validation(
                    "[worker.pi] timeout must be > 0".to_string(),
                ));
            }
            validate_worker_slots("pi", &pi.slots, &self.slots, &["a", "b"])?;
        }

        if let Some(agy) = &self.worker.agy {
            validate_command("agy", &agy.command)?;
            if agy.timeout == 0 {
                return Err(WeirError::Validation(
                    "[worker.agy] timeout must be > 0".to_string(),
                ));
            }
            validate_worker_slots("agy", &agy.slots, &self.slots, &["any"])?;
            if agy.quota_pattern.trim().is_empty() {
                return Err(WeirError::Validation(
                    "[worker.agy] quota_pattern must not be empty".to_string(),
                ));
            }
            // validates the duration string; error message names the key.
            if let Err(e) = parse_duration(&agy.quota_cooldown) {
                return Err(WeirError::Validation(format!(
                    "[worker.agy] quota_cooldown invalid: {e}"
                )));
            }
        }

        // ladders
        for (name, ladder) in &self.ladder {
            if ladder.steps.is_empty() {
                return Err(WeirError::Validation(format!(
                    "[ladder.{name}] steps must not be empty"
                )));
            }
            for step in &ladder.steps {
                validate_ladder_step(name, step, &self.worker)?;
            }
        }

        // checks
        for (name, check) in &self.check {
            if check.cmd.is_empty() {
                return Err(WeirError::Validation(format!(
                    "[check.{name}] cmd must not be empty"
                )));
            }
            if check.cmd[0].trim().is_empty() {
                return Err(WeirError::Validation(format!(
                    "[check.{name}] cmd[0] must not be empty"
                )));
            }
            if check.timeout == 0 {
                return Err(WeirError::Validation(format!(
                    "[check.{name}] timeout must be > 0"
                )));
            }
            if let Some(cwd) = &check.cwd {
                validate_relative_cwd(name, cwd)?;
            }
        }

        Ok(())
    }

    // ── path accessors (expand at call time) ─────────────────────────────────

    /// `paths.wt_roots` with `~` expanded.
    pub fn wt_roots_expanded(&self) -> Result<Vec<PathBuf>> {
        self.paths
            .wt_roots
            .iter()
            .map(|s| expand_tilde(s))
            .collect()
    }

    /// `paths.state_dir` with `~` expanded. Default:
    /// `$XDG_STATE_HOME/weir` if set and non-empty, else `~/.local/state/weir`.
    pub fn state_dir_expanded(&self) -> Result<PathBuf> {
        match &self.paths.state_dir {
            Some(s) => expand_tilde(s),
            None => default_state_dir(
                &std::env::var("XDG_STATE_HOME").ok(),
                &std::env::var("HOME").ok(),
            ),
        }
    }

    /// `paths.scratch` with `~` expanded. Reserved for the scratch mode; not
    /// consumed by any v1 command yet.
    #[allow(dead_code)]
    pub fn scratch_expanded(&self) -> Result<PathBuf> {
        match &self.paths.scratch {
            Some(s) => expand_tilde(s),
            None => expand_tilde(DEFAULT_SCRATCH),
        }
    }
}

/// Compute the default state dir from explicit env values (testable without
/// mutating process env).
fn default_state_dir(xdg: &Option<String>, home: &Option<String>) -> Result<PathBuf> {
    if let Some(v) = xdg {
        if !v.is_empty() {
            return Ok(PathBuf::from(v).join("weir"));
        }
    }
    match home {
        Some(h) if !h.is_empty() => Ok(PathBuf::from(h).join(".local/state/weir")),
        _ => Err(WeirError::Config(
            "HOME is not set; cannot compute default state_dir".to_string(),
        )),
    }
}

/// Validate a worker `command` value against the hard-coded allow-list.
fn validate_command(worker: &str, command: &str) -> Result<()> {
    if command.contains('/') {
        return Err(WeirError::Validation(format!(
            "[worker.{worker}] command {command:?} must not contain '/' (only bare wrapper names are allowed)"
        )));
    }
    if !ALLOWED_WORKER_COMMANDS.contains(&command) {
        return Err(WeirError::Validation(format!(
            "[worker.{worker}] command {command:?} is not an allowed worker wrapper (allowed: {:?})",
            ALLOWED_WORKER_COMMANDS
        )));
    }
    let expected = format!("{worker}-worker");
    if command != expected {
        return Err(WeirError::Validation(format!(
            "[worker.{worker}] command must be {expected:?}, got {command:?}"
        )));
    }
    Ok(())
}

/// Validate a worker's `slots` map: keys within `allowed_keys`, values naming
/// existing `[slots.*]` entries.
fn validate_worker_slots(
    worker: &str,
    slots: &BTreeMap<String, String>,
    defined: &BTreeMap<String, SlotDef>,
    allowed_keys: &[&str],
) -> Result<()> {
    for (key, slot_name) in slots {
        if !allowed_keys.contains(&key.as_str()) {
            return Err(WeirError::Validation(format!(
                "[worker.{worker}] slots key {key:?} not allowed (allowed: {:?})",
                allowed_keys
            )));
        }
        if !defined.contains_key(slot_name) {
            return Err(WeirError::Validation(format!(
                "[worker.{worker}] slots.{key} references undefined slot {slot_name:?}"
            )));
        }
    }
    Ok(())
}

/// Validate one ladder step `<worker>:<arg>`.
fn validate_ladder_step(name: &str, step: &str, workers: &Workers) -> Result<()> {
    let (worker, arg) = match step.split_once(':') {
        Some((w, a)) => (w, a),
        None => {
            return Err(WeirError::Validation(format!(
                "[ladder.{name}] step {step:?} must be '<worker>:<arg>'"
            )))
        }
    };
    if !matches!(worker, "pi" | "agy") {
        return Err(WeirError::Validation(format!(
            "[ladder.{name}] step {step:?} references unknown worker {worker:?} (must be pi or agy)"
        )));
    }
    if !workers.is_defined(worker) {
        return Err(WeirError::Validation(format!(
            "[ladder.{name}] step {step:?} references worker {worker:?} which is not defined under [worker.*]"
        )));
    }
    if arg.is_empty() {
        return Err(WeirError::Validation(format!(
            "[ladder.{name}] step {step:?} has an empty argument"
        )));
    }
    if worker == "pi" && !matches!(arg, "auto" | "a" | "b") {
        return Err(WeirError::Validation(format!(
            "[ladder.{name}] step {step:?}: pi argument must be one of auto|a|b"
        )));
    }
    Ok(())
}

/// Validate `[check.*] cwd`: must be relative with no `..` component.
fn validate_relative_cwd(name: &str, cwd: &str) -> Result<()> {
    let path = Path::new(cwd);
    if path.is_absolute() {
        return Err(WeirError::Validation(format!(
            "[check.{name}] cwd must be relative, got absolute {cwd:?}"
        )));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(WeirError::Validation(format!(
            "[check.{name}] cwd must not contain '..' (got {cwd:?})"
        )));
    }
    Ok(())
}

// ── version dispatch ──────────────────────────────────────────────────────────

/// Read the top-level integer `version` key. `None` means the key is absent
/// (pre-v2 file). Error if present but not an integer.
pub fn config_version(text: &str) -> Result<Option<i64>> {
    let table: toml::Table =
        toml::from_str(text).map_err(|e| WeirError::Config(format!("parse config: {e}")))?;
    match table.get("version") {
        None => Ok(None),
        Some(toml::Value::Integer(n)) => Ok(Some(*n)),
        Some(other) => Err(WeirError::Config(format!(
            "config 'version' key must be an integer, got {other}"
        ))),
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Expand a leading `~` / `~/` against `$HOME`. Error if HOME is unset/empty.
pub fn expand_tilde(s: &str) -> Result<PathBuf> {
    expand_tilde_with(s, &std::env::var("HOME").ok())
}

/// [`expand_tilde`] with an explicit HOME value (testable without env mutation).
pub fn expand_tilde_with(s: &str, home: &Option<String>) -> Result<PathBuf> {
    if s == "~" {
        return match home {
            Some(h) if !h.is_empty() => Ok(PathBuf::from(h)),
            _ => Err(WeirError::Config(
                "HOME is not set; cannot expand '~'".to_string(),
            )),
        };
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return match home {
            Some(h) if !h.is_empty() => Ok(PathBuf::from(h).join(rest)),
            _ => Err(WeirError::Config(
                "HOME is not set; cannot expand '~'".to_string(),
            )),
        };
    }
    Ok(PathBuf::from(s))
}

/// Parse a duration like `90s`, `30m`, `2h`, `1d`. Must be > 0.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, unit_mul) = if let Some(n) = s.strip_suffix('s') {
        (n, 1u64)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 86_400)
    } else {
        return Err(WeirError::Config(format!(
            "invalid duration {s:?}: expected integer + unit s|m|h|d"
        )));
    };
    let Ok(n) = num.trim().parse::<u64>() else {
        return Err(WeirError::Config(format!(
            "invalid duration {s:?}: expected integer + unit s|m|h|d"
        )));
    };
    if n == 0 {
        return Err(WeirError::Config(format!(
            "invalid duration {s:?}: must be > 0"
        )));
    }
    n.checked_mul(unit_mul)
        .map(Duration::from_secs)
        .ok_or_else(|| WeirError::Config(format!("invalid duration {s:?}: overflow")))
}

// Track which worker names appear — used by tests, `validate` output and
// future `validate --deep` checks.
impl Workers {
    /// Number of defined workers (0–2).
    pub fn defined_count(&self) -> usize {
        self.defined().len()
    }

    /// Set of defined worker names.
    pub fn defined_set(&self) -> HashSet<&'static str> {
        self.defined().into_iter().collect()
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const EXAMPLE: &str = include_str!("../../examples/weir.v2.example.toml");

    const MINIMAL: &str = r#"
version = 2
[worker.pi]
command = "pi-worker"
timeout = 900
"#;

    fn parse_ok(text: &str) -> ConfigV2 {
        let cfg = ConfigV2::parse(text).expect("should parse");
        cfg.validate().expect("should validate");
        cfg
    }

    fn parse_err(text: &str) -> String {
        match ConfigV2::parse(text).and_then(|c| c.validate()) {
            Err(WeirError::Validation(m)) => m,
            Err(other) => panic!("expected Validation error, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    fn write_tmp(text: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(text.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn example_file_parses_and_validates() {
        let cfg: ConfigV2 = toml::from_str(EXAMPLE).expect("example must parse");
        cfg.validate().expect("example must validate");
        assert_eq!(cfg.version, 2);
        assert!(cfg.worker.pi.is_some());
        assert!(cfg.worker.agy.is_some());
        assert_eq!(cfg.slots.len(), 3);
        assert_eq!(cfg.check.len(), 3);
        assert!(cfg.ladder.contains_key("default"));
    }

    #[test]
    fn minimal_valid_config() {
        let cfg = parse_ok(MINIMAL);
        assert!(cfg.worker.agy.is_none());
        assert_eq!(cfg.paths.wt_roots.len(), 2); // defaults applied
        assert_eq!(cfg.paths.scratch.as_deref(), Some("/tmp/pilot"));
    }

    #[test]
    fn defaults_applied_to_worker_fields() {
        let cfg = parse_ok(MINIMAL);
        let pi = cfg.worker.pi.unwrap();
        assert_eq!(pi.command, "pi-worker");
        assert_eq!(pi.replica_args, vec!["-b", "{replica}"]);
        let agy_cfg = parse_ok(
            r#"
version = 2
[worker.agy]
command = "agy-worker"
timeout = 3600
"#,
        );
        let agy = agy_cfg.worker.agy.unwrap();
        assert_eq!(agy.model_args, vec!["-m", "{model}"]);
        assert_eq!(agy.quota_pattern, "RESOURCE_EXHAUSTED");
        assert_eq!(agy.quota_cooldown, "30m");
        assert!(agy.default_model.is_none());
    }

    #[test]
    fn legacy_file_without_version_is_rejected() {
        let legacy = r#"
[[backend]]
name = "echo"
type = "stdio-cli"
command = "cat"
"#;
        assert_eq!(config_version(legacy).unwrap(), None);
        let f = write_tmp(legacy);
        match ConfigV2::load(f.path()) {
            Err(WeirError::Config(m)) => {
                assert!(m.contains("version = 2"), "{m}");
                assert!(m.contains("examples/weir.v2.example.toml"), "{m}");
            }
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn v2_file_loads() {
        let f = write_tmp(MINIMAL);
        assert!(ConfigV2::load(f.path()).is_ok());
    }

    #[test]
    fn version_3_rejected_by_load() {
        let f = write_tmp("version = 3\n");
        match ConfigV2::load(f.path()) {
            Err(WeirError::Config(m)) => assert!(m.contains("unsupported config version 3"), "{m}"),
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn config_version_non_integer() {
        assert!(matches!(
            config_version("version = \"two\""),
            Err(WeirError::Config(_))
        ));
    }

    #[test]
    fn version_must_be_two() {
        assert!(
            parse_err("version = 1\n[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1")
                .contains("version must be 2")
        );
    }

    #[test]
    fn rejects_bad_worker_commands() {
        // command = "agy" (bare name, not a wrapper)
        assert!(
            parse_err("version = 2\n[worker.pi]\ncommand = \"agy\"\ntimeout = 1\n")
                .contains("command")
        );
        // relative path
        assert!(
            parse_err("version = 2\n[worker.pi]\ncommand = \"./pi-worker\"\ntimeout = 1\n")
                .contains("must not contain '/'")
        );
        // absolute path
        assert!(parse_err(
            "version = 2\n[worker.pi]\ncommand = \"/usr/bin/pi-worker\"\ntimeout = 1\n"
        )
        .contains("must not contain '/'"));
        // unknown wrapper name
        assert!(parse_err(
            "version = 2\n[worker.pi]\ncommand = \"opencode-worker\"\ntimeout = 1\n"
        )
        .contains("not an allowed worker wrapper"));
        // right-shaped but wrong worker table: pi-worker under [worker.agy]
        assert!(
            parse_err("version = 2\n[worker.agy]\ncommand = \"pi-worker\"\ntimeout = 1\n")
                .contains("must be \"agy-worker\"")
        );
    }

    #[test]
    fn rejects_unknown_worker_table() {
        let err = ConfigV2::parse(
            "version = 2\n[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1\n[worker.opencode]\ncommand=\"opencode-worker\"\ntimeout=1\n",
        );
        match err {
            Err(WeirError::Config(m)) => assert!(m.contains("opencode"), "{m}"),
            other => panic!("expected Config parse error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_no_workers() {
        assert!(parse_err("version = 2\n").contains("at least one worker"));
    }

    #[test]
    fn rejects_zero_timeouts() {
        assert!(
            parse_err("version = 2\n[worker.pi]\ncommand=\"pi-worker\"\ntimeout = 0\n")
                .contains("[worker.pi] timeout")
        );
        assert!(parse_err(
            "version = 2\n[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1\n[check.lint]\ncmd=[\"tsc\"]\ntimeout = 0\n"
        )
        .contains("[check.lint] timeout"));
    }

    #[test]
    fn rejects_missing_worker_timeout() {
        // timeout is REQUIRED: serde must fail.
        assert!(ConfigV2::parse("version = 2\n[worker.pi]\ncommand=\"pi-worker\"\n").is_err());
        assert!(ConfigV2::parse(
            "version = 2\n[worker.agy]\ncommand=\"agy-worker\"\n[check.x]\ncmd=[\"c\"]\ntimeout=1\n"
        )
        .is_err());
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(ConfigV2::parse("version = 2\nbogus = 1\n").is_err());
        assert!(ConfigV2::parse(
            "version = 2\n[paths]\nbogus = 1\n[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1\n"
        )
        .is_err());
        assert!(ConfigV2::parse(
            "version = 2\n[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1\nbogus = true\n"
        )
        .is_err());
    }

    #[test]
    fn rejects_bad_slot_references() {
        assert!(parse_err(
            "version = 2\n[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1\nslots={ a = \"gpu-z\" }\n"
        )
        .contains("undefined slot"));
        assert!(parse_err(
            "version = 2\n[slots.gpu-a]\ncapacity = 0\n[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1\nslots={ a = \"gpu-a\" }\n"
        )
        .contains("capacity must be > 0"));
        // pi slot keys outside {a,b}; agy slot keys outside {any}
        assert!(parse_err(
            "version = 2\n[slots.gpu-a]\ncapacity=1\n[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1\nslots={ c = \"gpu-a\" }\n"
        )
        .contains("slots key \"c\""));
        assert!(parse_err(
            "version = 2\n[slots.agy]\ncapacity=2\n[worker.agy]\ncommand=\"agy-worker\"\ntimeout=1\nslots={ a = \"agy\" }\n"
        )
        .contains("slots key \"a\""));
    }

    #[test]
    fn rejects_bad_ladder_steps() {
        let pi_ok = "[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1\n";
        assert!(
            parse_err(&format!("version = 2\n{pi_ok}[ladder.default]\nsteps=[]\n"))
                .contains("steps must not be empty")
        );
        // No worker at all is rejected before the ladder is examined.
        assert!(
            parse_err("version = 2\n[ladder.default]\nsteps=[\"pi:auto\"]\n")
                .contains("at least one worker")
        );
        // Worker defined but not `agy`: the ladder step names an undefined worker.
        assert!(
            parse_err(&format!(
                "version = 2\n{pi_ok}[ladder.default]\nsteps=[\"agy:m1\"]\n"
            ))
            .contains("not defined under [worker.*]"),
            "ladder referencing undefined worker must fail"
        );
        assert!(parse_err(&format!(
            "version = 2\n{pi_ok}[ladder.default]\nsteps=[\"pi\"]\n"
        ))
        .contains("must be '<worker>:<arg>'"));
        assert!(parse_err(&format!(
            "version = 2\n{pi_ok}[ladder.default]\nsteps=[\"pi:zzz\"]\n"
        ))
        .contains("auto|a|b"));
        assert!(parse_err(&format!(
            "version = 2\n{pi_ok}[ladder.default]\nsteps=[\"pi:\"]\n"
        ))
        .contains("empty argument"));
        assert!(parse_err(&format!(
            "version = 2\n{pi_ok}[ladder.default]\nsteps=[\"zed:x\"]\n"
        ))
        .contains("unknown worker"),);
    }

    #[test]
    fn ladder_accepts_agy_any_nonempty_model() {
        parse_ok(
            "version = 2\n[worker.agy]\ncommand=\"agy-worker\"\ntimeout=1\n[ladder.default]\nsteps=[\"agy:gemini-3.8-flash-high\",\"agy:claude-opus-4-6-thinking\"]\n",
        );
    }

    #[test]
    fn rejects_bad_checks() {
        let pi_ok = "[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1\n";
        assert!(parse_err(&format!(
            "version = 2\n{pi_ok}[check.x]\ncmd=[]\ntimeout=1\n"
        ))
        .contains("cmd must not be empty"));
        assert!(parse_err(&format!(
            "version = 2\n{pi_ok}[check.x]\ncmd=[\"\"]\ntimeout=1\n"
        ))
        .contains("cmd[0] must not be empty"));
        assert!(parse_err(&format!(
            "version = 2\n{pi_ok}[check.x]\ncmd=[\"tsc\"]\ntimeout=1\ncwd=\"../x\"\n"
        ))
        .contains("must not contain '..'"));
        assert!(parse_err(&format!(
            "version = 2\n{pi_ok}[check.x]\ncmd=[\"tsc\"]\ntimeout=1\ncwd=\"/abs\"\n"
        ))
        .contains("relative"));
    }

    #[test]
    fn check_cwd_accepts_plain_relative() {
        parse_ok("version = 2\n[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1\n[check.x]\ncmd=[\"tsc\"]\ntimeout=1\ncwd=\"frontend\"\n");
    }

    #[test]
    fn rejects_bad_agy_quota_fields() {
        let head = "version = 2\n[worker.agy]\ncommand=\"agy-worker\"\ntimeout=1\n";
        assert!(parse_err(&format!("{head}quota_cooldown=\"0m\"\n")).contains("quota_cooldown"));
        assert!(parse_err(&format!("{head}quota_cooldown=\"abc\"\n")).contains("quota_cooldown"));
        assert!(parse_err(&format!("{head}quota_pattern=\"\"\n")).contains("quota_pattern"));
    }

    #[test]
    fn rejects_bad_paths() {
        assert!(parse_err(
            "version = 2\n[paths]\nwt_roots=[]\n[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1\n"
        )
        .contains("wt_roots must not be empty"));
        assert!(
            parse_err("version = 2\n[paths]\nwt_roots=[\"\"]\n[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1\n")
                .contains("wt_roots[0]")
        );
        assert!(parse_err(
            "version = 2\n[paths]\nscratch=\"\"\n[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1\n"
        )
        .contains("scratch"));
        // An omitted scratch falls back to the documented default, which validates.
        parse_ok(
            "version = 2\n[paths]\nwt_roots=[\"/wt\"]\n[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1\n",
        );
    }

    #[test]
    fn parse_duration_cases() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86_400));
        assert!(matches!(parse_duration("0m"), Err(WeirError::Config(_))));
        assert!(matches!(parse_duration("abc"), Err(WeirError::Config(_))));
        assert!(matches!(parse_duration("30"), Err(WeirError::Config(_))));
        assert!(matches!(parse_duration("1w"), Err(WeirError::Config(_))));
        assert!(matches!(parse_duration(""), Err(WeirError::Config(_))));
        assert!(matches!(parse_duration("-5s"), Err(WeirError::Config(_))));
        assert!(matches!(parse_duration("1.5s"), Err(WeirError::Config(_))));
    }

    #[test]
    fn expand_tilde_with_cases() {
        let home = Some("/home/u".to_string());
        assert_eq!(
            expand_tilde_with("~", &home).unwrap(),
            PathBuf::from("/home/u")
        );
        assert_eq!(
            expand_tilde_with("~/x", &home).unwrap(),
            PathBuf::from("/home/u/x")
        );
        assert_eq!(
            expand_tilde_with("/tmp/pilot", &home).unwrap(),
            PathBuf::from("/tmp/pilot")
        );
        assert_eq!(
            expand_tilde_with("rel", &home).unwrap(),
            PathBuf::from("rel")
        );
        // ~foo is NOT expanded (only "~" and "~/")
        assert_eq!(
            expand_tilde_with("~other/x", &home).unwrap(),
            PathBuf::from("~other/x")
        );
        // HOME missing or empty => error only when '~' is used
        assert!(expand_tilde_with("~", &None).is_err());
        assert!(expand_tilde_with("~/x", &None).is_err());
        assert!(expand_tilde_with("~", &Some("".into())).is_err());
        assert_eq!(expand_tilde_with("/x", &None).unwrap(), PathBuf::from("/x"));
    }

    #[test]
    fn state_dir_default_logic() {
        let home = Some("/home/u".to_string());
        // XDG wins when set and non-empty.
        assert_eq!(
            default_state_dir(&Some("/xdg".into()), &home).unwrap(),
            PathBuf::from("/xdg/weir")
        );
        // Empty XDG falls back to HOME.
        assert_eq!(
            default_state_dir(&Some("".into()), &home).unwrap(),
            PathBuf::from("/home/u/.local/state/weir")
        );
        assert_eq!(
            default_state_dir(&None, &home).unwrap(),
            PathBuf::from("/home/u/.local/state/weir")
        );
        // Neither set => error.
        assert!(default_state_dir(&None, &None).is_err());
        assert!(default_state_dir(&Some("".into()), &Some("".into())).is_err());
    }

    #[test]
    fn path_accessors_expand_configured_values() {
        let cfg = parse_ok(
            "version = 2\n[paths]\nwt_roots=[\"/wt/a\"]\nstate_dir=\"/state\"\nscratch=\"/tmp/p\"\n[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1\n",
        );
        assert_eq!(
            cfg.wt_roots_expanded().unwrap(),
            vec![PathBuf::from("/wt/a")]
        );
        assert_eq!(cfg.state_dir_expanded().unwrap(), PathBuf::from("/state"));
        assert_eq!(cfg.scratch_expanded().unwrap(), PathBuf::from("/tmp/p"));
    }
}
