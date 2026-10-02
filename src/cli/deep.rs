//! CLI subcommand: `weir validate --deep` — environment checks for a v2 config.
//!
//! The plain `validate` subcommand only proves the TOML parses and is
//! internally consistent. `--deep` additionally proves that the machine the
//! config is used on can actually run it:
//!
//! | Check | What it proves |
//! |-------|----------------|
//! | `wrapper <command>` | the worker wrapper is a real executable on `$PATH` |
//! | `bwrap` | `<bwrap> --ro-bind / / true` exits 0 (sandbox works) |
//! | `wt_root <path>` | every configured worktree root is in agent-jail's `WT_ROOTS` array |
//! | `state_dir` | the state directory can be created |
//!
//! Every check always runs; the exit code is 1 if any check failed. The
//! programs used by the bwrap / agent-jail checks are overridable through
//! `WEIR_BWRAP` / `WEIR_AGENT_JAIL` so the tests can drive them with fixtures.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::json;

use crate::config::v2::ConfigV2;

/// Result of one deep check.
#[derive(Debug)]
pub struct CheckResult {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

impl CheckResult {
    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok: true,
            detail: detail.into(),
        }
    }

    fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok: false,
            detail: detail.into(),
        }
    }
}

/// Settings for the environment probes (test seams).
#[derive(Debug, Default)]
pub struct DeepEnv {
    /// `$PATH` used for the wrapper lookup. `None` ⇒ the process `$PATH`.
    pub path: Option<OsString>,
    /// Program used for the bwrap probe. `None` ⇒ `$WEIR_BWRAP` or `"bwrap"`.
    pub bwrap: Option<PathBuf>,
    /// Path to the agent-jail script. `None` ⇒ `$WEIR_AGENT_JAIL` or
    /// `~/.local/bin/agent-jail`.
    pub agent_jail: Option<PathBuf>,
    /// `$HOME` for `~` expansion inside the agent-jail script.
    pub home: Option<String>,
}

impl DeepEnv {
    /// Read the seams from the real environment.
    pub fn from_env() -> Self {
        Self {
            path: std::env::var_os("PATH"),
            bwrap: std::env::var_os("WEIR_BWRAP").map(PathBuf::from),
            agent_jail: std::env::var_os("WEIR_AGENT_JAIL").map(PathBuf::from),
            home: std::env::var("HOME").ok(),
        }
    }
}

/// Run every deep check against `cfg` (in the documented order).
///
/// `path` is the config file, used for the agent-jail fallback location in
/// error messages.
pub fn run(cfg: &ConfigV2, path: &Path, env: &DeepEnv, json: bool) -> bool {
    let results = run_checks(cfg, env);
    let all_ok = results.iter().all(|r| r.ok);

    if json {
        let checks: Vec<serde_json::Value> = results
            .iter()
            .map(|r| json!({"name": r.name, "ok": r.ok, "detail": r.detail}))
            .collect();
        println!(
            "{}",
            json!({"status": if all_ok { "ok" } else { "error" }, "checks": checks})
        );
    } else {
        for r in &results {
            let tag = if r.ok { "ok  " } else { "FAIL" };
            println!("{tag} {}: {}", r.name, r.detail);
        }
        if !all_ok {
            eprintln!("deep validation failed for {}", path.display());
        }
    }

    all_ok
}

/// Pure-ish check runner (no printing) — returns one result per check.
pub fn run_checks(cfg: &ConfigV2, env: &DeepEnv) -> Vec<CheckResult> {
    let mut results = Vec::new();
    results.extend(check_wrappers(cfg, env));
    results.push(check_bwrap(env));
    results.extend(check_wt_roots(cfg, env));
    results.push(check_state_dir(cfg));
    results
}

// ── 1. worker wrappers on PATH ────────────────────────────────────────────────

