//! Every git invocation the task lifecycle makes.
//!
//! The program name is the constant [`GIT_PROGRAM`] — never taken from config,
//! the CLI or the environment. Each call runs with the target repo as
//! `current_dir` (`git -C`), detaches stdin, and captures stdout/stderr so a
//! failure becomes a message in the record instead of a panic.

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Output, Stdio};

/// The only program this module can exec.
pub const GIT_PROGRAM: &str = "git";

/// Run `git -C <repo> <args...>` with stdin detached and both pipes captured.
///
/// A spawn failure (git not on PATH) is reported as an `Output` with code 127
/// and the spawn error in stderr, so callers keep a single code path.
fn git(repo: &Path, args: &[&OsStr]) -> Output {
    let mut cmd = Command::new(GIT_PROGRAM);
    cmd.arg("-C");
    cmd.arg(repo);
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    match cmd.output() {
        Ok(out) => out,
        Err(e) => Output {
            status: exit_code(127),
            stdout: Vec::new(),
            stderr: format!("cannot run {GIT_PROGRAM}: {e}").into_bytes(),
        },
    }
}

/// A synthetic `ExitStatus` from a raw code (used only for the spawn-failure
/// path, where no real wait status exists).
#[cfg(unix)]
fn exit_code(code: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(code << 8)
}

/// stderr of a failed git call, trimmed and lossily decoded.
fn err_text(out: &Output) -> String {
    let text = String::from_utf8_lossy(&out.stderr);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        format!("git exited with {}", out.status)
    } else {
        trimmed.to_string()
    }
}

/// `git rev-parse --git-dir`: is `repo` a git repository (worktrees included)?
pub fn is_git_repo(repo: &Path) -> bool {
    git(repo, &[OsStr::new("rev-parse"), OsStr::new("--git-dir")])
        .status
        .success()
}

/// Resolve a commit-ish to a full sha. `None` when it does not resolve.
pub fn resolve_commit(repo: &Path, rev: &str) -> Option<String> {
    let rev = format!("{rev}^{{commit}}");
    let out = git(
        repo,
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            OsStr::new(&rev),
        ],
    );
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// `git worktree add -b wt/<id> <path> <base_commit>`.
pub fn worktree_add(
    repo: &Path,
    branch: &str,
    path: &Path,
    base_commit: &str,
) -> Result<(), String> {
    let out = git(
        repo,
        &[
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("-b"),
            OsStr::new(branch),
            path.as_os_str(),
            OsStr::new(base_commit),
        ],
    );
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git worktree add -b {branch} {} {base_commit}: {}",
            path.display(),
            err_text(&out)
        ))
    }
}

/// `git worktree remove --force <path>` (best effort; a missing worktree is not
/// an error worth failing the clean over).
pub fn worktree_remove(repo: &Path, path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let out = git(
        repo,
        &[
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--force"),
            path.as_os_str(),
        ],
    );
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git worktree remove --force {}: {}",
            path.display(),
            err_text(&out)
        ))
    }
}

/// `git worktree prune`.
pub fn worktree_prune(repo: &Path) -> Result<(), String> {
    let out = git(repo, &[OsStr::new("worktree"), OsStr::new("prune")]);
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("git worktree prune: {}", err_text(&out)))
    }
}

/// `git branch -D <branch>`: ignores "not found", reports anything else.
///
/// `branch` is a FULL branch name (`wt/<id>`), probed as `refs/heads/<branch>`.
/// It must not go through [`branch_exists`], which takes a bare task id.
pub fn branch_delete(repo: &Path, branch: &str) -> Result<(), String> {
    if !ref_exists(repo, &format!("refs/heads/{branch}")) {
        return Ok(());
    }
    let out = git(
        repo,
        &[OsStr::new("branch"), OsStr::new("-D"), OsStr::new(branch)],
    );
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("git branch -D {branch}: {}", err_text(&out)))
    }
}

/// Does branch `wt/<id>` exist as a local branch (used for id collision checks)?
pub fn branch_exists(repo: &Path, id: &str) -> bool {
    ref_exists(repo, &format!("refs/heads/wt/{id}"))
}

