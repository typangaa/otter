//! Config resolution (`--config` / `WEIR_CONFIG` / XDG / HOME), the v2
//! `weir validate` path and `weir validate --deep`.
//!
//! Every test pins the environment explicitly (`env_remove` for the weir
//! variables, `HOME` / `XDG_CONFIG_HOME` pointed at a temp dir) and runs the
//! binary with `current_dir` set to a temp dir, so the developer's real
//! `~/.config/weir/weir.toml` is never read and a `./weir.toml` can never be
//! picked up by accident.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;
use serde_json::Value;

/// The env vars that could steer config resolution or the deep probes.
const STEERING_VARS: [&str; 6] = [
    "WEIR_CONFIG",
    "XDG_CONFIG_HOME",
    "WEIR_AGENT_JAIL",
    "WEIR_BWRAP",
    "WEIR_STATE_DIR",
    "XDG_STATE_HOME",
];

/// A `weir` invocation with a hermetic environment, running in `cwd`.
fn weir_in(cwd: &Path) -> Command {
    let mut cmd = Command::cargo_bin("weir").expect("binary built");
    cmd.current_dir(cwd);
    for var in STEERING_VARS {
        cmd.env_remove(var);
    }
    cmd.env("HOME", cwd.join("home"));
    cmd.env("XDG_CONFIG_HOME", cwd.join("home/.config"));
    // Never let a test write leases, ledger or cooldowns into the developer's real state dir.
    cmd.env("XDG_STATE_HOME", cwd.join("state"));
    cmd
}

/// A full valid v2 config with every path under `root`.
fn v2_config(home: &Path) -> String {
    format!(
        r#"version = 2

[paths]
wt_roots = ["~/wt-a"]
state_dir = "{root}/state"
scratch = "{root}/scratch"

[slots.gpu-a]
capacity = 1

[worker.pi]
command = "pi-worker"
timeout = 900
slots = {{ a = "gpu-a" }}
"#,
        root = home.display()
    )
}

/// A legacy (v0.5, no `version` key) config.
const LEGACY_CONFIG: &str = r#"
[[backend]]
name = "echoer"
type = "stdio-cli"
command = "echo"
args = ["{prompt}"]
timeout_secs = 60
"#;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// Run `weir config path` in `cwd` (plus extra env) and return stdout trimmed.
fn config_path_stdout(cwd: &Path, extra_env: &[(&str, &Path)]) -> String {
    let mut cmd = weir_in(cwd);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.arg("config").arg("path").output().unwrap();
    assert!(
        out.status.success(),
        "expected success, got {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ── `weir config path`: resolution order ──────────────────────────────────────

#[test]
fn config_path_flag_beats_weir_config_env() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    let from_env = cwd.join("from-env.toml");
    let from_flag = cwd.join("from-flag.toml");
    write(&from_env, &v2_config(cwd));
    write(&from_flag, &v2_config(cwd));

    let mut cmd = weir_in(cwd);
    cmd.env("WEIR_CONFIG", &from_env);
    let out = cmd
        .args(["config", "path", "--config"])
        .arg(&from_flag)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        from_flag.to_str().unwrap()
    );
}

#[test]
fn config_path_uses_weir_config_env() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    let target = cwd.join("env-config.toml");
    write(&target, &v2_config(cwd));
    assert_eq!(
        config_path_stdout(cwd, &[("WEIR_CONFIG", &target)]),
        target.display().to_string()
    );
}

#[test]
fn config_path_uses_xdg_config_home() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    let xdg = cwd.join("xdg");
    write(&xdg.join("weir/weir.toml"), &v2_config(cwd));
    // HOME/.config/weir/weir.toml also exists; XDG must win.
    write(&cwd.join("home/.config/weir/weir.toml"), &v2_config(cwd));
    assert_eq!(
        config_path_stdout(cwd, &[("XDG_CONFIG_HOME", &xdg)]),
        xdg.join("weir/weir.toml").display().to_string()
    );
}

#[test]
fn config_path_falls_back_to_home_config_when_xdg_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    let home = cwd.join("home");
    write(&home.join(".config/weir/weir.toml"), &v2_config(cwd));
    let mut cmd = weir_in(cwd);
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", cwd.join("xdg-does-not-exist"));
    let out = cmd.arg("config").arg("path").output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        home.join(".config/weir/weir.toml").display().to_string()
    );
}

#[test]
fn config_path_json_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    let target = cwd.join("c.toml");
    write(&target, &v2_config(cwd));
    let mut cmd = weir_in(cwd);
    cmd.env("WEIR_CONFIG", &target);
    let out = cmd.args(["--json", "config", "path"]).output().unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["path"], target.display().to_string());
}

