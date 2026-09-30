//! `weir worker run` — end-to-end tests against fake `pi-worker` /
//! `agy-worker` fixtures.
//!
//! Everything is hermetic: a temp dir is both the cwd and `$HOME`, every env
//! var that could steer config resolution is removed, the fixture scripts are
//! *copied* into a temp `bin` dir (chmod 755) prepended to `PATH` — never run
//! straight out of the source tree — and the v2 config keeps its state/scratch
//! paths inside the temp dir.
//!
//! The fake wrappers choose their behaviour from `$FAKE_WORKER_MODE`; see
//! `tests/fixtures/bin/pi-worker`. Because cargo runs tests in threads that
//! share one process, every variable this file ever sets is pinned on every
//! invocation (`env` or `env_remove`), so no test can inherit another's value.
#![cfg(unix)]

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use serde_json::Value;

/// Everything this file sets or removes: the vars that could steer config
/// resolution (`WEIR_CONFIG`, `XDG_CONFIG_HOME`, the `--deep` probes, the
/// state root) plus the fixture knobs (`FAKE_WORKER_*`) and the spawn-knob
/// overrides (`WEIR_*_GRACE_SECS`). Every one is cleared on every invocation,
/// then re-set only where the test needs it.
const PINNED_VARS: [&str; 10] = [
    "WEIR_CONFIG",
    "XDG_CONFIG_HOME",
    "WEIR_AGENT_JAIL",
    "WEIR_BWRAP",
    "WEIR_STATE_DIR",
    "XDG_STATE_HOME",
    "FAKE_WORKER_MODE",
    "FAKE_WORKER_PIDFILE",
    "WEIR_DEADLINE_GRACE_SECS",
    "WEIR_KILL_GRACE_SECS",
];
/// Bytes of stdout weir keeps per stream (`spawn::OUTPUT_CAP`).
const OUTPUT_CAP: usize = 8 * 1024 * 1024;
/// The nine keys of the `--json` record.
const JSON_KEYS: [&str; 9] = [
    "worker",
    "replica",
    "model",
    "exit",
    "kind",
    "elapsed_ms",
    "stdout",
    "stderr_tail",
    "truncated",
];

// ── fixture plumbing ──────────────────────────────────────────────────────────

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// Copy a fixture script from `tests/fixtures/bin` into `dir/<name>` and make
/// it executable. Written via a temp name + rename: chmod-ing a script another
/// thread is spawning can fail with ETXTBSY.
fn copy_fixture(dir: &Path, name: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/bin")
        .join(name);
    let dst = dir.join(name);
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::copy(&src, &tmp).unwrap_or_else(|e| panic!("copy {}: {e}", src.display()));
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&tmp, &dst).unwrap();
    dst
}

/// The v2 config: pi (command `pi_command`) and agy, both 900 s, one
/// capacity-1 slot for pi replica `a`.
fn worker_config(cwd: &Path, pi_command: &str) -> String {
    format!(
        r#"version = 2

[paths]
wt_roots = ["{root}/wt"]
state_dir = "{root}/state"
scratch = "{root}/scratch"

[slots.gpu-a]
capacity = 1

[worker.pi]
command = "{pi_command}"
timeout = 900
slots = {{ a = "gpu-a" }}

[worker.agy]
command = "agy-worker"
timeout = 900
default_model = "test-model"
"#,
        root = cwd.display()
    )
}

/// One hermetic `weir worker run` fixture (temp dir pinned for the lifetime of
/// the test).
struct Fix {
    cwd: PathBuf,
    bin: PathBuf,
    cfg: PathBuf,
    workdir: PathBuf,
    prompt: PathBuf,
    #[allow(dead_code)]
    tmp: tempfile::TempDir,
}

impl Fix {
    /// Fixture whose `[worker.pi] command` is `pi_command`.
    fn with_pi_command(pi_command: &str) -> Fix {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        let bin = copy_fixture_dir(&cwd);
        let workdir = cwd.join("wt");
        std::fs::create_dir_all(&workdir).unwrap();
        let prompt = cwd.join("prompt.md");
        write(&prompt, "do the thing\n");

        let cfg = cwd.join("v2.toml");
        write(&cfg, &worker_config(&cwd, pi_command));

        Fix {
            cwd,
            bin,
            cfg,
            workdir,
            prompt,
            tmp,
        }
    }