/// Does this exact refname exist? Takes the full refname (`refs/heads/...`).
fn ref_exists(repo: &Path, refname: &str) -> bool {
    git(
        repo,
        &[
            OsStr::new("show-ref"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            OsStr::new(refname),
        ],
    )
    .status
    .success()
}

/// `git merge-base --is-ancestor <branch> <base>`: is the branch merged?
pub fn branch_merged(repo: &Path, branch: &str, base: &str) -> bool {
    git(
        repo,
        &[
            OsStr::new("merge-base"),
            OsStr::new("--is-ancestor"),
            OsStr::new(branch),
            OsStr::new(base),
        ],
    )
    .status
    .success()
}

/// Whether the worktree has uncommitted changes (tracked or untracked). A
/// worktree that cannot be inspected counts as dirty: the caller uses this to
/// refuse destruction, so doubt must err on the side of keeping it.
pub fn worktree_dirty(wt: &Path) -> bool {
    if !wt.exists() {
        return false;
    }
    let out = git(wt, &[OsStr::new("status"), OsStr::new("--porcelain")]);
    !out.status.success() || !out.stdout.is_empty()
}

/// One parsed `git diff --numstat` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumstatLine {
    /// Added lines; `None` for a binary file (git prints `-`).
    pub insertions: Option<u64>,
    /// Deleted lines; `None` for a binary file.
    pub deletions: Option<u64>,
    pub path: String,
}

/// Parse `git diff --numstat` output. Every line has exactly three tab
/// separated fields; the rename/copy forms fold old and new name into ONE
/// field (`old => new`, or `dir/{a => b}` inside a shared prefix):
///
/// ```text
/// N<TAB>M<TAB>path           normal
/// -<TAB>-<TAB>path           binary
/// -<TAB>-<TAB>old => new     rename/copy without content counts
/// N<TAB>M<TAB>old => new     rename/copy with content changes
/// ```
///
/// The *destination* path is what matters to us, so a rename is counted once,
/// at its new location.
pub fn parse_numstat(text: &str) -> Vec<NumstatLine> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        // `--numstat` emits the counts FIRST and the path last; the rename forms
        // fold both names into that single last field. Any other field count is
        // junk and gets skipped.
        let [ins, del, raw_path] = parts.as_slice() else {
            continue;
        };
        let ins = *ins;
        let del = *del;
        let Some(path) = rename_dest(raw_path) else {
            continue;
        };
        out.push(NumstatLine {
            insertions: parse_count(ins),
            deletions: parse_count(del),
            path: unquote_c_style(&path),
        });
    }
    out
}

/// The destination path of a possibly rename-annotated path field:
/// `a/b.txt => a/c.txt` yields `a/c.txt`, `dir/{a.txt => b.txt}` yields
/// `dir/b.txt`. A field with an unterminated `{` yields `None` (junk).
fn rename_dest(raw: &str) -> Option<String> {
    let Some((_, rest)) = raw.split_once(" => ") else {
        return Some(raw.to_string());
    };
    match raw.split_once('{') {
        // No brace: the whole field is `old => new`, destination is `rest`.
        None => Some(rest.to_string()),
        // `{...}` form: destination = prefix before `{` + the name after
        // `=> ` inside the braces + whatever follows the `}`.
        Some((prefix, _)) => {
            let arrow = raw.find("=> ")? + 3;
            let close = raw[arrow..].find('}')?;
            let inside = &raw[arrow..arrow + close];
            // Everything after the LAST `}` belongs to the destination too.
            let suffix = raw.rsplit_once('}')?.1;
            Some(format!("{prefix}{inside}{suffix}"))
        }
    }
}

/// A numstat count: digits, or `-` for binary (reported as `None`, i.e. 0).
fn parse_count(field: &str) -> Option<u64> {
    field.trim().parse::<u64>().ok()
}

