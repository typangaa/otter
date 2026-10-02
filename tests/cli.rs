//! End-to-end CLI integration tests.
//!
//! These drive the actual built `weir` binary (via `assert_cmd`) against a
//! throwaway `version = 2` `weir.toml`. POSIX shell is assumed, hence the
//! `unix` gate.
#![cfg(unix)]

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

/// A minimal valid v2 config.
const TEST_CONFIG: &str = r#"version = 2

[paths]
wt_roots = ["/tmp/weir-test-wt"]
state_dir = "STATE_DIR"

[slots]
gpu-a = { capacity = 1 }
gpu-b = { capacity = 1 }
agy   = { capacity = 2 }

[worker.pi]
timeout = 900
slots   = { a = "gpu-a", b = "gpu-b" }

[worker.agy]
timeout = 3600
slots   = { any = "agy" }

[ladder.default]
steps = ["pi:auto", "agy:some-model"]

[check.lint]
cmd     = ["true"]
timeout = 60
"#;

/// Write `TEST_CONFIG` into `dir` and return the path. The `TempDir` must be
/// kept alive by the caller for the file to survive.
fn write_config(dir: &Path) -> PathBuf {
    let path = dir.join("weir.toml");
    let state = dir.join("state");
    let text = TEST_CONFIG.replace("STATE_DIR", &state.display().to_string());
    std::fs::write(&path, text).expect("write test config");
    path
}

/// `weir --config <cfg> <args...>` ready to `.assert()`.
fn weir(cfg: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::cargo_bin("weir").expect("binary built");
    cmd.arg("--config").arg(cfg);
    cmd.args(args);
    cmd
}

/// Parse stdout of a successful run as JSON.
fn stdout_json(cfg: &Path, args: &[&str]) -> Value {
    let out = weir(cfg, args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).expect("stdout is valid JSON")
}

// ── validate / schema / version / status / config ─────────────────────────────

#[test]
fn validate_good_config_is_ok() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path());
    let v = stdout_json(&cfg, &["--json", "validate"]);
    assert_eq!(v["status"], "ok");
    assert_eq!(v["version"], 2);
    assert_eq!(v["workers"], 2);
    assert_eq!(v["ladders"], 1);
    assert_eq!(v["checks"], 1);
}

#[test]
fn validate_malformed_config_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("weir.toml");
    std::fs::write(&path, "this is = not valid = toml [[[").unwrap();
    weir(&path, &["validate"]).assert().failure();
}

#[test]
fn validate_rejects_pre_v2_config_with_migration_hint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("weir.toml");
    std::fs::write(
        &path,
        "[[backend]]\nname = \"echo\"\ntype = \"stdio-cli\"\ncommand = \"cat\"\n",
    )
    .unwrap();
    weir(&path, &["validate"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("version = 2"))
        .stderr(predicate::str::contains("examples/weir.v2.example.toml"));
}

#[test]
fn removed_v0_commands_are_gone() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path());
    for cmd in ["chat", "backend", "workflow"] {
        weir(&cfg, &[cmd]).assert().failure().code(2);
    }
}

#[test]
fn schema_describes_v2_only() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path());
    let v = stdout_json(&cfg, &["--json", "schema"]);
    assert_eq!(v["title"], "WeirConfigV2");
    assert_eq!(v["properties"]["version"]["const"], 2);
    for key in ["paths", "slots", "worker", "ladder", "check"] {
        assert!(v["properties"].get(key).is_some(), "missing {key}");
    }
    assert!(v["properties"].get("backend").is_none());
    assert!(v["properties"].get("workflow").is_none());
}

#[test]
fn version_flag_prints_crate_version() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path());
    weir(&cfg, &["--version"])
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn version_subcommand_json() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path());
    let v = stdout_json(&cfg, &["--json", "version"]);
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn status_summarises_v2_config() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path());
    let v = stdout_json(&cfg, &["--json", "status"]);
    assert_eq!(v["status"], "ok");
    assert_eq!(v["workers"], serde_json::json!(["agy", "pi"]));
    assert_eq!(v["slots"]["agy"], 2);
    assert_eq!(v["ladders"]["default"][0], "pi:auto");
    assert_eq!(v["checks"][0], "lint");
    assert_eq!(v["cooldowns"]["agy"], serde_json::json!({}));
}

#[test]
fn status_text_lists_sections() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path());
    weir(&cfg, &["status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Workers:"))
        .stdout(predicate::str::contains("Slots:"))
        .stdout(predicate::str::contains(
            "default: pi:auto -> agy:some-model",
        ));
}

#[test]
fn status_rejects_pre_v2_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("weir.toml");
    std::fs::write(&path, "[[backend]]\nname = \"x\"\n").unwrap();
    weir(&path, &["status"]).assert().code(1);
}

