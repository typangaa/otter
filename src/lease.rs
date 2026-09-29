//! GPU slot leases: `weir lease run` / `weir lease status`.
//!
//! A *slot* is a mutually-exclusive resource (one GPU, or one replica of a model
//! server) identified by a name and bound to a *replica* string. Slots are
//! represented by lock files under
//! `${XDG_STATE_HOME:-$HOME/.local/state}/weir/leases/<NAME>.lock`
//! (`WEIR_STATE_DIR` overrides the whole state root). Mutual exclusion uses
//! `flock(2)` on the lock file, so a lease is released automatically even if weir
//! is killed.
//!
//! These commands never read `weir.toml` — they are a resource gate plus a
//! command spawner.
//!
//! Exit codes:
//!
//! | Code | Meaning                                                         |
//! |------|-----------------------------------------------------------------|
//! |  0   | Child exited 0                                                  |
//! |  N   | The child's own exit code (`128+signal` if it died on a signal)  |
//! |  2   | Command rejected (basename must be `pi-worker` / `agy-worker`)    |
//! | 126  | Worker found but could not be spawned                             |
//! | 127  | Worker executable not found (`ErrorKind::NotFound`)               |
//! |  75  | Every slot stayed busy until `--wait-timeout` (EX_TEMPFAIL)      |

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Args;
use serde_json::Value;

/// Returned when no slot became free in time (sysexits `EX_TEMPFAIL`).
pub const EXIT_TIMEOUT: i32 = 75;
/// Returned when the command is rejected, or on a hard lease error.
pub const EXIT_ERROR: i32 = 2;
/// Returned when the worker executable cannot be found (shell convention).
const EXIT_NOT_FOUND: i32 = 127;
/// Returned when the worker exists but spawning it fails (shell convention).
const EXIT_SPAWN_FAILED: i32 = 126;
/// Grace period between SIGTERM and SIGKILL when weir is interrupted.
const TERM_GRACE: Duration = Duration::from_secs(10);
/// Poll interval while waiting for a free slot.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Suffix of a lease lock file.
const LOCK_SUFFIX: &str = ".lock";

// ── CLI args ───────────────────────────────────────────────────────────────────

/// One `--slot NAME=REPLICA` declaration.
#[derive(Debug, Clone)]
struct SlotSpec {
    /// Slot name — doubles as the lock file name.
    name: String,
    /// Value substituted for `{replica}` in the command args.
    replica: String,
}

impl SlotSpec {
    /// Parse `NAME=REPLICA` (the replica may be empty).
    fn parse(raw: &str) -> Result<Self, String> {
        let (name, replica) = raw
            .split_once('=')
            .ok_or_else(|| format!("invalid --slot {raw:?}: expected NAME=REPLICA"))?;
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(format!("invalid --slot {raw:?}: unusable slot name"));
        }
        Ok(Self {
            name: name.to_owned(),
            replica: replica.to_owned(),
        })
    }
}

/// `weir lease run` arguments.
#[derive(Debug, Clone, Args)]
pub struct LeaseRunArgs {
    /// Slot declaration NAME=REPLICA (repeatable). Tried in the order given,
    /// unless --affinity redirects the preference.
    #[arg(long = "slot", value_name = "NAME=REPLICA", required = true)]
    slots: Vec<String>,

    /// Affinity key: the slot that last served this key is tried first.
    #[arg(long, value_name = "KEY")]
    affinity: Option<String>,

    /// Seconds to keep retrying while all slots are busy. Default: unlimited.
    #[arg(long, value_name = "SECS")]
    wait_timeout: Option<u64>,

    /// The command to run under the lease (after `--`).
    #[arg(required = true, trailing_var_arg = true)]
    command: Vec<String>,
}

/// `weir lease status` arguments.
#[derive(Debug, Args)]
pub struct LeaseStatusArgs {
    /// Emit a JSON array instead of one line per slot.
    #[arg(long)]
    json: bool,
}

impl LeaseStatusArgs {
    /// Whether the user asked for JSON (either `weir --json …` or `… --json`).
    pub fn json_flag(&self) -> bool {
        self.json
    }
}

// ── state paths ────────────────────────────────────────────────────────────────

/// Root of weir's writable state: `$WEIR_STATE_DIR`, else
/// `${XDG_STATE_HOME:-$HOME/.local/state}/weir`.
fn state_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("WEIR_STATE_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    let base = match std::env::var("XDG_STATE_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(base.join("weir"))
}

