//! `weir task run|show|list|clean` — end-to-end integration tests.
//!
//! Everything is hermetic: a temp dir is both cwd and `$HOME`, all steering env
//! vars are cleared on every weir invocation, fixtures are copied into a temp
//! `bin` dir prepended to `PATH`, git runs with hermetic author/committer and
//! disabled global/system configs, and weir state/worktrees reside inside the
//! temp dir.
#![cfg(unix)]

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use assert_cmd::Command;
use serde_json::Value;

/// Env vars cleared on every weir invocation to guarantee hermetic execution.
const PINNED_VARS: [&str; 11] = [
    "WEIR_CONFIG",
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "WEIR_STATE_DIR",
    "FAKE_WORKER_MODE",
    "FAKE_WORKER_PIDFILE",
    "FAKE_EDIT_FILE",
    "FAKE_JAIL_LOG",
    "FAKE_JAIL_REFUSE",
    "WEIR_DEADLINE_GRACE_SECS",
    "WEIR_KILL_GRACE_SECS",
];

/// Git environment variables for hermetic git invocations.
const GIT_ENV_VARS: [(&str, &str); 6] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Weir Test"),
    ("GIT_AUTHOR_EMAIL", "weir-test@example.com"),
    ("GIT_COMMITTER_NAME", "Weir Test"),
    ("GIT_COMMITTER_EMAIL", "weir-test@example.com"),
];

/// The 21 keys of the `weir.task/1` JSON record defined in interface.md section 5.
const TASK_RECORD_KEYS: [&str; 21] = [
    "schema",
    "id",
    "status",
    "exit",
    "error",
    "repo",
    "base",
    "base_commit",
    "branch",
    "worktree",
    "worker",
    "replica",
    "model",
    "queue_wait_ms",
    "elapsed_ms",
    "created_at",
    "attempts",
    "answer",
    "diff",
    "checks",
    "cleanup",
];

// ── fixture and helper plumbing ──────────────────────────────────────────────

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// Copy a fixture script into `dir/<name>` and make it executable (chmod 755).
/// Writes to a temporary file, syncs, drops the file handle, chmods, and renames
/// into place to avoid ETXTBSY.
fn copy_fixture(dir: &Path, name: &str) -> PathBuf {
    use std::io::Write;

    std::fs::create_dir_all(dir).unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/bin")
        .join(name);
    let dst = dir.join(name);
    let tmp = dir.join(format!("{name}.tmp"));
    let content = std::fs::read(&src).unwrap_or_else(|e| panic!("read {}: {e}", src.display()));
    let mut file =
        std::fs::File::create(&tmp).unwrap_or_else(|e| panic!("create {}: {e}", tmp.display()));
    file.write_all(&content)
        .unwrap_or_else(|e| panic!("write {}: {e}", tmp.display()));
    file.sync_all()
        .unwrap_or_else(|e| panic!("sync {}: {e}", tmp.display()));
    drop(file);
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&tmp, &dst).unwrap();
    dst
}

/// Create `<cwd>/bin` with `pi-worker`, `agy-worker`, and `agent-jail`.
fn copy_fixture_dir(cwd: &Path) -> PathBuf {
    let bin = cwd.join("bin");
    for name in ["pi-worker", "agy-worker", "agent-jail"] {
        copy_fixture(&bin, name);
    }
    bin
}

/// Hermetic git command builder.
fn git_cmd(repo: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(repo);
    for (k, v) in GIT_ENV_VARS {
        cmd.env(k, v);
    }
    cmd
}

/// Validates task ID format `^\d{8}-[a-z0-9-]+-[0-9a-f]{4}$` by hand (no regex crate).
fn validate_task_id(id: &str) -> bool {
    if id.len() < 8 + 1 + 1 + 1 + 4 {
        return false;
    }
    let bytes = id.as_bytes();
    if !bytes[..8].iter().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if bytes[8] != b'-' {
        return false;
    }
    let last_dash = match id.rfind('-') {
        Some(idx) => idx,
        None => return false,
    };
    if last_dash <= 8 {
        return false;
    }
    let hex_part = &id[last_dash + 1..];
    if hex_part.len() != 4
        || !hex_part
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    {
        return false;
    }
    let middle = &id[9..last_dash];
    if middle.is_empty()
        || !middle
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return false;
    }
    true
}

/// Parse stdout of an invocation as a single JSON Value.
fn json_of(out: &std::process::Output) -> Value {
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("not valid JSON ({e}): {text}"))
}

/// Generate the v2 TOML config from interface.md section 8.
fn v2_config(wt_root: &Path, state_dir: &Path, scratch: &Path, pi_command: &str) -> String {
    format!(
        r#"version = 2

[paths]
wt_roots = ["{wt_root}"]
state_dir = "{state_dir}"
scratch = "{scratch}"

[slots.gpu-a]
capacity = 1

[slots.gpu-b]
capacity = 1

[worker.pi]
command = "{pi_command}"
timeout = 900
slots = {{ a = "gpu-a", b = "gpu-b" }}

[worker.agy]
command = "agy-worker"
timeout = 900
default_model = "test-model"

[check.pass]
cmd = ["true"]
timeout = 30

[check.fail]
cmd = ["sh", "-c", "echo boom; exit 1"]
timeout = 30

[check.has-edit]
cmd = ["test", "-f", "edited.txt"]
timeout = 30
"#,
        wt_root = wt_root.display(),
        state_dir = state_dir.display(),
        scratch = scratch.display(),
        pi_command = pi_command,
    )
}

/// Hermetic test fixture environment.
struct Fix {
    cwd: PathBuf,
    bin: PathBuf,
    cfg: PathBuf,
    repo: PathBuf,
    wt_root: PathBuf,
    state_dir: PathBuf,
    #[allow(dead_code)]
    scratch: PathBuf,
    #[allow(dead_code)]
    prompt: PathBuf,
    #[allow(dead_code)]
    tmp: tempfile::TempDir,
}

