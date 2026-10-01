//! Run one `[check.NAME]` under `agent-jail`.
//!
//! A check is a plain command from the config, executed inside the task's
//! worktree with `agent-jail` as its only supervisor: the program name is the
//! constant [`AGENT_JAIL_PROGRAM`], never taken from config, the CLI or the
//! environment. weir itself never runs the check command directly.
//!
//! The child gets its own process group, both streams are drained
//! concurrently (so a chatty check can never deadlock on a full pipe), and an
//! expired check is SIGTERMed as a *group* — grandchildren included — with a
//! SIGKILL 2 s later, so no orphaned `sleep` or compiler survives the run.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use tokio::process::Command;

use crate::task::ledger::CheckResult;

/// The only program this module can exec.
pub const AGENT_JAIL_PROGRAM: &str = "agent-jail";

/// Bytes retained per stream; beyond that the *head* of the stream is dropped.
const STREAM_CAP: usize = 1024 * 1024;
/// Number of trailing lines kept in [`CheckResult::tail`].
const TAIL_LINES: usize = 60;
/// Grace period between SIGTERM and SIGKILL when a check exceeds its timeout.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// Extra grace for the stream readers, so a grandchild that inherited a pipe
/// end and survived cannot hang the run.
const READER_GRACE: Duration = Duration::from_secs(2);
/// Exit code reported when weir's timeout killed the check.
const EXIT_TIMED_OUT: i32 = 124;
/// Exit code reported when the check could not even be spawned.
const EXIT_SPAWN_FAILED: i32 = 127;

/// argv (without the program) for one check:
///
/// ```text
/// [<worktree>, "--", cmd...]                                     check.cwd is None
/// [<worktree>, "--", "env", "-C", <worktree/cwd>, "--", cmd...]  check.cwd is Some
/// ```
///
/// The worktree path is passed through unchanged — it is the same string the
/// record carries, so what agent-jail sees is what `task show` reports.
pub fn check_argv(check: &crate::config::v2::Check, worktree: &Path) -> Vec<String> {
    let mut argv = vec![worktree.to_string_lossy().into_owned(), "--".to_string()];
    if let Some(cwd) = &check.cwd {
        argv.push("env".to_string());
        argv.push("-C".to_string());
        argv.push(cwd_path(worktree, cwd).to_string_lossy().into_owned());
        argv.push("--".to_string());
    }
    argv.extend(check.cmd.iter().cloned());
    argv
}

/// Join the config's `cwd` to the worktree. An absolute `cwd` is used as given;
/// a `..` component is rejected by config validation, so none reaches here.
fn cwd_path(worktree: &Path, cwd: &str) -> PathBuf {
    let rel = Path::new(cwd);
    if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        worktree.join(rel)
    }
}

/// Run one check to completion and report it. Never fails: a spawn error, a
/// timeout or a signal death all become a non-zero [`CheckResult::exit`], so
/// the caller can still run every remaining check.
pub async fn run_check(
    name: &str,
    check: &crate::config::v2::Check,
    worktree: &Path,
) -> CheckResult {
    run_check_with_program(name, check, worktree, Path::new(AGENT_JAIL_PROGRAM)).await
}

/// SIGINT and SIGTERM streams. Once `run_worker` has installed tokio's signal
/// handlers the default actions are gone for the rest of the process, so the
/// check phase has to listen itself or Ctrl-C would be silently ignored.
pub struct Interrupts {
    int: tokio::signal::unix::Signal,
    term: tokio::signal::unix::Signal,
}

impl Interrupts {
    /// `None` when the streams cannot be registered.
    pub fn install() -> Option<Self> {
        use tokio::signal::unix::{signal, SignalKind};
        Some(Self {
            int: signal(SignalKind::interrupt()).ok()?,
            term: signal(SignalKind::terminate()).ok()?,
        })
    }

    async fn recv(&mut self) -> i32 {
        tokio::select! {
            _ = self.int.recv() => libc::SIGINT,
            _ = self.term.recv() => libc::SIGTERM,
        }
    }
}

/// Wait for the next signal, or forever when there is nothing to listen to.
async fn next_signal(interrupts: &mut Option<&mut Interrupts>) -> i32 {
    match interrupts {
        Some(i) => i.recv().await,
        None => std::future::pending().await,
    }
}

