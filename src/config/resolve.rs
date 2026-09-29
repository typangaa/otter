//! Config file location.
//!
//! Resolution order (first hit wins):
//!
//! 1. `--config PATH` (or the `WEIR_CONFIG` env var, applied by `clap`).
//! 2. `$XDG_CONFIG_HOME/weir/weir.toml`
//! 3. `$HOME/.config/weir/weir.toml`
//!
//! **There is deliberately no fallback to `./weir.toml`.** A repo-controlled
//! file must never decide which binaries weir executes; a clone containing
//! `weir.toml` would otherwise point `weir chat` at an attacker-chosen
//! wrapper. When nothing is found the error lists every path considered.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::error::{Result, WeirError};

/// Suffix appended to the config directory to form the config file name.
const CONFIG_FILE: &str = "weir.toml";

/// Pure config-path resolution; no process state is read.
///
/// * `explicit` — value of `--config` / `WEIR_CONFIG`. When set it is returned
///   **as-is even if the file does not exist**, so the caller reports the
///   requested path instead of silently falling back to another config.
///   An empty value is treated as unset.
/// * `xdg_config_home` — value of `$XDG_CONFIG_HOME`. Considered only when it
///   is set, non-empty and the resulting file exists.
/// * `home` — value of `$HOME`. Considered only when set, non-empty and the
///   resulting `~/.config/weir/weir.toml` exists.
///
/// Returns [`WeirError::Config`] whose message starts with
/// `"no weir config found; searched:"` and lists every path considered when no
/// candidate is usable.
pub fn resolve_config_path(
    explicit: Option<&Path>,
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }

    let mut searched: Vec<PathBuf> = Vec::new();

    if let Some(dir) = non_empty_os(xdg_config_home) {
        let candidate = dir.join("weir").join(CONFIG_FILE);
        searched.push(candidate);
    }
    if let Some(dir) = non_empty_os(home) {
        let candidate = dir.join(".config").join("weir").join(CONFIG_FILE);
        searched.push(candidate);
    }

    if let Some(hit) = searched.iter().find(|c| c.is_file()) {
        return Ok(hit.clone());
    }

    Err(not_found(&searched))
}