impl Fix {
    fn new() -> Self {
        Self::with_pi_command("pi-worker")
    }

    fn with_pi_command(pi_command: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().to_path_buf();
        let bin = copy_fixture_dir(&cwd);

        let wt_root = cwd.join("wt");
        let state_dir = cwd.join("state");
        let scratch = cwd.join("scratch");
        std::fs::create_dir_all(&wt_root).expect("create wt_root");
        std::fs::create_dir_all(&state_dir).expect("create state_dir");
        std::fs::create_dir_all(&scratch).expect("create scratch");

        let repo = cwd.join("repo");
        std::fs::create_dir_all(&repo).expect("create repo");

        let status = git_cmd(&repo)
            .args(["init", "-b", "main"])
            .status()
            .expect("git init");
        assert!(status.success());

        let readme = repo.join("README.md");
        write(&readme, "# Test Repo\n");

        let status = git_cmd(&repo)
            .args(["add", "README.md"])
            .status()
            .expect("git add");
        assert!(status.success());

        let status = git_cmd(&repo)
            .args(["commit", "-m", "initial commit"])
            .status()
            .expect("git commit");
        assert!(status.success());

        let prompt = cwd.join("p.md");
        write(&prompt, "fix tones\n");

        let cfg = cwd.join("v2.toml");
        write(&cfg, &v2_config(&wt_root, &state_dir, &scratch, pi_command));

        Fix {
            cwd,
            bin,
            cfg,
            repo,
            wt_root,
            state_dir,
            scratch,
            prompt,
            tmp,
        }
    }

    fn cmd(&self) -> Command {
        self.cmd_with_config(&self.cfg)
    }

    fn cmd_with_config(&self, cfg: &Path) -> Command {
        let mut cmd = Command::cargo_bin("weir").expect("binary built");
        cmd.timeout(Duration::from_secs(60));
        for var in PINNED_VARS {
            cmd.env_remove(var);
        }
        for (k, v) in GIT_ENV_VARS {
            cmd.env(k, v);
        }
        let sys_path = std::env::var("PATH").unwrap_or_default();
        cmd.current_dir(&self.cwd)
            .env("HOME", &self.cwd)
            .env("PATH", format!("{}:{sys_path}", self.bin.display()))
            .arg("--config")
            .arg(cfg);
        cmd
    }

    fn assert_no_side_effects(&self) {
        if self.wt_root.exists() {
            let entries = std::fs::read_dir(&self.wt_root).expect("read wt_root");
            let subdirs: Vec<_> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .collect();
            assert!(
                subdirs.is_empty(),
                "found subdirectories under wt_root: {subdirs:?}"
            );
        }
        let out = git_cmd(&self.repo)
            .args(["branch", "--list", "wt/*"])
            .output()
            .expect("git branch --list");
        let branches = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(
            branches.is_empty(),
            "found wt/* branches in repo: {branches}"
        );
        let ledger = self.state_dir.join("ledger.jsonl");
        assert!(!ledger.exists(), "ledger file {ledger:?} should not exist");
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[test]
fn edit_produces_one_file_diff() {
    let fix = Fix::new();
    let mut cmd = fix.cmd();
    cmd.env("FAKE_WORKER_MODE", "edit");
    cmd.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--json",
    ]);
    let out = cmd.output().expect("output");
    assert_eq!(out.status.code(), Some(0));

    let v = json_of(&out);
    assert_eq!(v["schema"], "weir.task/1");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["exit"], 0);
    assert_eq!(v["diff"]["files"], 1);
    assert_eq!(v["diff"]["insertions"], 1);

    let id = v["id"].as_str().expect("id is string");
    assert!(
        validate_task_id(id),
        "id {id:?} does not match ^\\d{{8}}-[a-z0-9-]+-[0-9a-f]{{4}}$"
    );
    assert_eq!(v["branch"], format!("wt/{id}"));

    let wt_str = v["worktree"].as_str().expect("worktree is string");
    let wt_path = PathBuf::from(wt_str);
    assert!(wt_path.exists(), "worktree dir does not exist: {wt_path:?}");
    let canonical_wt = wt_path.canonicalize().unwrap();
    let canonical_wt_root = fix.wt_root.canonicalize().unwrap();
    assert!(
        canonical_wt.starts_with(&canonical_wt_root),
        "worktree {canonical_wt:?} is not under wt_root {canonical_wt_root:?}"
    );
    assert!(
        wt_path.join("edited.txt").exists(),
        "edited.txt does not exist in worktree"
    );

    let patch_str = v["diff"]["patch"].as_str().expect("patch is string");
    let patch_path = PathBuf::from(patch_str);
    assert!(
        patch_path.exists(),
        "patch file does not exist: {patch_path:?}"
    );
    let patch_content = std::fs::read_to_string(&patch_path).expect("read patch");
    assert!(
        patch_content.contains("edited.txt"),
        "patch file does not contain edited.txt: {patch_content}"
    );

    assert_eq!(v["attempts"][0]["replica"], "a");
    assert_eq!(v["cleanup"], "kept");

    let prompt_copy = fix.state_dir.join("tasks").join(id).join("prompt.md");
    assert!(
        prompt_copy.exists(),
        "prompt copy does not exist at {prompt_copy:?}"
    );
    let perms = std::fs::metadata(&prompt_copy).unwrap().permissions();
    assert_eq!(
        perms.mode() & 0o777,
        0o600,
        "prompt file mode should be 0600"
    );

    let obj = v.as_object().expect("record must be a JSON object");
    let actual_keys: BTreeSet<&str> = obj.keys().map(|k| k.as_str()).collect();
    let expected_keys: BTreeSet<&str> = TASK_RECORD_KEYS.iter().copied().collect();
    assert_eq!(
        actual_keys, expected_keys,
        "task record key set does not match schema"
    );
}

