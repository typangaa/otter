//! Spawn a worker wrapper in its own process group, drain its pipes under a
//! byte cap, and enforce a hard deadline.
//!
//! The wrapper contract (argv): `program <args_prefix...> -t <timeout_secs>
//! <workdir> <prompt_file>` — e.g. `pi-worker -b b -t 3000 /wt/x /tmp/p.md`.
//! Only the two allow-listed wrapper names may be spawned (PATH resolution;
//! never a path). The child always runs in its own process group
//! (`process_group(0)`) with stdin detached, so weir can kill the *whole*
//! group — grandchildren included — on timeout, on interruption, or as routine
//! cleanup after a normal exit.
//!
//! Timeouts are bounded by [`MAX_TIMEOUT_SECS`] (7 days): a larger value — e.g.
//! `--timeout 18446744073709551615` — is a usage error rather than an arithmetic
//! overflow in the deadline.
//!
//! Pipe handling: stdout/stderr are read concurrently, each capped at
//! [`OUTPUT_CAP`] bytes. Past the cap the readers keep reading (so the child
//! never blocks on a full pipe) and flag `truncated`. The *head* of each
//! stream (first [`OUTPUT_CAP`] bytes) is kept for `stdout`; stderr
//! *additionally* gets a rolling tail buffer holding its last
//! [`STDERR_TAIL_CAP`] bytes, so `stderr_tail` really is the last lines of a
//! truncated stream (the usual place an error message lives) instead of the
//! last lines of its first 8 MiB. Data lands in shared buffers so a reader
//! abort (pipes held open by a lingering grandchild) still yields everything
//! collected so far.
//!
//! Lifetimes: the signal streams and the hard-deadline duration are prepared
//! *before* `spawn()`, and everything after the spawn is covered by a
//! [`GroupGuard`] which SIGKILLs the whole process group on drop unless it was
//! disarmed — so no early return (or panic) can leak a live worker tree.

use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::task::JoinHandle;

use crate::config::v2::ALLOWED_WORKER_COMMANDS;
use crate::exit::ExitKind;

/// Maximum bytes retained per output stream. Beyond this the reader keeps
/// draining (discarding) so the child never blocks, and `truncated` is set.
pub const OUTPUT_CAP: usize = 8 * 1024 * 1024;

/// Rolling stderr tail capacity: the last [`STDERR_TAIL_CAP`] bytes of the
/// stream are always kept, however long the stream is. Sized so that
/// [`STDERR_TAIL_LINES`] lines of ordinary logging always fit.
pub const STDERR_TAIL_CAP: usize = 64 * 1024;

/// Longest timeout weir accepts, in seconds (7 days). A larger value is
/// refused: `timeout_secs + grace` would overflow `u64` nanoseconds inside
/// `tokio::time::sleep` and panic, and a worker that effectively never times
/// out is a leak, not a feature.
pub const MAX_TIMEOUT_SECS: u64 = 7 * 24 * 3600;

/// Default extra seconds beyond `timeout_secs` before weir hard-kills the
/// group. The wrapper's own `-t` should fire first; this is the backstop.
const DEFAULT_DEADLINE_GRACE: Duration = Duration::from_secs(60);
/// Default wait between SIGTERM and SIGKILL when weir kills the group.
const DEFAULT_KILL_GRACE: Duration = Duration::from_secs(10);
/// How long readers may take to finish after the group is gone before they
/// are aborted and the partial buffers are used.
const READER_DRAIN_GRACE: Duration = Duration::from_secs(2);
/// Number of trailing stderr lines kept in `WorkerOutcome::stderr_tail`.
const STDERR_TAIL_LINES: usize = 60;
/// Exit code reported when weir's own hard deadline killed the group.
const EXIT_DEADLINE: i32 = 124;

/// Everything `run_worker` needs to launch one wrapper invocation.
#[derive(Debug, Clone)]
pub struct WorkerSpec {
    /// Wrapper program name, e.g. `pi-worker` — must be allow-listed and is
    /// resolved via PATH by `Command` (never a path itself).
    pub program: String,
    /// Already-expanded replica/model args, e.g. `["-b", "a"]`. Emitted before
    /// the `-t <secs>` pair.
    pub args_prefix: Vec<String>,
    /// Timeout handed to the wrapper as `-t <secs>`, and the base of weir's
    /// hard deadline (`timeout_secs + grace`). Must be `> 0` and no greater
    /// than [`MAX_TIMEOUT_SECS`] or [`run_worker`] refuses with `InvalidInput`.
    pub timeout_secs: u64,
    /// Working directory argument (passed through, the wrapper chdirs).
    pub workdir: PathBuf,
    /// Prompt file argument.
    pub prompt_file: PathBuf,
    /// Optional quota pattern for [`ExitKind`] classification of exit 3.
    pub quota_pattern: Option<String>,
}