/// Undo git's C-style quoting of a path (`"foo\tn.txt"`).
fn unquote_c_style(raw: &str) -> String {
    let bytes = raw.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return raw.to_string();
    }
    let inner = &raw[1..raw.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// The `diff` counts: `files`, `insertions`, `deletions` from
/// `git diff --numstat <base>` run in the worktree, after `git add -N .` so
/// untracked files count. Binary files count as a file with 0 lines changed.
pub fn diff_stat(wt: &Path, base_commit: &str) -> (u64, u64, u64) {
    // Intent-to-add: without it, brand-new files stay invisible to `diff`.
    git(wt, &[OsStr::new("add"), OsStr::new("-N"), OsStr::new(".")]);
    let out = git(
        wt,
        &[
            OsStr::new("diff"),
            OsStr::new("--numstat"),
            OsStr::new(base_commit),
        ],
    );
    if !out.status.success() {
        return (0, 0, 0);
    }
    let mut files = 0u64;
    let mut insertions = 0u64;
    let mut deletions = 0u64;
    for line in parse_numstat(&String::from_utf8_lossy(&out.stdout)) {
        files += 1;
        insertions += line.insertions.unwrap_or(0);
        deletions += line.deletions.unwrap_or(0);
    }
    (files, insertions, deletions)
}

/// Write `git diff --binary <base_commit>` to `patch_path` (created with mode
/// 0600 inside the task dir). Returns the number of bytes written.
pub fn write_patch(wt: &Path, base_commit: &str, patch_path: &Path) -> Result<u64, String> {
    let out = git(
        wt,
        &[
            OsStr::new("diff"),
            OsStr::new("--binary"),
            OsStr::new(base_commit),
        ],
    );
    if !out.status.success() {
        return Err(format!(
            "git diff --binary {base_commit}: {}",
            err_text(&out)
        ));
    }
    write_private_file(patch_path, &out.stdout)
        .map_err(|e| format!("cannot write patch {}: {e}", patch_path.display()))?;
    Ok(out.stdout.len() as u64)
}

/// Write `contents` to `path` with mode 0600, creating parent directories.
pub fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        // create_dir_all honours the umask for the directories it makes; force
        // 0700 so the task dir never ends up group/other readable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?
        .write_all(contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    /// A git command pinned to this test's own identity and config files, so
    /// unit tests never depend on the developer's ~/.gitconfig. Never touches
    /// the process environment.
    fn test_git(repo: &Path, args: &[&str]) -> Output {
        let mut cmd = Command::new(GIT_PROGRAM);
        cmd.current_dir(repo);
        cmd.args(args);
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.output()
            .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"))
    }

    fn init_repo(dir: &Path) {
        init_repo_with(dir, &[]);
    }

    /// `init_repo` plus, when `files` is non-empty, a second commit adding them.
    /// The files are created on disk before that commit.
    fn init_repo_with(dir: &Path, files: &[(&str, &[u8])]) {
        std::fs::create_dir_all(dir).expect("create repo dir");
        let mut args_list: Vec<Vec<&str>> = vec![
            vec!["init", "-q", "-b", "main"],
            vec![
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "seed",
            ],
        ];
        if !files.is_empty() {
            for (name, contents) in files {
                std::fs::write(dir.join(name), *contents).expect("write fixture");
            }
            let mut add: Vec<&str> = vec!["add"];
            add.extend(files.iter().map(|(name, _)| *name));
            args_list.push(add);
            args_list.push(vec![
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "-m",
                "fixtures",
            ]);
        }
        for args in args_list {
            let out = test_git(dir, &args);
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn numstat_parsing_covers_every_shape() {
        let text = "1\t2\tREADME.md\n-\t-\tassets/logo.png\n0\t0\tnew-empty.txt\n";
        let lines = parse_numstat(text);
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            NumstatLine {
                insertions: Some(1),
                deletions: Some(2),
                path: "README.md".to_string()
            }
        );
        // Binary: `-` counts as nothing (None => 0 in the caller).
        assert_eq!(
            lines[1],
            NumstatLine {
                insertions: None,
                deletions: None,
                path: "assets/logo.png".to_string()
            }
        );
        assert_eq!(lines[2].insertions, Some(0));

        // Trailing newline, blank lines and junk lines.
        assert_eq!(parse_numstat("\n\n1\t1\tx\n").len(), 1);
        assert_eq!(parse_numstat("").len(), 0);
        assert_eq!(parse_numstat("not-numstat\n").len(), 0);

        // Renames are counted once, at the destination.
        let ren = parse_numstat("3\t1\told/name.txt => new/name.txt\n");
        assert_eq!(ren.len(), 1);
        assert_eq!(ren[0].path, "new/name.txt");
        assert_eq!(ren[0].insertions, Some(3));
        assert_eq!(ren[0].deletions, Some(1));

        let ren_bin = parse_numstat("-\t-\tdir/{a.txt => b.txt}\n");
        assert_eq!(ren_bin.len(), 1);
        assert_eq!(ren_bin[0].path, "dir/b.txt");
        assert_eq!(ren_bin[0].insertions, None);

        // C-style quoted paths are unquoted.
        let q = parse_numstat("1\t0\t\"weird\\tname.txt\"\n");
        assert_eq!(q[0].path, "weird\tname.txt");
    }

    #[test]
    fn diff_stat_counts_a_real_edit_including_binary_and_untracked() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        // The base commit already contains README.md and a tracked binary, so
        // the edits below are real modifications, not new-file additions.
        let readme: &[u8] = b"line 1\nline 2\n";
        let base_blob: &[u8] = &[0u8, 1, 2, 0, 255, 0];
        init_repo_with(&repo, &[("README.md", readme), ("blob.bin", base_blob)]);

        let wt = tmp.path().join("wt");
        let base = resolve_commit(&repo, "main").expect("base sha");
        std::fs::create_dir_all(wt.parent().expect("parent")).expect("parents");
        worktree_add(&repo, "wt/test-1", &wt, &base).expect("worktree add");

        // The edits live in the worktree, not in the primary checkout.
        std::fs::write(wt.join("README.md"), "line 1 changed\nline 2\nline 3\n").expect("edit");
        std::fs::write(wt.join("new.txt"), "fresh\n").expect("fresh");
        std::fs::write(wt.join("blob.bin"), [0u8, 1, 2, 0, 255, 0, 7, 8]).expect("edit blob");

        let (files, insertions, deletions) = diff_stat(&wt, &base);
        assert_eq!(files, 3, "README.md + new.txt + blob.bin");
        // README.md: line 1 rewritten (+1 -1) and line 3 added (+1) => +2 -1.
        // new.txt: one new line => +1. blob.bin: binary, counts as a file with
        // 0 changed lines. Totals: +3 -1.
        assert_eq!(insertions, 3);
        assert_eq!(deletions, 1);

        let patch = tmp.path().join("patch.diff");
        let bytes = write_patch(&wt, &base, &patch).expect("write patch");
        assert!(bytes > 0);
        let text = std::fs::read_to_string(&patch).expect("read patch");
        assert!(text.contains("README.md"), "{text}");
        assert!(text.contains("new.txt"), "{text}");
        // The tracked binary must be carried as a real binary patch.
        assert!(text.contains("GIT binary patch"), "{text}");
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&patch)
                .expect("meta")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn repo_and_branch_probes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        assert!(is_git_repo(&repo));
        assert!(!is_git_repo(tmp.path()));
        assert!(resolve_commit(&repo, "main").is_some());
        assert!(resolve_commit(&repo, "no-such-rev").is_none());
        assert!(!branch_exists(&repo, "nope"));

        let wt = tmp.path().join("wt/task-branch");
        let base = resolve_commit(&repo, "main").expect("sha");
        worktree_add(&repo, "wt/task-branch", &wt, &base).expect("add");
        assert!(branch_exists(&repo, "task-branch"));
        // A fresh branch whose tip equals the base IS an ancestor of it, so the
        // ancestor test reports it as merged.
        assert!(branch_merged(&repo, "wt/task-branch", &base));
        // git refuses to delete a branch a worktree has checked out: remove and
        // prune the worktree first, then the delete works.
        worktree_remove(&repo, &wt).expect("remove worktree");
        worktree_prune(&repo).expect("prune");
        branch_delete(&repo, "wt/task-branch").expect("delete");
        assert!(!branch_exists(&repo, "task-branch"));
        // Deleting again is a no-op, not an error.
        branch_delete(&repo, "wt/task-branch").expect("delete twice");
    }

    #[test]
    fn merged_branch_detection_uses_ancestor_test() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let base = resolve_commit(&repo, "main").expect("sha");

        // Commit on a side branch: NOT merged into main.
        test_git(
            &repo,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "checkout",
                "-q",
                "-b",
                "wt/side",
            ],
        );
        std::fs::write(repo.join("x.txt"), "x\n").expect("write");
        test_git(&repo, &["add", "x.txt"]);
        let out = test_git(
            &repo,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "-m",
                "side",
            ],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!branch_merged(&repo, "wt/side", &base));
        assert!(!branch_merged(&repo, "wt/side", "main"));
    }

    #[test]
    fn worktree_remove_and_prune_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let base = resolve_commit(&repo, "main").expect("sha");
        let wt = tmp.path().join("wt/gone");
        worktree_add(&repo, "wt/gone", &wt, &base).expect("add");
        assert!(wt.exists());
        worktree_remove(&repo, &wt).expect("remove");
        assert!(!wt.exists());
        worktree_prune(&repo).expect("prune");
        // Removing an already-absent worktree is fine.
        worktree_remove(&repo, &wt).expect("remove again");
    }

    #[test]
    fn failing_git_returns_a_message_not_a_panic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = worktree_add(
            tmp.path(),
            "wt/x",
            &tmp.path().join("wt/x"),
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        )
        .expect_err("must fail");
        assert!(err.contains("git worktree add"), "{err}");
    }
}
