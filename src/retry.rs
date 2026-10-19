//! Retry policy for [`Cmd`](crate::Cmd).
//!
//! Wraps `backon`'s `ExponentialBuilder` with sane defaults and a
//! transient-error predicate.

use std::sync::Arc;
use std::time::Duration;

use backon::ExponentialBuilder;

use crate::error::RunError;

/// How retries are scheduled and which errors trigger them.
///
/// The default policy retries up to 3 times with exponential backoff
/// (100ms → 200ms → 400ms, with jitter). The default [predicate](default_transient)
/// only retries [`RunError::NonZeroExit`] whose stderr matches `"stale"` or
/// `".lock"` (jj working-copy staleness, git/jj lock-file contention).
///
/// [`RunError::Spawn`] and [`RunError::Timeout`] are **never** retried by the
/// default predicate — a missing binary doesn't become available, and a hung
/// process retried is still hung. Users who want to retry either can supply
/// a custom predicate via [`RetryPolicy::when`] or [`Cmd::retry_when`](crate::Cmd::retry_when).
#[derive(Clone)]
pub struct RetryPolicy {
    pub(crate) backoff: ExponentialBuilder,
    pub(crate) predicate: Arc<dyn Fn(&RunError) -> bool + Send + Sync>,
}

impl RetryPolicy {
    /// Construct a policy with custom backoff and the default predicate.
    pub fn with_backoff(backoff: ExponentialBuilder) -> Self {
        Self {
            backoff,
            predicate: Arc::new(default_transient),
        }
    }

    /// Replace the predicate. The predicate receives a [`RunError`] and
    /// returns whether to retry.
    pub fn when(mut self, f: impl Fn(&RunError) -> bool + Send + Sync + 'static) -> Self {
        self.predicate = Arc::new(f);
        self
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            backoff: ExponentialBuilder::default()
                .with_factor(2.0)
                .with_min_delay(Duration::from_millis(100))
                .with_max_times(3)
                .with_jitter(),
            predicate: Arc::new(default_transient),
        }
    }
}

impl std::fmt::Debug for RetryPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryPolicy")
            .field("backoff", &"<ExponentialBuilder>")
            .field("predicate", &"<closure>")
            .finish()
    }
}