#[test]
fn config_path_prints_explicit_path() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path());
    weir(&cfg, &["config", "path"])
        .assert()
        .success()
        .stdout(predicate::str::contains(cfg.display().to_string()));
}

#[test]
fn missing_explicit_config_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.toml");
    weir(&missing, &["status"]).assert().code(2);
}

// ── lease run / lease status ───────────────────────────────────────────────────
//
// `weir lease` never reads weir.toml, so these tests only need a throwaway
// `WEIR_STATE_DIR` plus a fake `pi-worker` on PATH. The fake logs
// `start <replica> <pid> <unix_ms>` / `end <replica> <pid> <unix_ms>` so
// overlap can be checked from the log itself, and takes its behaviour from env
// vars (`FAKE_SLEEP`, `FAKE_EXIT`) instead of argv — argv is what the
// `{replica}` substitution tests assert on.

use std::thread;
use std::time::{Duration, Instant};

/// Shell script of the fake worker: logs start/end, sleeps, exits with
/// `FAKE_EXIT`, and echoes its argv to `FAKE_ARGS` when given.
///
/// Log line: `<event> <replica> <pid> <unix_ms>`. The worker is invoked as
/// `pi-worker -b <replica> …`, so `$2` is the value `{replica}` was replaced
/// with (`$1` is `-b`; `printf "$@"` never includes the program name).
const FAKE_PI_WORKER: &str = r#"#!/bin/sh
if [ -n "${FAKE_ARGS:-}" ]; then printf '%s\n' "$@" >> "$FAKE_ARGS"; fi
echo "start $2 $$ $(date +%s%3N)" >> "$FAKE_LOG"
sleep "${FAKE_SLEEP:-0}"
echo "end $2 $$ $(date +%s%3N)" >> "$FAKE_LOG"
exit "${FAKE_EXIT:-0}"
"#;

/// Create `<dir>/bin/pi-worker` (the only basename `weir lease` accepts).
fn fake_worker(dir: &Path) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("create fake bin dir");
    let script = bin.join("pi-worker");
    std::fs::write(&script, FAKE_PI_WORKER).expect("write fake pi-worker");
    std::process::Command::new("chmod")
        .arg("+x")
        .arg(&script)
        .status()
        .expect("chmod fake pi-worker");
    bin
}

/// A `weir lease` invocation: no config file, state root and PATH pointed at
/// the caller's tempdir.
fn lease_cmd(state_dir: &Path, bin: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::cargo_bin("weir").expect("binary built");
    // `--config` defaults to a path that does not exist: `lease` must succeed
    // regardless, since it never loads weir.toml.
    cmd.env("WEIR_STATE_DIR", state_dir);
    cmd.env("PATH", path_with(bin));
    cmd.env("FAKE_LOG", state_dir.join("holders.log"));
    cmd.args(args);
    cmd
}

