// consumed by P2+
#![allow(dead_code)]

//! Typed classification of wrapper exit codes.
//!
//! Mirrors the wrapper contract: 0 ok, 124 timeout, 3 empty-or-quota,
//! 6 denied, 5 jail refused, 2 usage. Quota vs empty on exit 3 is disambiguated
//! by searching stderr for the worker's `quota_pattern`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    Ok,
    CheckFailed,
    Timeout,
    Empty,
    Quota,
    Denied,
    JailRefused,
    Usage,
    Error,
}

impl ExitKind {
    /// Classify a wrapper exit code.
    ///
    /// - 0 → `Ok`
    /// - 124 → `Timeout`
    /// - 3 → `Quota` if `quota_pattern` is Some(non-empty) and stderr contains
    ///   it, else `Empty`
    /// - 6 → `Denied`, 5 → `JailRefused`, 2 → `Usage`
    /// - anything else → `Error`
    pub fn from_wrapper(code: i32, stderr: &str, quota_pattern: Option<&str>) -> ExitKind {
        match code {
            0 => ExitKind::Ok,
            124 => ExitKind::Timeout,
            3 => match quota_pattern {
                Some(p) if !p.is_empty() && stderr.contains(p) => ExitKind::Quota,
                _ => ExitKind::Empty,
            },
            6 => ExitKind::Denied,
            5 => ExitKind::JailRefused,
            2 => ExitKind::Usage,
            _ => ExitKind::Error,
        }
    }

    /// The process exit code weir itself re-uses for this kind.
    pub fn to_process_code(self) -> i32 {
        match self {
            ExitKind::Ok => 0,
            ExitKind::CheckFailed => 10,
            ExitKind::Timeout => 124,
            ExitKind::Empty => 3,
            ExitKind::Quota => 7,
            ExitKind::Denied => 6,
            ExitKind::JailRefused => 5,
            ExitKind::Usage => 2,
            ExitKind::Error => 1,
        }
    }

    /// Stable string label (used in JSON records).
    pub fn as_str(self) -> &'static str {
        match self {
            ExitKind::Ok => "ok",
            ExitKind::CheckFailed => "check_failed",
            ExitKind::Timeout => "timeout",
            ExitKind::Empty => "empty",
            ExitKind::Quota => "quota",
            ExitKind::Denied => "denied",
            ExitKind::JailRefused => "jail_refused",
            ExitKind::Usage => "usage",
            ExitKind::Error => "error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [ExitKind; 9] = [
        ExitKind::Ok,
        ExitKind::CheckFailed,
        ExitKind::Timeout,
        ExitKind::Empty,
        ExitKind::Quota,
        ExitKind::Denied,
        ExitKind::JailRefused,
        ExitKind::Usage,
        ExitKind::Error,
    ];

    #[test]
    fn from_wrapper_direct_mappings() {
        assert_eq!(ExitKind::from_wrapper(0, "", None), ExitKind::Ok);
        assert_eq!(ExitKind::from_wrapper(124, "", None), ExitKind::Timeout);
        assert_eq!(ExitKind::from_wrapper(6, "", None), ExitKind::Denied);
        assert_eq!(ExitKind::from_wrapper(5, "", None), ExitKind::JailRefused);
        assert_eq!(ExitKind::from_wrapper(2, "", None), ExitKind::Usage);
        assert_eq!(ExitKind::from_wrapper(1, "", None), ExitKind::Error);
        assert_eq!(from_wrapper_unrelated_check(), ExitKind::Error);
    }

    fn from_wrapper_unrelated_check() -> ExitKind {
        ExitKind::from_wrapper(137, "killed", Some("RESOURCE_EXHAUSTED"))
    }

    #[test]
    fn from_wrapper_exit3_quota_vs_empty() {
        // pattern present in stderr => Quota
        assert_eq!(
            ExitKind::from_wrapper(
                3,
                "agy-worker: status=ok error=...RESOURCE_EXHAUSTED...",
                Some("RESOURCE_EXHAUSTED")
            ),
            ExitKind::Quota
        );
        // pattern absent from stderr => Empty
        assert_eq!(
            ExitKind::from_wrapper(3, "some other failure", Some("RESOURCE_EXHAUSTED")),
            ExitKind::Empty
        );
        // pattern None => Empty
        assert_eq!(
            ExitKind::from_wrapper(3, "RESOURCE_EXHAUSTED", None),
            ExitKind::Empty
        );
        // empty-string pattern => Empty (never matches everything)
        assert_eq!(
            ExitKind::from_wrapper(3, "RESOURCE_EXHAUSTED", Some("")),
            ExitKind::Empty
        );
    }

    #[test]
    fn to_process_code_mapping() {
        assert_eq!(ExitKind::Ok.to_process_code(), 0);
        assert_eq!(ExitKind::CheckFailed.to_process_code(), 10);
        assert_eq!(ExitKind::Timeout.to_process_code(), 124);
        assert_eq!(ExitKind::Empty.to_process_code(), 3);
        assert_eq!(ExitKind::Quota.to_process_code(), 7);
        assert_eq!(ExitKind::Denied.to_process_code(), 6);
        assert_eq!(ExitKind::JailRefused.to_process_code(), 5);
        assert_eq!(ExitKind::Usage.to_process_code(), 2);
        assert_eq!(ExitKind::Error.to_process_code(), 1);
    }

    #[test]
    fn as_str_mapping() {
        assert_eq!(ExitKind::Ok.as_str(), "ok");
        assert_eq!(ExitKind::CheckFailed.as_str(), "check_failed");
        assert_eq!(ExitKind::Timeout.as_str(), "timeout");
        assert_eq!(ExitKind::Empty.as_str(), "empty");
        assert_eq!(ExitKind::Quota.as_str(), "quota");
        assert_eq!(ExitKind::Denied.as_str(), "denied");
        assert_eq!(ExitKind::JailRefused.as_str(), "jail_refused");
        assert_eq!(ExitKind::Usage.as_str(), "usage");
        assert_eq!(ExitKind::Error.as_str(), "error");
    }

    #[test]
    fn as_str_and_process_codes_are_unique() {
        let mut strs: Vec<&str> = ALL.iter().map(|k| k.as_str()).collect();
        strs.sort_unstable();
        strs.dedup();
        assert_eq!(strs.len(), ALL.len());

        let mut codes: Vec<i32> = ALL.iter().map(|k| k.to_process_code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), ALL.len());
    }
}