#[test]
fn no_config_anywhere_and_a_local_weir_toml_is_a_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    // The trap: a repo-controlled ./weir.toml must never be picked up.
    write(&cwd.join("weir.toml"), LEGACY_CONFIG);

    let out = weir_in(cwd).arg("config").arg("path").output().unwrap();
    assert_eq!(out.status.code(), Some(2), "expected exit code 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no weir config found; searched:"),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            &cwd.join("home/.config/weir/weir.toml")
                .display()
                .to_string()
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("--config / WEIR_CONFIG were not set"),
        "{stderr}"
    );
    assert!(stderr.contains("./weir.toml"), "{stderr}");
    // It must not claim to have *found* the local file.
    assert!(
        !stderr.contains(&format!("found {}", cwd.join("weir.toml").display())),
        "{stderr}"
    );
}

#[test]
fn config_consuming_commands_fail_with_2_without_a_config() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    write(&cwd.join("weir.toml"), LEGACY_CONFIG);

    for args in [vec!["validate"], vec!["status"]] {
        let out = weir_in(cwd).args(&args).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?} => {:?}",
            out.status.code()
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("no weir config found"),
            "{args:?}"
        );
    }
}

#[test]
fn json_mode_reports_the_resolution_failure_on_stdout_too() {
    let tmp = tempfile::tempdir().unwrap();
    let out = weir_in(tmp.path())
        .args(["--json", "validate"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "error");
    assert!(v["error"]
        .as_str()
        .unwrap()
        .contains("no weir config found"));
}

#[test]
fn explicit_config_pointing_at_a_missing_file_is_a_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("nope.toml");
    let out = weir_in(tmp.path())
        .arg("--config")
        .arg(&missing)
        .arg("validate")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("config file not found"), "{stderr}");
    assert!(stderr.contains("nope.toml"), "{stderr}");
}

// ── commands that must work with no config at all ─────────────────────────────

#[test]
fn lease_version_and_schema_work_without_any_config() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    write(&cwd.join("weir.toml"), LEGACY_CONFIG); // must be ignored

    for args in [
        vec!["lease", "status", "--json"],
        vec!["version"],
        vec!["--version"],
        vec!["schema"],
    ] {
        let out = weir_in(cwd)
            .env("WEIR_STATE_DIR", cwd.join("state"))
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

// ── validate: legacy and v2 ───────────────────────────────────────────────────

#[test]
fn validate_legacy_config_is_rejected_with_migration_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    let cfg = cwd.join("legacy.toml");
    write(&cfg, LEGACY_CONFIG);

    let out = weir_in(cwd)
        .arg("--config")
        .arg(&cfg)
        .arg("validate")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("version = 2") && stderr.contains("examples/weir.v2.example.toml"),
        "{stderr}"
    );
}

#[test]
fn validate_v2_ok_text_and_json() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    let cfg = cwd.join("v2.toml");
    write(&cfg, &v2_config(cwd));

    let out = weir_in(cwd)
        .arg("--config")
        .arg(&cfg)
        .arg("validate")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Config valid (v2):"), "{stdout}");
    assert!(
        stdout.contains("1 worker(s), 0 ladder(s), 0 check(s)"),
        "{stdout}"
    );

    let out = weir_in(cwd)
        .arg("--config")
        .arg(&cfg)
        .args(["--json", "validate"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["version"], 2);
    assert_eq!(v["workers"], 1);
    assert_eq!(v["ladders"], 0);
    assert_eq!(v["checks"], 0);
    assert_eq!(v["path"], cfg.display().to_string());
}

#[test]
fn validate_shipped_v2_example_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let out = weir_in(tmp.path())
        .arg("--config")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/weir.v2.example.toml"))
        .args(["--json", "validate"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["version"], 2);
    assert_eq!(v["workers"], 2);
    assert_eq!(v["ladders"], 1);
    assert_eq!(v["checks"], 3);
}

/// Write a v2 config whose worker section is `worker_toml`, then validate it.
fn assert_v2_rejected(cwd: &Path, worker_toml: &str, expected_substrings: &[&str]) {
    let cfg = cwd.join("bad.toml");
    write(
        &cfg,
        &format!(
            "version = 2\n[paths]\nwt_roots=[\"~/wt-a\"]\nstate_dir=\"{}/state\"\n{worker_toml}",
            cwd.display()
        ),
    );
    let out = weir_in(cwd)
        .arg("--config")
        .arg(&cfg)
        .arg("validate")
        .output()
        .unwrap();
    assert_ne!(out.status.code(), Some(0), "{worker_toml} must be rejected");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for needle in expected_substrings {
        assert!(
            combined.contains(needle),
            "{needle} missing from: {combined}"
        );
    }
}