/// `lease status --json` parsed, or `None` if the command failed / was not JSON.
fn status_json(state_dir: &Path, bin: &Path) -> Option<Value> {
    let out = lease_cmd(state_dir, bin, &["lease", "status", "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Look up one slot in a `lease status --json` array.
fn slot_row<'a>(rows: &'a Value, slot: &str) -> Option<&'a Value> {
    rows["slots"]
        .as_array()?
        .iter()
        .find(|r| r["slot"].as_str() == Some(slot))
}

/// Wait (with a deadline, no fixed sleeps) until `pred` holds; poll every 50 ms.
fn wait_until(what: &str, pred: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if pred() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for: {what}");
}

/// A background `lease run` that keeps a slot busy.
///
/// On drop the guard sends weir SIGTERM (so weir can forward it to the worker's
/// process group), waits up to 5 s for it to exit, and only then falls back to
/// SIGKILL — no orphan `sleep` process outlives a test.
struct Holder {
    slot: String,
    child: std::process::Child,
}

impl Holder {
    /// Spawn `<slot>=<replica>` holding for `sleep_secs`, then wait until
    /// `lease status --json` reports the slot held.
    fn new(state_dir: &Path, bin: &Path, slot_spec: &str) -> Self {
        let (slot, _) = slot_spec.split_once('=').unwrap_or((slot_spec, ""));
        let child = std::process::Command::new(assert_cmd::cargo::cargo_bin("weir"))
            .env("WEIR_STATE_DIR", state_dir)
            .env("PATH", path_with(bin))
            .env("FAKE_LOG", state_dir.join("holders.log"))
            .env("FAKE_SLEEP", "120")
            .args([
                "lease",
                "run",
                "--slot",
                slot_spec,
                "--",
                "pi-worker",
                "hold",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn holder");
        let holder = Self {
            slot: slot.to_owned(),
            child,
        };
        holder.wait_held(state_dir, bin);
        holder
    }

    /// Poll `lease status --json` (with a deadline) until this slot is held.
    fn wait_held(&self, state_dir: &Path, bin: &Path) {
        let slot = self.slot.clone();
        let state = state_dir.to_path_buf();
        let bin = bin.to_path_buf();
        wait_until(&format!("{slot} held"), move || {
            status_json(&state, &bin)
                .and_then(|rows| slot_row(&rows, &slot).map(|row| row["state"] == "held"))
                .unwrap_or(false)
        });
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        // SIGTERM weir first so it can forward the signal to the worker's
        // process group; only escalate to SIGKILL if it lingers past the
        // grace period, so no `sleep 120` worker is ever orphaned.
        let pid = self.child.id() as i32;
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        return;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(_) => {
                    let _ = self.child.wait();
                    return;
                }
            }
        }
    }
}

/// PATH with the fake-worker `bin` prepended.
fn path_with(bin: &Path) -> String {
    format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// 1. Four concurrent runs over two slots: two at a time in parallel, never two
///    holders on the same replica, and the whole batch well under 3.5 s.
#[test]
fn lease_run_serialises_per_slot_across_processes() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let bin = fake_worker(dir.path());
    let log = state.join("holders.log");

    let started = Instant::now();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let state = state.clone();
            let bin = bin.clone();
            thread::spawn(move || {
                let out = lease_cmd(
                    &state,
                    &bin,
                    &[
                        "lease",
                        "run",
                        "--slot",
                        "gpu-a=a",
                        "--slot",
                        "gpu-b=b",
                        "--",
                        "pi-worker",
                        "-b",
                        "{replica}",
                    ],
                )
                .env("FAKE_SLEEP", "1")
                .output()
                .expect("run lease");
                assert!(
                    out.status.success(),
                    "lease run failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                String::from_utf8_lossy(&out.stderr).into_owned()
            })
        })
        .collect();
    let summaries: Vec<String> = handles
        .into_iter()
        .map(|h| h.join().expect("worker thread"))
        .collect();
    let wall = started.elapsed();

    // Two slots × 1 s of work ⇒ ~2 s total; 3.5 s proves no needless queueing.
    assert!(
        wall < Duration::from_millis(3500),
        "took too long: {wall:?}"
    );
    for line in &summaries {
        assert!(
            line.contains("weir lease: slot=gpu-"),
            "missing summary line: {line}"
        );
    }

    // No two holders may overlap on the same replica.
    let text = std::fs::read_to_string(&log).expect("holder log");
    let mut intervals: Vec<(String, u64, u64)> = Vec::new();
    let mut replicas: Vec<String> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let f: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(f.len(), 4, "bad log line {n}: {line:?}");
        let (kind, replica, ts) = (f[0], f[1].to_owned(), f[3].parse::<u64>().unwrap());
        if kind == "start" {
            assert!(
                !replica.is_empty(),
                "empty replica field in log line: {line:?}"
            );
            replicas.push(replica.clone());
            intervals.push((replica, ts, u64::MAX));
        } else {
            let slot = intervals
                .iter_mut()
                .rev()
                .find(|(r, _, end)| r == &replica && *end == u64::MAX)
                .expect("end without matching start");
            slot.2 = ts;
        }
    }
    for (i, (ra, s1, e1)) in intervals.iter().enumerate() {
        assert_ne!(*e1, u64::MAX, "unclosed interval for {ra}");
        for (rb, s2, e2) in intervals.iter().skip(i + 1) {
            if ra == rb {
                assert!(
                    *e1 <= *s2 || *e2 <= *s1,
                    "overlapping holders on replica {ra}: {s1}-{e1} and {s2}-{e2}"
                );
            }
        }
    }
    assert_eq!(intervals.len(), 4, "one interval per run");
    // Two slots exist ⇒ both replicas must have been used across the 4 runs.
    assert_eq!(
        replicas
            .iter()
            .collect::<std::collections::HashSet::<&String>>()
            .len(),
        2,
        "expected both replicas a and b, got {replicas:?}"
    );
}

/// 2. `{replica}` is substituted with the replica of the slot that won.
#[test]
fn lease_run_substitutes_replica() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let bin = fake_worker(dir.path());
    let args_log = state.join("args.txt");

    // gpu-a is busy ⇒ gpu-b wins ⇒ the replica must be `b`.
    let holder = Holder::new(&state, &bin, "gpu-a=a");
    holder.wait_held(&state, &bin);

    lease_cmd(
        &state,
        &bin,
        &[
            "lease",
            "run",
            "--slot",
            "gpu-a=a",
            "--slot",
            "gpu-b=b",
            "--",
            "pi-worker",
            "-b",
            "{replica}",
            "job-{replica}.md",
        ],
    )
    .env("FAKE_ARGS", &args_log)
    .assert()
    .success()
    .stderr(predicate::str::contains("slot=gpu-b replica=b"));

    let logged = std::fs::read_to_string(&args_log).expect("args log");
    // weir passes only the arguments to the child — `printf '%s\n' "$@"`
    // excludes the program name (argv[0] is never in "$@").
    assert_eq!(logged, "-b\nb\njob-b.md\n", "args as seen by the worker");
}