    fn new() -> Fix {
        Fix::with_pi_command("pi-worker")
    }

    /// A `weir` invocation: hermetic env, config from `--config`, fake bins
    /// first on PATH (system bins stay reachable for the fixtures' `sh`,
    /// `head`, `tr`).
    fn cmd(&self) -> Command {
        self.cmd_with_config(&self.cfg)
    }

    /// Same hermetic environment, but `--config` points at `cfg`.
    fn cmd_with_config(&self, cfg: &Path) -> Command {
        let mut cmd = Command::cargo_bin("weir").expect("binary built");
        for var in PINNED_VARS {
            cmd.env_remove(var);
        }
        let sys_path = std::env::var("PATH").unwrap_or_default();
        cmd.current_dir(&self.cwd)
            .env("HOME", self.cwd.join("home"))
            .env("XDG_CONFIG_HOME", self.cwd.join("home/.config"))
            .env("XDG_STATE_HOME", self.cwd.join("state"))
            .env("PATH", format!("{}:{sys_path}", self.bin.display()))
            .arg("--config")
            .arg(cfg);
        cmd
    }

    /// `weir worker run --worker WORKER [extra] [--json] <workdir> <prompt>`
    /// with `FAKE_WORKER_MODE=mode`.
    fn run(&self, worker: &str, mode: &str, extra: &[&str], json: bool) -> std::process::Output {
        let mut cmd = self.cmd();
        cmd.env("FAKE_WORKER_MODE", mode);
        let mut argv: Vec<&str> = vec!["worker", "run", "--worker", worker];
        argv.extend_from_slice(extra);
        if json {
            argv.push("--json");
        }
        cmd.args(&argv)
            .arg(&self.workdir)
            .arg(&self.prompt)
            .output()
            .unwrap()
    }

    /// The raw `std::process::Command` behind [`Fix::cmd`], for the one test
    /// that must spawn without waiting.
    fn std_cmd(&self, mode: &str, argv: &[&str]) -> std::process::Command {
        let mut cmd = std::process::Command::new(Command::cargo_bin("weir").unwrap().get_program());
        for var in PINNED_VARS {
            cmd.env_remove(var);
        }
        let sys_path = std::env::var("PATH").unwrap_or_default();
        cmd.current_dir(&self.cwd)
            .env("HOME", self.cwd.join("home"))
            .env("XDG_CONFIG_HOME", self.cwd.join("home/.config"))
            .env("XDG_STATE_HOME", self.cwd.join("state"))
            .env("PATH", format!("{}:{sys_path}", self.bin.display()))
            .env("FAKE_WORKER_MODE", mode)
            .arg("--config")
            .arg(&self.cfg)
            .args(argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd
    }
}

/// Create `<cwd>/bin` with both fake wrappers in it.
fn copy_fixture_dir(cwd: &Path) -> PathBuf {
    let bin = cwd.join("bin");
    for name in ["pi-worker", "agy-worker"] {
        copy_fixture(&bin, name);
    }
    bin
}

/// Write an executable `#!/bin/sh` script (temp name + rename, see
/// [`copy_fixture`]).
fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let dst = dir.join(name);
    let tmp = dir.join(format!("{name}.tmp"));
    write(&tmp, &format!("#!/bin/sh\n{body}\n"));
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&tmp, &dst).unwrap();
    dst
}

/// Parse stdout of an `--json` run as one JSON object.
fn json_of(out: &std::process::Output) -> Value {
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("not one JSON object ({e}): {text}"))
}

/// Assert the record has exactly the nine keys; return `(exit, kind)`.
fn code_and_kind(out: &std::process::Output) -> (i32, String) {
    let v = json_of(out);
    let obj = v
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {v}"));
    for key in JSON_KEYS {
        assert!(obj.contains_key(key), "missing key {key}: {v}");
    }
    assert_eq!(obj.len(), JSON_KEYS.len(), "unexpected keys: {v}");
    (
        out.status.code().expect("exit code"),
        v["kind"].as_str().unwrap().to_string(),
    )
}

/// The `stdout` lines of an `args`-mode run.
fn argv_lines(out: &std::process::Output) -> Vec<String> {
    json_of(out)["stdout"]
        .as_str()
        .unwrap()
        .lines()
        .map(|s| s.to_string())
        .collect()
}