/// Each configured worker `command` must be a bare name found as an
/// executable file on `$PATH`.
fn check_wrappers(cfg: &ConfigV2, env: &DeepEnv) -> Vec<CheckResult> {
    let commands: Vec<&str> = [
        cfg.worker.pi.as_ref().map(|w| w.command.as_str()),
        cfg.worker.agy.as_ref().map(|w| w.command.as_str()),
    ]
    .into_iter()
    .flatten()
    .collect();

    commands
        .iter()
        .map(|cmd| match which_on_path(cmd, env.path.as_deref()) {
            Some(found) => CheckResult::ok(
                format!("wrapper {cmd}"),
                format!("found {}", found.display()),
            ),
            None => CheckResult::fail(
                format!("wrapper {cmd}"),
                format!(
                    "not found as an executable on PATH ({} on PATH)",
                    cmd_which_note(cmd)
                ),
            ),
        })
        .collect()
}

/// Explain why a command can never be found on PATH.
fn cmd_which_note(cmd: &str) -> String {
    if cmd.is_empty() {
        "command is empty".to_string()
    } else if cmd.contains('/') {
        "command contains '/'".to_string()
    } else {
        "no executable of this name".to_string()
    }
}

/// Locate `cmd` (a bare name, no `/`) as an executable file in `path`.
fn which_on_path(cmd: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    if cmd.is_empty() || cmd.contains('/') {
        return None;
    }
    let path = match path {
        Some(p) => p.to_os_string(),
        None => std::env::var_os("PATH")?,
    };
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(cmd);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Regular file with at least one owner execute bit.
fn is_executable_file(p: &Path) -> bool {
    match std::fs::metadata(p) {
        Ok(md) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                md.is_file() && md.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                md.is_file()
            }
        }
        Err(_) => false,
    }
}

// ── 2. bwrap ──────────────────────────────────────────────────────────────────

/// Run `<bwrap> --ro-bind / / true` and require exit 0.
fn check_bwrap(env: &DeepEnv) -> CheckResult {
    let pgm = env
        .bwrap
        .clone()
        .or_else(|| std::env::var_os("WEIR_BWRAP").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("bwrap"));

    // Plain `true`, resolved by bwrap via PATH inside the jail.
    let args: Vec<&str> = vec!["--ro-bind", "/", "/", "true"];

    match run_capturing(&pgm, &args) {
        Ok((Some(0), _, _)) => CheckResult::ok(
            "bwrap",
            format!("{} --ro-bind / / true exited 0", pgm.display()),
        ),
        Ok((code, _, stderr)) => CheckResult::fail(
            "bwrap",
            format!(
                "{} --ro-bind / / true exited {}{}",
                pgm.display(),
                code.map(|c| c.to_string())
                    .unwrap_or_else(|| "after timeout (killed)".into()),
                tail(&sanitize(&stderr))
            ),
        ),
        Err(e) => CheckResult::fail("bwrap", format!("cannot spawn {}: {e}", pgm.display())),
    }
}

// ── 3. wt_roots ⊆ agent-jail WT_ROOTS ────────────────────────────────────────