/// 3. Anything other than `pi-worker` / `agy-worker` is refused with exit 2,
///    and no lease file (or leases dir) is created.
#[test]
fn lease_run_rejects_non_worker_command() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let bin = fake_worker(dir.path());

    lease_cmd(
        &state,
        &bin,
        &[
            "lease", "run", "--slot", "gpu-a=a", "--", "bash", "-c", "echo hi",
        ],
    )
    .assert()
    .code(2)
    .stderr(predicate::str::contains("pi-worker"));

    assert!(
        !state.join("leases").exists(),
        "a rejected command must not create lease files"
    );
}

/// 4. The child's exit code is propagated verbatim.
#[test]
fn lease_run_propagates_child_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let bin = fake_worker(dir.path());

    lease_cmd(
        &state,
        &bin,
        &[
            "lease",
            "run",
            "--slot",
            "gpu-a=a",
            "--",
            "pi-worker",
            "job",
        ],
    )
    .env("FAKE_EXIT", "7")
    .assert()
    .code(7)
    .stderr(predicate::str::contains("exit=7"));
}

/// 5. With every slot busy, `--wait-timeout 1` gives up with exit 75.
#[test]
fn lease_run_wait_timeout_exits_75() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let bin = fake_worker(dir.path());

    let a = Holder::new(&state, &bin, "gpu-a=a");
    let b = Holder::new(&state, &bin, "gpu-b=b");
    a.wait_held(&state, &bin);
    b.wait_held(&state, &bin);

    let started = Instant::now();
    lease_cmd(
        &state,
        &bin,
        &[
            "lease",
            "run",
            "--slot",
            "gpu-a=a",
            "--slot",
            "gpu-b=b",
            "--wait-timeout",
            "1",
            "--",
            "pi-worker",
            "job",
        ],
    )
    .assert()
    .code(75)
    .failure()
    .stderr(predicate::str::contains("timeout"));
    assert!(started.elapsed() >= Duration::from_millis(900));
}

/// 6. `lease status` reports a slot held while its command runs, free after it.
#[test]
fn lease_status_reports_held_then_free() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let bin = fake_worker(dir.path());

    // No lease file yet ⇒ empty list.
    let rows = status_json(&state, &bin).expect("status json");
    assert_eq!(rows["slots"].as_array().map(Vec::len), Some(0));

    let runner = std::process::Command::new(assert_cmd::cargo::cargo_bin("weir"))
        .env("WEIR_STATE_DIR", &state)
        .env("PATH", path_with(&bin))
        .env("FAKE_LOG", state.join("holders.log"))
        .env("FAKE_SLEEP", "30")
        .args([
            "lease",
            "run",
            "--slot",
            "gpu-a=alpha",
            "--",
            "pi-worker",
            "long-job",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn runner");

    // Held, with the record of the live holder.
    wait_until("gpu-a held", || {
        status_json(&state, &bin)
            .and_then(|rows| {
                slot_row(&rows, "gpu-a")
                    .map(|r| r["state"] == "held" && r["record"]["replica"] == "alpha")
            })
            .unwrap_or(false)
    });
    let rows = status_json(&state, &bin).unwrap();
    let row = slot_row(&rows, "gpu-a").unwrap();
    assert_eq!(row["record"]["slot"], "gpu-a");
    assert!(row["record"]["command"]
        .as_str()
        .unwrap()
        .contains("long-job"));
    assert!(row["record"]["started_at"].as_u64().unwrap() > 0);
    assert!(row["record"]["pid"].as_u64().unwrap() > 0);

    // Human output shows the same slot as held.
    lease_cmd(&state, &bin, &["lease", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("gpu-a"))
        .stdout(predicate::str::contains("held"));

    // Interrupt the run: weir forwards SIGTERM to the child's process group.
    // Signal the child directly (not our own group, which SIGTERM would hit).
    let runner_pid = runner.id();
    unsafe {
        libc::kill(runner_pid as i32, libc::SIGTERM);
    }
    let out = runner.wait_with_output().expect("wait for runner");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains("weir lease: slot=gpu-a replica=alpha"),
        "summary line missing: {stderr}"
    );

    // And the lease is gone (slot registered but free).
    wait_until("gpu-a free", || {
        status_json(&state, &bin)
            .and_then(|rows| slot_row(&rows, "gpu-a").map(|r| r["state"] == "free"))
            .unwrap_or(false)
    });
}