/// [`run_check`] that also stops on SIGINT/SIGTERM: the check's process group
/// is killed (TERM, grace, KILL) and the signal number is returned beside the
/// result so the caller can abandon the remaining checks.
pub async fn run_check_interruptible(
    name: &str,
    check: &crate::config::v2::Check,
    worktree: &Path,
    interrupts: &mut Interrupts,
) -> (CheckResult, Option<i32>) {
    run_check_inner(
        name,
        check,
        worktree,
        Path::new(AGENT_JAIL_PROGRAM),
        Some(interrupts),
    )
    .await
}

/// [`run_check`] with the jail program injectable, so the unit tests can drive
/// a fixture instead of the real `agent-jail`. Production code only ever calls
/// [`run_check`], which passes the hard-coded [`AGENT_JAIL_PROGRAM`].
async fn run_check_with_program(
    name: &str,
    check: &crate::config::v2::Check,
    worktree: &Path,
    program: &Path,
) -> CheckResult {
    run_check_inner(name, check, worktree, program, None)
        .await
        .0
}

async fn run_check_inner(
    name: &str,
    check: &crate::config::v2::Check,
    worktree: &Path,
    program: &Path,
    mut interrupts: Option<&mut Interrupts>,
) -> (CheckResult, Option<i32>) {
    let started = Instant::now();
    let argv = check_argv(check, worktree);

    let mut cmd = Command::new(program);
    cmd.args(&argv);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Own process group, so the timeout can `killpg` the whole tree.
    cmd.process_group(0);
    // Backstop: never leave the check running if this future is dropped.
    cmd.kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            // No process exists: report it the way a shell does (127), with the
            // reason as the tail.
            let result = CheckResult {
                name: name.to_string(),
                exit: EXIT_SPAWN_FAILED,
                elapsed_ms: elapsed_ms(started),
                timed_out: false,
                tail: format!("cannot run {}: {e}", program.display()),
            };
            return (result, None);
        }
    };

    // pgid == child pid (`process_group(0)`); `id()` is Some until the child is
    // reaped, and 0 then simply means "nothing to signal".
    let pgid = child.id().unwrap_or(0) as i32;

    // Both streams are drained concurrently: a check writing more than the pipe
    // buffer into one of them must never block while we wait on the other.
    let out_task = child.stdout.take().map(read_to_end);
    let err_task = child.stderr.take().map(read_to_end);

    let deadline = tokio::time::sleep(Duration::from_secs(check.timeout.max(1)));
    tokio::pin!(deadline);

    // `ours` is set the moment WE signal the group: from then on a signal death
    // is our doing and must be reported as a timeout (124), while a status the
    // child produced before our first signal stays its own.
    let mut ours = false;
    let mut interrupted: Option<i32> = None;
    let status = tokio::select! {
        // The bias keeps a ready `wait()` winning over an already-elapsed
        // deadline, so a check that finished in time is never a timeout.
        biased;
        s = child.wait() => s,
        sig = next_signal(&mut interrupts) => {
            ours = true;
            interrupted = Some(sig);
            stop_group(&mut child, pgid).await
        }
        _ = deadline.as_mut() => {
            ours = true;
            stop_group(&mut child, pgid).await
        }
    };

    let timed_out = ours && interrupted.is_none() && signal_of(&status).is_some();
    let exit = match (&status, timed_out) {
        // Our SIGTERM/SIGKILL: the contract's timeout code, not `128 + sig`.
        (_, true) => EXIT_TIMED_OUT,
        // Weir itself was interrupted: report the conventional `128 + sig`.
        (_, false) if interrupted.is_some() => 128 + interrupted.unwrap_or(0),
        (Ok(s), false) => exit_code_of(s),
        // A `wait()` error means there is no status at all.
        (Err(_), false) => EXIT_SPAWN_FAILED,
    };

    let result = CheckResult {
        name: name.to_string(),
        exit,
        elapsed_ms: elapsed_ms(started),
        timed_out,
        tail: tail_text(&collect(out_task).await, &collect(err_task).await),
    };
    (result, interrupted)
}