#[test]
fn passing_and_failing_checks() {
    let fix = Fix::new();
    let jail_log_path = fix.cwd.join("jail.log");

    // Part 1: --check has-edit --check fail -> exit 10, status check_failed
    let mut cmd = fix.cmd();
    cmd.env("FAKE_WORKER_MODE", "edit");
    cmd.env("FAKE_JAIL_LOG", &jail_log_path);
    cmd.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--check",
        "has-edit",
        "--check",
        "fail",
        "--json",
    ]);
    let out = cmd.output().expect("output");
    assert_eq!(out.status.code(), Some(10));

    let v = json_of(&out);
    assert_eq!(v["status"], "check_failed");
    assert_eq!(v["exit"], 10);

    let checks = v["checks"].as_array().expect("checks array");
    assert_eq!(checks.len(), 2);
    assert_eq!(checks[0]["name"], "has-edit");
    assert_eq!(checks[0]["exit"], 0);
    assert_eq!(checks[1]["name"], "fail");
    assert_eq!(checks[1]["exit"], 1);

    let fail_tail = checks[1]["tail"].as_str().expect("fail check tail");
    assert!(
        fail_tail.contains("boom"),
        "fail tail does not contain boom: {fail_tail}"
    );

    let jail_log = std::fs::read_to_string(&jail_log_path).expect("read jail log");
    let wt = v["worktree"].as_str().expect("worktree string");
    let has_edit_line = jail_log
        .lines()
        .find(|l| l.contains("-- test -f edited.txt"))
        .expect("line with '-- test -f edited.txt' not found in jail log");
    let first_field = has_edit_line.split_whitespace().next().unwrap();
    assert_eq!(
        first_field, wt,
        "first field in jail log line {has_edit_line:?} does not match worktree {wt}"
    );

    // Part 2: --check pass alone -> exit 0 status ok
    let mut cmd2 = fix.cmd();
    cmd2.env("FAKE_WORKER_MODE", "edit");
    cmd2.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--check",
        "pass",
        "--json",
    ]);
    let out2 = cmd2.output().expect("output");
    assert_eq!(out2.status.code(), Some(0));

    let v2 = json_of(&out2);
    assert_eq!(v2["status"], "ok");
    assert_eq!(v2["exit"], 0);
    assert_eq!(v2["checks"][0]["name"], "pass");
    assert_eq!(v2["checks"][0]["exit"], 0);
}

#[test]
fn ledger_round_trips_through_show() {
    let fix = Fix::new();
    let mut cmd = fix.cmd();
    cmd.env("FAKE_WORKER_MODE", "edit");
    cmd.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--json",
    ]);
    let out = cmd.output().expect("output");
    assert_eq!(out.status.code(), Some(0));

    let run_val = json_of(&out);
    let id = run_val["id"].as_str().expect("id is string");

    // task show ID --json
    let mut show_cmd = fix.cmd();
    show_cmd.args(["task", "show", id, "--json"]);
    let show_out = show_cmd.output().expect("task show output");
    assert_eq!(show_out.status.code(), Some(0));
    let show_val = json_of(&show_out);
    assert_eq!(show_val, run_val, "show JSON does not match run JSON");

    // Check ledger.jsonl
    let ledger_path = fix.state_dir.join("ledger.jsonl");
    assert!(ledger_path.exists(), "ledger.jsonl does not exist");
    let ledger_content = std::fs::read_to_string(&ledger_path).expect("read ledger");
    let found = ledger_content
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .any(|v| v == run_val);
    assert!(
        found,
        "ledger.jsonl did not contain a line matching run_val"
    );

    // task show ID --patch
    let mut patch_cmd = fix.cmd();
    patch_cmd.args(["task", "show", id, "--patch"]);
    let patch_out = patch_cmd.output().expect("task show --patch output");
    assert_eq!(patch_out.status.code(), Some(0));
    let patch_stdout = String::from_utf8_lossy(&patch_out.stdout).to_string();

    let patch_file_path = run_val["diff"]["patch"]
        .as_str()
        .expect("patch path string");
    let patch_file_content = std::fs::read_to_string(patch_file_path).expect("read patch file");
    assert_eq!(patch_stdout, patch_file_content);
    assert!(!patch_file_content.is_empty());
}

#[test]
fn list_shows_the_task() {
    let fix = Fix::new();

    let prompt1 = fix.cwd.join("p1.md");
    write(&prompt1, "first task\n");
    let mut cmd1 = fix.cmd();
    cmd1.env("FAKE_WORKER_MODE", "edit");
    cmd1.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p1.md",
        "--json",
    ]);
    let out1 = cmd1.output().expect("task 1 output");
    assert_eq!(out1.status.code(), Some(0));
    let id1 = json_of(&out1)["id"].as_str().expect("id1").to_string();

    let prompt2 = fix.cwd.join("p2.md");
    write(&prompt2, "second task\n");
    let mut cmd2 = fix.cmd();
    cmd2.env("FAKE_WORKER_MODE", "edit");
    cmd2.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p2.md",
        "--json",
    ]);
    let out2 = cmd2.output().expect("task 2 output");
    assert_eq!(out2.status.code(), Some(0));
    let id2 = json_of(&out2)["id"].as_str().expect("id2").to_string();

    // task list --json
    let mut list_cmd = fix.cmd();
    list_cmd.args(["task", "list", "--json"]);
    let list_out = list_cmd.output().expect("task list output");
    assert_eq!(list_out.status.code(), Some(0));
    let list_val = json_of(&list_out);
    let list_arr = list_val.as_array().expect("list must be JSON array");
    let ids: Vec<&str> = list_arr
        .iter()
        .map(|item| item["id"].as_str().expect("id is string"))
        .collect();
    assert!(ids.contains(&id1.as_str()), "missing id1 {id1} in {ids:?}");
    assert!(ids.contains(&id2.as_str()), "missing id2 {id2} in {ids:?}");

    // task list --since 1h --json
    let mut since_cmd = fix.cmd();
    since_cmd.args(["task", "list", "--since", "1h", "--json"]);
    let since_out = since_cmd.output().expect("task list --since output");
    assert_eq!(since_out.status.code(), Some(0));
    let since_val = json_of(&since_out);
    let since_arr = since_val.as_array().expect("since list must be JSON array");
    let since_ids: Vec<&str> = since_arr
        .iter()
        .map(|item| item["id"].as_str().expect("id is string"))
        .collect();
    assert!(
        since_ids.contains(&id1.as_str()),
        "missing id1 {id1} in --since list: {since_ids:?}"
    );
    assert!(
        since_ids.contains(&id2.as_str()),
        "missing id2 {id2} in --since list: {since_ids:?}"
    );
}