/// Directory holding the `<NAME>.lock` files.
fn leases_dir(root: &Path) -> PathBuf {
    root.join("leases")
}

/// A slot's lock file path.
fn lock_path(root: &Path, slot: &str) -> PathBuf {
    leases_dir(root).join(format!("{slot}{LOCK_SUFFIX}"))
}

// ── flock helper ───────────────────────────────────────────────────────────────

/// An open lease file. Holding the exclusive lock *is* the lease; dropping this
/// value (or killing the process) releases the slot.
pub struct LeaseFile {
    file: File,
}

impl LeaseFile {
    /// Open an existing lease file without locking it.
    fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            file: std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?,
        })
    }

    /// Non-blocking `LOCK_EX`; [`std::io::ErrorKind::WouldBlock`] means busy.
    fn try_lock(&self) -> std::io::Result<()> {
        let rc = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// Write the lease record while the lock is held. Truncates first so no
    /// stale bytes from a previous holder can linger.
    fn write_record(&self, json: &str) -> std::io::Result<()> {
        let mut file = &self.file;
        file.rewind()?;
        file.set_len(0)?;
        file.write_all(json.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }

    /// Read the record bytes. Best effort: a holder interrupted mid-write must
    /// never break `status`.
    fn snapshot(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut file = &self.file;
        let _ = file.rewind();
        let _ = file.read_to_end(&mut buf);
        buf
    }
}

impl Drop for LeaseFile {
    fn drop(&mut self) {
        // Explicit unlock for clarity; closing the fd would release it anyway.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Open a slot's lock file (creating it if absent — that registration is what
/// makes the slot show up in `lease status`) and take the exclusive lock.
fn acquire(path: &Path) -> std::io::Result<LeaseFile> {
    if !path.exists() {
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
        {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    let lease = LeaseFile::open(path)?;
    lease.try_lock()?;
    Ok(lease)
}

// ── command allow-list ─────────────────────────────────────────────────────────

/// File-name portion of a command path, as bytes.
fn basename_bytes(cmd: &OsStr) -> &[u8] {
    let bytes = cmd.as_bytes();
    match bytes.iter().rposition(|b| *b == b'/') {
        Some(i) => &bytes[i + 1..],
        None => bytes,
    }
}

/// The hard-coded allow-list: basename must be exactly `pi-worker` or
/// `agy-worker`. Checked before any directory, lock file or lease is touched.
fn check_command_allowed(cmd: &OsStr) -> Result<(), String> {
    let name = String::from_utf8_lossy(basename_bytes(cmd)).into_owned();
    if name != "pi-worker" && name != "agy-worker" {
        return Err(format!(
            "refusing to run {cmd:?}: only commands named 'pi-worker' or 'agy-worker' may \
             take a GPU lease (the basename of the executable is checked)"
        ));
    }
    Ok(())
}

// ── affinity map ───────────────────────────────────────────────────────────────

/// Read `{key: slot}` from `affinity.json`; missing or corrupt means empty.
fn read_affinity(path: &Path) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return map;
    };
    if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&text) {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                map.insert(k, s.to_owned());
            }
        }
    }
    map
}

/// Persist `key -> slot` atomically: write a temp file in the same directory,
/// then rename over the target.
fn write_affinity(path: &Path, key: &str, slot: &str) -> std::io::Result<()> {
    let mut map = read_affinity(path);
    map.insert(key.to_owned(), slot.to_owned());
    let json = serde_json::to_string_pretty(&map).unwrap_or_else(|_| "{}".to_owned());

    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, json)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

// ── lease record ───────────────────────────────────────────────────────────────

/// Unix seconds (std-only timestamp).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The JSON written into the lock file while the lease is held.
fn record_json(slot: &str, replica: &str, pid: u32, program: &str, args: &[String]) -> String {
    let mut full = Vec::with_capacity(1 + args.len());
    full.push(program.to_owned());
    full.extend_from_slice(args);
    serde_json::json!({
        "slot": slot,
        "replica": replica,
        "pid": pid,
        "command": full.join(" "),
        "started_at": now_unix(),
    })
    .to_string()
}

/// Parse lock-file bytes into a record; `None` when empty or malformed.
fn parse_record(bytes: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(bytes);
    if text.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&text).ok()
}

// ── lease run ──────────────────────────────────────────────────────────────────

