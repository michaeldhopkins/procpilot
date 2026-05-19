use std::fmt;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use crate::cmd_display::CmdDisplay;

/// Maximum bytes of stdout/stderr retained on `NonZeroExit`, `Timeout`, and
/// `Cancelled` error variants. Anything beyond this is dropped from the front
/// (FIFO), keeping the most recent output — usually the most relevant for
/// debugging.
pub const STREAM_SUFFIX_SIZE: usize = 128 * 1024;

/// Error type for subprocess execution.
///
/// Distinguishes between:
/// - [`Spawn`](Self::Spawn): infrastructure failure (binary missing, fork failed, etc.)
/// - [`NonZeroExit`](Self::NonZeroExit): the command ran and reported failure via exit code
/// - [`Timeout`](Self::Timeout): the command was killed after exceeding its timeout
/// - [`Cancelled`](Self::Cancelled): the caller signaled cancellation via
///   [`Cmd::cancel`](crate::Cmd::cancel)
///
/// All variants carry a [`CmdDisplay`] that formats the command shell-style
/// for logging (with secret redaction if the command was marked `.secret()`).
///
/// `NonZeroExit`, `Timeout`, and `Cancelled` variants carry the **last 128 KiB**
/// of stdout and stderr (capped by [`STREAM_SUFFIX_SIZE`]) — enough context to
/// debug most failures, bounded so a runaway process can't blow up your error
/// path's memory.
///
/// # Attempt count
///
/// `NonZeroExit`, `Timeout`, and `Cancelled` carry an `attempts` field
/// (read via [`attempts`](Self::attempts)) — the 1-based index of the attempt
/// on which this error occurred. For a `Cmd` without [`retry`](crate::Cmd::retry),
/// this is always 1. For a retried command, it tells the caller how many tries
/// happened before the final error (e.g. `attempts == 3` on a default policy
/// means the retry budget was exhausted). `Spawn` doesn't carry it because
/// `Spawn` failures by default don't trigger retries.
///
/// Marked `#[non_exhaustive]` so future variants can be added without
/// breaking callers. Match with a wildcard arm to handle unknown variants
/// defensively.
///
/// ```no_run
/// # use procpilot::{Cmd, RunError};
/// let cmd = Cmd::new("git").args(&["show", "maybe-missing-ref"]);
/// let maybe_bytes = match cmd.run() {
///     Ok(output) => Some(output.stdout),
///     Err(RunError::NonZeroExit { .. }) => None,   // ref not found
///     Err(e) => return Err(e.into()),              // real failure bubbles up
/// };
/// # Ok::<(), anyhow::Error>(())
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum RunError {
    /// Failed to spawn the child process. The binary may be missing, the
    /// working directory may not exist, or the OS may have refused the fork.
    Spawn {
        command: CmdDisplay,
        source: io::Error,
    },
    /// The child process ran but exited non-zero. `stdout`/`stderr` carry
    /// the last [`STREAM_SUFFIX_SIZE`] bytes captured before exit (empty
    /// for inherited stderr/stdout). `attempts` is the 1-based retry
    /// attempt on which this failure occurred (1 if `.retry()` was not set).
    NonZeroExit {
        command: CmdDisplay,
        status: ExitStatus,
        stdout: Vec<u8>,
        stderr: String,
        attempts: u32,
    },
    /// The child process was killed after exceeding the caller's timeout.
    /// `elapsed` records how long the process ran. `stdout`/`stderr` carry
    /// any output collected before the kill signal. `attempts` is the
    /// 1-based retry attempt on which the timeout fired.
    Timeout {
        command: CmdDisplay,
        elapsed: Duration,
        stdout: Vec<u8>,
        stderr: String,
        attempts: u32,
    },
    /// The caller signaled cancellation via [`Cmd::cancel`](crate::Cmd::cancel).
    /// The child was killed (`SIGTERM`, then `SIGKILL` after the grace
    /// period on Unix; immediate `TerminateProcess` on Windows).
    /// `stdout`/`stderr` carry any output captured before the kill.
    /// `attempts` is the 1-based retry attempt that was in flight when
    /// cancellation fired; if the flag was already set before the first
    /// spawn, `attempts == 1` and the captured streams are empty.
    Cancelled {
        command: CmdDisplay,
        stdout: Vec<u8>,
        stderr: String,
        attempts: u32,
    },
}