#[test]
fn clean_removes_worktree_and_branch() {
    let fix = Fix::new();
    let mut cmd = fix.cmd();
    cmd.env("FAKE_WORKER_MODE", "edit");
    cmd.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--json",
    ]);
    let out = cmd.output().expect("output");
    assert_eq!(out.status.code(), Some(0));

    let run_val = json_of(&out);
    let id = run_val["id"].as_str().expect("id").to_string();
    let wt = PathBuf::from(run_val["worktree"].as_str().expect("worktree"));
    let patch = PathBuf::from(run_val["diff"]["patch"].as_str().expect("patch"));
    assert!(wt.exists(), "worktree should exist before clean");
    assert!(patch.exists(), "patch file should exist before clean");

    // task clean ID --json
    let mut clean_cmd = fix.cmd();
    clean_cmd.args(["task", "clean", &id, "--json"]);
    let clean_out = clean_cmd.output().expect("clean output");
    assert_eq!(clean_out.status.code(), Some(0));
    let clean_val = json_of(&clean_out);
    let removed = clean_val["removed"]
        .as_array()
        .expect("removed array in clean output");
    assert!(
        removed.iter().any(|item| item.as_str() == Some(&id)),
        "removed array {removed:?} does not contain {id}"
    );

    // worktree dir gone
    assert!(!wt.exists(), "worktree dir should be gone: {wt:?}");

    // git branch --list wt/<id> empty
    let branch_out = git_cmd(&fix.repo)
        .args(["branch", "--list", &format!("wt/{id}")])
        .output()
        .expect("git branch");
    assert!(
        String::from_utf8_lossy(&branch_out.stdout)
            .trim()
            .is_empty(),
        "branch wt/{id} should be deleted"
    );

    // task show ID --json has cleanup == "removed"
    let mut show_cmd = fix.cmd();
    show_cmd.args(["task", "show", &id, "--json"]);
    let show_out = show_cmd.output().expect("show output");
    assert_eq!(show_out.status.code(), Some(0));
    let show_val = json_of(&show_out);
    assert_eq!(show_val["cleanup"], "removed");

    // patch file still exists
    assert!(
        patch.exists(),
        "patch file {patch:?} should still exist after clean"
    );
}

#[test]
fn clean_after_worktree_dir_was_deleted() {
    let fix = Fix::new();
    let mut cmd = fix.cmd();
    cmd.env("FAKE_WORKER_MODE", "edit");
    cmd.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--json",
    ]);
    let out = cmd.output().expect("output");
    assert_eq!(out.status.code(), Some(0));

    let run_val = json_of(&out);
    let id = run_val["id"].as_str().expect("id").to_string();
    let wt = PathBuf::from(run_val["worktree"].as_str().expect("worktree"));
    assert!(wt.exists(), "worktree should exist before simulated rm -rf");

    // Delete the worktree directory, simulating a user who rm -rf'd it
    std::fs::remove_dir_all(&wt).expect("remove_dir_all worktree");
    assert!(!wt.exists(), "worktree dir should be gone after removal");

    // task clean ID --json
    let mut clean_cmd = fix.cmd();
    clean_cmd.args(["task", "clean", &id, "--json"]);
    let clean_out = clean_cmd.output().expect("clean output");
    assert_eq!(clean_out.status.code(), Some(0));
    let clean_val = json_of(&clean_out);
    let removed = clean_val["removed"]
        .as_array()
        .expect("removed array in clean output");
    assert!(
        removed.iter().any(|item| item.as_str() == Some(&id)),
        "removed array {removed:?} does not contain {id}"
    );

    // git branch --list wt/<id> empty
    let branch_out = git_cmd(&fix.repo)
        .args(["branch", "--list", &format!("wt/{id}")])
        .output()
        .expect("git branch");
    assert!(
        String::from_utf8_lossy(&branch_out.stdout)
            .trim()
            .is_empty(),
        "branch wt/{id} should be deleted"
    );

    // task show ID --json has cleanup == "removed"
    let mut show_cmd = fix.cmd();
    show_cmd.args(["task", "show", &id, "--json"]);
    let show_out = show_cmd.output().expect("show output");
    assert_eq!(show_out.status.code(), Some(0));
    let show_val = json_of(&show_out);
    assert_eq!(show_val["cleanup"], "removed");
}

#[test]
fn unknown_check_or_disallowed_command_is_refused_before_side_effects() {
    let fix = Fix::new();

    // (1) --check nope -> exit 2
    let mut cmd1 = fix.cmd();
    cmd1.env("FAKE_WORKER_MODE", "edit");
    cmd1.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--check",
        "nope",
        "--json",
    ]);
    let out1 = cmd1.output().expect("output 1");
    assert_eq!(out1.status.code(), Some(2));
    fix.assert_no_side_effects();

    // (2) a config with [worker.pi] command = "sh" -> exit 2
    let fix_bad_cmd = Fix::with_pi_command("sh");
    let mut cmd2 = fix_bad_cmd.cmd();
    cmd2.env("FAKE_WORKER_MODE", "edit");
    cmd2.args([
        "task",
        "run",
        "--repo",
        fix_bad_cmd.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--json",
    ]);
    let out2 = cmd2.output().expect("output 2");
    assert_eq!(out2.status.code(), Some(2));
    fix_bad_cmd.assert_no_side_effects();
}