/// Every configured worktree root must appear in agent-jail's `WT_ROOTS`.
fn check_wt_roots(cfg: &ConfigV2, env: &DeepEnv) -> Vec<CheckResult> {
    let roots = match cfg.wt_roots_expanded() {
        Ok(r) => r,
        Err(e) => return vec![CheckResult::fail("agent-jail WT_ROOTS", e.to_string())],
    };

    let script_path = match agent_jail_path(env) {
        Some(p) => p,
        None => {
            return vec![CheckResult::fail(
                "agent-jail WT_ROOTS",
                "HOME is not set and WEIR_AGENT_JAIL is unset; cannot locate agent-jail",
            )]
        }
    };

    let script = match std::fs::read_to_string(&script_path) {
        Ok(s) => s,
        Err(e) => {
            return vec![CheckResult::fail(
                "agent-jail WT_ROOTS",
                format!("cannot read {}: {e}", script_path.display()),
            )]
        }
    };

    let home = env.home.clone().unwrap_or_default();
    let jail_roots = match parse_wt_roots(&script, &home) {
        Some(v) => v,
        None => {
            return vec![CheckResult::fail(
                "agent-jail WT_ROOTS",
                format!(
                    "no WT_ROOTS=( ... ) array found in {}",
                    script_path.display()
                ),
            )]
        }
    };

    roots
        .iter()
        .map(|root| {
            let want = trim_trailing_slash(&root.display().to_string());
            if jail_roots.iter().any(|r| trim_trailing_slash(r) == want) {
                CheckResult::ok(
                    format!("wt_root {want}"),
                    format!("present in {} WT_ROOTS", script_path.display()),
                )
            } else {
                let have = if jail_roots.is_empty() {
                    "(none)".to_string()
                } else {
                    jail_roots
                        .iter()
                        .map(|r| trim_trailing_slash(r))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                CheckResult::fail(
                    format!("wt_root {want}"),
                    format!(
                        "not in agent-jail WT_ROOTS at {} (agent-jail has: {have})",
                        script_path.display()
                    ),
                )
            }
        })
        .collect()
}

/// Where to read the agent-jail script from.
fn agent_jail_path(env: &DeepEnv) -> Option<PathBuf> {
    if let Some(p) = &env.agent_jail {
        return Some(p.clone());
    }
    match std::env::var_os("WEIR_AGENT_JAIL") {
        Some(p) => Some(PathBuf::from(p)),
        None => env
            .home
            .as_deref()
            .filter(|h| !h.is_empty())
            .map(|h| PathBuf::from(h).join(".local/bin/agent-jail")),
    }
}

/// Trim trailing slashes (keeping a lone "/" as "/").
fn trim_trailing_slash(s: &str) -> String {
    let t = s.trim_end_matches('/');
    if t.is_empty() {
        "/".to_string()
    } else {
        t.to_string()
    }
}

/// Parse the bash array assignment `WT_ROOTS=( ... )` out of a shell script.
///
/// Handles single- and multi-line assignments, quoted (single or double) or
/// bare elements, and expands a leading `~`, `$HOME` or `${HOME}` to `home`.
/// Lines whose first non-whitespace token is `#` are ignored, so a commented
/// out `# WT_ROOTS=(` neither matches nor contributes elements.
///
/// Returns `None` when the script has no `WT_ROOTS=(` assignment (or the value
/// is not closed with `)`), i.e. when the jail's roots cannot be determined.
pub fn parse_wt_roots(script: &str, home: &str) -> Option<Vec<String>> {
    // Scan line by line so a `WT_ROOTS=(` inside a comment never matches.
    let mut open = false; // inside a multi-line array value
    let mut found = false; // we have seen the assignment
    let mut collected = String::new();

    for line in script.lines() {
        if open {
            // Inside the array: strip any trailing comment, accumulate until ')'.
            let body = match line.find('#') {
                Some(hash) => &line[..hash],
                None => line,
            };
            match body.find(')') {
                Some(close) => {
                    collected.push_str(&body[..close]);
                    open = false;
                    break;
                }
                None => {
                    collected.push_str(body);
                    collected.push('\n');
                }
            }
            continue;
        }

        // Outside the array: whole-line comments never match.
        if line.trim_start().starts_with('#') {
            continue;
        }
        let col = match find_assignment(line) {
            Some(c) => c,
            None => continue,
        };
        found = true;
        let rest = &line[col + "WT_ROOTS=(".len()..];
        // Drop a comment on the opening line, then look for the closer.
        let rest = match rest.find('#') {
            Some(hash) => &rest[..hash],
            None => rest,
        };
        match rest.find(')') {
            Some(close) => {
                collected.push_str(&rest[..close]);
                break;
            }
            None => {
                collected.push_str(rest);
                collected.push('\n');
                open = true;
            }
        }
    }

    if !found {
        return None;
    }
    if open {
        // Reached EOF with an unterminated array.
        return None;
    }

    let els: Vec<String> = split_shell_words(&collected)
        .into_iter()
        .map(|e| expand_root(&e, home))
        .filter(|e| !e.is_empty())
        .collect();
    Some(els)
}

/// Column at which `line` assigns `WT_ROOTS=(`, or `None`.
///
/// The match must be the start of the assignment (only whitespace before it),
/// so `MY_WT_ROOTS=(` and `echo "WT_ROOTS=(" do not match.
fn find_assignment(line: &str) -> Option<usize> {
    let col = line.find("WT_ROOTS=(")?;
    if line[..col].chars().all(char::is_whitespace) {
        Some(col)
    } else {
        None
    }
}

/// Expand a leading `~`, `$HOME` or `${HOME}` in one `WT_ROOTS` element.
fn expand_root(raw: &str, home: &str) -> String {
    if let Some(rest) = raw.strip_prefix("${HOME}") {
        return format!("{home}{rest}");
    }
    if let Some(rest) = raw.strip_prefix("$HOME") {
        return format!("{home}{rest}");
    }
    if raw == "~" {
        return home.to_string();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return format!("{home}/{rest}");
    }
    raw.to_string()
}

/// Whitespace-split a string into shell words, dropping the outer quotes.
///
/// Inside single quotes only `'` is special; inside double quotes `\` escapes
/// the next character. Anything else is taken literally.
fn split_shell_words(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut had_content = false;
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                quoted = true;
                // single quotes: copy verbatim up to the closing quote
                while let Some(&c2) = chars.peek() {
                    if c2 == '\'' {
                        chars.next();
                        break;
                    }
                    cur.push(c2);
                    had_content = true;
                    chars.next();
                }
            }
            '"' => {
                quoted = true;
                while let Some(c2) = chars.next() {
                    if c2 == '"' {
                        break;
                    }
                    if c2 == '\\' {
                        if let Some(n) = chars.next() {
                            cur.push(n);
                            had_content = true;
                            continue;
                        }
                        break;
                    }
                    cur.push(c2);
                    had_content = true;
                }
            }
            c if c.is_whitespace() => {
                if had_content || quoted {
                    out.push(std::mem::take(&mut cur));
                    quoted = false;
                    had_content = false;
                }
            }
            c => {
                cur.push(c);
                had_content = true;
            }
        }
    }
    if had_content || quoted {
        out.push(cur);
    }
    out
}