/// `/proc/<pid>/stat` state letter, or `None` when the process is gone.
fn proc_state(pid: &str) -> Option<String> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    let text = String::from_utf8_lossy(&stat).to_string();
    // "pid (comm) state …" — comm may contain spaces and parentheses.
    let after = text.rfind(')')?;
    Some(text[after + 1..].trim_start().chars().next()?.to_string())
}

// ── (a) grandchildren die with the process group ──────────────────────────────

#[test]
fn timeout_kills_the_whole_process_group() {
    let fix = Fix::new();
    let pidfile = fix.cwd.join("grandchild.pid");

    let mut cmd = fix.std_cmd(
        "grandchild",
        &[
            "worker",
            "run",
            "--worker",
            "pi",
            "--timeout",
            "1",
            "--json",
        ],
    );
    cmd.env("FAKE_WORKER_PIDFILE", &pidfile)
        .env("WEIR_DEADLINE_GRACE_SECS", "1")
        .env("WEIR_KILL_GRACE_SECS", "1")
        .arg(&fix.workdir)
        .arg(&fix.prompt);
    let started = Instant::now();
    // wait_with_output drains both pipes, so the child can never block on one.
    let out = cmd.output().unwrap();
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(20),
        "weir took {elapsed:?}; it must not wait for the grandchild's `sleep 300`"
    );
    assert_eq!(out.status.code(), Some(124), "expected the timeout code");
    let v = json_of(&out);
    assert_eq!(v["kind"], "timeout");
    assert_eq!(v["exit"], 124);

    // The grandchild must be gone: no /proc entry, or a zombie (already dead,
    // only its exit status lingering).
    let pid = std::fs::read_to_string(&pidfile)
        .unwrap_or_else(|e| panic!("pidfile {}: {e}", pidfile.display()))
        .trim()
        .to_string();
    assert!(!pid.is_empty(), "fixture wrote an empty pidfile");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match proc_state(&pid).as_deref() {
            None | Some("Z") => break,
            Some(state) => {
                assert!(
                    Instant::now() < deadline,
                    "grandchild {pid} still in state {state:?} 2 s after weir exited"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

// ── (b)(c)(d) exit-code classification ────────────────────────────────────────

#[test]
fn quota_is_exit_7_kind_quota_for_agy() {
    let fix = Fix::new();
    let out = fix.run("agy", "quota", &[], true);
    assert_eq!(code_and_kind(&out), (7, "quota".to_string()));
    // Human mode classifies identically (same code, summary line on stderr).
    let human = fix.run("agy", "quota", &[], false);
    assert_eq!(human.status.code(), Some(7));
    assert!(
        String::from_utf8_lossy(&human.stderr).contains("kind=quota"),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
}

#[test]
fn empty_is_exit_3_kind_empty_for_agy() {
    let fix = Fix::new();
    let out = fix.run("agy", "empty", &[], true);
    assert_eq!(code_and_kind(&out), (3, "empty".to_string()));
}

#[test]
fn quota_message_without_a_pattern_is_empty_for_pi() {
    // pi has no quota_pattern: the very same wrapper output is "empty".
    let fix = Fix::new();
    let out = fix.run("pi", "quota", &[], true);
    let (code, kind) = code_and_kind(&out);
    assert_eq!(code, 3);
    assert_eq!(kind, "empty");
    assert_eq!(json_of(&out)["exit"], 3);
    assert!(
        json_of(&out)["stderr_tail"]
            .as_str()
            .unwrap()
            .contains("RESOURCE_EXHAUSTED"),
        "the wrapper message should still be reported: {}",
        json_of(&out)
    );
}

#[test]
fn denied_is_exit_6() {
    let fix = Fix::new();
    let out = fix.run("pi", "denied", &[], true);
    assert_eq!(code_and_kind(&out), (6, "denied".to_string()));
}

// ── (e) output cap ────────────────────────────────────────────────────────────

#[test]
fn oversized_stdout_is_capped_and_flagged() {
    let fix = Fix::new();
    let out = fix.run("pi", "big", &[], true);
    let (code, kind) = code_and_kind(&out);
    assert_eq!(code, 0);
    assert_eq!(kind, "ok");
    let v = json_of(&out);
    assert_eq!(v["truncated"], true);
    let stdout = v["stdout"].as_str().unwrap();
    assert!(
        stdout.len() <= OUTPUT_CAP,
        "stdout {} bytes > cap {OUTPUT_CAP}",
        stdout.len()
    );
    assert!(stdout.chars().all(|c| c == 'x'), "content lost");
}

// ── (f) argv layout and the JSON record ───────────────────────────────────────

#[test]
fn json_ok_record_pi_with_replica() {
    let fix = Fix::new();
    let out = fix.run("pi", "ok", &["--replica", "a"], true);
    assert_eq!(code_and_kind(&out), (0, "ok".to_string()));
    let v = json_of(&out);
    assert_eq!(v["worker"], "pi");
    assert_eq!(v["replica"], "a");
    assert!(v["model"].is_null(), "pi never has a model: {v}");
    assert_eq!(v["exit"], 0);
    assert_eq!(v["truncated"], false);
    assert!(v["elapsed_ms"].as_u64().unwrap() < 60_000);
    assert_eq!(
        v["stdout"].as_str().unwrap().split(" -t ").next().unwrap(),
        "fake ok args: -b a"
    );
}

#[test]
fn json_ok_record_agy_with_default_model() {
    let fix = Fix::new();
    let out = fix.run("agy", "ok", &[], true);
    assert_eq!(code_and_kind(&out), (0, "ok".to_string()));
    let v = json_of(&out);
    assert_eq!(v["worker"], "agy");
    assert!(v["replica"].is_null(), "agy never has a replica: {v}");
    assert_eq!(v["model"], "test-model");
}

#[test]
fn argv_layout_pi() {
    let fix = Fix::new();
    let out = fix.run("pi", "args", &["--replica", "a"], true);
    assert_eq!(
        argv_lines(&out),
        vec![
            "-b".to_string(),
            "a".to_string(),
            "-t".to_string(),
            "900".to_string(),
            fix.workdir
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string(),
            fix.prompt
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string(),
        ]
    );
}

#[test]
fn argv_pi_without_replica_has_no_b_flag_and_honours_timeout() {
    let fix = Fix::new();
    let out = fix.run("pi", "args", &["--timeout", "17"], true);
    assert_eq!(
        argv_lines(&out),
        vec![
            "-t".to_string(),
            "17".to_string(),
            fix.workdir
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string(),
            fix.prompt
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string(),
        ]
    );
}

#[test]
fn argv_layout_agy() {
    let fix = Fix::new();
    let out = fix.run("agy", "args", &[], true);
    assert_eq!(
        argv_lines(&out),
        vec![
            "-m".to_string(),
            "test-model".to_string(),
            "-t".to_string(),
            "900".to_string(),
            fix.workdir
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string(),
            fix.prompt
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string(),
        ]
    );

    // --model overrides default_model, in argv and in the record.
    let out = fix.run("agy", "args", &["--model", "other-model"], true);
    assert_eq!(argv_lines(&out)[..2], ["-m", "other-model"]);
    assert_eq!(json_of(&out)["model"], "other-model");
}

#[test]
fn human_output_is_verbatim_stdout_plus_one_summary_line() {
    let fix = Fix::new();
    let out = fix.run("pi", "ok", &["--replica", "a"], false);
    assert_eq!(out.status.code(), Some(0));
    // stdout carries exactly the wrapper's output — no weir decoration, and no
    // newline invented when the wrapper did not write one.
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        stdout.starts_with("fake ok args: -b a -t 900 "),
        "{stdout:?}"
    );
    assert!(stdout.ends_with('\n'), "no newline may be invented");
    // Exactly the wrapper's single line: weir adds nothing to stdout.
    assert_eq!(stdout.lines().count(), 1, "{stdout:?}");

    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), 1, "exactly one summary line: {stderr:?}");
    for needle in [
        "weir: worker=pi",
        "replica=a",
        "model=-",
        "exit=0",
        "kind=ok",
        "elapsed=",
        "ms",
        "truncated=false",
    ] {
        assert!(
            lines[0].contains(needle),
            "{needle:?} missing from {lines:?}"
        );
    }
}

#[test]
fn global_json_flag_alone_also_emits_json() {
    let fix = Fix::new();
    let out = fix
        .cmd()
        .env("FAKE_WORKER_MODE", "ok")
        .args(["--json", "worker", "run", "--worker", "pi"])
        .arg(&fix.workdir)
        .arg(&fix.prompt)
        .output()
        .unwrap();
    assert_eq!(code_and_kind(&out), (0, "ok".to_string()));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("weir: worker="));
}