#[test]
fn worker_jail_refusal_is_exit_5() {
    let fix = Fix::new();
    let mut cmd = fix.cmd();
    cmd.env("FAKE_WORKER_MODE", "jail");
    cmd.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--check",
        "pass",
        "--json",
    ]);
    let out = cmd.output().expect("output");
    assert_eq!(out.status.code(), Some(5));

    let v = json_of(&out);
    assert_eq!(v["status"], "jail_refused");
    assert_eq!(v["exit"], 5);
    let checks = v["checks"].as_array().expect("checks must be array");
    assert!(
        checks.is_empty(),
        "checks should be empty [] when jail refused"
    );

    let ledger = fix.state_dir.join("ledger.jsonl");
    assert!(ledger.exists(), "ledger file should exist");
    let content = std::fs::read_to_string(&ledger).expect("read ledger");
    let id = v["id"].as_str().expect("id is string");
    assert!(
        content.lines().any(|l| l.contains(id)),
        "ledger line does not exist for task id {id}"
    );
}

#[test]
fn agy_quota_is_exit_7() {
    let fix = Fix::new();
    let mut cmd = fix.cmd();
    cmd.env("FAKE_WORKER_MODE", "quota");
    cmd.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "agy",
        "--prompt-file",
        "p.md",
        "--json",
    ]);
    let out = cmd.output().expect("output");
    assert_eq!(out.status.code(), Some(7));

    let v = json_of(&out);
    assert_eq!(v["status"], "quota");
    assert_eq!(v["exit"], 7);
    assert_eq!(v["worker"], "agy");
    assert_eq!(v["model"], "test-model");
    assert!(
        v["replica"].is_null(),
        "replica should be null for agy worker"
    );
}

#[test]
fn check_jail_refusal_fails_the_check() {
    let fix = Fix::new();
    let mut cmd = fix.cmd();
    cmd.env("FAKE_WORKER_MODE", "edit");
    cmd.env("FAKE_JAIL_REFUSE", "1");
    cmd.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--check",
        "pass",
        "--json",
    ]);
    let out = cmd.output().expect("output");
    assert_eq!(out.status.code(), Some(10));

    let v = json_of(&out);
    assert_eq!(v["status"], "check_failed");
    assert_eq!(v["exit"], 10);
    assert_eq!(v["checks"][0]["name"], "pass");
    assert_eq!(v["checks"][0]["exit"], 5);
}

#[test]
fn wrong_flag_for_worker_is_usage_error() {
    let fix = Fix::new();
    let mut cmd = fix.cmd();
    cmd.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "agy",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--json",
    ]);
    let out = cmd.output().expect("output");
    assert_eq!(out.status.code(), Some(2));
    fix.assert_no_side_effects();
}

#[test]
fn cleanup_on_success_removes() {
    let fix = Fix::new();

    // Success with edit mode -> cleanup=="removed", worktree gone, branch gone
    let mut cmd1 = fix.cmd();
    cmd1.env("FAKE_WORKER_MODE", "edit");
    cmd1.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--cleanup",
        "on-success",
        "--json",
    ]);
    let out1 = cmd1.output().expect("output 1");
    assert_eq!(out1.status.code(), Some(0));

    let v1 = json_of(&out1);
    assert_eq!(v1["status"], "ok");
    assert_eq!(v1["cleanup"], "removed");
    let wt1 = PathBuf::from(v1["worktree"].as_str().expect("worktree string"));
    assert!(!wt1.exists(), "worktree should be removed on success");
    let id1 = v1["id"].as_str().expect("id string");
    let branch_out1 = git_cmd(&fix.repo)
        .args(["branch", "--list", &format!("wt/{id1}")])
        .output()
        .expect("git branch");
    assert!(
        String::from_utf8_lossy(&branch_out1.stdout)
            .trim()
            .is_empty(),
        "branch wt/{id1} should be removed"
    );

    // Failure with empty mode (exit 3) -> cleanup=="kept"
    let prompt2 = fix.cwd.join("p2.md");
    write(&prompt2, "second task\n");
    let mut cmd2 = fix.cmd();
    cmd2.env("FAKE_WORKER_MODE", "empty");
    cmd2.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p2.md",
        "--cleanup",
        "on-success",
        "--json",
    ]);
    let out2 = cmd2.output().expect("output 2");
    assert_eq!(out2.status.code(), Some(3));

    let v2 = json_of(&out2);
    assert_eq!(v2["status"], "empty");
    assert_eq!(v2["exit"], 3);
    assert_eq!(v2["cleanup"], "kept");
    let wt2 = PathBuf::from(v2["worktree"].as_str().expect("worktree string"));
    assert!(
        wt2.exists(),
        "worktree should be kept on failure when policy is on-success"
    );
}

// ── Brief C tests ────────────────────────────────────────────────────────────

