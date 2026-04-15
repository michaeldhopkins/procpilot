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
/// `Spawn` (binary-missing errors don't get better) or `Timeout` (a hung
/// process retried is still hung).
pub fn default_transient(err: &RunError) -> bool {
    match err {
        RunError::NonZeroExit { stderr, .. } => {
            stderr.contains("stale") || stderr.contains(".lock")
        }
        RunError::Spawn { .. } | RunError::Timeout { .. } => false,
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
        }
    }

    #[test]
    fn default_transient_retries_stale() {
        assert!(default_transient(&fake_non_zero("The working copy is stale")));
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
    fn custom_predicate() {
        let policy = RetryPolicy::default().when(|err| match err {
            RunError::NonZeroExit { stderr, .. } => stderr.contains("network"),
            _ => false,
        });
        assert!((policy.predicate)(&fake_non_zero("network unreachable")));
        assert!(!(policy.predicate)(&fake_non_zero(".lock")));
    }

    #[test]
    fn debug_does_not_panic() {
        let _ = format!("{:?}", RetryPolicy::default());
    }
}