// ── (g) allow-list ────────────────────────────────────────────────────────────

#[test]
fn a_config_command_outside_the_allow_list_is_refused_before_spawning() {
    // The config claims the pi worker is `sh`, and `<bin>/sh` would both print
    // a marker and touch a file if it ever ran.
    let fix = Fix::with_pi_command("sh");
    let marker = fix.cwd.join("spawned.marker");
    write_script(
        &fix.bin,
        "sh",
        &format!(": > '{}'\nprintf 'SH-RAN\\n'\n", marker.to_string_lossy()),
    );

    let out = fix.run("pi", "ok", &[], false);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!combined.contains("SH-RAN"), "{combined}");
    assert!(!marker.exists(), "the refused program must never run");
    assert_eq!(out.status.code(), Some(2), "{combined}");
    assert!(
        combined.contains("not an allowed worker wrapper"),
        "{combined}"
    );
}

/// The worker contract: only `pi-worker` / `agy-worker` may be exec'd, and a
/// refusal happens before anything is spawned. Asserted through the CLI (an
/// integration test cannot import the crate; the direct `run_worker` unit test
/// lives in `src/worker/spawn.rs`).
#[test]
fn no_program_outside_the_allow_list_can_be_reached() {
    for command in ["sh", "touch", "agy", "PI-WORKER", "./pi-worker", ""] {
        let fix = Fix::with_pi_command(command);
        // Any executable by that name would create the marker.
        let marker = fix.cwd.join("spawned.marker");
        write_script(
            &fix.bin,
            if command.is_empty() {
                "empty-name"
            } else {
                command
            },
            &format!(": > '{}'\n", marker.to_string_lossy()),
        );
        let out = fix.run("pi", "ok", &[], false);
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            out.status.code(),
            Some(2),
            "config command {command:?} must be refused: {combined}"
        );
        assert!(!marker.exists(), "{command:?} was spawned: {combined}");
    }
}

