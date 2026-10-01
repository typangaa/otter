//! Task id allocation and validation.
//!
//! A generated id has the shape `<yyyymmdd>-<slug>-<4hex>`: the UTC date of
//! admission, a slug derived from the prompt file stem, and four hex digits of
//! entropy mixed from the clock's nanoseconds and the process id (no rand
//! crate). An explicit `--id` must be a single safe path component so that
//! `tasks/<ID>` and `wt/<ID>` can never escape the state dir or name a
//! qualified branch.

use std::hash::{Hash, Hasher};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Longest slug we keep, so the whole id stays comfortably short.
const SLUG_MAX: usize = 24;
/// Slug used when there is no file stem to derive one from (stdin, or a stem
/// with no `[a-z0-9]` at all).
const DEFAULT_SLUG: &str = "task";
/// How many times a *generated* id may collide before we give up.
const MAX_ID_RETRIES: usize = 8;

/// Days since the Unix epoch for `secs` (floor division, so times before the
/// epoch would still round down correctly).
fn days_from_epoch(secs: i64) -> i64 {
    secs.div_euclid(86_400)
}

/// Civil calendar date (year, month, day) from days since 1970-01-01, using
/// Howard Hinnant's `civil_from_days` algorithm. std has no calendar support,
/// and no new crate may be added.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // 0..146096
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // 0..399
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // 0..365
    let mp = (5 * doy + 2) / 153; // 0..11, March-based
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // 1..31
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // 1..12
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `yyyymmdd` for a Unix timestamp, interpreted in UTC.
pub fn yyyymmdd_from_unix(secs: i64) -> String {
    let (y, m, d) = civil_from_days(days_from_epoch(secs));
    format!("{y:04}{m:02}{d:02}")
}

/// Slug from a prompt file path: the file stem, lowercased, every character
/// outside `[a-z0-9]` replaced by `-`, runs of `-` collapsed, leading and
/// trailing `-` trimmed, max [`SLUG_MAX`] characters (trimmed again so it never
/// ends on a `-`). Empty results become [`DEFAULT_SLUG`].
pub fn slug_for_prompt(path: Option<&Path>) -> String {
    let stem = match path {
        Some(p) => p.file_stem().map(|s| s.to_string_lossy().into_owned()),
        None => None,
    };
    let stem = stem.unwrap_or_default();
    let mut out = String::new();
    for c in stem.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let mut out: String = out.chars().take(SLUG_MAX).collect();
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        DEFAULT_SLUG.to_string()
    } else {
        out
    }
}

/// Validate an explicit `--id`: a single safe path component,
/// `^[a-z0-9][a-z0-9-]{0,63}$`. Anything else could escape `tasks/<ID>` or
/// produce a qualified git branch name.
pub fn validate_explicit_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "task run: --id {id:?} must match ^[a-z0-9][a-z0-9-]{{0,63}}$ \
             (lowercase letters, digits and '-', starting with a letter or digit)"
        ))
    }
}

/// Four hex digits mixed from the clock's sub-second nanoseconds and the pid.
fn hex4(nanos: u64, pid: u64) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    nanos.hash(&mut hasher);
    pid.hash(&mut hasher);
    format!("{:04x}", (hasher.finish() >> 16) as u32 & 0xffff)
}

/// One candidate id for this run (date + slug + entropy).
pub fn candidate_id(unix_secs: i64, slug: &str, nanos: u64, pid: u64) -> String {
    format!(
        "{}-{}-{}",
        yyyymmdd_from_unix(unix_secs),
        slug,
        hex4(nanos, pid)
    )
}

/// Why a candidate id is unavailable, for the usage-error message.
pub struct Collision {
    pub reason: String,
}

impl Collision {
    /// `tasks/<id>` dir, `wt/<id>` branch or `<wt_root>/<id>` path already
    /// exists. `None` means the candidate is free.
    pub fn check(id: &str, tasks_dir: &Path, repo: &Path, wt_root: &Path) -> Option<Collision> {
        if tasks_dir.join(id).exists() {
            return Some(Collision {
                reason: format!("{} already exists", tasks_dir.join(id).display()),
            });
        }
        if crate::task::git::branch_exists(repo, id) {
            return Some(Collision {
                reason: format!("branch wt/{id} already exists"),
            });
        }
        if wt_root.join(id).exists() {
            return Some(Collision {
                reason: format!("{} already exists", wt_root.join(id).display()),
            });
        }
        None
    }