#[test]
fn clean_refuses_main_develop_and_outside_wt_root() {
    use std::io::Write;

    let fix = Fix::new();
    let mut cmd = fix.cmd();
    cmd.env("FAKE_WORKER_MODE", "edit");
    cmd.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--json",
    ]);
    let out = cmd.output().expect("output");
    assert_eq!(out.status.code(), Some(0));

    let real_val = json_of(&out);
    let real_wt_str = real_val["worktree"].as_str().expect("worktree string");
    let real_wt = PathBuf::from(real_wt_str);
    assert!(real_wt.exists(), "real task worktree should exist");

    let ledger_path = fix.state_dir.join("ledger.jsonl");
    assert!(ledger_path.exists(), "ledger.jsonl should exist");

    // (a) branch "main"
    let mut rec_a = real_val.clone();
    rec_a["id"] = Value::String("forged-main".to_string());
    rec_a["branch"] = Value::String("main".to_string());
    rec_a["worktree"] = Value::String(fix.wt_root.join("forged-main").display().to_string());

    // (b) branch "develop"
    let mut rec_b = real_val.clone();
    rec_b["id"] = Value::String("forged-develop".to_string());
    rec_b["branch"] = Value::String("develop".to_string());
    rec_b["worktree"] = Value::String(fix.wt_root.join("forged-develop").display().to_string());

    // (c) worktree = the repo itself (outside wt_root)
    let mut rec_c = real_val.clone();
    rec_c["id"] = Value::String("forged-outside".to_string());
    rec_c["branch"] = Value::String("wt/forged-outside".to_string());
    rec_c["worktree"] = Value::String(fix.repo.display().to_string());

    // (d) worktree with .. escape outside wt_root (<wt_root>/../outside-escape)
    let escape_dir = fix.cwd.join("outside-escape");
    std::fs::create_dir_all(&escape_dir).expect("create outside-escape dir");
    let mut rec_d = real_val.clone();
    rec_d["id"] = Value::String("forged-escape".to_string());
    rec_d["branch"] = Value::String("wt/forged-escape".to_string());
    rec_d["worktree"] = Value::String(fix.wt_root.join("../outside-escape").display().to_string());

    // (e) branch wt/main (protected name under wt/)
    let mut rec_e = real_val.clone();
    rec_e["id"] = Value::String("main".to_string());
    rec_e["branch"] = Value::String("wt/main".to_string());
    rec_e["worktree"] = Value::String(fix.wt_root.join("main").display().to_string());

    // (f) branch wt/develop (protected name under wt/)
    let mut rec_f = real_val.clone();
    rec_f["id"] = Value::String("develop".to_string());
    rec_f["branch"] = Value::String("wt/develop".to_string());
    rec_f["worktree"] = Value::String(fix.wt_root.join("develop").display().to_string());

    // Create the branches in the repo so we can verify clean does not delete them
    for b in [
        "develop",
        "wt/main",
        "wt/develop",
        "wt/forged-outside",
        "wt/forged-escape",
    ] {
        let status = git_cmd(&fix.repo)
            .args(["branch", b])
            .status()
            .expect("git branch");
        assert!(status.success());
    }

    // Append forged weir.task/1 lines to state/ledger.jsonl
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&ledger_path)
        .expect("open ledger.jsonl for appending");
    for rec in [&rec_a, &rec_b, &rec_c, &rec_d, &rec_e, &rec_f] {
        writeln!(file, "{}", serde_json::to_string(rec).unwrap()).expect("write forged line");
    }
    drop(file);

    // task clean <forged-id> --json must exit 2 for each
    for forged_id in [
        "forged-main",
        "forged-develop",
        "forged-outside",
        "forged-escape",
        "main",
        "develop",
    ] {
        let mut clean_cmd = fix.cmd();
        clean_cmd.args(["task", "clean", forged_id, "--json"]);
        let clean_out = clean_cmd.output().expect("task clean output");
        assert_eq!(
            clean_out.status.code(),
            Some(2),
            "cleaning {forged_id} should exit 2"
        );
        let clean_val = json_of(&clean_out);
        let skipped = clean_val["skipped"]
            .as_array()
            .expect("skipped should be an array");
        assert!(
            skipped.iter().any(|item| item["id"] == forged_id),
            "skipped array should contain {forged_id}"
        );
    }

    // afterwards the repo's refused branches still exist
    for branch_name in [
        "main",
        "develop",
        "wt/main",
        "wt/develop",
        "wt/forged-outside",
        "wt/forged-escape",
    ] {
        let branch_out = git_cmd(&fix.repo)
            .args(["branch", "--list", branch_name])
            .output()
            .expect("git branch");
        let branch_str = String::from_utf8_lossy(&branch_out.stdout);
        assert!(
            branch_str.contains(branch_name),
            "{branch_name} branch should still exist: {branch_str}"
        );
    }

    // the repo dir and outside escape dir still exist
    assert!(fix.repo.exists(), "repo dir should still exist");
    assert!(escape_dir.exists(), "outside escape dir should still exist");

    // the real task's worktree still exists
    assert!(
        real_wt.exists(),
        "real task worktree should still exist at {real_wt:?}"
    );
}

#[test]
fn replica_auto_picks_a_slot() {
    let fix = Fix::new();

    // First run: --replica auto
    let mut cmd1 = fix.cmd();
    cmd1.env("FAKE_WORKER_MODE", "args");
    cmd1.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "auto",
        "--prompt-file",
        "p.md",
        "--json",
    ]);
    let out1 = cmd1.output().expect("output 1");
    assert_eq!(out1.status.code(), Some(0));

    let v1 = json_of(&out1);
    assert_eq!(v1["status"], "ok");
    assert_eq!(v1["exit"], 0);
    let r1 = v1["attempts"][0]["replica"]
        .as_str()
        .expect("attempts[0].replica should be string");
    assert!(
        r1 == "a" || r1 == "b",
        "attempts[0].replica must be 'a' or 'b', got {r1}"
    );

    let ans1 = v1["answer"].as_str().expect("answer should be string");
    let lines1: Vec<&str> = ans1.lines().collect();
    let b_idx1 = lines1
        .iter()
        .position(|&l| l == "-b")
        .expect("wrapper args should contain '-b'");
    assert_eq!(
        lines1.get(b_idx1 + 1).copied(),
        Some(r1),
        "arg after '-b' should match chosen replica {r1}"
    );

    // Second run: no --replica
    let prompt2 = fix.cwd.join("p2.md");
    write(&prompt2, "second prompt for auto replica\n");
    let mut cmd2 = fix.cmd();
    cmd2.env("FAKE_WORKER_MODE", "args");
    cmd2.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--prompt-file",
        "p2.md",
        "--json",
    ]);
    let out2 = cmd2.output().expect("output 2");
    assert_eq!(out2.status.code(), Some(0));

    let v2 = json_of(&out2);
    assert_eq!(v2["status"], "ok");
    assert_eq!(v2["exit"], 0);
    let r2 = v2["attempts"][0]["replica"]
        .as_str()
        .expect("attempts[0].replica should be string");
    assert!(
        r2 == "a" || r2 == "b",
        "attempts[0].replica must be 'a' or 'b', got {r2}"
    );

    let ans2 = v2["answer"].as_str().expect("answer should be string");
    let lines2: Vec<&str> = ans2.lines().collect();
    let b_idx2 = lines2
        .iter()
        .position(|&l| l == "-b")
        .expect("wrapper args should contain '-b'");
    assert_eq!(
        lines2.get(b_idx2 + 1).copied(),
        Some(r2),
        "arg after '-b' should match chosen replica {r2}"
    );
}