#[test]
fn validate_rejects_bad_v2_configs() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();

    // command must be a bare <worker>-wrapper name
    assert_v2_rejected(
        cwd,
        "[worker.pi]\ncommand = \"agy\"\ntimeout = 900\n",
        &["command"],
    );
    assert_v2_rejected(
        cwd,
        "[worker.pi]\ncommand = \"./pi-worker\"\ntimeout = 900\n",
        &["must not contain '/'"],
    );
    assert_v2_rejected(
        cwd,
        "[worker.pi]\ncommand = \"opencode-worker\"\ntimeout = 900\n",
        &["not an allowed worker wrapper"],
    );
    // timeout must be > 0
    assert_v2_rejected(
        cwd,
        "[worker.pi]\ncommand = \"pi-worker\"\ntimeout = 0\n",
        &["[worker.pi] timeout"],
    );
    // unknown keys are a hard error
    assert_v2_rejected(
        cwd,
        "[worker.pi]\ncommand = \"pi-worker\"\ntimeout = 900\nbogus = 1\n",
        &[],
    );
    // a whole unknown worker table
    assert_v2_rejected(
        cwd,
        "[worker.pi]\ncommand = \"pi-worker\"\ntimeout = 900\n[worker.opencode]\ncommand = \"opencode-worker\"\ntimeout = 900\n",
        &["opencode"],
    );
}

// ── validate --deep ───────────────────────────────────────────────────────────

/// Write an executable script robustly: write to a temporary file in the same
/// directory, sync_all, drop the file, chmod 755, and rename into place.
/// This prevents ETXTBSY when other test threads fork concurrently.
fn write_executable(path: &Path, contents: &str) {
    use std::io::Write;

    let parent = path.parent().expect("parent dir");
    std::fs::create_dir_all(parent).unwrap();
    let name = path.file_name().unwrap().to_str().unwrap();
    let tmp = parent.join(format!("{name}.tmp"));
    let mut file =
        std::fs::File::create(&tmp).unwrap_or_else(|e| panic!("create {}: {e}", tmp.display()));
    file.write_all(contents.as_bytes())
        .unwrap_or_else(|e| panic!("write {}: {e}", tmp.display()));
    file.sync_all()
        .unwrap_or_else(|e| panic!("sync {}: {e}", tmp.display()));
    drop(file);
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&tmp, path)
        .unwrap_or_else(|e| panic!("rename {} -> {}: {e}", tmp.display(), path.display()));
}

/// Create an executable `#!/bin/sh` script named `name` in `dir`.
///
/// Written to a temporary name, synced, closed, made executable, then renamed into place:
/// chmod-ing a script that another thread is spawning can fail with ETXTBSY.
fn make_bin(dir: &Path, name: &str, body: &str) -> PathBuf {
    let script = dir.join(name);
    write_executable(&script, &format!("#!/bin/sh\n{body}\n"));
    script
}

/// A good environment for `--deep`: wrappers on PATH, a working fake bwrap, an
/// agent-jail fixture whose WT_ROOTS holds `$HOME/wt-a`.
struct DeepFixture {
    cwd: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    jail: PathBuf,
    bwrap: PathBuf,
}

fn deep_fixture(with_extra_root: bool) -> DeepFixture {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_path_buf();
    let home = cwd.join("home");
    let bin = cwd.join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();

    make_bin(&bin, "pi-worker", "exit 0");
    make_bin(&bin, "agy-worker", "exit 0");
    let bwrap = make_bin(&bin, "bwrap", "exit 0");
    let jail = cwd.join("agent-jail");
    let jail_roots = if with_extra_root {
        "WT_ROOTS=(\"$HOME/wt-a\" \"$HOME/wt-other\")"
    } else {
        "WT_ROOTS=(\"$HOME/wt-a\")"
    };
    write_executable(
        &jail,
        &format!(
            "#!/bin/bash\n# agent-jail fixture\n{jail_roots}\nexec bwrap --ro-bind / / \"$@\"\n"
        ),
    );

    std::mem::forget(tmp); // the fixture outlives this fn on purpose
    DeepFixture {
        cwd,
        home,
        bin,
        jail,
        bwrap,
    }
}