/// The result of one wrapper run.
#[derive(Debug, Clone)]
pub struct WorkerOutcome {
    /// Wrapper exit code; `128 + signal` when killed by a signal; `124` when
    /// weir's own hard deadline killed the group.
    pub exit_code: i32,
    /// Classification of `exit_code` against the wrapper contract.
    pub kind: ExitKind,
    /// Captured stdout (lossy UTF-8, at most [`OUTPUT_CAP`] bytes — the head
    /// of the stream when truncated).
    pub stdout: String,
    /// Last [`STDERR_TAIL_LINES`] lines of stderr, taken from the rolling tail
    /// buffer: the *end* of the stream even when it exceeded [`OUTPUT_CAP`]
    /// (the head cap only limits `stdout` and the head copy consulted for
    /// quota classification).
    pub stderr_tail: String,
    /// Wall-clock milliseconds from spawn to reap.
    pub elapsed_ms: u64,
    /// `true` if EITHER stream exceeded [`OUTPUT_CAP`] bytes.
    pub truncated: bool,
    /// `true` if weir's hard deadline (or a SIGINT/SIGTERM to weir) killed
    /// the process group. Read by the JSON report through `serde_json::json!`
    /// in `cli::emit`, which the dead-code pass cannot see.
    #[allow(dead_code)]
    pub deadline_killed: bool,
}

/// Reject anything that is not exactly one of the allow-listed wrapper names.
/// Names containing `/` are rejected outright (no paths, ever).
pub fn check_program_allowed(program: &str) -> Result<(), String> {
    if program.contains('/') {
        return Err(format!(
            "worker program {program:?} must not contain '/' (only bare wrapper names \
             resolved via PATH are allowed)"
        ));
    }
    if !ALLOWED_WORKER_COMMANDS.contains(&program) {
        return Err(format!(
            "worker program {program:?} is not an allowed worker wrapper (allowed: {:?})",
            ALLOWED_WORKER_COMMANDS
        ));
    }
    Ok(())
}

/// Replace the literal `{var}` in each template element with `value`.
/// Elements that do not contain the placeholder are left untouched.
pub fn expand_args(template: &[String], var: &str, value: &str) -> Vec<String> {
    let placeholder = format!("{{{var}}}");
    template
        .iter()
        .map(|elem| elem.replace(&placeholder, value))
        .collect()
}

/// Read the inner buffer of a capped collector: at most `OUTPUT_CAP` bytes,
/// copied out under the lock so the reader can be aborted at any time.
fn snapshot(buf: &Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    let guard = buf.lock().unwrap_or_else(|e| e.into_inner());
    guard.iter().take(OUTPUT_CAP).copied().collect()
}

/// Append `chunk` to the rolling tail buffer in `buf`, dropping from the front
/// whatever exceeds `cap`. The buffer therefore always holds the *last* `cap`
/// bytes fed to it — the byte-level equivalent of `tail -c`.
fn push_rolling_tail(buf: &Arc<Mutex<Vec<u8>>>, chunk: &[u8], cap: usize) {
    let mut guard = buf.lock().unwrap_or_else(|e| e.into_inner());
    if chunk.len() >= cap {
        // This chunk alone fills (or overflows) the window.
        guard.clear();
        guard.extend_from_slice(&chunk[chunk.len() - cap..]);
        return;
    }
    guard.extend_from_slice(chunk);
    let excess = guard.len().saturating_sub(cap);
    if excess > 0 {
        guard.drain(..excess);
    }
}

/// Copy a rolling tail buffer out under the lock (abort-safe, like
/// [`snapshot`]).
fn snapshot_tail(buf: &Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    let guard = buf.lock().unwrap_or_else(|e| e.into_inner());
    guard.clone()
}

/// Last N lines of a byte buffer, lossily decoded. If the buffer is cut
/// mid-line the partial leading line is still included.
fn tail_lines(bytes: &[u8], n: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    for line in text
        .lines()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        out.push_str(line);
        out.push('\n');
    }
    // Do not invent a trailing newline the child never wrote.
    if out.ends_with('\n') && !text.ends_with('\n') {
        out.pop();
    }
    out
}