/// `weir lease run --slot NAME=REPLICA... [--affinity KEY] [--wait-timeout S] -- CMD...`
///
/// Returns the child's exit code (`128+signal` on a signal death),
/// [`EXIT_TIMEOUT`] when the wait expired, or [`EXIT_ERROR`] when the command is
/// rejected / the lease could not be set up.
pub async fn run(args: LeaseRunArgs) -> i32 {
    let Some(program) = args.command.first() else {
        eprintln!("weir lease: error: no command given after --");
        return EXIT_ERROR;
    };

    // Security check first: before any directory, lock file or lease.
    if let Err(msg) = check_command_allowed(OsStr::new(program)) {
        eprintln!("weir lease: error: {msg}");
        return EXIT_ERROR;
    }

    let slots = match parse_slots(&args.slots) {
        Ok(slots) => slots,
        Err(msg) => {
            eprintln!("weir lease: error: {msg}");
            return EXIT_ERROR;
        }
    };

    let Some(root) = state_root() else {
        eprintln!(
            "weir lease: error: cannot resolve a state directory \
             (set WEIR_STATE_DIR, XDG_STATE_HOME or HOME)"
        );
        return EXIT_ERROR;
    };

    // The wait loop sleeps between polls, so it runs on the blocking pool; that
    // keeps the runtime's signal driver free to forward SIGTERM/SIGINT later.
    let owned = args.clone();
    let program = program.to_owned();
    let root_for_acquire = root.clone();
    let acquired =
        match tokio::task::spawn_blocking(move || acquire_lease(&owned, &slots, &root_for_acquire))
            .await
        {
            Ok(Ok(acquired)) => acquired,
            Ok(Err(AcquireError::Timeout)) => return EXIT_TIMEOUT,
            Ok(Err(AcquireError::Io(e))) => {
                eprintln!("weir lease: error: {e}");
                return EXIT_ERROR;
            }
            Err(e) => {
                eprintln!("weir lease: error: {e}");
                return EXIT_ERROR;
            }
        };

    match run_with_lease(acquired, OsStr::new(&program)).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("weir lease: error: {e}");
            // The lease is released when `acquired` drops, on every path.
            if e.kind() == std::io::ErrorKind::NotFound {
                EXIT_NOT_FOUND
            } else {
                EXIT_SPAWN_FAILED
            }
        }
    }
}

/// Parse and validate the `--slot NAME=REPLICA` values.
fn parse_slots(raw: &[String]) -> Result<Vec<SlotSpec>, String> {
    raw.iter()
        .map(|spec| SlotSpec::parse(spec))
        .collect::<Result<Vec<_>, _>>()
}

/// A slot that was acquired, plus everything needed to run under it.
///
/// The `LeaseFile` field is never read: holding its flock *is* the lease, and it
/// is released when this value is dropped.
#[allow(dead_code)]
pub struct AcquiredLease {
    /// Slot that won the race.
    slot: SlotSpec,
    /// The flock handle — releasing it (drop) releases the slot.
    lease: LeaseFile,
    /// Time spent waiting for a free slot.
    queue_wait_ms: u64,
    /// Arguments for the spawned worker (with `{replica}` resolved) — the
    /// program name is *not* included, it is passed separately to exec.
    child_args: Vec<String>,
}

/// Why waiting for a slot stopped.
enum AcquireError {
    /// `--wait-timeout` expired with every slot busy.
    Timeout,
    /// I/O failure (state dir, open, flock).
    Io(std::io::Error),
}

/// Wait for a free slot and record the lease (lock file + affinity). Runs on the
/// blocking pool because the wait loop sleeps.
fn acquire_lease(
    args: &LeaseRunArgs,
    slots: &[SlotSpec],
    root: &Path,
) -> Result<AcquiredLease, AcquireError> {
    let (idx, lease, queue_wait_ms) = wait_for_slot(args, slots, root).map_err(|e| match e {
        WaitError::Timeout => AcquireError::Timeout,
        WaitError::Io(e) => AcquireError::Io(e),
    })?;
    let chosen = slots[idx].clone();
    // Substitute {replica} everywhere, then drop argv[0]: the child receives
    // only the arguments, the program name is passed separately to exec.
    let resolved: Vec<String> = args
        .command
        .iter()
        .map(|a| a.replace("{replica}", &chosen.replica))
        .collect();
    let (program, rest) = resolved.split_first().expect("command is non-empty");
    let child_args: Vec<String> = rest.to_vec();

    // Record the lease while still holding the lock — the full command line,
    // program included, is what the user typed.
    lease
        .write_record(&record_json(
            &chosen.name,
            &chosen.replica,
            std::process::id(),
            program,
            &child_args,
        ))
        .map_err(AcquireError::Io)?;
    if let Some(key) = &args.affinity {
        let file = root.join("affinity.json");
        if let Err(e) = write_affinity(&file, key, &chosen.name) {
            eprintln!("weir lease: warning: cannot update {}: {e}", file.display());
        }
    }

    Ok(AcquiredLease {
        slot: chosen,
        lease,
        queue_wait_ms,
        child_args,
    })
}