/// The config used by the --deep tests: one wt_root under HOME, state_dir in cwd.
fn deep_config(home: &Path, extra_root: bool) -> String {
    let wt_roots = if extra_root {
        format!(
            "[\"{}/wt-a\", \"{}/wt-missing\"]",
            home.display(),
            home.display()
        )
    } else {
        format!("[\"{}/wt-a\"]", home.display())
    };
    format!(
        "version = 2\n\
         [paths]\n\
         wt_roots = {wt_roots}\n\
         state_dir = \"{root}/state\"\n\
         scratch = \"{root}/scratch\"\n\
         [worker.pi]\n\
         command = \"pi-worker\"\n\
         timeout = 900\n",
        root = home.parent().unwrap().display()
    )
}

/// Run `weir validate --deep` against a fixture, returning the output.
fn run_deep(dir: &DeepFixture, extra_root: bool) -> std::process::Output {
    let cfg = dir.cwd.join("v2.toml");
    write(&cfg, &deep_config(&dir.home, extra_root));
    run_deep_cfg(dir, &cfg)
}

/// Run `weir validate --deep` with `cfg`, using the fixture's environment.
fn run_deep_cfg(dir: &DeepFixture, cfg: &Path) -> std::process::Output {
    let mut cmd = weir_in(&dir.cwd);
    // Pin HOME/XDG to the fixture, PATH to the fake bins + system bins.
    cmd.env("HOME", &dir.home);
    cmd.env("XDG_CONFIG_HOME", dir.home.join(".config"));
    cmd.env("PATH", format!("{}:/usr/bin:/bin", dir.bin.display()));
    cmd.env("WEIR_BWRAP", &dir.bwrap);
    cmd.env("WEIR_AGENT_JAIL", &dir.jail);
    cmd.arg("--config")
        .arg(cfg)
        .arg("validate")
        .arg("--deep")
        .output()
        .unwrap()
}

#[test]
fn deep_all_checks_pass() {
    let dir = deep_fixture(false);
    let out = run_deep(&dir, false);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "exit {:?}\nstdout:{stdout}\nstderr:{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("ok   wrapper pi-worker"), "{stdout}");
    assert!(stdout.contains("ok   bwrap"), "{stdout}");
    assert!(
        stdout.contains(&format!("ok   wt_root {}/wt-a", dir.home.display())),
        "{stdout}"
    );
    assert!(stdout.contains("ok   state_dir"), "{stdout}");
    assert!(!stdout.contains("FAIL"), "{stdout}");
}

#[test]
fn deep_json_all_ok() {
    let dir = deep_fixture(false);
    let cfg = dir.cwd.join("v2.toml");
    write(&cfg, &deep_config(&dir.home, false));
    let mut cmd = weir_in(&dir.cwd);
    cmd.env("HOME", &dir.home);
    cmd.env("PATH", format!("{}:/usr/bin:/bin", dir.bin.display()));
    cmd.env("WEIR_BWRAP", &dir.bwrap);
    cmd.env("WEIR_AGENT_JAIL", &dir.jail);
    let out = cmd
        .arg("--config")
        .arg(&cfg)
        .args(["--json", "validate", "--deep"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout:{} stderr:{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "ok");
    let checks = v["checks"].as_array().unwrap();
    assert!(checks.len() >= 4);
    assert!(checks.iter().all(|c| c["ok"] == Value::Bool(true)));
    assert!(checks.iter().any(|c| c["name"] == "bwrap"));
    assert!(checks.iter().any(|c| c["name"] == "state_dir"));
}

#[test]
fn deep_fails_on_wt_root_missing_from_agent_jail() {
    let dir = deep_fixture(true);
    let out = run_deep(&dir, true);
    assert_eq!(out.status.code(), Some(1));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("wt_root"), "{combined}");
    assert!(combined.contains("wt-missing"), "{combined}");
    assert!(combined.contains("FAIL"), "{combined}");
    // The failing line names the roots agent-jail does have.
    assert!(combined.contains("agent-jail has"), "{combined}");
    // The other checks still ran.
    assert!(combined.contains("wrapper pi-worker"), "{combined}");
}

#[test]
fn deep_fails_on_missing_wrapper() {
    let dir = deep_fixture(false);
    // A config whose worker is agy-worker, but only pi-worker is on PATH.
    let cfg = dir.cwd.join("v2.toml");
    write(
        &cfg,
        &format!(
            "version = 2\n[paths]\nwt_roots=[\"{h}/wt-a\"]\nstate_dir=\"{r}/state\"\n[worker.agy]\ncommand = \"agy-worker\"\ntimeout = 900\n",
            h = dir.home.display(),
            r = dir.cwd.display()
        ),
    );
    std::fs::remove_file(dir.bin.join("agy-worker")).unwrap();
    let out = run_deep_cfg(&dir, &cfg);
    assert_eq!(out.status.code(), Some(1));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("FAIL wrapper agy-worker"), "{combined}");
}