/// Env-overridable duration knob (integer seconds); unparsable or empty
/// values fall back to `default_val`. Test-only knobs
/// (`WEIR_DEADLINE_GRACE_SECS`, `WEIR_KILL_GRACE_SECS`).
fn env_grace(key: &str, default_val: Duration) -> Duration {
    match std::env::var(key).ok().filter(|s| !s.is_empty()) {
        Some(raw) => match raw.parse::<u64>() {
            Ok(secs) => Duration::from_secs(secs),
            Err(_) => default_val,
        },
        None => default_val,
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

/// SIGTERM the group, wait up to `kill_grace` for `child` to exit, then
/// SIGKILL it. Returns the child status.
async fn term_then_kill(
    child: &mut tokio::process::Child,
    pgid: i32,
    kill_grace: Duration,
) -> std::io::Result<std::process::ExitStatus> {
    kill_group(pgid, libc::SIGTERM);
    tokio::select! {
        status = child.wait() => status,
        _ = tokio::time::sleep(kill_grace) => {
            kill_group(pgid, libc::SIGKILL);
            child.wait().await
        }
    }
}

/// Spawn, drain and supervise one wrapper run. See the module docs for the
/// full contract.
///
/// Thin wrapper around [`spawn_and_wait`]: the spawn half lives there so a
/// caller that must do something *between* the spawn and the supervision (the
/// integration tests, for instance) can use [`WorkerHandle`] directly and keep
/// the same cleanup guarantees.
pub async fn run_worker(spec: &WorkerSpec) -> std::io::Result<WorkerOutcome> {
    let handle = spawn_worker(spec)?;
    handle.join().await
}

/// A worker wrapper that has been spawned and is being supervised: signals
/// watched, deadline armed, and its process group SIGKILLed if this handle is
/// ever dropped.
pub struct WorkerHandle {
    child: tokio::process::Child,
    started: Instant,
    pgid: i32,
    /// Dropped (and with it the group cleanup) when the handle is dropped.
    _guard: GroupGuard,
    sigint: tokio::signal::unix::Signal,
    sigterm: tokio::signal::unix::Signal,
    hard_deadline: std::pin::Pin<Box<tokio::time::Sleep>>,
    kill_grace: Duration,
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    stderr_tail_buf: Arc<Mutex<Vec<u8>>>,
    out_truncated: Arc<AtomicBool>,
    err_truncated: Arc<AtomicBool>,
    out_task: Option<JoinHandle<()>>,
    err_task: Option<JoinHandle<()>>,
    quota_pattern: Option<String>,
}

/// Validate `spec`, build argv and spawn the wrapper in its own process group.
///
/// Public so a caller (and the integration tests) can hold a live, already
/// supervised [`WorkerHandle`]: dropping it without `.join()`-ing must SIGKILL
/// the whole group, which is the guarantee `run_worker` relies on for its own
/// early exits.
///
/// Everything that can fail *before* a process exists happens here; once this
/// returns `Ok`, dropping the [`WorkerHandle`] (or `.join()`-ing it) is what
/// reaps the worker tree.
pub fn spawn_worker(spec: &WorkerSpec) -> std::io::Result<WorkerHandle> {
    // Security check before anything is spawned.
    if let Err(msg) = check_program_allowed(&spec.program) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, msg));
    }

    // The hard deadline is computed here, before anything is spawned: a huge
    // `timeout_secs` (e.g. `--timeout 18446744073709551615`) would overflow
    // `timeout_secs + grace` in nanoseconds and panic *after* a worker tree
    // was already live. Checked again here so direct callers cannot bypass
    // `weir worker run`'s argument validation.
    if spec.timeout_secs > MAX_TIMEOUT_SECS {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "worker run: timeout {} exceeds the maximum of {MAX_TIMEOUT_SECS} seconds",
                spec.timeout_secs
            ),
        ));
    }
    if spec.timeout_secs == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "worker run: timeout must be > 0",
        ));
    }

    let deadline_grace = env_grace("WEIR_DEADLINE_GRACE_SECS", DEFAULT_DEADLINE_GRACE);
    let kill_grace = env_grace("WEIR_KILL_GRACE_SECS", DEFAULT_KILL_GRACE);
    // Saturating on both sides: `deadline_grace` comes from an env knob and is
    // not range-checked, so clamp the total rather than trusting `checked_add`.
    let deadline_after = deadline_grace
        .as_secs()
        .saturating_add(spec.timeout_secs)
        .min(MAX_TIMEOUT_SECS);

    // argv: program + args_prefix + -t <secs> + workdir + prompt_file.
    let mut cmd = tokio::process::Command::new(&spec.program);
    cmd.args(&spec.args_prefix);
    cmd.arg("-t");
    cmd.arg(spec.timeout_secs.to_string());
    cmd.arg(&spec.workdir);
    cmd.arg(&spec.prompt_file);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Own process group so the whole tree (grandchildren included) can be
    // killed with killpg; kill_on_drop as a last-resort belt.
    cmd.process_group(0);
    cmd.kill_on_drop(true);

    // The signal streams are registered BEFORE the spawn. Installing them
    // afterwards left a window in which a SIGTERM/SIGINT arriving that early
    // killed nothing but weir itself, orphaning the worker tree.
    let sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let hard_deadline: std::pin::Pin<Box<tokio::time::Sleep>> =
        Box::pin(tokio::time::sleep(Duration::from_secs(deadline_after)));

    let mut child = cmd.spawn()?;
    let started = Instant::now();
    // pgid == child pid (`process_group(0)`); `id()` is Some until reaped.
    let pgid = child.id().unwrap_or(0) as i32;

    let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stderr_tail_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let out_truncated = Arc::new(AtomicBool::new(false));
    let err_truncated = Arc::new(AtomicBool::new(false));

    // Drain both pipes concurrently; a child that fills a pipe must never
    // block us, and we must never block on it.
    let out_task = child.stdout.take().map(|pipe| {
        let buf = Arc::clone(&stdout_buf);
        let flag = Arc::clone(&out_truncated);
        tokio::spawn(drain_pipe(pipe, buf, flag))
    });
    let err_task = child.stderr.take().map(|pipe| {
        let buf = Arc::clone(&stderr_buf);
        let tail = Arc::clone(&stderr_tail_buf);
        let flag = Arc::clone(&err_truncated);
        tokio::spawn(drain_pipe_with_tail(pipe, buf, tail, flag))
    });

    Ok(WorkerHandle {
        child,
        started,
        pgid,
        // From here on, any path that drops this handle without completing
        // `join` SIGKILLs the whole group.
        _guard: GroupGuard::new(pgid),
        sigint,
        sigterm,
        hard_deadline,
        kill_grace,
        stdout_buf,
        stderr_buf,
        stderr_tail_buf,
        out_truncated,
        err_truncated,
        out_task,
        err_task,
        quota_pattern: spec.quota_pattern.clone(),
    })
}