impl RunError {
    /// The command that failed, formatted for display (shell-quoted,
    /// secret-redacted).
    pub fn command(&self) -> &CmdDisplay {
        match self {
            Self::Spawn { command, .. } => command,
            Self::NonZeroExit { command, .. } => command,
            Self::Timeout { command, .. } => command,
            Self::Cancelled { command, .. } => command,
        }
    }

    /// The program name. Convenience for `self.command().program()`.
    pub fn program(&self) -> &std::ffi::OsStr {
        self.command().program()
    }

    /// The captured stderr suffix, if any. None for spawn failures.
    ///
    /// Stderr is stored after lossy UTF-8 decoding — any invalid sequences
    /// from the child appear as U+FFFD replacement characters. Callers that
    /// truly need raw bytes should inspect the child's stderr directly via
    /// [`Cmd::spawn`](crate::Cmd::spawn).
    pub fn stderr(&self) -> Option<&str> {
        match self {
            Self::NonZeroExit { stderr, .. } => Some(stderr),
            Self::Timeout { stderr, .. } => Some(stderr),
            Self::Cancelled { stderr, .. } => Some(stderr),
            Self::Spawn { .. } => None,
        }
    }

    /// The captured stdout suffix, if any. None for spawn failures.
    pub fn stdout(&self) -> Option<&[u8]> {
        match self {
            Self::NonZeroExit { stdout, .. } => Some(stdout),
            Self::Timeout { stdout, .. } => Some(stdout),
            Self::Cancelled { stdout, .. } => Some(stdout),
            Self::Spawn { .. } => None,
        }
    }

    /// The exit status, if the process actually ran to completion.
    /// None for spawn failures, timeouts, and cancellations.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        match self {
            Self::NonZeroExit { status, .. } => Some(*status),
            Self::Spawn { .. } | Self::Timeout { .. } | Self::Cancelled { .. } => None,
        }
    }

    /// The 1-based retry attempt number on which this error occurred.
    ///
    /// Always 1 for `Spawn` (spawn failures don't trigger retries by
    /// default; this method returns 1 rather than `Option<u32>` to keep
    /// the API ergonomic). For `NonZeroExit`/`Timeout`/`Cancelled`, this
    /// is the attempt on which the failure occurred — e.g. 3 on a default
    /// `RetryPolicy` means the retry budget was fully consumed.
    pub fn attempts(&self) -> u32 {
        match self {
            Self::Spawn { .. } => 1,
            Self::NonZeroExit { attempts, .. } => *attempts,
            Self::Timeout { attempts, .. } => *attempts,
            Self::Cancelled { attempts, .. } => *attempts,
        }
    }

    /// Whether this error represents a non-zero exit (the command ran and reported failure).
    pub fn is_non_zero_exit(&self) -> bool {
        matches!(self, Self::NonZeroExit { .. })
    }

    /// Whether this error represents a spawn failure (couldn't start the process).
    pub fn is_spawn_failure(&self) -> bool {
        matches!(self, Self::Spawn { .. })
    }

    /// Whether this error represents a timeout (process killed after exceeding its time budget).
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout { .. })
    }

    /// Whether this error represents caller-driven cancellation via
    /// [`Cmd::cancel`](crate::Cmd::cancel).
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled { .. })
    }

    /// Stamp this error with the attempt number it was produced on.
    /// Used by the retry loop to backfill `attempts` after the operation
    /// returns. No-op for `Spawn`.
    pub(crate) fn with_attempts(mut self, n: u32) -> Self {
        match &mut self {
            Self::NonZeroExit { attempts, .. }
            | Self::Timeout { attempts, .. }
            | Self::Cancelled { attempts, .. } => *attempts = n,
            Self::Spawn { .. } => {}
        }
        self
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { command, source } => {
                write!(f, "failed to spawn `{command}`: {source}")
            }
            Self::NonZeroExit {
                command,
                status,
                stderr,
                ..
            } => {
                let trimmed = stderr.trim();
                if trimmed.is_empty() {
                    write!(f, "`{command}` exited with {status}")
                } else {
                    write!(f, "`{command}` exited with {status}: {trimmed}")
                }
            }
            Self::Timeout {
                command,
                elapsed,
                stderr,
                ..
            } => {
                let trimmed = stderr.trim();
                if trimmed.is_empty() {
                    write!(
                        f,
                        "`{command}` killed after timeout ({:.1}s)",
                        elapsed.as_secs_f64()
                    )
                } else {
                    write!(
                        f,
                        "`{command}` killed after timeout ({:.1}s); last stderr: {trimmed}",
                        elapsed.as_secs_f64()
                    )
                }
            }
            Self::Cancelled {
                command, stderr, ..
            } => {
                let trimmed = stderr.trim();
                if trimmed.is_empty() {
                    write!(f, "`{command}` cancelled by caller")
                } else {
                    write!(
                        f,
                        "`{command}` cancelled by caller; last stderr: {trimmed}"
                    )
                }
            }
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            Self::NonZeroExit { .. } | Self::Timeout { .. } | Self::Cancelled { .. } => None,
        }
    }
}