#[test]
fn deep_fails_on_broken_bwrap() {
    let dir = deep_fixture(false);
    make_bin(
        &dir.bin,
        "bwrap",
        "echo bubblewrap: Operation not permitted >&2; exit 1",
    );
    let out = run_deep(&dir, false);
    assert_eq!(out.status.code(), Some(1));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("FAIL bwrap"), "{combined}");
    assert!(combined.contains("Operation not permitted"), "{combined}");
}

#[test]
fn deep_fails_when_agent_jail_script_is_missing() {
    let dir = deep_fixture(false);
    std::fs::remove_file(&dir.jail).unwrap();
    let out = run_deep(&dir, false);
    assert_eq!(out.status.code(), Some(1));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("FAIL agent-jail WT_ROOTS"), "{combined}");
}

#[test]
fn deep_fails_when_agent_jail_has_no_wt_roots_array() {
    let dir = deep_fixture(false);
    write_executable(
        &dir.jail,
        "#!/bin/bash\n# no array here\nexec bwrap \"$@\"\n",
    );
    let out = run_deep(&dir, false);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("FAIL agent-jail WT_ROOTS"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn deep_on_legacy_config_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    let cfg = cwd.join("legacy.toml");
    write(&cfg, LEGACY_CONFIG);

    let out = weir_in(cwd)
        .arg("--config")
        .arg(&cfg)
        .arg("validate")
        .arg("--deep")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("missing 'version = 2'"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // json mode: the error is on stdout too.
    let out = weir_in(cwd)
        .arg("--config")
        .arg(&cfg)
        .args(["--json", "validate", "--deep"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "error");
    assert!(v["error"].as_str().unwrap().contains("version = 2"));
}

#[test]
fn deep_reports_a_state_dir_that_cannot_be_created() {
    let dir = deep_fixture(false);
    // Block state_dir creation by making its parent a regular file.
    let blocked = dir.cwd.join("blocker");
    write(&blocked, "not a directory");
    let cfg = dir.cwd.join("v2.toml");
    write(
        &cfg,
        &format!(
            "version = 2\n[paths]\nwt_roots=[\"{h}/wt-a\"]\nstate_dir=\"{b}/state\"\n[worker.pi]\ncommand = \"pi-worker\"\ntimeout = 900\n",
            h = dir.home.display(),
            b = blocked.display()
        ),
    );
    let out = run_deep_cfg(&dir, &cfg);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("FAIL state_dir"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// Sanity check that the fake-bwrap fixture really is what `--deep` runs:
/// a `PATH`-based invocation (`WEIR_BWRAP` unset) must find `<bin>/bwrap`.
#[test]
fn deep_uses_the_real_bwrap_when_weir_bwrap_is_unset() {
    let dir = deep_fixture(false);
    let cfg = dir.cwd.join("v2.toml");
    write(&cfg, &deep_config(&dir.home, false));
    let mut cmd = weir_in(&dir.cwd);
    cmd.env("HOME", &dir.home);
    cmd.env("PATH", format!("{}:/usr/bin:/bin", dir.bin.display()));
    cmd.env_remove("WEIR_BWRAP");
    cmd.env("WEIR_AGENT_JAIL", &dir.jail);
    let out = cmd
        .arg("--config")
        .arg(&cfg)
        .arg("validate")
        .arg("--deep")
        .output()
        .unwrap();
    // The fixture bwrap exits 0, so everything still passes.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stdout:{stdout} stderr:{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("ok   bwrap"), "{stdout}");
}

/// `sh -c` guard: the fixture scripts really are executables, not data.
#[test]
fn fixture_bins_are_executable() {
    let dir = deep_fixture(false);
    let bin = dir.bin.join("pi-worker");
    let mut last_err = None;
    let mut status = None;
    for _ in 0..20 {
        match StdCommand::new(&bin).status() {
            Ok(s) => {
                status = Some(s);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => panic!("exec {}: {e}", bin.display()),
        }
    }
    let status = status.unwrap_or_else(|| {
        panic!(
            "exec {} timed out waiting for ETXTBSY to clear: {:?}",
            bin.display(),
            last_err
        )
    });
    assert!(status.success());
    let e = std::fs::metadata(&bin).unwrap();
    assert_ne!(e.permissions().mode() & 0o111, 0);
}