impl WorkerHandle {
    /// Wait for the wrapper under the hard deadline and the SIGINT/SIGTERM
    /// watchers, reap the group and assemble the outcome.
    pub async fn join(mut self) -> std::io::Result<WorkerOutcome> {
        // `Sleep` is !Unpin; boxing it once at spawn time lets `select!` poll
        // it through the box while the handle stays a plain struct field.
        let hard_deadline = self.hard_deadline.as_mut();
        let (status, deadline_killed) = tokio::select! {
            status = self.child.wait() => (status?, false),
            _ = hard_deadline => {
                // The wrapper ignored its own -t: TERM the group, TERM grace, KILL.
                (
                    term_then_kill(&mut self.child, self.pgid, self.kill_grace).await?,
                    true,
                )
            }
            _ = self.sigint.recv() => {
                (
                    term_then_kill(&mut self.child, self.pgid, self.kill_grace).await?,
                    true,
                )
            }
            _ = self.sigterm.recv() => {
                (
                    term_then_kill(&mut self.child, self.pgid, self.kill_grace).await?,
                    true,
                )
            }
        };

        // The direct child is reaped. Leftover grandchildren must not survive:
        // always SIGKILL the group once (ESRCH ignored), then stand down the
        // guard — everything below runs with the group known dead.
        kill_group(self.pgid, libc::SIGKILL);
        self._guard.disarm();

        // Grandchildren may still hold the pipes open — give the readers a
        // short grace to finish, then abort and use the partial buffers.
        drain_readers(&mut self.out_task, &mut self.err_task).await;

        let elapsed_ms = self.started.elapsed().as_millis() as u64;

        let stdout_bytes = snapshot(&self.stdout_buf);
        let stderr_bytes = snapshot(&self.stderr_buf);
        // The rolling tail equals the whole stream while it stayed under
        // STDERR_TAIL_CAP, so it is always the right source for the reported
        // tail.
        let stderr_tail_bytes = snapshot_tail(&self.stderr_tail_buf);
        let truncated = self.out_truncated.load(Ordering::Relaxed)
            || self.err_truncated.load(Ordering::Relaxed);

        // When weir killed the group (deadline or interruption), report a
        // timeout (124) rather than 128+sig.
        let exit_code = if deadline_killed {
            EXIT_DEADLINE
        } else {
            exit_code_of(&status)
        };
        // Quota classification consults both ends of a possibly truncated
        // stream: the retained head plus the rolling tail. The head alone
        // would classify a quota message arriving after the first 8 MiB as
        // "empty".
        let stderr_classify = format!(
            "{head}{tail}",
            head = String::from_utf8_lossy(&stderr_bytes),
            tail = String::from_utf8_lossy(&stderr_tail_bytes)
        );
        let kind =
            ExitKind::from_wrapper(exit_code, &stderr_classify, self.quota_pattern.as_deref());

        Ok(WorkerOutcome {
            exit_code,
            kind,
            stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
            stderr_tail: tail_lines(&stderr_tail_bytes, STDERR_TAIL_LINES),
            elapsed_ms,
            truncated,
            deadline_killed,
        })
    }
}