/// [`resolve_config_path`] reading the real process environment.
pub fn resolve(explicit: Option<&Path>) -> Result<PathBuf> {
    resolve_config_path(
        explicit,
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// An [`OsStr`] that is present and not empty, as a [`Path`].
fn non_empty_os(v: Option<&OsStr>) -> Option<&Path> {
    match v {
        // XDG spec: relative values must be ignored (no back-door CWD lookup).
        Some(s) if !s.is_empty() && Path::new(s).is_absolute() => Some(Path::new(s)),
        _ => None,
    }
}

/// The "nothing found" error, listing every path considered.
fn not_found(searched: &[PathBuf]) -> WeirError {
    let mut msg = String::from("no weir config found; searched:"); // marker: keep in sync with tests
    if searched.is_empty() {
        msg.push_str(
            "\n  (no search directory available: neither XDG_CONFIG_HOME nor HOME is set)",
        );
    }
    for p in searched {
        msg.push_str(&format!("\n  - {}", p.display()));
    }
    msg.push_str("\n  (--config / WEIR_CONFIG were not set)");
    msg.push_str("\n  (note: ./weir.toml in the current directory is intentionally not searched)");
    WeirError::Config(msg)
}

/// The message used when `--config` points at a file that does not exist.
pub fn missing_explicit(path: &Path) -> WeirError {
    WeirError::Config(format!(
        "config file not found: {} (--config / WEIR_CONFIG was set; ./weir.toml in the current directory is intentionally not searched)",
        path.display()
    ))
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create `<dir>/weir/weir.toml` (or `<dir>/.config/weir/weir.toml`).
    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "version = 2\n").unwrap();
    }

    /// An `OsString` for a path (test helper).
    fn os(p: &Path) -> std::ffi::OsString {
        p.as_os_str().to_os_string()
    }

    /// The [`WeirError::Config`] payload without the `Display` prefix.
    fn raw(err: &WeirError) -> String {
        match err {
            WeirError::Config(m) => m.clone(),
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn explicit_wins_over_xdg_and_home() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path().join("xdg");
        let home = tmp.path().join("home");
        touch(&xdg.join("weir").join(CONFIG_FILE));
        touch(&home.join(".config").join("weir").join(CONFIG_FILE));
        let explicit = tmp.path().join("given.toml");

        let got = resolve_config_path(
            Some(&explicit),
            Some(os(&xdg).as_os_str()),
            Some(os(&home).as_os_str()),
        )
        .unwrap();
        assert_eq!(got, explicit);
    }

    #[test]
    fn explicit_is_returned_even_when_the_file_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.toml");
        let got = resolve_config_path(Some(&missing), None, None).unwrap();
        assert_eq!(got, missing);
    }

    #[test]
    fn xdg_wins_over_home_when_both_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path().join("xdg");
        let home = tmp.path().join("home");
        let xdg_file = xdg.join("weir").join(CONFIG_FILE);
        let home_file = home.join(".config").join("weir").join(CONFIG_FILE);
        touch(&xdg_file);
        touch(&home_file);

        let got = resolve_config_path(
            None,
            Some(os(&xdg).as_os_str()),
            Some(os(&home).as_os_str()),
        )
        .unwrap();
        assert_eq!(got, xdg_file);
    }

    #[test]
    fn missing_xdg_file_falls_through_to_home() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path().join("xdg"); // deliberately left empty
        let home = tmp.path().join("home");
        let home_file = home.join(".config").join("weir").join(CONFIG_FILE);
        touch(&home_file);

        let got = resolve_config_path(
            None,
            Some(os(&xdg).as_os_str()),
            Some(os(&home).as_os_str()),
        )
        .unwrap();
        assert_eq!(got, home_file);
    }

    #[test]
    fn empty_env_values_count_as_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        touch(&home.join(".config").join("weir").join(CONFIG_FILE));
        let home_os = os(&home);

        // Empty XDG => only the HOME candidate is considered.
        let got = resolve_config_path(None, Some(OsStr::new("")), Some(&home_os)).unwrap();
        assert_eq!(got, home.join(".config").join("weir").join(CONFIG_FILE));

        // Empty HOME contributes no candidate; empty XDG likewise, so with
        // both empty nothing is searched at all.
        let err =
            resolve_config_path(None, Some(OsStr::new("")), Some(OsStr::new(""))).unwrap_err();
        let msg = raw(&err);
        assert!(msg.starts_with("no weir config found; searched:"), "{msg}");
        assert!(msg.contains("no search directory available"), "{msg}");
        // Empty XDG (with a usable HOME) searches HOME only — and finds it.
        let err = resolve_config_path(None, Some(OsStr::new("")), Some(&home_os)).unwrap();
        assert_eq!(err, home.join(".config").join("weir").join(CONFIG_FILE));
        // With a HOME that holds no config, the HOME path is the only line.
        let empty_home = tmp.path().join("empty-home");
        let err = resolve_config_path(
            None,
            Some(OsStr::new("")),
            Some(os(&empty_home).as_os_str()),
        )
        .unwrap_err();
        let msg = raw(&err);
        assert!(
            msg.contains(
                &empty_home
                    .join(".config")
                    .join("weir")
                    .join(CONFIG_FILE)
                    .display()
                    .to_string()
            ),
            "{msg}"
        );
        assert!(!msg.contains(&home.display().to_string()), "{msg}");

        // An empty --config is treated as unset by clap's env handling and by
        // `resolve`: it contributes no explicit path, so the candidates apply.
        let empty_home = tmp.path().join("nohome");
        let err = resolve_config_path(Some(Path::new("")), None, Some(os(&empty_home).as_os_str()))
            .unwrap();
        assert_eq!(err, PathBuf::from(""));
    }

    #[test]
    fn nothing_found_lists_both_paths_and_the_current_dir_note() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path().join("xdg");
        let home = tmp.path().join("home");
        let err = resolve_config_path(
            None,
            Some(os(&xdg).as_os_str()),
            Some(os(&home).as_os_str()),
        )
        .unwrap_err();
        match err {
            WeirError::Config(ref m) => {
                assert!(m.starts_with("no weir config found; searched:"), "{m}");
                assert!(
                    m.contains(&xdg.join("weir").join(CONFIG_FILE).display().to_string()),
                    "{m}"
                );
                assert!(
                    m.contains(
                        &home
                            .join(".config")
                            .join("weir")
                            .join(CONFIG_FILE)
                            .display()
                            .to_string()
                    ),
                    "{m}"
                );
                assert!(m.contains("--config / WEIR_CONFIG were not set"), "{m}");
                assert!(m.contains("./weir.toml"), "{m}");
                // One path per line.
                assert_eq!(m.lines().count(), 1 + 2 + 2, "{m}");
            }
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn weir_toml_in_the_current_directory_is_never_returned() {
        // A repo-controlled ./weir.toml must never be picked up, even when the
        // searched directories hold nothing.
        let cwd = tempfile::tempdir().unwrap();
        let repo = cwd.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join(CONFIG_FILE), "version = 2\n").unwrap();

        let xdg = cwd.path().join("xdg");
        let home = cwd.path().join("home");
        let err = resolve_config_path(
            None,
            Some(os(&xdg).as_os_str()),
            Some(os(&home).as_os_str()),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains(&repo.join(CONFIG_FILE).display().to_string()),
            "{msg}"
        );
        assert!(msg.contains("./weir.toml"), "{msg}");
    }

    #[test]
    fn no_env_at_all_reports_no_search_directory() {
        let err = resolve_config_path(None, None, None).unwrap_err();
        let msg = raw(&err);
        assert!(msg.starts_with("no weir config found; searched:"), "{msg}");
        assert!(msg.contains("no search directory available"), "{msg}");
    }

    #[test]
    fn directories_named_but_not_files_do_not_match() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path().join("xdg");
        // A *directory* at the candidate location is not a config file.
        fs::create_dir_all(xdg.join("weir").join(CONFIG_FILE)).unwrap();
        assert!(resolve_config_path(None, Some(os(&xdg).as_os_str()), None).is_err());
    }

    #[test]
    fn wrapper_uses_the_real_environment() {
        // `resolve` must obey an explicit path without touching the env.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("c.toml");
        assert_eq!(resolve(Some(&p)).unwrap(), p);
    }
}