#[test]
fn prompt_stdin_and_explicit_id() {
    let fix = Fix::new();

    // First run: --prompt-stdin with "hello" and --id my-task-1
    let mut cmd1 = fix.cmd();
    cmd1.env("FAKE_WORKER_MODE", "edit");
    cmd1.write_stdin(b"hello".to_vec());
    cmd1.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-stdin",
        "--id",
        "my-task-1",
        "--json",
    ]);
    let out1 = cmd1.output().expect("output 1");
    assert_eq!(out1.status.code(), Some(0));

    let v1 = json_of(&out1);
    assert_eq!(v1["id"], "my-task-1");
    assert_eq!(v1["branch"], "wt/my-task-1");

    let prompt_file = fix
        .state_dir
        .join("tasks")
        .join("my-task-1")
        .join("prompt.md");
    assert!(
        prompt_file.exists(),
        "prompt.md should exist at {prompt_file:?}"
    );
    let prompt_content = std::fs::read_to_string(&prompt_file).expect("read prompt.md");
    assert_eq!(prompt_content, "hello");

    let ledger_path = fix.state_dir.join("ledger.jsonl");
    assert!(ledger_path.exists(), "ledger.jsonl should exist");
    let ledger_lines_before = std::fs::read_to_string(&ledger_path)
        .expect("read ledger")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    assert_eq!(ledger_lines_before, 1);

    // Second run with the same --id -> exit 2 and no new ledger line
    let mut cmd2 = fix.cmd();
    cmd2.env("FAKE_WORKER_MODE", "edit");
    cmd2.write_stdin(b"hello again".to_vec());
    cmd2.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-stdin",
        "--id",
        "my-task-1",
        "--json",
    ]);
    let out2 = cmd2.output().expect("output 2");
    assert_eq!(out2.status.code(), Some(2));

    let ledger_lines_after = std::fs::read_to_string(&ledger_path)
        .expect("read ledger")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    assert_eq!(
        ledger_lines_after, ledger_lines_before,
        "second run with duplicate id must not append a ledger line"
    );

    // Third run: --id ../x -> exit 2, no side effects
    let mut cmd3 = fix.cmd();
    cmd3.env("FAKE_WORKER_MODE", "edit");
    cmd3.write_stdin(b"hello".to_vec());
    cmd3.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-stdin",
        "--id",
        "../x",
        "--json",
    ]);
    let out3 = cmd3.output().expect("output 3");
    assert_eq!(out3.status.code(), Some(2));

    let ledger_lines_final = std::fs::read_to_string(&ledger_path)
        .expect("read ledger")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    assert_eq!(
        ledger_lines_final, ledger_lines_before,
        "--id ../x must not append a ledger line"
    );

    assert!(
        !fix.state_dir.join("tasks").join("x").exists(),
        "tasks/x must not exist"
    );
    assert!(
        !fix.state_dir.join("x").exists(),
        "state_dir/x must not exist"
    );
    assert!(!fix.wt_root.join("x").exists(), "wt_root/x must not exist");
    assert!(!fix.cwd.join("x").exists(), "cwd/x must not exist");

    let branch_out = git_cmd(&fix.repo)
        .args(["branch", "--list", "*x*"])
        .output()
        .expect("git branch");
    let branches = String::from_utf8_lossy(&branch_out.stdout)
        .trim()
        .to_string();
    assert!(
        branches.is_empty(),
        "no branch matching *x* should exist, got: {branches}"
    );
}

#[test]
fn check_cwd_runs_in_subdir() {
    let fix = Fix::new();

    // Commit sub/ directory to repo
    let sub_dir = fix.repo.join("sub");
    std::fs::create_dir_all(&sub_dir).expect("create sub dir");
    write(&sub_dir.join(".gitkeep"), "");
    let status = git_cmd(&fix.repo)
        .args(["add", "sub"])
        .status()
        .expect("git add sub");
    assert!(status.success());
    let status = git_cmd(&fix.repo)
        .args(["commit", "-m", "add sub directory"])
        .status()
        .expect("git commit sub");
    assert!(status.success());

    // Config with extra [check.in-sub] cwd = "sub", cmd = ["sh", "-c", "pwd"]
    let cfg_text = format!(
        "{}\n[check.in-sub]\ncwd = \"sub\"\ncmd = [\"sh\", \"-c\", \"pwd\"]\ntimeout = 30\n",
        v2_config(&fix.wt_root, &fix.state_dir, &fix.scratch, "pi-worker")
    );
    let custom_cfg = fix.cwd.join("v2_sub.toml");
    write(&custom_cfg, &cfg_text);

    let mut cmd = fix.cmd_with_config(&custom_cfg);
    cmd.env("FAKE_WORKER_MODE", "edit");
    cmd.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--check",
        "in-sub",
        "--json",
    ]);
    let out = cmd.output().expect("output");
    assert_eq!(out.status.code(), Some(0));

    let v = json_of(&out);
    assert_eq!(v["status"], "ok");
    assert_eq!(v["exit"], 0);

    let checks = v["checks"].as_array().expect("checks array");
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["name"], "in-sub");
    assert_eq!(checks[0]["exit"], 0);

    let tail = checks[0]["tail"].as_str().expect("tail is string");
    assert!(
        tail.trim().ends_with("/sub"),
        "check tail should end with /sub, got: {tail:?}"
    );
}