/// Default transient-error predicate.
///
/// Retries on `NonZeroExit` whose stderr contains `"stale"` (jj working-copy
/// staleness) or `".lock"` (git/jj lock-file contention). Never retries
/// `Spawn` (binary-missing errors don't get better), `Timeout` (a hung
/// process retried is still hung), or `Cancelled` (the caller asked to
/// stop — retrying would contradict that).
pub fn default_transient(err: &RunError) -> bool {
    match err {
        RunError::NonZeroExit { stderr, .. } => {
            stderr.contains("stale") || stderr.contains(".lock")
        }
        RunError::Spawn { .. } | RunError::Timeout { .. } | RunError::Cancelled { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd_display::CmdDisplay;
    use std::io;

    fn cd(prog: &str) -> CmdDisplay {
        CmdDisplay::new(prog.into(), vec![], false)
    }

    fn fake_non_zero(stderr: &str) -> RunError {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(256)
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(1)
        };
        RunError::NonZeroExit {
            command: cd("x"),
            status,
            stdout: vec![],
            stderr: stderr.to_string(),
            attempts: 1,
        }
    }

    fn fake_spawn() -> RunError {
        RunError::Spawn {
            command: cd("x"),
            source: io::Error::new(io::ErrorKind::NotFound, "missing"),
        }
    }

    fn fake_timeout() -> RunError {
        RunError::Timeout {
            command: cd("x"),
            elapsed: Duration::from_secs(30),
            stdout: vec![],
            stderr: String::new(),
            attempts: 1,
        }
    }

    #[test]
    fn default_transient_retries_stale() {
        assert!(default_transient(&fake_non_zero(
            "The working copy is stale"
        )));
    }

    #[test]
    fn default_transient_retries_lock() {
        assert!(default_transient(&fake_non_zero(
            "fatal: Unable to create '/repo/.git/index.lock'"
        )));
    }

    #[test]
    fn default_transient_skips_other_nonzero() {
        assert!(!default_transient(&fake_non_zero("invalid revision")));
    }

    #[test]
    fn default_transient_skips_spawn() {
        assert!(!default_transient(&fake_spawn()));
    }

    #[test]
    fn default_transient_skips_timeout() {
        assert!(!default_transient(&fake_timeout()));
    }

    #[test]
    fn default_transient_skips_cancelled() {
        let cancelled = RunError::Cancelled {
            command: cd("x"),
            stdout: vec![],
            stderr: String::new(),
            attempts: 1,
        };
        assert!(!default_transient(&cancelled));
    }

    #[test]
    fn custom_predicate() {
        let policy = RetryPolicy::default().when(|err| match err {
            RunError::NonZeroExit { stderr, .. } => stderr.contains("network"),
            _ => false,
        });
        assert!((policy.predicate)(&fake_non_zero("network unreachable")));
        assert!(!(policy.predicate)(&fake_non_zero(".lock")));
    }

    #[test]
    fn with_backoff_keeps_custom_schedule_and_default_predicate() {
        use backon::BackoffBuilder;
        let policy = RetryPolicy::with_backoff(ExponentialBuilder::default().with_max_times(0));
        assert_eq!(policy.backoff.build().next(), None);
        assert!((policy.predicate)(&fake_non_zero(
            "The working copy is stale"
        )));
        assert!(!(policy.predicate)(&fake_spawn()));
    }

    #[test]
    fn debug_includes_struct_and_fields() {
        let s = format!("{:?}", RetryPolicy::default());
        assert!(
            s.contains("RetryPolicy") && s.contains("<ExponentialBuilder>"),
            "{s}"
        );
    }

    mod properties {
        use proptest::prelude::*;

        /// `default_transient` for `NonZeroExit` is exactly: stderr contains
        /// `"stale"` or `".lock"`. A small reference oracle. Any disagreement
        /// between this and the production code is a real bug.
        fn non_zero_exit_should_retry_ref(stderr: &str) -> bool {
            stderr.contains("stale") || stderr.contains(".lock")
        }

        proptest! {
            /// `default_transient(&fake_non_zero(s))` agrees with the substring
            /// reference for every stderr string. This is the defining
            /// invariant of the predicate: it does no more, no less, than
            /// look for those two substrings. Arbitrary strings via
            /// `"a\\p{ASCII}*"` keep the bytes exactly what we feed the
            /// oracle, so the comparison is meaningful.
            #[test]
            fn default_transient_matches_substring_reference_for_non_zero_exit(
                stderr in "\\p{ASCII}*",
            ) {
                let err = super::fake_non_zero(&stderr);
                let got = super::default_transient(&err);
                let want = non_zero_exit_should_retry_ref(&stderr);
                prop_assert_eq!(got, want, "stderr = {:?}", stderr);
            }

            /// `default_transient` is false on `Spawn`, `Timeout`, and
            /// `Cancelled` — regardless of what stderr the caller put in
            /// the error. These variants report infrastructure / control
            /// failures that don't improve on retry, so the predicate must
            /// refuse to schedule a retry for them. (Arbitrary stderr
            /// catches the bug where someone accidentally drops into the
            /// `NonZeroExit` arm by variant.)
            #[test]
            fn default_transient_never_retries_non_running_variants(
                stderr in "\\p{ASCII}*",
            ) {
                // Spawn: carry no stderr field at all in this variant, so
                // we test that even when stderr is *implicit* the answer
                // is false. Then we also exhaustively check the other two
                // variants with arbitrary stderr strings, including ones
                // that *would* match the substring rule — they must still
                // be refused.
                prop_assert!(!super::default_transient(&super::fake_spawn()));
                prop_assert!(!super::default_transient(&super::fake_timeout()));
                let cancelled = super::RunError::Cancelled {
                    command: super::cd("x"),
                    stdout: vec![],
                    stderr,
                    attempts: 1,
                };
                prop_assert!(!super::default_transient(&cancelled));
            }

            /// Metamorphic property: the predicate on a concatenation
            /// equals the OR of the predicate on each half. This is
            /// the algebraic statement of "the predicate looks only
            /// for substrings — split the message anywhere and the
            /// verdict is the disjunction of the two halves". It's
            /// a stronger claim than the per-string oracle: it
            /// constrains the predicate to be *purely substring-based*
            /// and forbids implementations that inspect character
            /// counts, lengths, frequencies, or any other feature
            /// that could behave differently on a concatenation.
            #[test]
            fn default_transient_on_concatenation_is_or_of_halves(
                left in "\\p{ASCII}*",
                right in "\\p{ASCII}*",
            ) {
                let combined = format!("{left}{right}");
                let got_combined =
                    super::default_transient(&super::fake_non_zero(&combined));
                let got_left =
                    super::default_transient(&super::fake_non_zero(&left));
                let got_right =
                    super::default_transient(&super::fake_non_zero(&right));
                prop_assert_eq!(
                    got_combined, got_left || got_right,
                    "combined = {:?}, left verdict = {}, right verdict = {}",
                    combined, got_left, got_right,
                );
            }

            /// `RetryPolicy::default()`'s predicate is the same function
            /// as the free-standing `default_transient`. The two paths
            /// into the default policy (the type's `Default` impl and
            /// the free function) must agree on every error variant.
            #[test]
            fn retry_policy_default_uses_default_transient_predicate(
                stderr in "\\p{ASCII}*",
            ) {
                let policy = super::RetryPolicy::default();
                let nz = super::fake_non_zero(&stderr);
                prop_assert_eq!((policy.predicate)(&nz), super::default_transient(&nz));
                prop_assert_eq!((policy.predicate)(&super::fake_spawn()), super::default_transient(&super::fake_spawn()));
                prop_assert_eq!((policy.predicate)(&super::fake_timeout()), super::default_transient(&super::fake_timeout()));
                let cancelled = super::RunError::Cancelled {
                    command: super::cd("x"),
                    stdout: vec![],
                    stderr,
                    attempts: 1,
                };
                prop_assert_eq!((policy.predicate)(&cancelled), super::default_transient(&cancelled));
            }
        }
    }
}