/// Read `pipe` to EOF, keeping at most [`OUTPUT_CAP`] bytes (the *head*) in
/// `buf`. Past the cap the rest is read and discarded (so the writer never
/// blocks) and `truncated` is set.
async fn drain_pipe(
    pipe: impl AsyncReadExt + Unpin,
    buf: Arc<Mutex<Vec<u8>>>,
    truncated: Arc<AtomicBool>,
) {
    drain_pipe_inner(pipe, Some(buf), None, truncated).await;
}

/// Like [`drain_pipe`], but every byte read is additionally fed to a rolling
/// tail buffer that keeps the last [`STDERR_TAIL_CAP`] bytes, so the end of a
/// stream survives the head cap. Used for stderr.
async fn drain_pipe_with_tail(
    pipe: impl AsyncReadExt + Unpin,
    buf: Arc<Mutex<Vec<u8>>>,
    tail: Arc<Mutex<Vec<u8>>>,
    truncated: Arc<AtomicBool>,
) {
    drain_pipe_inner(pipe, Some(buf), Some((tail, STDERR_TAIL_CAP)), truncated).await
}

/// Shared reader loop: `head` receives at most [`OUTPUT_CAP`] bytes, `tail`
/// — when supplied — receives the last `tail_cap` bytes, and every byte past the
/// head cap flips `truncated` (counted, not merely hinted at, so a reader that
/// is handed a stream smaller than the cap never reports truncation).
async fn drain_pipe_inner(
    mut pipe: impl AsyncReadExt + Unpin,
    head: Option<Arc<Mutex<Vec<u8>>>>,
    tail: Option<(Arc<Mutex<Vec<u8>>>, usize)>,
    truncated: Arc<AtomicBool>,
) {
    let mut chunk = vec![0u8; 64 * 1024];
    // Bytes retained in the head buffer; tracked beside the buffer so the
    // "past the cap" decision does not need the lock on every chunk.
    let mut kept = 0usize;
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                if let Some((tail_buf, cap)) = &tail {
                    push_rolling_tail(tail_buf, &chunk[..n], *cap);
                }
                match &head {
                    Some(buf) => {
                        let take = if kept < OUTPUT_CAP {
                            (OUTPUT_CAP - kept).min(n)
                        } else {
                            0
                        };
                        if take > 0 {
                            let mut guard = buf.lock().unwrap_or_else(|e| e.into_inner());
                            // `kept` only counts bytes appended here, so the
                            // head buffer never grows past OUTPUT_CAP.
                            guard.extend_from_slice(&chunk[..take]);
                            kept += take;
                        }
                        if take < n {
                            // The remainder of this chunk was read and thrown
                            // away: the stream is longer than what we keep.
                            truncated.store(true, Ordering::Relaxed);
                        }
                    }
                    None => {
                        // No head buffer at all: everything read is discarded.
                        truncated.store(true, Ordering::Relaxed);
                    }
                }
            }
            Err(_) => break,
        }
    }
}

/// SIGKILL the worker's process group on drop unless [`GroupGuard::disarm`]
/// was called first. This is the single cleanup path for every early exit out
/// of [`run_worker`] after a successful spawn: a `?`, a panic while unwinding,
/// or a task abort (`kill_on_drop` drops this guard before killing the direct
/// child, so grandchildren are covered too).
struct GroupGuard {
    pgid: i32,
    armed: bool,
}

impl GroupGuard {
    fn new(pgid: i32) -> Self {
        Self { pgid, armed: true }
    }

    /// The group is known dead (normal path): stand the drop handler down.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            kill_group(self.pgid, libc::SIGKILL);
        }
    }
}