/// SIGTERM the group, give it [`KILL_GRACE`], then SIGKILL, and reap the child.
async fn stop_group(child: &mut tokio::process::Child, pgid: i32) -> std::io::Result<ExitStatus> {
    kill_group(pgid, libc::SIGTERM);
    tokio::select! {
        // The status may still be the child's own if it exited on its own
        // account before the signal landed; the caller's `ours` flag keeps the
        // two apart.
        s = child.wait() => s,
        _ = tokio::time::sleep(KILL_GRACE) => {
            kill_group(pgid, libc::SIGKILL);
            child.wait().await
        }
    }
}

/// Drain one pipe to EOF on its own task, keeping a rolling window of the last
/// [`STREAM_CAP`] bytes. The readers must stay independent of the child wait —
/// that is what keeps a large output from deadlocking.
fn read_to_end<R>(stream: R) -> tokio::task::JoinHandle<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = stream;
        let mut kept: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8 * 1024];
        loop {
            match tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await {
                Ok(0) => break,
                Ok(n) => push_rolling(&mut kept, &buf[..n]),
                // A read error (child died mid-write): keep what we have.
                Err(_) => break,
            }
        }
        kept
    })
}

/// Join a reader, but never forever: once the group is gone, a grandchild that
/// still holds the pipe end must not stall the run. On timeout the reader's
/// buffered bytes are given up.
async fn collect(task: Option<tokio::task::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    let Some(task) = task else {
        return Vec::new();
    };
    match tokio::time::timeout(READER_GRACE, task).await {
        Ok(joined) => joined.unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Append `chunk`, dropping from the front whatever exceeds [`STREAM_CAP`] —
/// i.e. keep the *tail* of the stream.
fn push_rolling(buf: &mut Vec<u8>, chunk: &[u8]) {
    if chunk.len() >= STREAM_CAP {
        buf.clear();
        buf.extend_from_slice(&chunk[chunk.len() - STREAM_CAP..]);
        return;
    }
    buf.extend_from_slice(chunk);
    let excess = buf.len().saturating_sub(STREAM_CAP);
    if excess > 0 {
        buf.drain(..excess);
    }
}

/// The signal a process died from, if any (`None` for a normal exit or when
/// there is no status at all).
fn signal_of(status: &std::io::Result<ExitStatus>) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.as_ref().ok().and_then(|s| s.signal())
}

/// The exit code to report: the raw code, or `128 + signal` on signal death.
fn exit_code_of(status: &ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => code,
        (None, Some(sig)) => 128 + sig,
        (None, None) => 1,
    }
}

/// `killpg(pgid, sig)`. Errors are ignored: ESRCH / EPERM mean the group is
/// already gone and there is nothing better to do either way.
fn kill_group(pgid: i32, sig: libc::c_int) {
    if pgid <= 0 {
        return;
    }
    unsafe {
        libc::killpg(pgid, sig);
    }
}