#[test]
fn list_since_and_human_output() {
    let fix = Fix::new();

    let mut cmd = fix.cmd();
    cmd.env("FAKE_WORKER_MODE", "edit");
    cmd.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--json",
    ]);
    let out = cmd.output().expect("output");
    assert_eq!(out.status.code(), Some(0));

    let run_val = json_of(&out);
    let id = run_val["id"].as_str().expect("id is string").to_string();

    // Sleep 2 seconds so task is older than 1s
    std::thread::sleep(Duration::from_secs(2));

    // task list --since 1s --json returns []
    let mut since_cmd = fix.cmd();
    since_cmd.args(["task", "list", "--since", "1s", "--json"]);
    let since_out = since_cmd.output().expect("task list --since output");
    assert_eq!(since_out.status.code(), Some(0));

    let since_val = json_of(&since_out);
    let since_arr = since_val
        .as_array()
        .expect("since output must be JSON array");
    assert!(
        since_arr.is_empty(),
        "task list --since 1s after 2s sleep should be [], got: {since_arr:?}"
    );

    // task list without --json prints a line containing the id
    let mut human_cmd = fix.cmd();
    human_cmd.args(["task", "list"]);
    let human_out = human_cmd.output().expect("task list human output");
    assert_eq!(human_out.status.code(), Some(0));

    let stdout = String::from_utf8_lossy(&human_out.stdout);
    assert!(
        stdout.lines().any(|l| l.contains(&id)),
        "task list human output should contain id {id}, got:\n{stdout}"
    );
}

#[test]
fn check_timeout_is_reported() {
    use std::time::Instant;

    let fix = Fix::new();

    // Extra [check.slow] cmd = ["sleep", "31.5"], timeout = 1
    let cfg_text = format!(
        "{}\n[check.slow]\ncmd = [\"sleep\", \"31.5\"]\ntimeout = 1\n",
        v2_config(&fix.wt_root, &fix.state_dir, &fix.scratch, "pi-worker")
    );
    let custom_cfg = fix.cwd.join("v2_slow.toml");
    write(&custom_cfg, &cfg_text);

    let start = Instant::now();
    let mut cmd = fix.cmd_with_config(&custom_cfg);
    cmd.env("FAKE_WORKER_MODE", "edit");
    cmd.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--check",
        "slow",
        "--json",
    ]);
    let out = cmd.output().expect("output");
    let elapsed = start.elapsed();

    assert_eq!(out.status.code(), Some(10));

    let v = json_of(&out);
    assert_eq!(v["status"], "check_failed");
    assert_eq!(v["exit"], 10);

    let checks = v["checks"].as_array().expect("checks array");
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["name"], "slow");
    assert_eq!(checks[0]["timed_out"], true);
    assert_eq!(checks[0]["exit"], 124);

    assert!(
        elapsed < Duration::from_secs(20),
        "whole run took {elapsed:?}, expected < 20 s"
    );

    // Afterwards pgrep -f "sleep 31.5" finds no process started by this test
    let pgrep_out = std::process::Command::new("pgrep")
        .args(["-f", "sleep 31.5"])
        .output()
        .expect("pgrep should run");
    let matched = String::from_utf8_lossy(&pgrep_out.stdout)
        .trim()
        .to_string();
    assert!(
        matched.is_empty(),
        "pgrep -f 'sleep 31.5' found lingering process(es): {matched}"
    );
}

#[test]
fn clean_merged_keeps_a_task_whose_branch_has_no_commits() {
    let fix = Fix::new();
    let mut cmd = fix.cmd();
    cmd.env("FAKE_WORKER_MODE", "edit");
    cmd.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--json",
    ]);
    let out = cmd.output().expect("output");
    assert_eq!(out.status.code(), Some(0));
    let run_val = json_of(&out);
    let id = run_val["id"].as_str().expect("id").to_string();
    let wt = PathBuf::from(run_val["worktree"].as_str().expect("worktree"));
    assert!(wt.exists());

    // The jailed worker cannot commit, so wt/<id> still equals its base: it is
    // an "ancestor of base" without being merged. --merged must not touch it.
    let mut clean_cmd = fix.cmd();
    clean_cmd.args(["task", "clean", "--merged", "--json"]);
    let clean_out = clean_cmd.output().expect("clean output");
    let clean_val = json_of(&clean_out);
    let removed = clean_val["removed"].as_array().expect("removed array");
    assert!(
        !removed.iter().any(|item| item.as_str() == Some(&id)),
        "--merged removed an unmerged task: {clean_val}"
    );
    assert!(wt.exists(), "worktree must survive clean --merged");
}

#[test]
fn explicit_id_main_is_a_usage_error() {
    let fix = Fix::new();
    let mut cmd = fix.cmd();
    cmd.env("FAKE_WORKER_MODE", "edit");
    cmd.args([
        "task",
        "run",
        "--repo",
        fix.repo.to_str().unwrap(),
        "--base",
        "main",
        "--worker",
        "pi",
        "--replica",
        "a",
        "--prompt-file",
        "p.md",
        "--id",
        "main",
        "--json",
    ]);
    let out = cmd.output().expect("output");
    assert_eq!(out.status.code(), Some(2));
}