/// Run the child under `acquired`, print the summary line, release the lease.
async fn run_with_lease(acquired: AcquiredLease, program: &OsStr) -> std::io::Result<i32> {
    let started = Instant::now();
    let status = spawn_and_wait(program, &acquired.child_args).await?;

    // Exactly one summary line on stderr, then release the lease.
    let exit = exit_code_of(&status);
    eprintln!(
        "weir lease: slot={} replica={} queue_wait_ms={} elapsed_ms={} exit={}",
        acquired.slot.name,
        acquired.slot.replica,
        acquired.queue_wait_ms,
        started.elapsed().as_millis() as u64,
        exit
    );
    // `acquired` (and with it the flock handle) is released here.
    Ok(exit)
}

/// Exit code to re-propagate: the raw code, or `128 + signal` on signal death.
fn exit_code_of(status: &ExitStatus) -> i32 {
    match (status.code(), status.signal()) {
        (Some(code), _) => code,
        (None, Some(sig)) => 128 + sig,
        (None, None) => 1,
    }
}

/// Why `wait_for_slot` returned without a lease.
enum WaitError {
    /// Every slot stayed busy until `--wait-timeout`.
    Timeout,
    /// I/O failure.
    Io(std::io::Error),
}

/// Take the first free slot in preference order, polling every
/// [`POLL_INTERVAL`] until `--wait-timeout` expires.
fn wait_for_slot(
    args: &LeaseRunArgs,
    slots: &[SlotSpec],
    root: &Path,
) -> Result<(usize, LeaseFile, u64), WaitError> {
    std::fs::create_dir_all(leases_dir(root)).map_err(WaitError::Io)?;

    let order = preference_order(args, slots, root);
    let deadline = args
        .wait_timeout
        .map(|secs| Instant::now() + Duration::from_secs(secs));
    let waited = Instant::now();

    loop {
        for &i in &order {
            match acquire(&lock_path(root, &slots[i].name)) {
                Ok(lease) => return Ok((i, lease, waited.elapsed().as_millis() as u64)),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(WaitError::Io(e)),
            }
        }
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                eprintln!(
                    "weir lease: timeout: no slot free within {}s (slots: {})",
                    args.wait_timeout.unwrap_or(0),
                    slots
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                return Err(WaitError::Timeout);
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Slot preference: the slot that last served the affinity key first, then the
/// order given on the command line.
fn preference_order(args: &LeaseRunArgs, slots: &[SlotSpec], root: &Path) -> Vec<usize> {
    let mut order: Vec<usize> = (0..slots.len()).collect();
    let hit = args
        .affinity
        .as_ref()
        .and_then(|key| read_affinity(&root.join("affinity.json")).get(key).cloned());
    if let Some(hit) = hit {
        if let Some(pos) = slots.iter().position(|s| s.name == hit) {
            order.remove(pos);
            order.insert(0, pos);
        }
    }
    order
}

// ── child spawn + signal forwarding ────────────────────────────────────────────

/// Spawn the worker with `args` (argv[1..] only — the program name is passed
/// separately to exec) in its own process group and wait for it. If weir itself
/// receives SIGINT or SIGTERM, SIGTERM is forwarded to that process group and
/// SIGKILL follows after [`TERM_GRACE`] if the child is still alive — so the
/// child is never orphaned.
async fn spawn_and_wait(program: &OsStr, args: &[String]) -> std::io::Result<ExitStatus> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    // Own process group, so a Ctrl-C / SIGTERM aimed at weir can be forwarded to
    // the whole group with `killpg` and never leaves an orphaned worker.
    cmd.process_group(0);

    // `tokio::process::Command` mirrors the `std` builder API and adds an
    // awaitable `wait()`, which is what lets the signal forwarding below work.
    // A spawn error (NotFound / other) propagates to `run`, which maps it to
    // exit 127 / 126 and releases the lease.
    let child = cmd.spawn()?;
    // The child's process group id equals its pid (`process_group(0)`). `id()`
    // only returns None once tokio has reaped the child, which cannot happen
    // before the first await; 0 then simply means "nothing to signal".
    let pgid = child.id().unwrap_or(0) as i32;
    wait_forwarding_signals(child, pgid).await
}

async fn wait_forwarding_signals(
    mut child: tokio::process::Child,
    pgid: i32,
) -> std::io::Result<ExitStatus> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    loop {
        tokio::select! {
            status = child.wait() => return status,
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
        // Interrupted: be stubborn — SIGTERM the group, hard-kill after grace.
        kill_group(pgid, libc::SIGTERM);
        tokio::select! {
            status = child.wait() => return status,
            _ = tokio::time::sleep(TERM_GRACE) => kill_group(pgid, libc::SIGKILL),
        }
    }
}

/// `killpg(pgid, sig)`. Errors are ignored: ESRCH / EPERM mean the group is
/// already gone, and there is nothing better to do either way.
fn kill_group(pgid: i32, sig: libc::c_int) {
    if pgid <= 0 {
        return;
    }
    unsafe {
        libc::killpg(pgid, sig);
    }
}

// ── lease status ───────────────────────────────────────────────────────────────

/// One row of `weir lease status`.
#[derive(Debug, serde::Serialize)]
pub struct SlotStatus {
    pub slot: String,
    /// `"free"` or `"held"`.
    pub state: &'static str,
    /// The holder's JSON record while held, else `null`.
    pub record: Value,
}

/// `weir lease status` — probe every `<NAME>.lock` with a non-blocking flock
/// that is released immediately (the file is never truncated) and print one line
/// per slot, or a JSON array with `--json`.
pub fn status(json: bool) -> i32 {
    let rows = match collect_status() {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("weir lease: error: {e}");
            return EXIT_ERROR;
        }
    };

    if json {
        match serde_json::to_string_pretty(&rows) {
            Ok(text) => {
                println!("{text}");
                0
            }
            Err(e) => {
                eprintln!("weir lease: error: {e}");
                EXIT_ERROR
            }
        }
    } else {
        if rows.is_empty() {
            println!("(no lease slots registered under {})", leases_display());
        }
        for row in &rows {
            print_human(row);
        }
        0
    }
}

/// Sorted status rows for every lease file present. A missing leases directory
/// simply means "no slots registered".
fn collect_status() -> std::io::Result<Vec<SlotStatus>> {
    let Some(root) = state_root() else {
        return Ok(Vec::new());
    };
    let dir = leases_dir(&root);
    let mut names: Vec<String> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter_map(|e| slot_name_of(&e.file_name()))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    names.sort();

    Ok(names
        .iter()
        .map(|slot| probe(slot, &lock_path(&root, slot)))
        .collect())
}

/// `<NAME>.lock` file name → `NAME`.
fn slot_name_of(file_name: &OsStr) -> Option<String> {
    let name = file_name.to_string_lossy();
    name.strip_suffix(LOCK_SUFFIX)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Probe one slot: lockable ⇒ free (lock released on drop); busy ⇒ held, with
/// the record its holder wrote under the lock.
fn probe(slot: &str, path: &Path) -> SlotStatus {
    let mut row = SlotStatus {
        slot: slot.to_owned(),
        state: "free",
        record: Value::Null,
    };
    let Ok(lease) = LeaseFile::open(path) else {
        return row;
    };
    if lease.try_lock().is_err() {
        row.state = "held";
        row.record = parse_record(&lease.snapshot()).unwrap_or(Value::Null);
    }
    row
}

fn leases_display() -> String {
    match state_root() {
        Some(root) => leases_dir(&root).display().to_string(),
        None => "<no state dir>".to_owned(),
    }
}

/// Human-readable one-liner, e.g.
/// `gpu-a  held  pid=123 replica=a command=pi-worker -b a`.
fn print_human(row: &SlotStatus) {
    if row.state == "free" {
        println!("{:<20} free", row.slot);
        return;
    }
    let mut parts = Vec::new();
    for key in ["pid", "replica", "command", "started_at"] {
        if let Some(v) = row.record.get(key) {
            let v = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            parts.push(format!("{key}={v}"));
        }
    }
    println!("{:<20} held  {}", row.slot, parts.join(" "));
}
