//! The append-only task ledger (`<state>/ledger.jsonl`).
//!
//! One JSON object per line. Two schemas live in the same file:
//!
//! * `weir.task/1` — the full record `task run --json` printed (see
//!   [`crate::task::run`]).
//! * `weir.task-event/1` — a `clean` event, appended when a worktree and branch
//!   are removed. It only carries `id`, `at` and `cleanup`.
//!
//! Readers skip blank lines, unparsable lines and unknown schemas, so a torn
//! write (or a record from a future version) never makes `show`/`list` fail.
//! Appending opens with create+append, holds an exclusive `flock` for the
//! duration and writes `line + "\n"` in a single `write_all`, so concurrent
//! weir processes never interleave partial lines.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Schema tag of a task record.
pub const RECORD_SCHEMA: &str = "weir.task/1";
/// Schema tag of a ledger event (currently only `clean`).
pub const EVENT_SCHEMA: &str = "weir.task-event/1";

/// `diff` — the file counts and the patch file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiffInfo {
    pub files: u64,
    pub insertions: u64,
    pub deletions: u64,
    pub patch: String,
}

/// One entry of `attempts`: one wrapper invocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attempt {
    pub worker: String,
    pub replica: Option<String>,
    pub model: Option<String>,
    /// Raw wrapper exit code.
    pub exit: i32,
    /// [`crate::exit::ExitKind`] label.
    pub kind: String,
    pub queue_wait_ms: u64,
    pub elapsed_ms: u64,
    pub stderr_tail: String,
    pub truncated: bool,
}

/// One entry of `checks`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckResult {
    pub name: String,
    pub exit: i32,
    pub elapsed_ms: u64,
    pub timed_out: bool,
    pub tail: String,
}

/// The `weir.task/1` record. Field order here *is* the JSON key order, which
/// the contract pins.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskRecord {
    pub schema: String,
    pub id: String,
    pub status: String,
    pub exit: i32,
    pub error: Option<String>,
    pub repo: String,
    pub base: String,
    pub base_commit: String,
    pub branch: String,
    pub worktree: String,
    pub worker: String,
    pub replica: Option<String>,
    pub model: Option<String>,
    pub queue_wait_ms: u64,
    pub elapsed_ms: u64,
    pub created_at: i64,
    pub attempts: Vec<Attempt>,
    pub answer: String,
    pub diff: Option<DiffInfo>,
    pub checks: Vec<CheckResult>,
    pub cleanup: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_worktrees: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_branches: Option<Vec<String>>,
}

impl TaskRecord {
    /// A record with the fixed schema tag set; everything else starts empty.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            schema: RECORD_SCHEMA.to_string(),
            id: id.into(),
            status: String::new(),
            exit: 0,
            error: None,
            repo: String::new(),
            base: String::new(),
            base_commit: String::new(),
            branch: String::new(),
            worktree: String::new(),
            worker: String::new(),
            replica: None,
            model: None,
            queue_wait_ms: 0,
            elapsed_ms: 0,
            created_at: 0,
            attempts: Vec::new(),
            answer: String::new(),
            diff: None,
            checks: Vec::new(),
            cleanup: String::new(),
            extra_worktrees: None,
            extra_branches: None,
        }
    }

    /// Serialise to a single line (no trailing newline).
    pub fn to_line(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

/// A parsed ledger line.
#[derive(Debug, Clone)]
pub enum Entry {
    Record(Box<TaskRecord>),
    Clean { id: String, at: i64 },
}

/// Ledger path inside the state root.
pub fn ledger_path(state: &Path) -> PathBuf {
    state.join("ledger.jsonl")
}

/// Append one raw line under an exclusive flock, creating the file (and its
/// parent) with owner-only permissions: the ledger holds prompts and worker
/// answers, which must not be world-readable.
fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            // Owner-only, whatever the umask. `DirBuilder::mode` is still masked
            // by the umask on some platforms, so pin 0700 explicitly on the leaf
            // we created. A pre-existing directory belongs to the user and is
            // deliberately left untouched.
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // A pre-existing ledger that is not owned by us (or otherwise
            // unreadable): report it instead of dying with a bare EACCES.
            return Err(std::io::Error::new(
                e.kind(),
                format!("cannot open ledger {}: {e}", path.display()),
            ));
        }
        Err(e) => return Err(e),
    };
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // One write for record and newline: a failure between two writes would
    // leave an unterminated line that the next append glues itself onto.
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');
    let written = file.write_all(buf.as_bytes());
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
    written
}

/// Append a task record (one line, exactly what `task run --json` printed).
pub fn append_record(state: &Path, record: &TaskRecord) -> std::io::Result<()> {
    let line = record
        .to_line()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    append_line(&ledger_path(state), &line)
}

/// Append a `clean` event for `id`.
pub fn append_clean(state: &Path, id: &str, at: i64) -> std::io::Result<()> {
    /// Field order is the documented one (a struct, unlike a `json!` map).
    #[derive(Serialize)]
    struct CleanEvent<'a> {
        schema: &'a str,
        event: &'a str,
        id: &'a str,
        at: i64,
        cleanup: &'a str,
    }
    let event = CleanEvent {
        schema: EVENT_SCHEMA,
        event: "clean",
        id,
        at,
        cleanup: "removed",
    };
    let line = serde_json::to_string(&event)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    append_line(&ledger_path(state), &line)
}