    /// Read the current time, derive entropy, and return an id that does not
    /// collide — regenerating up to [`MAX_ID_RETRIES`] times.
    pub fn allocate(
        slug: &str,
        tasks_dir: &Path,
        repo: &Path,
        wt_root: &Path,
    ) -> Result<String, Collision> {
        let mut pid = std::process::id() as u64;
        for _ in 0..MAX_ID_RETRIES {
            let now = SystemTime::now();
            let secs = now
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let nanos = now
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(0);
            let id = candidate_id(secs, slug, nanos, pid);
            match Collision::check(&id, tasks_dir, repo, wt_root) {
                // Free: this is the id.
                None => return Ok(id),
                // Taken: bump the pid input so the next tick of the same clock
                // yields a different suffix.
                Some(_) => pid = pid.wrapping_add(1),
            }
        }
        Err(Collision {
            reason: format!("could not allocate a free task id within {MAX_ID_RETRIES} attempts"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yyyymmdd_from_fixed_timestamps() {
        assert_eq!(yyyymmdd_from_unix(0), "19700101");
        // 1790000000 = 2026-09-21 14:13:20 UTC, i.e. 20717 days after the
        // epoch (20717 * 86400 = 1789948800, remainder 51200 s).
        assert_eq!(yyyymmdd_from_unix(1_790_000_000), "20260921");
        assert_eq!(yyyymmdd_from_unix(1_700_000_000), "20231114");
        // Leap-day boundaries and the end-of-year rollover.
        assert_eq!(yyyymmdd_from_unix(1_709_208_000), "20240229");
        assert_eq!(yyyymmdd_from_unix(1_709_294_400), "20240301");
        assert_eq!(yyyymmdd_from_unix(1_735_689_599), "20241231");
        assert_eq!(yyyymmdd_from_unix(1_735_689_600), "20250101");
        // Before the epoch rounds down (floor), not towards zero.
        assert_eq!(yyyymmdd_from_unix(-1), "19691231");
    }

    #[test]
    fn slug_sanitises_the_prompt_stem() {
        let cases = [
            ("Fix-Tones_in Repo!", "fix-tones-in-repo"),
            ("résumé", "r-sum"),
            ("a--b__c", "a-b-c"),
            ("  spaced  out  name  ", "spaced-out-name"),
            ("UPPER", "upper"),
            ("v2.1 draft", "v2-1-draft"),
        ];
        for (stem, want) in cases {
            let p = std::path::Path::new("/tmp/prompts").join(format!("{stem}.md"));
            assert_eq!(slug_for_prompt(Some(&p)), want, "stem {stem:?}");
        }
    }

    #[test]
    fn slug_truncates_to_24_without_a_trailing_dash() {
        let p = std::path::Path::new("/tmp/abcdefghijklmnopqrstuvwxyz99.md");
        let slug = slug_for_prompt(Some(p));
        assert_eq!(slug.len(), 24);
        assert_eq!(slug, "abcdefghijklmnopqrstuvwx");
    }

    #[test]
    fn slug_truncation_landing_on_a_separator_trims_it() {
        // The 24th character of the sanitised stem is the separator here, so the
        // 24-char truncation ends on '-' and that dash gets trimmed away.
        let p = std::path::Path::new("/tmp/abcdefghijklmnopqrstuvw-xyz.md");
        let slug = slug_for_prompt(Some(p));
        assert_eq!(slug, "abcdefghijklmnopqrstuvw");
        assert!(!slug.ends_with('-'));
    }

    #[test]
    fn slug_falls_back_to_task() {
        assert_eq!(slug_for_prompt(None), "task");
        // A stem with no ASCII alphanumeric at all.
        let p = std::path::Path::new("/tmp/---.md");
        assert_eq!(slug_for_prompt(Some(p)), "task");
        // `..` has no file stem.
        let p = std::path::Path::new("/tmp/a/..");
        assert_eq!(slug_for_prompt(Some(p)), "task");
    }

    #[test]
    fn explicit_id_validation() {
        for good in [
            "a",
            "0",
            "fix-tones",
            "20260922-fix-tones-3fa2",
            "a".repeat(64).as_str(),
        ] {
            assert!(
                validate_explicit_id(good).is_ok(),
                "{good:?} must be accepted"
            );
        }
        for bad in [
            "", "-", "-x", "A", "Fix", "a_b", "a b", "a/b", "a\\b", "..", ".", "a.b", "wt/x",
            "\u{00e9}",
        ] {
            assert!(
                validate_explicit_id(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
        // 65 characters is one too many.
        let long = "a".repeat(65);
        assert!(validate_explicit_id(&long).is_err());
    }

    #[test]
    fn candidate_id_has_the_documented_shape() {
        let id = candidate_id(1_790_000_000, "fix-tones", 123_456, 4242);
        let bytes = id.as_bytes();
        assert!(bytes[..8].iter().all(|b| b.is_ascii_digit()), "{id}");
        assert!(id.starts_with("20260921-fix-tones-"), "{id}");
        let hex = id.rsplit('-').next().unwrap();
        assert_eq!(hex.len(), 4, "{id}");
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{id}"
        );

        // Different entropy gives a different suffix; the same inputs do not.
        let other = candidate_id(1_790_000_000, "fix-tones", 654_321, 4242);
        assert_ne!(id, other);
        assert_eq!(
            other,
            candidate_id(1_790_000_000, "fix-tones", 654_321, 4242)
        );
    }
}
