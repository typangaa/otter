//! CLI subcommands: `weir status`, `weir version`, `weir schema`.

use std::path::Path;

use serde_json::json;

use crate::config::v2::ConfigV2;

// ── status ────────────────────────────────────────────────────────────────────

/// Print a summary of the v2 config: workers, slots, ladders, checks and any
/// active agy quota cooldowns recorded under `paths.state_dir`.
///
/// For live lease occupancy use `weir lease status`.
pub fn show_status(cfg: &ConfigV2, path: &Path, json: bool) {
    let state_dir = cfg.state_dir_expanded().ok();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let cooldowns: Vec<(String, i64)> = state_dir
        .as_deref()
        .map(|d| crate::task::cooldown::read_store(d).agy)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, until)| *until > now)
        .collect();

    let workers: Vec<&str> = {
        let mut v: Vec<&str> = cfg.worker.defined_set().into_iter().collect();
        v.sort_unstable();
        v
    };

    if json {
        let slots: serde_json::Map<String, serde_json::Value> = cfg
            .slots
            .iter()
            .map(|(k, v)| (k.clone(), json!(v.capacity)))
            .collect();
        let ladders: serde_json::Map<String, serde_json::Value> = cfg
            .ladder
            .iter()
            .map(|(k, v)| (k.clone(), json!(v.steps)))
            .collect();
        let cds: serde_json::Map<String, serde_json::Value> = cooldowns
            .iter()
            .map(|(m, until)| (m.clone(), json!(until)))
            .collect();
        println!(
            "{}",
            json!({
                "status": "ok",
                "version": 2,
                "path": path.display().to_string(),
                "workers": workers,
                "slots": slots,
                "ladders": ladders,
                "checks": cfg.check.keys().collect::<Vec<_>>(),
                "state_dir": state_dir.as_ref().map(|d| d.display().to_string()),
                "cooldowns": { "agy": cds },
            })
        );
        return;
    }

    println!("Config:    {} (version 2)", path.display());
    println!("Workers:   {} configured", workers.len());
    for w in &workers {
        println!("  - {w}");
    }
    println!("Slots:     {} configured", cfg.slots.len());
    for (name, slot) in &cfg.slots {
        println!("  - {:<12} capacity {}", name, slot.capacity);
    }
    println!("Ladders:   {} configured", cfg.ladder.len());
    for (name, ladder) in &cfg.ladder {
        println!("  - {}: {}", name, ladder.steps.join(" -> "));
    }
    println!("Checks:    {} configured", cfg.check.len());
    for name in cfg.check.keys() {
        println!("  - {name}");
    }
    if let Some(d) = &state_dir {
        println!("State dir: {}", d.display());
    }
    for (model, until) in &cooldowns {
        let mins = (until - now + 59) / 60;
        println!("agy cooldown {mins}m (until unix {until}) for model {model}");
    }
}

// ── version ───────────────────────────────────────────────────────────────────

/// Print crate version metadata.
pub fn show_version(json: bool) {
    let name = env!("CARGO_PKG_NAME");
    let version = env!("CARGO_PKG_VERSION");
    let description = env!("CARGO_PKG_DESCRIPTION");
    let repo = env!("CARGO_PKG_REPOSITORY");

    if json {
        println!(
            "{}",
            json!({
                "name":        name,
                "version":     version,
                "description": description,
                "repository":  repo,
            })
        );
    } else {
        println!("{name} {version}");
        println!("{description}");
        println!("Repository: {repo}");
    }
}

// ── schema ────────────────────────────────────────────────────────────────────

/// Print the JSON Schema for `weir.toml` (the `version = 2` schema).
///
/// The schema is inlined as a [`serde_json::json!`] literal so the binary
/// carries no extra dependencies.
pub fn show_schema(json_flag: bool) {
    let schema = build_schema();

    if json_flag {
        println!("{}", schema);
    } else {
        // Pretty-print even in text mode — schema is inherently structured.
        println!(
            "{}",
            serde_json::to_string_pretty(&schema).expect("schema serialization is infallible")
        );
    }
}