/// Read every entry, skipping blank, malformed and unknown-schema lines.
pub fn read_all(state: &Path) -> Vec<Entry> {
    let text = match std::fs::read_to_string(ledger_path(state)) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(obj) = value.as_object() else {
            continue;
        };
        match obj.get("schema").and_then(|s| s.as_str()) {
            Some(RECORD_SCHEMA) => {
                if let Ok(record) = serde_json::from_value::<TaskRecord>(value.clone()) {
                    out.push(Entry::Record(Box::new(record)));
                }
            }
            Some(EVENT_SCHEMA) if obj.get("event").and_then(|e| e.as_str()) == Some("clean") => {
                if let (Some(id), Some(at)) = (
                    obj.get("id").and_then(|v| v.as_str()),
                    obj.get("at").and_then(|v| v.as_i64()),
                ) {
                    out.push(Entry::Clean {
                        id: id.to_owned(),
                        at,
                    });
                }
            }
            // Unknown schema (a future version): ignored, as documented.
            _ => {}
        }
    }
    out
}

/// The latest record per id, with any later `clean` event applied (which sets
/// `cleanup` to `removed`).
pub fn latest_by_id(state: &Path) -> BTreeMap<String, TaskRecord> {
    let mut latest: BTreeMap<String, TaskRecord> = BTreeMap::new();
    // A clean event only counts when it comes after the record we kept, so a
    // re-run of an already-cleaned id shows its fresh state again.
    let mut cleaned_at: BTreeMap<String, i64> = BTreeMap::new();
    for entry in read_all(state) {
        match entry {
            Entry::Record(record) => {
                latest.insert(record.id.clone(), *record);
            }
            Entry::Clean { id, at } => {
                cleaned_at.insert(id, at);
            }
        }
    }
    for (id, at) in cleaned_at {
        if let Some(record) = latest.get_mut(&id) {
            if at >= record.created_at {
                record.cleanup = "removed".to_string();
            }
        }
    }
    latest
}

/// One record by id (latest, clean applied).
pub fn find(state: &Path, id: &str) -> Option<TaskRecord> {
    latest_by_id(state).get(id).cloned()
}