// ── 4. state_dir ──────────────────────────────────────────────────────────────

/// `create_dir_all` on the expanded state dir must succeed.
fn check_state_dir(cfg: &ConfigV2) -> CheckResult {
    match cfg.state_dir_expanded() {
        Ok(dir) => match std::fs::create_dir_all(&dir) {
            Ok(()) => CheckResult::ok("state_dir", format!("{} ready", dir.display())),
            Err(e) => {
                CheckResult::fail("state_dir", format!("cannot create {}: {e}", dir.display()))
            }
        },
        Err(e) => CheckResult::fail("state_dir", e.to_string()),
    }
}

// ── process + text helpers ────────────────────────────────────────────────────

/// Spawn `pgm` with `args`, stdin null, and capture stdout/stderr.
///
/// Used for the bwrap probe, which gets the default time limit.
fn run_capturing(pgm: &Path, args: &[&str]) -> std::io::Result<(Option<i32>, String, String)> {
    run_capturing_with_timeout(pgm, args, std::time::Duration::from_secs(20))
}

/// Spawn `pgm`, kill it if it outlives `limit`, capture stdout/stderr.
///
/// The wait is bounded: a program that never writes and never exits would
/// otherwise hang `--deep` forever (the child keeps the pipes open). On
/// timeout the child is killed and reported with no exit code.
fn run_capturing_with_timeout(
    pgm: &Path,
    args: &[&str],
    limit: std::time::Duration,
) -> std::io::Result<(Option<i32>, String, String)> {
    // A script that was just written can briefly fail to exec with ETXTBSY
    // while a concurrently forked process still holds its write fd; retry.
    let mut attempts = 0;
    let mut child = loop {
        match Command::new(pgm)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && attempts < 20 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            other => break other?,
        }
    };

    // Take both pipes out of the child so they can be drained from helper
    // threads; dropping them closes the write end and unblocks the child.
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let out_h = std::thread::spawn(move || drain_pipe(&mut out));
    let err_h = std::thread::spawn(move || drain_pipe(&mut err));

    let deadline = std::time::Instant::now() + limit;
    let mut status = None;
    let mut timed_out = false;
    loop {
        match child.try_wait()? {
            Some(s) => {
                status = Some(s);
                break;
            }
            None if std::time::Instant::now() >= deadline => {
                timed_out = true;
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    drop(child);

    // A killed child may still hold the pipe ends open for a moment; drain
    // with a deadline so a hanging program cannot hang `--deep`.
    let stdout = join_deadline(out_h, deadline);
    let stderr = join_deadline(err_h, deadline + limit);
    Ok((
        if timed_out {
            None
        } else {
            status.and_then(|s| s.code())
        },
        stdout,
        stderr,
    ))
}

/// Join a drain thread, giving up (empty output) once `deadline` passes.
fn join_deadline(h: std::thread::JoinHandle<String>, deadline: std::time::Instant) -> String {
    while !h.is_finished() {
        if std::time::Instant::now() >= deadline {
            return String::new();
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    h.join().unwrap_or_default()
}

/// Read a pipe to EOF, capped at 64 KiB.
fn drain_pipe<R: Read>(pipe: &mut Option<R>) -> String {
    let Some(pipe) = pipe.as_mut() else {
        return String::new();
    };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < 64 * 1024 {
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// ` (stderr: …)` suffix with at most the last 200 chars of `s`.
fn tail(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(200);
    format!(" (stderr: {})", chars[start..].iter().collect::<String>())
}

/// Replace control characters (notably NUL from a binary file) with '?'.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() && c != '\n' { '?' } else { c })
        .collect()
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn parsed(text: &str) -> Vec<String> {
        parse_wt_roots(text, "/home/u").expect("should find array")
    }

    #[test]
    fn parses_single_line_array() {
        let script = "#!/bin/bash\nWT_ROOTS=(\"/home/u/wt-a\" \"/home/u/wt-b\")\nexec bwrap\n";
        assert_eq!(parsed(script), vec!["/home/u/wt-a", "/home/u/wt-b"]);
    }

    #[test]
    fn parses_multi_line_array() {
        let script = r#"#!/bin/bash
FOO=1
WT_ROOTS=(
  "/home/u/wt-a"
  '/home/u/wt-b'
  /srv/wt-c
)
exec bwrap
"#;
        assert_eq!(
            parsed(script),
            vec!["/home/u/wt-a", "/home/u/wt-b", "/srv/wt-c"]
        );
    }

    #[test]
    fn parses_single_quoted_and_bare_elements() {
        assert_eq!(
            parsed("WT_ROOTS=('/tmp/a' /tmp/b)"),
            vec!["/tmp/a", "/tmp/b"]
        );
    }

    #[test]
    fn expands_home_forms() {
        assert_eq!(
            parsed("WT_ROOTS=(\"$HOME/wt-a\" ${HOME}/wt-b ~/wt-c ~)"),
            vec!["/home/u/wt-a", "/home/u/wt-b", "/home/u/wt-c", "/home/u"]
        );
    }

    #[test]
    fn trims_nothing_but_keeps_elements_verbatim_otherwise() {
        // Trailing-slash normalisation happens at comparison time.
        assert_eq!(trim_trailing_slash("/a/b///"), "/a/b");
        assert_eq!(trim_trailing_slash("/"), "/");
        assert_eq!(trim_trailing_slash("/a"), "/a");
    }

    #[test]
    fn missing_array_returns_none() {
        assert!(parse_wt_roots("#!/bin/bash\necho hi\n", "/home/u").is_none());
        assert!(parse_wt_roots("", "/home/u").is_none());
        assert!(parse_wt_roots("WT_ROOTS=/tmp/a\n", "/home/u").is_none());
    }

    #[test]
    fn ignores_commented_out_lines() {
        // The real assignment is commented out => None.
        assert!(parse_wt_roots("# WT_ROOTS=(\"/tmp/a\")\necho hi\n", "/home/u").is_none());
        // A comment before a real assignment is ignored, its contents dropped.
        assert_eq!(
            parse_wt_roots("# WT_ROOTS=(\"/nope\")\nWT_ROOTS=(\"/tmp/a\")\n", "/home/u").unwrap(),
            vec!["/tmp/a"]
        );
        // A commented line *inside* the array contributes nothing.
        assert_eq!(
            parse_wt_roots("WT_ROOTS=(\n  \"/tmp/a\"\n  # \"/tmp/b\"\n)\n", "/home/u").unwrap(),
            vec!["/tmp/a"]
        );
    }

    #[test]
    fn empty_array_is_some_empty() {
        assert_eq!(
            parse_wt_roots("WT_ROOTS=()\n", "/home/u").unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn unterminated_array_returns_none() {
        assert!(parse_wt_roots("WT_ROOTS=(\n  \"/tmp/a\"\n", "/home/u").is_none());
    }

    #[test]
    fn does_not_match_substring_names() {
        // `MY_WT_ROOTS=(` must not be mistaken for the assignment.
        assert!(parse_wt_roots("MY_WT_ROOTS=(\"/tmp/a\")\n", "/home/u").is_none());
    }

    #[test]
    fn split_shell_words_cases() {
        assert_eq!(split_shell_words("a b\tc"), vec!["a", "b", "c"]);
        assert_eq!(split_shell_words("'a b' c"), vec!["a b", "c"]);
        assert_eq!(split_shell_words("\"a b\""), vec!["a b"]);
        assert_eq!(split_shell_words("  "), Vec::<String>::new());
        assert_eq!(split_shell_words("\"\""), vec![""]);
    }

    #[test]
    fn which_on_path_rejects_slashes_and_finds_scripts() {
        assert!(which_on_path("", None).is_none());
        assert!(which_on_path("a/b", None).is_none());
        assert!(which_on_path("/bin/sh", None).is_none());
    }

    /// Create an executable `#!/bin/sh …` script inside `dir`.
    /// Create an executable `#!/bin/sh …` script. Written to a temporary name
    /// first, then made executable and renamed into place: chmod-ing a script
    /// another thread may be spawning can otherwise fail with ETXTBSY.
    #[cfg(unix)]
    fn make_bin(dir: &Path, name: &str, body: &str) -> PathBuf {
        let script = dir.join(name);
        let tmp = dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&tmp).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmp, perms).unwrap();
        std::fs::rename(&tmp, &script).unwrap();
        script
    }

    #[cfg(unix)]
    #[test]
    fn which_on_path_finds_executables_in_the_given_path() {
        let dir = tempfile::tempdir().unwrap();
        let script = make_bin(dir.path(), "fake-worker", "exit 0");
        // A non-executable file with the right name is not a match.
        std::fs::write(dir.path().join("data-only"), b"x").unwrap();

        let p = dir.path().join("does-not-exist");
        let path = std::env::join_paths([dir.path().to_path_buf(), p.clone()]).unwrap();

        assert_eq!(
            which_on_path("fake-worker", Some(&path)).as_deref(),
            Some(script.as_path())
        );
        assert!(which_on_path("missing-worker", Some(&path)).is_none());
        assert!(which_on_path("data-only", Some(&path)).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn wrapper_check_uses_env_path() {
        let dir = tempfile::tempdir().unwrap();
        make_bin(dir.path(), "pi-worker", "exit 0");
        let cfg = ConfigV2::parse("version = 2\n[worker.pi]\ncommand=\"pi-worker\"\ntimeout=1\n")
            .unwrap();
        let env = DeepEnv {
            path: Some(dir.path().into()),
            bwrap: Some(PathBuf::from("/bin/true")),
            agent_jail: None,
            home: None,
        };
        let rs = check_wrappers(&cfg, &env);
        assert_eq!(rs.len(), 1);
        assert!(rs[0].ok, "{}", rs[0].detail);
        assert_eq!(rs[0].name, "wrapper pi-worker");

        // Empty PATH => FAIL naming the wrapper.
        let env2 = DeepEnv {
            path: Some(OsString::new()),
            ..env
        };
        let rs = check_wrappers(&cfg, &env2);
        assert!(!rs[0].ok);
        assert!(rs[0].name.contains("pi-worker"), "{}", rs[0].name);
    }

    #[cfg(unix)]
    #[test]
    fn bwrap_check_reports_spawn_failure_and_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();

        // Non-existent program => FAIL mentioning spawn.
        let env = DeepEnv {
            path: None,
            bwrap: Some(dir.path().join("no-such-bwrap")),
            agent_jail: None,
            home: None,
        };
        let r = check_bwrap(&env);
        assert!(!r.ok);
        assert!(r.detail.contains("cannot spawn"), "{}", r.detail);

        // A fixture that ignores its arguments and exits 0 => ok.
        let ok = make_bin(dir.path(), "ok-bwrap", "exit 0");
        let ok_env = DeepEnv {
            path: None,
            bwrap: Some(ok),
            agent_jail: None,
            home: None,
        };
        let r = check_bwrap(&ok_env);
        assert!(r.ok, "{}", r.detail);

        // Non-zero exit => FAIL with the exit code.
        let bad = make_bin(dir.path(), "bad-bwrap", "echo boom >&2; exit 1");
        let bad_env = DeepEnv {
            path: None,
            bwrap: Some(bad),
            agent_jail: None,
            home: None,
        };
        let r = check_bwrap(&bad_env);
        assert!(!r.ok);
        assert!(r.detail.contains("exited 1"), "{}", r.detail);
        assert!(r.detail.contains("boom"), "{}", r.detail);
    }

    #[cfg(unix)]
    #[test]
    fn hung_program_is_killed_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let fake = make_bin(dir.path(), "hanging-bwrap", "sleep 30");
        let (code, _, _) = run_capturing_with_timeout(
            &fake,
            &["--ro-bind", "/", "/", "true"],
            std::time::Duration::from_millis(300),
        )
        .unwrap();
        assert_eq!(code, None, "timeout must be reported as no exit code");

        // A program that exits quickly still reports its code.
        let quick = make_bin(dir.path(), "quick-bwrap", "echo hi; exit 3");
        let (code, out, _) =
            run_capturing_with_timeout(&quick, &["x"], std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(code, Some(3));
        assert_eq!(out.trim(), "hi");
    }

    #[test]
    fn sanitize_and_tail_helpers() {
        assert_eq!(sanitize("a\u{0}b"), "a?b");
        assert_eq!(tail(""), "");
        assert_eq!(tail("boom"), " (stderr: boom)");
        let long = format!("{}end", "x".repeat(300));
        let t = tail(&long);
        assert!(t.ends_with("end)"), "{t}");
        // " (stderr: " + the last 200 chars + the closing paren.
        assert_eq!(t.chars().count(), " (stderr: ".chars().count() + 201, "{t}");
        // The tail keeps the END of the message.
        assert!(t.ends_with("end)"));
    }
}