/// Spawn a `sleep` as its own process group, hand its pgid to a [`GroupGuard`]
/// and drop the guard; `disarm` decides whether the sleep dies. Returns `true`
/// when reality matches the arming: dead within `within` when armed, still
/// alive after waiting the whole `within` when disarmed. The sleep is reaped
/// before returning (dropping a `std::process::Child` would leak it).
#[cfg(test)]
fn group_guard_state_matches(disarm: bool, within: Duration) -> bool {
    use std::os::unix::process::CommandExt;

    let mut proc = std::process::Command::new("sleep")
        .arg("60")
        // Own process group, exactly like the worker contract.
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let pgid = proc.id() as i32;
    let mut guard = GroupGuard::new(pgid);
    if disarm {
        guard.disarm();
    }
    drop(guard);

    let deadline = Instant::now() + within;
    let observed = loop {
        match proc.try_wait().expect("try_wait") {
            // Killed by the guard, not by anything else.
            Some(status) => break status.signal() == Some(libc::SIGKILL),
            None => {
                if Instant::now() >= deadline {
                    // Armed: never died. Disarmed: still alive, as intended.
                    break disarm;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    let _ = proc.kill();
    let _ = proc.wait();
    observed
}

/// Give the two reader tasks at most [`READER_DRAIN_GRACE`] to finish, then
/// abort them — partial buffers are still usable.
async fn drain_readers(
    out_task: &mut Option<JoinHandle<()>>,
    err_task: &mut Option<JoinHandle<()>>,
) {
    for task in [out_task, err_task] {
        if let Some(handle) = task.as_mut() {
            let _ = tokio::time::timeout(READER_DRAIN_GRACE, &mut *handle).await;
            handle.abort();
        }
    }
}

/// Exit code from a wait status: the raw code, or `128 + signal` on signal
/// death.
fn exit_code_of(status: &std::process::ExitStatus) -> i32 {
    match (status.code(), status.signal()) {
        (Some(code), _) => code,
        (None, Some(sig)) => 128 + sig,
        (None, None) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_program_allowed_accepts_wrappers() {
        assert!(check_program_allowed("pi-worker").is_ok());
        assert!(check_program_allowed("agy-worker").is_ok());
    }

    #[test]
    fn check_program_allowed_rejects_everything_else() {
        for bad in [
            "agy",
            "sh",
            "pi",
            "pi-worker2",
            "/bin/pi-worker",
            "./pi-worker",
            "PI-WORKER",
            "",
        ] {
            assert!(
                check_program_allowed(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn check_program_allowed_rejects_paths_even_for_wrappers() {
        // '/' is rejected before the allow-list, with a distinct message.
        let err = check_program_allowed("/usr/local/bin/pi-worker").unwrap_err();
        assert!(err.contains("must not contain '/'"), "{err}");
    }

    #[test]
    fn expand_args_replaces_only_the_placeholder() {
        let template = vec!["-b".to_string(), "{replica}".to_string()];
        assert_eq!(expand_args(&template, "replica", "a"), ["-b", "a"]);

        // Other placeholders are left alone.
        let template = vec![
            "-m".to_string(),
            "{model}".to_string(),
            "{replica}".to_string(),
        ];
        assert_eq!(
            expand_args(&template, "replica", "b"),
            ["-m", "{model}", "b"]
        );

        // Repeated occurrences within one element.
        let template = vec!["{replica}+{replica}".to_string()];
        assert_eq!(expand_args(&template, "replica", "x"), ["x+x"]);

        // No placeholder at all → unchanged.
        let template = vec!["-b".to_string()];
        assert_eq!(expand_args(&template, "replica", "a"), ["-b"]);
    }

    #[test]
    fn tail_lines_keeps_last_n() {
        let text = (1..=100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = tail_lines(text.as_bytes(), 60);
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), 60);
        assert_eq!(lines[0], "line 41");
        assert_eq!(lines[59], "line 100");
    }

    #[test]
    fn tail_lines_short_input_and_no_trailing_newline() {
        assert_eq!(tail_lines(b"one\ntwo", 3), "one\ntwo");
        assert_eq!(tail_lines(b"one\ntwo\n", 3), "one\ntwo\n");
        assert_eq!(tail_lines(b"", 3), "");
    }

    #[test]
    fn tail_lines_drops_only_at_hard_cap_edge() {
        // More lines than n: oldest are dropped.
        let tail = tail_lines(b"a\nb\nc\nd\n", 2);
        assert_eq!(tail, "c\nd\n");
    }

    #[test]
    fn max_timeout_secs_is_seven_days() {
        assert_eq!(MAX_TIMEOUT_SECS, 604_800);
        assert_eq!(STDERR_TAIL_CAP, 64 * 1024);
    }

    #[test]
    fn rolling_tail_keeps_only_the_last_cap_bytes() {
        let tail: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = 1024;
        let total = cap * 64 + 7;

        // Build the reference stream and feed the same bytes in 64-byte chunks.
        let all: Vec<u8> = (0..total).map(|i| b'a' + (i % 26) as u8).collect();
        for chunk in all.chunks(64) {
            push_rolling_tail(&tail, chunk, cap);
        }

        let kept = snapshot_tail(&tail);
        assert_eq!(kept.len(), cap, "tail grew past the cap");
        assert_eq!(kept, all[all.len() - cap..], "tail is not the end");
    }

    #[test]
    fn rolling_tail_handles_chunks_at_or_above_the_cap() {
        let tail: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

        // A chunk far larger than the cap leaves only its last `cap` bytes.
        push_rolling_tail(&tail, &vec![b'x'; 5000], 100);
        assert_eq!(snapshot_tail(&tail), vec![b'x'; 100]);

        // An exact-cap chunk replaces the window; a following small chunk
        // shifts it.
        push_rolling_tail(&tail, &[b'y'; 100], 100);
        push_rolling_tail(&tail, b"ab", 100);
        assert_eq!(
            snapshot_tail(&tail),
            [vec![b'y'; 98], b"ab".to_vec()].concat()
        );

        // Empty input never grows the window.
        push_rolling_tail(&tail, b"", 100);
        assert_eq!(snapshot_tail(&tail).len(), 100);
    }

    #[tokio::test]
    async fn drain_pipe_with_tail_keeps_the_end_of_a_huge_stream() {
        // 200 KiB of filler where only the final line matters. The stream
        // stays under the 8 MiB head cap, so the head copy never sees the
        // message's context and `truncated` must stay false.
        let mut input: Vec<u8> = vec![b'x'; 200 * 1024];
        input.extend_from_slice(b"\nRESOURCE_EXHAUSTED\n");

        let head: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let tail: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let flag = Arc::new(AtomicBool::new(false));
        drain_pipe_with_tail(
            input.as_slice(),
            Arc::clone(&head),
            Arc::clone(&tail),
            Arc::clone(&flag),
        )
        .await;

        assert!(
            !flag.load(Ordering::Relaxed),
            "a stream below the head cap is not truncated"
        );
        let head_bytes = snapshot(&head);
        assert_eq!(head_bytes.len(), input.len());
        // The head copy holds the whole (uncapped) stream here, message and
        // all — it is the *tail* buffer that makes the end of a capped stream
        // available, as the next test over the real cap shows.

        let tail_bytes = snapshot_tail(&tail);
        assert_eq!(tail_bytes.len(), STDERR_TAIL_CAP);
        // A trailing newline is not invented, so the reported tail ends with
        // the message on its own line.
        let tail_text = tail_lines(&tail_bytes, STDERR_TAIL_LINES);
        assert!(
            tail_text.ends_with("RESOURCE_EXHAUSTED\n"),
            "tail must end with the final line: {tail_text:?}"
        );
        assert_eq!(tail_text.lines().last().unwrap(), "RESOURCE_EXHAUSTED");
    }

    /// The regression this all exists for: a stream far past the head cap whose
    /// last line is the interesting one. The head buffer stops mid-filler, the
    /// rolling tail holds the end, and the two concatenated (what quota
    /// classification sees) contain the message.
    #[tokio::test]
    async fn rolling_tail_recovers_a_message_past_the_head_cap() {
        let mut input: Vec<u8> = vec![b'x'; OUTPUT_CAP + 32];
        input.extend_from_slice(b"\nRESOURCE_EXHAUSTED\n");

        let head: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let tail: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let flag = Arc::new(AtomicBool::new(false));
        drain_pipe_with_tail(
            input.as_slice(),
            Arc::clone(&head),
            Arc::clone(&tail),
            Arc::clone(&flag),
        )
        .await;

        assert!(flag.load(Ordering::Relaxed));
        let head_text = String::from_utf8_lossy(&snapshot(&head)).into_owned();
        assert_eq!(head_text.len(), OUTPUT_CAP);
        assert!(
            !head_text.contains("RESOURCE_EXHAUSTED"),
            "test premise: the head cap must cut before the message"
        );

        let tail_text = tail_lines(&snapshot_tail(&tail), STDERR_TAIL_LINES);
        assert_eq!(
            tail_text.lines().last().unwrap(),
            "RESOURCE_EXHAUSTED",
            "tail must end with the message: {tail_text:?}"
        );

        // What ExitKind::from_wrapper is handed for classification.
        let classified = format!(
            "{head_text}{}",
            String::from_utf8_lossy(&snapshot_tail(&tail))
        );
        assert!(classified.contains("RESOURCE_EXHAUSTED"));
    }

    #[tokio::test]
    async fn drain_pipe_head_cap_and_truncation_flag() {
        // Just past the cap: exactly OUTPUT_CAP kept, truncation flagged.
        let input = vec![b'q'; OUTPUT_CAP + 10];
        let head: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let flag = Arc::new(AtomicBool::new(false));
        drain_pipe(input.as_slice(), Arc::clone(&head), Arc::clone(&flag)).await;
        assert_eq!(snapshot(&head).len(), OUTPUT_CAP);
        assert!(flag.load(Ordering::Relaxed));

        // Exactly at the cap: nothing was discarded, so no truncation.
        let head: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let flag = Arc::new(AtomicBool::new(false));
        drain_pipe(
            vec![b'q'; OUTPUT_CAP].as_slice(),
            Arc::clone(&head),
            Arc::clone(&flag),
        )
        .await;
        assert_eq!(snapshot(&head).len(), OUTPUT_CAP);
        assert!(!flag.load(Ordering::Relaxed));

        // Small stream: fully kept, unflagged, and the tail copy (when asked
        // for) equals the whole stream.
        let head: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let tail: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let flag = Arc::new(AtomicBool::new(false));
        drain_pipe_with_tail(
            b"one\ntwo".as_slice(),
            Arc::clone(&head),
            Arc::clone(&tail),
            Arc::clone(&flag),
        )
        .await;
        assert_eq!(snapshot(&head), b"one\ntwo");
        assert_eq!(snapshot_tail(&tail), b"one\ntwo");
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn group_guard_kills_the_group_on_drop() {
        assert!(
            group_guard_state_matches(false, Duration::from_secs(5)),
            "armed guard must SIGKILL the group"
        );
    }

    #[test]
    fn disarmed_group_guard_leaves_the_group_alone() {
        assert!(
            group_guard_state_matches(true, Duration::from_millis(300)),
            "disarmed guard must not kill the group"
        );
    }

    #[test]
    fn env_grace_falls_back_on_unparsable() {
        // Not racing anything: unique keys per assertion, removed right after.
        std::env::set_var("WEIR_TEST_ONLY_GRACE", "42");
        assert_eq!(
            env_grace("WEIR_TEST_ONLY_GRACE", Duration::from_secs(1)),
            Duration::from_secs(42)
        );
        std::env::set_var("WEIR_TEST_ONLY_GRACE", "abc");
        assert_eq!(
            env_grace("WEIR_TEST_ONLY_GRACE", Duration::from_secs(7)),
            Duration::from_secs(7)
        );
        std::env::set_var("WEIR_TEST_ONLY_GRACE", "");
        assert_eq!(
            env_grace("WEIR_TEST_ONLY_GRACE", Duration::from_secs(9)),
            Duration::from_secs(9)
        );
        std::env::remove_var("WEIR_TEST_ONLY_GRACE");
    }

    /// The allow-list is enforced inside `run_worker` itself: a non-wrapper
    /// program is refused with `InvalidInput` before `Command::spawn` runs.
    #[tokio::test]
    async fn run_worker_refuses_a_program_outside_the_allow_list() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("marker");
        let workdir = tmp.path().join("wt");
        std::fs::create_dir_all(&workdir).unwrap();
        let prompt = tmp.path().join("prompt.md");
        std::fs::write(&prompt, "x").unwrap();

        let spec = WorkerSpec {
            program: "touch".to_string(),
            // If `touch` ever ran, the marker would exist.
            args_prefix: vec![marker.to_string_lossy().into_owned()],
            timeout_secs: 5,
            workdir,
            prompt_file: prompt,
            quota_pattern: None,
        };
        let err = run_worker(&spec).await.expect_err("touch must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err:?}");
        assert!(
            err.to_string().contains("not an allowed worker wrapper"),
            "{err}"
        );
        assert!(
            !marker.exists(),
            "the refused program must never be spawned"
        );
    }

    /// A timeout beyond `MAX_TIMEOUT_SECS` is refused with `InvalidInput`
    /// before anything is spawned — it used to overflow the deadline and panic
    /// with the worker tree already live. `0` is refused as well.
    #[tokio::test]
    async fn run_worker_refuses_timeouts_outside_the_allowed_range() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = tmp.path().join("wt");
        std::fs::create_dir_all(&workdir).unwrap();
        let prompt = tmp.path().join("prompt.md");
        std::fs::write(&prompt, "x").unwrap();

        for (secs, needle) in [
            (u64::MAX, "exceeds the maximum"),
            (MAX_TIMEOUT_SECS + 1, "exceeds the maximum"),
            (1 << 62, "exceeds the maximum"),
            (0, "must be > 0"),
        ] {
            let spec = WorkerSpec {
                program: "pi-worker".to_string(),
                args_prefix: Vec::new(),
                timeout_secs: secs,
                workdir: workdir.clone(),
                prompt_file: prompt.clone(),
                quota_pattern: None,
            };
            let err = run_worker(&spec)
                .await
                .expect_err("timeout {secs} must be refused");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err:?}");
            assert!(err.to_string().contains(needle), "{err} (want {needle:?})");
        }
    }

    #[test]
    fn exit_code_of_signal_maps_to_128_plus_sig() {
        // 137 = 128 + 9 (SIGKILL) — the classic wrapper-kill code.
        assert_eq!(ExitKind::from_wrapper(137, "", None), ExitKind::Error);
        assert_eq!(EXIT_DEADLINE, 124);
    }
}