/// All records sorted by `created_at` ascending, then id — the `task list` order.
pub fn list_sorted(state: &Path) -> Vec<TaskRecord> {
    let mut rows: Vec<TaskRecord> = latest_by_id(state).into_values().collect();
    rows.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn record(id: &str, created_at: i64, cleanup: &str) -> TaskRecord {
        let mut r = TaskRecord::new(id);
        r.status = "ok".to_string();
        r.exit = 0;
        r.repo = "/repo".to_string();
        r.base = "main".to_string();
        r.base_commit = "0".repeat(40);
        r.branch = format!("wt/{id}");
        r.worktree = format!("/wt/{id}");
        r.worker = "pi".to_string();
        r.replica = Some("a".to_string());
        r.created_at = created_at;
        r.cleanup = cleanup.to_string();
        r.attempts = vec![Attempt {
            worker: "pi".to_string(),
            replica: Some("a".to_string()),
            model: None,
            exit: 0,
            kind: "ok".to_string(),
            queue_wait_ms: 1,
            elapsed_ms: 2,
            stderr_tail: String::new(),
            truncated: false,
        }];
        r.answer = "done\n".to_string();
        r.diff = Some(DiffInfo {
            files: 1,
            insertions: 1,
            deletions: 0,
            patch: format!("/state/tasks/{id}/patch.diff"),
        });
        r.checks = vec![CheckResult {
            name: "pass".to_string(),
            exit: 0,
            elapsed_ms: 3,
            timed_out: false,
            tail: String::new(),
        }];
        r
    }

    #[test]
    fn append_two_records_and_a_clean_event_read_back() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path();

        append_record(state, &record("t-one", 100, "kept")).expect("append 1");
        append_record(state, &record("t-two", 200, "kept")).expect("append 2");
        append_clean(state, "t-one", 300).expect("clean event");

        let entries = read_all(state);
        assert_eq!(entries.len(), 3, "{entries:?}");

        let by_id = latest_by_id(state);
        assert_eq!(by_id.len(), 2);
        assert_eq!(by_id["t-one"].cleanup, "removed", "clean event not applied");
        assert_eq!(by_id["t-two"].cleanup, "kept");
        assert_eq!(find(state, "t-two").expect("find").created_at, 200);
        assert!(find(state, "nope").is_none());

        let listed = list_sorted(state);
        assert_eq!(
            listed.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["t-one", "t-two"]
        );

        // The file really is line-oriented JSONL.
        let text = std::fs::read_to_string(ledger_path(state)).expect("read ledger");
        assert_eq!(text.lines().count(), 3);
    }

    #[test]
    fn append_creates_the_file_with_owner_only_permissions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // A deliberately open umask: the ledger must still end up 0600.
        let previous = unsafe { libc::umask(0o000) };
        let result = (|| -> std::io::Result<()> {
            // The parent directory is created too, and must not stay open.
            let state = tmp.path().join("nested/state");
            append_record(&state, &record("t-perm", 1, "kept"))?;
            append_clean(&state, "t-perm", 2)?;
            let mode = std::fs::metadata(ledger_path(&state))?.permissions();
            assert_eq!(mode.mode() & 0o777, 0o600, "ledger must be 0600");
            let dir_mode = std::fs::metadata(&state)?.permissions();
            assert_eq!(dir_mode.mode() & 0o777, 0o700, "state dir must be 0700");
            Ok(())
        })();
        unsafe { libc::umask(previous) };
        result.expect("append under umask 000");
    }

    #[test]
    fn malformed_blank_and_unknown_lines_are_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path();
        let good = record("t-good", 10, "kept");
        let good_line = good.to_line().expect("serialise");
        let junk = format!(
            "\n   \nnot json at all\n{{\"unterminated\": 1\n[1,2,3]\n{}\n{}\n\
             {{\"schema\":\"weir.task-event/1\",\"event\":\"unboot\",\"id\":\"x\",\"at\":1}}\n\
             {{\"schema\":\"weir.task/99\",\"id\":\"future\"}}\n",
            good_line,
            serde_json::json!({"schema":"weir.task/1","id":"missing-fields"})
        );
        std::fs::write(ledger_path(state), junk).expect("write");

        let entries = read_all(state);
        assert_eq!(entries.len(), 1, "{entries:?}");
        match &entries[0] {
            Entry::Record(r) => assert_eq!(r.id, "t-good"),
            other => panic!("expected the good record, got {other:?}"),
        }
        assert_eq!(find(state, "t-good").expect("find").id, "t-good");
    }

    #[test]
    fn clean_before_a_newer_record_does_not_stick() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path();
        append_record(state, &record("t-re", 100, "kept")).expect("append");
        append_clean(state, "t-re", 150).expect("clean");
        assert_eq!(find(state, "t-re").expect("find").cleanup, "removed");

        // A re-run after the clean shows the fresh record, not "removed".
        append_record(state, &record("t-re", 200, "kept")).expect("append");
        assert_eq!(find(state, "t-re").expect("find").cleanup, "kept");
    }

    #[test]
    fn missing_ledger_reads_as_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(read_all(tmp.path()).is_empty());
        assert!(latest_by_id(tmp.path()).is_empty());
        assert!(list_sorted(tmp.path()).is_empty());
    }

    /// Every key appears as `"key":` and the first occurrences are in the given
    /// order (top-level keys of this schema all precede any nested duplicate).
    fn assert_key_order(line: &str, keys: &[&str]) {
        let mut last = 0usize;
        for key in keys {
            let pos = line
                .find(&format!("\"{key}\":"))
                .unwrap_or_else(|| panic!("key {key} missing in {line}"));
            assert!(pos > last, "key {key} is out of order in {line}");
            last = pos;
        }
    }

    #[test]
    fn serde_round_trip_preserves_key_order() {
        let original = record("20260921-fix-tones-3fa2", 1_790_000_000, "kept");
        let line = original.to_line().expect("serialise");
        let back: TaskRecord = serde_json::from_str(&line).expect("deserialise");
        assert_eq!(back, original, "round trip lost data");

        // The JSON key order must be exactly the contract's. A parsed `Value`
        // sorts its keys (no `preserve_order` feature), so the order is asserted
        // on the raw line: the byte offset of each `"<key>":` must grow.
        let value: serde_json::Value = serde_json::from_str(&line).expect("parse");
        assert_eq!(value.as_object().expect("object").len(), 21);
        assert_key_order(
            &line,
            &[
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
            ],
        );

        // `diff` is null when absent, and the key is still there.
        let mut nodiff = original;
        nodiff.diff = None;
        let line = nodiff.to_line().expect("serialise");
        let value: serde_json::Value = serde_json::from_str(&line).expect("parse");
        assert!(value["diff"].is_null());
        assert!(value["error"].is_null());
        assert!(value["model"].is_null());
    }

    #[test]
    fn clean_event_line_has_exactly_the_documented_keys() {
        let tmp = tempfile::tempdir().expect("tempdir");
        append_clean(tmp.path(), "abc", 42).expect("clean");
        let text = std::fs::read_to_string(ledger_path(tmp.path())).expect("read");
        let raw = text.trim();
        // Key order is checked on the raw line (a parsed `Value` would sort).
        assert!(
            raw.starts_with(&format!(
                "{{\"schema\":\"{EVENT_SCHEMA}\",\"event\":\"clean\","
            )),
            "unexpected line {raw}"
        );
        let value: serde_json::Value = serde_json::from_str(raw).expect("parse");
        assert_eq!(value.as_object().expect("object").len(), 5);
        assert_key_order(raw, &["schema", "event", "id", "at", "cleanup"]);
        assert_eq!(value["schema"], EVENT_SCHEMA);
        assert_eq!(value["event"], "clean");
        assert_eq!(value["cleanup"], "removed");
        assert_eq!(value["at"], 42);
    }
}