/// The last [`TAIL_LINES`] lines of stdout followed by stderr, as one string.
fn tail_text(stdout: &[u8], stderr: &[u8]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for stream in [stdout, stderr] {
        for line in tail_lines(stream, TAIL_LINES.saturating_sub(lines.len())) {
            lines.push(line);
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut out = lines.join("\n");
    // A trailing newline exists only if the last contributing stream wrote one.
    let ends_with_line = if stderr.is_empty() {
        stdout.ends_with(b"\n")
    } else {
        stderr.ends_with(b"\n")
    };
    if ends_with_line {
        out.push('\n');
    }
    out
}

/// The last `n` lines of a byte buffer, lossily decoded. A buffer cut mid-line
/// keeps that partial line.
fn tail_lines(bytes: &[u8], n: usize) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(bytes);
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(n)..]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Milliseconds since `started`, saturating (never negative).
fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::v2::Check;

    fn check(cwd: Option<&str>, cmd: &[&str], timeout: u64) -> Check {
        Check {
            cwd: cwd.map(str::to_string),
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
            timeout,
        }
    }

    #[test]
    fn argv_without_cwd() {
        let argv = check_argv(
            &check(None, &["cargo", "test", "--all"], 30),
            Path::new("/wt/20260921-task-1f3a"),
        );
        assert_eq!(
            argv,
            vec!["/wt/20260921-task-1f3a", "--", "cargo", "test", "--all"]
        );
    }

    #[test]
    fn argv_with_cwd_prepends_env_c() {
        let argv = check_argv(
            &check(Some("sub"), &["sh", "-c", "pwd"], 5),
            Path::new("/wt/wt-1"),
        );
        assert_eq!(
            argv,
            vec![
                "/wt/wt-1",
                "--",
                "env",
                "-C",
                "/wt/wt-1/sub",
                "--",
                "sh",
                "-c",
                "pwd"
            ]
        );
    }

    #[test]
    fn argv_keeps_an_absolute_cwd_and_the_command_own_dash_dashes() {
        let argv = check_argv(
            &check(Some("/elsewhere"), &["git", "--", "status"], 1),
            Path::new("/wt/wt-1"),
        );
        assert_eq!(
            argv,
            vec![
                "/wt/wt-1",
                "--",
                "env",
                "-C",
                "/elsewhere",
                "--",
                "git",
                "--",
                "status"
            ]
        );
    }

    #[test]
    fn cwd_joining_prefers_an_absolute_path() {
        assert_eq!(cwd_path(Path::new("/wt/w"), "sub"), Path::new("/wt/w/sub"));
        assert_eq!(cwd_path(Path::new("/wt/w"), "/sub"), Path::new("/sub"));
    }

    #[test]
    fn tail_is_stdout_then_stderr_capped_at_60_lines() {
        let stdout = "a\nb\n".as_bytes();
        let stderr = (1..=70)
            .map(|i| format!("e{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = tail_text(stdout, stderr.as_bytes());
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), 60);
        assert_eq!(lines[0], "a");
        assert_eq!(lines[1], "b");
        assert_eq!(lines[2], "e13", "stderr keeps its last 58 lines");
        assert_eq!(lines[59], "e70");

        // One stream only, and neither.
        assert_eq!(tail_text(stdout, b""), "a\nb\n");
        assert_eq!(tail_text(b"one", b"two"), "one\ntwo");
        assert_eq!(tail_text(b"", b""), "");
    }

    #[test]
    fn push_rolling_keeps_the_last_megabyte() {
        let mut buf = Vec::new();
        for _ in 0..300 {
            push_rolling(&mut buf, &[b'x'; 8 * 1024]);
        }
        assert_eq!(buf.len(), STREAM_CAP);
        // One chunk larger than the cap keeps its own tail.
        push_rolling(&mut buf, &[b'y'; STREAM_CAP + 10]);
        assert_eq!(buf.len(), STREAM_CAP);
        assert!(buf.iter().all(|b| *b == b'y'));
    }

    #[test]
    fn signal_death_maps_to_128_plus_signal() {
        use std::os::unix::process::ExitStatusExt;
        let ok = |code: i32| Ok(ExitStatus::from_raw(code << 8));
        let signalled = |sig: i32| Ok(ExitStatus::from_raw(sig));
        assert_eq!(exit_code_of(&ExitStatus::from_raw(0)), 0);
        assert_eq!(exit_code_of(&ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(exit_code_of(&ExitStatus::from_raw(15)), 143);

        // A signal death is only a timeout once WE have signalled the group.
        assert_eq!(signal_of(&signalled(15)), Some(15));
        assert_eq!(signal_of(&ok(0)), None);
        assert_eq!(signal_of(&Err(std::io::Error::other("gone"))), None);
    }

    /// A stand-in `agent-jail` in `dir`, whose body is `body`.
    fn fake_jail(dir: &Path, body: &str) -> PathBuf {
        let jail = dir.join("agent-jail");
        std::fs::write(&jail, format!("#!/bin/sh\n{body}\n")).expect("write jail");
        std::fs::set_permissions(&jail, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        jail
    }

    /// Body of a pass-through jail: handles the plain `<dir> -- CMD` form and
    /// the `env -C DIR -- CMD` prefix a `cwd` check produces.
    fn pass_through() -> String {
        [
            r#"if [ "$3" = "-C" ]; then cd "$4" || exit 2; shift 4; else shift 2; fi"#,
            r#"if [ "$1" = "--" ]; then shift; fi"#,
            r#"exec "$@""#,
        ]
        .join("\n")
    }

    /// A freshly written fixture script can hit ETXTBSY ("Text file busy")
    /// when another test thread forks while the file is still open for
    /// writing; that is a test-harness race, so retry the spawn a few times.
    async fn run_with_retry(
        name: &str,
        check: &crate::config::v2::Check,
        worktree: &Path,
        program: &Path,
    ) -> CheckResult {
        let mut result = run_check_with_program(name, check, worktree, program).await;
        for _ in 0..20 {
            if !(result.exit == 127 && result.tail.contains("Text file busy")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            result = run_check_with_program(name, check, worktree, program).await;
        }
        result
    }

    #[tokio::test]
    async fn spawn_failure_is_127_with_the_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("no-such-jail");
        let result = run_with_retry(
            "nope",
            &check(None, &["true"], 5),
            Path::new("/wt/x"),
            &missing,
        )
        .await;

        assert_eq!(result.name, "nope");
        assert_eq!(result.exit, 127);
        assert!(!result.timed_out);
        assert!(
            result.tail.contains("cannot run") && result.tail.contains("no-such-jail"),
            "{:?}",
            result.tail
        );
    }

    #[tokio::test]
    async fn passing_check_reports_zero_and_runs_in_the_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let jail = fake_jail(dir.path(), &pass_through());
        std::fs::create_dir_all(dir.path().join("sub")).expect("mkdir");

        let result = run_with_retry(
            "pass",
            &check(Some("sub"), &["sh", "-c", "pwd"], 30),
            dir.path(),
            &jail,
        )
        .await;

        assert_eq!(result.exit, 0);
        assert!(!result.timed_out);
        // `env -C <worktree>/sub` really moved the check's working directory.
        assert!(result.tail.trim_end().ends_with("sub"), "{:?}", result.tail);
    }

    #[tokio::test]
    async fn refusal_is_a_failing_check_with_exit_5() {
        let dir = tempfile::tempdir().expect("tempdir");
        let jail = fake_jail(dir.path(), "echo no >&2\nexit 5");
        let result = run_with_retry("pass", &check(None, &["true"], 30), dir.path(), &jail).await;

        assert_eq!(result.exit, 5);
        assert!(!result.timed_out);
        assert_eq!(result.tail, "no\n");
    }

    #[tokio::test]
    async fn both_streams_are_drained() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Far more than a pipe buffer on BOTH streams: a reader that waited on
        // one stream before reading the other would deadlock here.
        let body = "head -c 300000 /dev/zero | tr '\\0' 'o'; \
                    head -c 300000 /dev/zero | tr '\\0' 'e' >&2; exit 4";
        let jail = fake_jail(dir.path(), body);
        let result = run_with_retry(
            "big",
            &check(None, &["sh", "-c", "ignored"], 30),
            dir.path(),
            &jail,
        )
        .await;

        assert_eq!(result.exit, 4);
        assert!(result.tail.contains('o'), "stdout lost: {:?}", result.tail);
        assert!(result.tail.contains('e'), "stderr lost: {:?}", result.tail);
    }

    #[tokio::test]
    async fn timeout_kills_the_group_and_reports_124() {
        let dir = tempfile::tempdir().expect("tempdir");
        let jail = fake_jail(dir.path(), &pass_through());

        let result = run_with_retry(
            "slow",
            &check(None, &["sleep", "31.5"], 1),
            dir.path(),
            &jail,
        )
        .await;

        assert!(result.timed_out);
        assert_eq!(result.exit, 124);
        assert!(result.elapsed_ms < 10_000, "{} ms", result.elapsed_ms);

        // The whole group is gone: no `sleep 31.5` survives the check.
        let pgrep = std::process::Command::new("pgrep")
            .args(["-f", "sleep 31.5"])
            .output()
            .expect("pgrep runs");
        assert!(
            String::from_utf8_lossy(&pgrep.stdout).trim().is_empty(),
            "lingering processes: {}",
            String::from_utf8_lossy(&pgrep.stdout)
        );
    }
}