// ── usage errors ──────────────────────────────────────────────────────────────

#[test]
fn pi_with_model_is_a_usage_error() {
    let fix = Fix::new();
    let out = fix.run("pi", "ok", &["--model", "x"], false);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--model does not apply"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn agy_with_replica_is_a_usage_error() {
    let fix = Fix::new();
    let out = fix.run("agy", "ok", &["--replica", "a"], false);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--replica does not apply"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn zero_timeout_is_a_usage_error() {
    let fix = Fix::new();
    let out = fix.run("pi", "ok", &["--timeout", "0"], false);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--timeout must be > 0"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn nonexistent_workdir_is_a_usage_error() {
    let fix = Fix::new();
    let out = fix
        .cmd()
        .env("FAKE_WORKER_MODE", "ok")
        .args(["worker", "run", "--worker", "pi"])
        .arg(fix.cwd.join("no-such-dir"))
        .arg(&fix.prompt)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is not an existing directory"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn nonexistent_prompt_file_is_a_usage_error() {
    let fix = Fix::new();
    let out = fix
        .cmd()
        .env("FAKE_WORKER_MODE", "ok")
        .args(["worker", "run", "--worker", "pi"])
        .arg(&fix.workdir)
        .arg(fix.cwd.join("no-such-prompt.md"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is not an existing file"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_legacy_config_is_a_usage_error() {
    let fix = Fix::new();
    let cfg = fix.cwd.join("legacy.toml");
    write(
        &cfg,
        r#"[[backend]]
name = "echoer"
type = "stdio-cli"
command = "echo"
args = ["{prompt}"]
timeout_secs = 60
"#,
    );
    let out = fix
        .cmd_with_config(&cfg)
        .env("FAKE_WORKER_MODE", "ok")
        .args(["worker", "run", "--worker", "pi"])
        .arg(&fix.workdir)
        .arg(&fix.prompt)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("requires a version = 2 config"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_config_without_the_requested_worker_table_is_a_usage_error() {
    let fix = Fix::new();
    let cfg = fix.cwd.join("pi-only.toml");
    write(
        &cfg,
        &format!(
            "version = 2\n[paths]\nwt_roots=[\"{r}/wt\"]\nstate_dir=\"{r}/state\"\nscratch=\"{r}/scratch\"\n[worker.pi]\ncommand = \"pi-worker\"\ntimeout = 900\n",
            r = fix.cwd.display()
        ),
    );
    let out = fix
        .cmd_with_config(&cfg)
        .env("FAKE_WORKER_MODE", "ok")
        .args(["worker", "run", "--worker", "agy"])
        .arg(&fix.workdir)
        .arg(&fix.prompt)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no [worker.agy] table"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_wrapper_missing_from_path_is_127() {
    let fix = Fix::new();
    let empty = fix.cwd.join("empty-bin");
    std::fs::create_dir_all(&empty).unwrap();
    let out = fix
        .cmd()
        .env("FAKE_WORKER_MODE", "ok")
        .env("PATH", &empty)
        .args(["worker", "run", "--worker", "pi", "--json"])
        .arg(&fix.workdir)
        .arg(&fix.prompt)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("weir worker: error"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── (h2) a quota message past the retained head cap ───────────────────────────

/// The stderr head cap used to make both the reported tail and the quota
/// classification look at the *first* 8 MiB only. A wrapper that floods stderr
/// and then reports its quota error last must still be classified `quota`, and
/// `stderr_tail` must end with that last line.
#[test]
fn quota_message_after_an_oversized_stderr_is_still_quota() {
    let fix = Fix::new();
    let out = fix.run("agy", "big-err", &[], true);
    let (code, kind) = code_and_kind(&out);
    assert_eq!(code, 7, "quota is exit 7 for agy: {out:?}");
    assert_eq!(kind, "quota");
    let v = json_of(&out);
    assert_eq!(v["truncated"], true);

    let tail = v["stderr_tail"].as_str().unwrap();
    // The tail window is filled with the filler byte ('y') up to its 64 KiB
    // capacity, so compare the last line rather than the whole window.
    let last = tail.lines().last().unwrap();
    assert!(
        last.ends_with("RESOURCE_EXHAUSTED"),
        "last stderr_tail line must be the quota message, got {last:?}"
    );
    assert!(
        tail.ends_with("RESOURCE_EXHAUSTED\n"),
        "stderr_tail must end with the quota line, got {tail:?}"
    );
    // A tail, not the whole 8 MiB.
    assert!(
        tail.len() <= 64 * 1024,
        "tail too big: {} bytes",
        tail.len()
    );

    // Without a quota pattern (pi) the same stream is merely "empty" — but the
    // message is still visible at the end of the tail.
    let out = fix.run("pi", "big-err", &[], true);
    let (code, kind) = code_and_kind(&out);
    assert_eq!((code, kind.as_str()), (3, "empty"));
    assert!(json_of(&out)["stderr_tail"]
        .as_str()
        .unwrap()
        .lines()
        .last()
        .unwrap()
        .ends_with("RESOURCE_EXHAUSTED"));
}

/// The old head-capped stderr made `stderr_tail` the last lines of the first
/// 8 MiB — for a long stream, a fragment of filler that says nothing about why
/// the wrapper failed. It must be the end of the stream instead.
#[test]
fn stderr_tail_of_an_oversized_stream_is_its_end() {
    let fix = Fix::new();
    let out = fix.run("agy", "big-err", &[], true);
    let tail = json_of(&out)["stderr_tail"].as_str().unwrap().to_string();
    assert!(tail.ends_with("RESOURCE_EXHAUSTED\n"), "{tail:?}");

    // The last 60 lines are the quota message plus at most one (partial) line
    // of filler — the 8 MiB of filler ahead of it contributes nothing.
    assert!(
        tail.lines().count() <= 2,
        "expected the tail to be the final line plus its filler remainder, got {} lines",
        tail.lines().count()
    );
    // And it really is a window of the filler stream, not an empty string: the
    // filler byte is what precedes the message.
    assert!(tail.contains('y'), "tail lost its context: {tail:?}");
}

// ── (h) timeout upper bound ───────────────────────────────────────────────────

/// `--timeout 18446744073709551615` used to reach
/// `Duration::from_secs(secs) + grace` after the worker was already spawned
/// and panic (exit 101). It must be a plain usage error now.
#[test]
fn huge_timeout_is_a_usage_error_and_never_panics() {
    let fix = Fix::new();
    let out = fix.run("pi", "ok", &["--timeout", "18446744073709551615"], false);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert_ne!(
        out.status.code(),
        Some(101),
        "weir must not panic on a huge timeout: {stderr}"
    );
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("exceeds the maximum of 604800 seconds"),
        "{stderr}"
    );

    // Same through --json: exit 2, one JSON error object, no panic.
    let out = fix.run("pi", "ok", &["--timeout", "18446744073709551615"], true);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(json_of(&out)["status"], "error");
}

/// An empty `--replica` / `--model` value, or one that starts with `-`, is a
/// usage error (exit 2): both would land in the wrapper's argv, where an empty
/// argument stalls some wrappers and a dash-leading one is reparsed as an
/// option.
#[test]
fn empty_replica_or_model_is_a_usage_error() {
    for (worker, flag) in [("pi", "--replica"), ("agy", "--model")] {
        let fix = Fix::new();
        // `--flag=` is how an empty value gets past clap's own parsing.
        let arg = format!("{flag}=");
        let out = fix.run(worker, "ok", &[&arg], false);
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert_eq!(out.status.code(), Some(2), "{worker} {arg}: {stderr}");
        assert!(
            stderr.contains(&format!("{flag} must not be empty")),
            "{worker} {arg}: {stderr}"
        );
    }
}

/// Same rule for a leading dash. `--flag -x` is refused by clap itself before
/// weir runs (also exit 2), so the values are passed as `--flag=-x` to reach
/// weir's own validation.
#[test]
fn dash_leading_replica_or_model_is_a_usage_error() {
    for (worker, flag, value) in [
        ("pi", "--replica", "-"),
        ("pi", "--replica", "--help"),
        ("agy", "--model", "-"),
        ("agy", "--model", "-m"),
    ] {
        let fix = Fix::new();
        let arg = format!("{flag}={value}");
        let out = fix.run(worker, "ok", &[&arg], false);
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert_eq!(out.status.code(), Some(2), "{worker} {arg}: {stderr}");
        assert!(
            // Rust's Debug quoting of the offending value is part of the message.
            stderr.contains(&format!("{flag} value {value:?} must not start with '-'")),
            "{worker} {arg}: {stderr}"
        );
    }

    // Even the form clap lets through as a separate argument is refused by clap
    // with its own usage error — exit 2 either way, nothing is spawned.
    let fix = Fix::new();
    let out = fix.run("agy", "ok", &["--model", "-x"], false);
    assert_eq!(out.status.code(), Some(2));
}

/// `[worker.agy] default_model` goes through the same check as `--model`: a
/// config cannot smuggle a flag-shaped value into the wrapper's argv.
#[test]
fn dash_leading_default_model_is_a_usage_error() {
    let fix = Fix::new();
    let cfg = fix.cwd.join("dash-model.toml");
    write(
        &cfg,
        &format!(
            "version = 2\n[paths]\nwt_roots=[\"{r}/wt\"]\nstate_dir=\"{r}/state\"\nscratch=\"{r}/scratch\"\n[worker.agy]\ncommand = \"agy-worker\"\ntimeout = 900\ndefault_model = \"--help\"\n",
            r = fix.cwd.display()
        ),
    );
    let out = fix
        .cmd_with_config(&cfg)
        .env("FAKE_WORKER_MODE", "ok")
        .args(["worker", "run", "--worker", "agy"])
        .arg(&fix.workdir)
        .arg(&fix.prompt)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("default_model value \"--help\" must not start with '-'")
            && stderr.contains("[worker.agy] default_model"),
        "{stderr}"
    );
}

/// A dash *inside* a value is ordinary and must keep working.
#[test]
fn dash_inside_a_model_still_works() {
    let fix = Fix::new();
    let out = fix.run("agy", "ok", &["--model", "gpt-4o-mini"], true);
    assert_eq!(json_of(&out)["model"], "gpt-4o-mini");
    assert!(
        json_of(&out)["stdout"]
            .as_str()
            .unwrap()
            .starts_with("fake ok args: -m gpt-4o-mini -t "),
        "{:?}",
        json_of(&out)
    );
}

// ── (i) SIGTERM mid-run takes the whole tree with it ──────────────────────────

/// Poll `/proc` until `pid` is gone (or a zombie — dead, only its exit status
/// lingering), failing after `within`.
fn assert_gone(pid: &str, within: Duration, what: &str) {
    let deadline = Instant::now() + within;
    loop {
        match proc_state(pid).as_deref() {
            None | Some("Z") => return,
            Some(state) => {
                assert!(
                    Instant::now() < deadline,
                    "{what} {pid} still in state {state:?} after {within:?}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Start `weir worker run` in `grandchild` mode, SIGTERM weir while its
/// wrapper is still running, and assert that weir stops promptly *and* that the
/// whole worker tree — wrapper and grandchild — is gone with it.
///
/// The signal streams are registered before the spawn and a drop guard covers
/// everything after it, so a signal that arrives this early can no longer kill
/// weir alone and orphan the tree.
#[test]
fn sigterm_midrun_stops_weir_and_kills_the_grandchild() {
    let fix = Fix::new();
    let pidfile = fix.cwd.join("grandchild.pid");

    let mut child = fix
        .std_cmd(
            "grandchild",
            &[
                "worker",
                "run",
                "--worker",
                "pi",
                "--timeout",
                "300",
                "--json",
            ],
        )
        .env("FAKE_WORKER_PIDFILE", &pidfile)
        // Short TERM→KILL escalation: weir must be gone about a second after
        // the TERM rather than waiting out the wrapper's own 300 s.
        .env("WEIR_KILL_GRACE_SECS", "1")
        .env("WEIR_DEADLINE_GRACE_SECS", "60")
        .arg(&fix.workdir)
        .arg(&fix.prompt)
        .spawn()
        .expect("spawn weir");

    // Drain weir's stderr on another thread (it can never block on a full
    // pipe) and use it as the readiness signal: weir initialises its logging
    // first thing, so once a line appears the process is past argument parsing
    // and the runtime is coming up.
    let stderr_pipe = child.stderr.take().expect("stderr piped");
    let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let sink = Arc::clone(&stderr_buf);
        std::thread::spawn(move || {
            let mut pipe = stderr_pipe;
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            *sink.lock().unwrap() = buf;
        });
    }
    let stderr_so_far = || String::from_utf8_lossy(&stderr_buf.lock().unwrap().clone()).to_string();
    let ready_deadline = Instant::now() + Duration::from_secs(15);

    // Wait for the wrapper to record its grandchild — weir is mid-run now.
    let pid = loop {
        match std::fs::read_to_string(&pidfile) {
            Ok(text) if !text.trim().is_empty() => break text.trim().to_string(),
            _ => {
                // weir died before its worker started: not the scenario under
                // test, and nothing to do with a signal (none sent yet).
                if child.try_wait().expect("try_wait").is_some() {
                    panic!(
                        "weir exited before the pidfile appeared: {}",
                        stderr_so_far()
                    );
                }
                if Instant::now() > ready_deadline {
                    panic!("no pidfile after 15 s: {}", stderr_so_far());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    };

    send_sigterm(child.id());

    // Reap weir, bounded: it must react within its 1 s TERM→KILL grace, not
    // anywhere near the 300 s the wrapper was asked for. `wait_with_output`
    // would hang forever if it did, so poll first and only read once EOF is
    // guaranteed by the reap.
    let started = Instant::now();
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None => {
                assert!(
                    started.elapsed() < Duration::from_secs(20),
                    "weir still running 20 s after SIGTERM: {}",
                    stderr_so_far()
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    };
    let mut stdout = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_end(&mut stdout).expect("read stdout");
    }
    let out = std::process::Output {
        status,
        stdout,
        stderr: stderr_buf.lock().unwrap().clone(),
    };
    assert_eq!(
        out.status.code(),
        Some(124),
        "an interrupted run reports the timeout code: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json_of(&out);
    assert_eq!(v["kind"], "timeout");

    // The grandchild must not have outlived weir.
    assert_gone(&pid, Duration::from_secs(5), "grandchild");
}

/// Send SIGTERM to `pid` through `kill(1)` (the test crate has no libc dep).
fn send_sigterm(pid: u32) {
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .expect("kill(1) available");
    assert!(status.success(), "kill -TERM {pid} failed");
}