/// Truncate a byte buffer to retain only the last `STREAM_SUFFIX_SIZE` bytes,
/// dropping from the front (oldest first). Used by the runner to bound
/// captured output before constructing an error.
pub(crate) fn truncate_suffix(mut buf: Vec<u8>) -> Vec<u8> {
    if buf.len() > STREAM_SUFFIX_SIZE {
        let drop = buf.len() - STREAM_SUFFIX_SIZE;
        buf.drain(..drop);
    }
    buf
}

/// String version of [`truncate_suffix`].
pub(crate) fn truncate_suffix_string(s: String) -> String {
    if s.len() <= STREAM_SUFFIX_SIZE {
        return s;
    }
    // Drop from the front. UTF-8 safe: find a char boundary.
    let drop = s.len() - STREAM_SUFFIX_SIZE;
    let mut start = drop;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cd(prog: &str, args: &[&str]) -> CmdDisplay {
        CmdDisplay::new(
            prog.into(),
            args.iter().map(|s| std::ffi::OsString::from(*s)).collect(),
            false,
        )
    }

    fn spawn_error() -> RunError {
        RunError::Spawn {
            command: cd("git", &["status"]),
            source: io::Error::new(io::ErrorKind::NotFound, "not found"),
        }
    }

    fn non_zero_exit(stderr: &str) -> RunError {
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
            command: cd("git", &["status"]),
            status,
            stdout: Vec::new(),
            stderr: stderr.to_string(),
            attempts: 1,
        }
    }

    fn timeout_error() -> RunError {
        RunError::Timeout {
            command: cd("git", &["fetch"]),
            elapsed: Duration::from_secs(30),
            stdout: Vec::new(),
            stderr: "Fetching origin".into(),
            attempts: 1,
        }
    }

    fn cancelled_error() -> RunError {
        RunError::Cancelled {
            command: cd("git", &["log"]),
            stdout: Vec::new(),
            stderr: "Working".into(),
            attempts: 1,
        }
    }

    #[test]
    fn program_returns_name() {
        assert_eq!(spawn_error().program(), std::ffi::OsStr::new("git"));
        assert_eq!(non_zero_exit("").program(), std::ffi::OsStr::new("git"));
        assert_eq!(timeout_error().program(), std::ffi::OsStr::new("git"));
        assert_eq!(cancelled_error().program(), std::ffi::OsStr::new("git"));
    }

    #[test]
    fn stderr_only_for_completed_or_killed() {
        assert_eq!(spawn_error().stderr(), None);
        assert_eq!(non_zero_exit("boom").stderr(), Some("boom"));
        assert_eq!(timeout_error().stderr(), Some("Fetching origin"));
        assert_eq!(cancelled_error().stderr(), Some("Working"));
    }

    #[test]
    fn stdout_only_for_completed_or_killed() {
        assert!(spawn_error().stdout().is_none());
        assert!(non_zero_exit("").stdout().is_some());
        assert!(timeout_error().stdout().is_some());
        assert!(cancelled_error().stdout().is_some());
    }

    #[test]
    fn exit_status_only_for_non_zero_exit() {
        assert!(spawn_error().exit_status().is_none());
        assert!(non_zero_exit("").exit_status().is_some());
        assert!(timeout_error().exit_status().is_none());
        assert!(cancelled_error().exit_status().is_none());
    }

    #[test]
    fn predicates() {
        assert!(spawn_error().is_spawn_failure());
        assert!(non_zero_exit("").is_non_zero_exit());
        assert!(timeout_error().is_timeout());
        assert!(cancelled_error().is_cancelled());
    }

    #[test]
    fn attempts_default_to_one() {
        assert_eq!(spawn_error().attempts(), 1);
        assert_eq!(non_zero_exit("").attempts(), 1);
        assert_eq!(timeout_error().attempts(), 1);
        assert_eq!(cancelled_error().attempts(), 1);
    }

    #[test]
    fn with_attempts_sets_value_on_retry_aware_variants() {
        assert_eq!(non_zero_exit("").with_attempts(3).attempts(), 3);
        assert_eq!(timeout_error().with_attempts(5).attempts(), 5);
        assert_eq!(cancelled_error().with_attempts(2).attempts(), 2);
    }

    #[test]
    fn with_attempts_noop_on_spawn() {
        // Spawn doesn't track attempts; with_attempts must not change it.
        assert_eq!(spawn_error().with_attempts(7).attempts(), 1);
    }

    #[test]
    fn display_spawn_failure() {
        let msg = format!("{}", spawn_error());
        assert!(msg.contains("spawn"));
        assert!(msg.contains("git status"));
    }

    #[test]
    fn display_non_zero_exit_with_stderr() {
        let msg = format!("{}", non_zero_exit("something broke"));
        assert!(msg.contains("git status"));
        assert!(msg.contains("something broke"));
    }

    #[test]
    fn display_timeout() {
        let msg = format!("{}", timeout_error());
        assert!(msg.contains("git fetch"));
        assert!(msg.contains("timeout"));
    }

    #[test]
    fn display_cancelled() {
        let msg = format!("{}", cancelled_error());
        assert!(msg.contains("git log"));
        assert!(msg.contains("cancelled"));
    }

    #[test]
    fn error_source_for_spawn() {
        use std::error::Error;
        assert!(spawn_error().source().is_some());
    }

    #[test]
    fn error_source_none_for_non_spawn() {
        use std::error::Error;
        assert!(non_zero_exit("").source().is_none());
        assert!(timeout_error().source().is_none());
        assert!(cancelled_error().source().is_none());
    }

    #[test]
    fn wraps_into_anyhow() {
        let err = spawn_error();
        let _: anyhow::Error = err.into();
    }

    #[test]
    fn truncate_suffix_keeps_short_buffers() {
        let v = vec![1, 2, 3];
        assert_eq!(truncate_suffix(v.clone()), v);
    }

    #[test]
    fn truncate_suffix_drops_from_front() {
        let big: Vec<u8> = (0..(STREAM_SUFFIX_SIZE + 100) as u32)
            .map(|n| (n & 0xff) as u8)
            .collect();
        let truncated = truncate_suffix(big.clone());
        assert_eq!(truncated.len(), STREAM_SUFFIX_SIZE);
        assert_eq!(truncated.last(), big.last());
    }

    #[test]
    fn truncate_suffix_string_keeps_short() {
        let s = String::from("hello");
        assert_eq!(truncate_suffix_string(s), "hello");
    }

    #[test]
    fn truncate_suffix_string_drops_from_front() {
        let s = "x".repeat(STREAM_SUFFIX_SIZE + 100);
        let truncated = truncate_suffix_string(s);
        assert_eq!(truncated.len(), STREAM_SUFFIX_SIZE);
    }

    #[test]
    fn truncate_suffix_string_preserves_utf8() {
        // Construct a string where the boundary cut would land mid-codepoint
        // if not handled. Each 'é' is 2 bytes.
        let s = format!("{}{}", "x".repeat(STREAM_SUFFIX_SIZE), "é".repeat(50));
        let truncated = truncate_suffix_string(s);
        // Must be valid UTF-8 (compiler-enforced via String type, but make
        // sure we didn't panic on a non-boundary index).
        assert!(!truncated.is_empty());
    }

    #[test]
    fn secret_command_in_display() {
        let cmd_secret =
            CmdDisplay::new("docker".into(), vec!["login".into(), "hunter2".into()], true);
        let err = RunError::Spawn {
            command: cmd_secret,
            source: io::Error::new(io::ErrorKind::NotFound, "missing"),
        };
        let msg = format!("{err}");
        assert!(!msg.contains("hunter2"), "secret leaked: {msg}");
        assert!(msg.contains("docker"));
        assert!(msg.contains("<secret>"));
    }
}