fn build_schema() -> serde_json::Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id":     "https://github.com/typangaa/otter/weir.toml.schema.json",
        "title":   "WeirConfigV2",
        "description": "weir.toml (version = 2): task runner and GPU slot scheduler for jailed workers.",
        "type": "object",
        "required": ["version"],
        "additionalProperties": false,
        "properties": {
            "version": {
                "const": 2,
                "description": "Config schema version; must be 2."
            },
            "paths": { "$ref": "#/$defs/Paths" },
            "slots": {
                "type": "object",
                "description": "[slots.NAME] lease slots.",
                "additionalProperties": { "$ref": "#/$defs/Slot" }
            },
            "worker": {
                "type": "object",
                "description": "Worker definitions; only 'pi' and 'agy' exist. At least one is required.",
                "additionalProperties": false,
                "properties": {
                    "pi":  { "$ref": "#/$defs/PiWorker" },
                    "agy": { "$ref": "#/$defs/AgyWorker" }
                }
            },
            "ladder": {
                "type": "object",
                "description": "[ladder.NAME] escalation ladders.",
                "additionalProperties": { "$ref": "#/$defs/Ladder" }
            },
            "check": {
                "type": "object",
                "description": "[check.NAME] jailed verification commands.",
                "additionalProperties": { "$ref": "#/$defs/Check" }
            }
        },

        "$defs": {
            "Paths": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "wt_roots": {
                        "type": "array",
                        "minItems": 1,
                        "items": { "type": "string" },
                        "default": ["~/Documents/echomeo-wt", "~/Documents/otter-wt"],
                        "description": "Worktree roots; absolute or starting with '~'."
                    },
                    "state_dir": {
                        "type": "string",
                        "description": "Leases, cooldown.json, ledger.jsonl, tasks/<id>/. Default: ${XDG_STATE_HOME:-~/.local/state}/weir."
                    },
                    "scratch": {
                        "type": "string",
                        "default": "/tmp/pilot",
                        "description": "Scratch directory; must not be empty."
                    }
                }
            },
            "Slot": {
                "type": "object",
                "required": ["capacity"],
                "additionalProperties": false,
                "properties": {
                    "capacity": { "type": "integer", "minimum": 1 }
                }
            },
            "PiWorker": {
                "type": "object",
                "required": ["timeout"],
                "additionalProperties": false,
                "properties": {
                    "command": { "type": "string", "default": "pi-worker", "description": "Bare wrapper name on PATH; only 'pi-worker' is allowed." },
                    "replica_args": { "type": "array", "items": { "type": "string" }, "default": ["-b", "{replica}"] },
                    "timeout": { "type": "integer", "minimum": 1, "description": "Wall-clock seconds." },
                    "slots": {
                        "type": "object",
                        "description": "Replica key (a|b) to slot name.",
                        "additionalProperties": false,
                        "properties": {
                            "a": { "type": "string" },
                            "b": { "type": "string" }
                        }
                    }
                }
            },
            "AgyWorker": {
                "type": "object",
                "required": ["timeout"],
                "additionalProperties": false,
                "properties": {
                    "command": { "type": "string", "default": "agy-worker", "description": "Bare wrapper name on PATH; only 'agy-worker' is allowed." },
                    "model_args": { "type": "array", "items": { "type": "string" }, "default": ["-m", "{model}"] },
                    "default_model": { "type": "string" },
                    "timeout": { "type": "integer", "minimum": 1, "description": "Wall-clock seconds." },
                    "slots": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": { "any": { "type": "string" } }
                    },
                    "quota_pattern": { "type": "string", "default": "RESOURCE_EXHAUSTED", "description": "Non-empty stderr marker for a quota failure." },
                    "quota_cooldown": { "type": "string", "default": "30m", "pattern": "^[0-9]+[smhd]$", "description": "Integer plus unit s|m|h|d, greater than zero." }
                }
            },
            "Ladder": {
                "type": "object",
                "required": ["steps"],
                "additionalProperties": false,
                "properties": {
                    "steps": {
                        "type": "array",
                        "minItems": 1,
                        "items": { "type": "string", "pattern": "^(pi|agy):.+$" },
                        "description": "Each step is '<worker>:<arg>'; pi args: auto|a|b; agy args: a model id."
                    },
                    "fresh_worktree_per_step": { "type": "boolean", "default": true }
                }
            },
            "Check": {
                "type": "object",
                "required": ["cmd", "timeout"],
                "additionalProperties": false,
                "properties": {
                    "cwd": { "type": "string", "description": "Relative path inside the worktree; no '..'." },
                    "cmd": { "type": "array", "minItems": 1, "items": { "type": "string" } },
                    "timeout": { "type": "integer", "minimum": 1, "description": "Seconds." }
                }
            }
        }
    })
}
