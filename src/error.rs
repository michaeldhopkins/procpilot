use std::fmt;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;

/// Error type for subprocess execution.
///
/// Distinguishes between:
/// - [`Spawn`](Self::Spawn): infrastructure failure (binary missing, fork failed, etc.)
/// - [`NonZeroExit`](Self::NonZeroExit): the command ran and reported failure via exit code
/// - [`Timeout`](Self::Timeout): the command was killed after exceeding its timeout
///
/// Marked `#[non_exhaustive]` so future variants can be added without breaking callers.
/// Match with a wildcard arm to handle unknown variants defensively.
///
/// ```no_run
/// # use procpilot::{run_cmd, RunError};
/// let maybe_bytes = match run_cmd("git", &["show", "maybe-missing-ref"]) {
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
        program: String,
        source: io::Error,
    },
    /// The child process ran but exited non-zero. For captured commands,
    /// `stdout` and `stderr` contain what the process wrote before exiting.
    /// For inherited commands ([`crate::run_cmd_inherited`]), they are empty.
    NonZeroExit {
        program: String,
        args: Vec<String>,
        status: ExitStatus,
        stdout: Vec<u8>,
        stderr: String,
    },
    /// The child process was killed after exceeding the caller's timeout.
    ///
    /// Any output written to stdout/stderr before the kill signal is included
    /// when available. The `elapsed` field records how long the process ran.
    Timeout {
        program: String,
        args: Vec<String>,
        elapsed: Duration,
        stdout: Vec<u8>,
        stderr: String,
    },
}

impl RunError {
    /// The program name that failed (e.g., `"git"`, `"jj"`).
    pub fn program(&self) -> &str {
        match self {
            Self::Spawn { program, .. } => program,
            Self::NonZeroExit { program, .. } => program,
            Self::Timeout { program, .. } => program,
        }
    }

    /// The captured stderr, if any. None for spawn failures.
    pub fn stderr(&self) -> Option<&str> {
        match self {
            Self::NonZeroExit { stderr, .. } => Some(stderr),
            Self::Timeout { stderr, .. } => Some(stderr),
            Self::Spawn { .. } => None,
        }
    }

    /// The exit status, if the process actually ran to completion.
    /// None for spawn failures and timeouts.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        match self {
            Self::NonZeroExit { status, .. } => Some(*status),
            Self::Spawn { .. } | Self::Timeout { .. } => None,
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
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { program, source } => {
                write!(f, "failed to spawn {program}: {source}")
            }
            Self::NonZeroExit {
                program,
                args,
                status,
                stderr,
                ..
            } => {
                let trimmed = stderr.trim();
                if trimmed.is_empty() {
                    write!(f, "{program} {} exited with {status}", args.join(" "))
                } else {
                    write!(
                        f,
                        "{program} {} exited with {status}: {trimmed}",
                        args.join(" ")
                    )
                }
            }
            Self::Timeout {
                program,
                args,
                elapsed,
                ..
            } => {
                write!(
                    f,
                    "{program} {} killed after timeout ({:.1}s)",
                    args.join(" "),
                    elapsed.as_secs_f64()
                )
            }
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            Self::NonZeroExit { .. } | Self::Timeout { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn_error() -> RunError {
        RunError::Spawn {
            program: "git".into(),
            source: io::Error::new(io::ErrorKind::NotFound, "not found"),
        }
    }

    fn non_zero_exit(stderr: &str) -> RunError {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(256) // exit code 1 encoded in high byte
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(1)
        };

        RunError::NonZeroExit {
            program: "git".into(),
            args: vec!["status".into()],
            status,
            stdout: Vec::new(),
            stderr: stderr.to_string(),
        }
    }

    fn timeout_error() -> RunError {
        RunError::Timeout {
            program: "git".into(),
            args: vec!["fetch".into()],
            elapsed: Duration::from_secs(30),
            stdout: Vec::new(),
            stderr: "Fetching origin".into(),
        }
    }

    #[test]
    fn program_returns_name() {
        assert_eq!(spawn_error().program(), "git");
        assert_eq!(non_zero_exit("").program(), "git");
        assert_eq!(timeout_error().program(), "git");
    }

    #[test]
    fn stderr_only_for_completed_or_timed_out() {
        assert_eq!(spawn_error().stderr(), None);
        assert_eq!(non_zero_exit("boom").stderr(), Some("boom"));
        assert_eq!(timeout_error().stderr(), Some("Fetching origin"));
    }

    #[test]
    fn exit_status_only_for_non_zero_exit() {
        assert!(spawn_error().exit_status().is_none());
        assert!(non_zero_exit("").exit_status().is_some());
        assert!(timeout_error().exit_status().is_none());
    }

    #[test]
    fn is_non_zero_exit_predicate() {
        assert!(!spawn_error().is_non_zero_exit());
        assert!(non_zero_exit("").is_non_zero_exit());
        assert!(!timeout_error().is_non_zero_exit());
    }

    #[test]
    fn is_spawn_failure_predicate() {
        assert!(spawn_error().is_spawn_failure());
        assert!(!non_zero_exit("").is_spawn_failure());
        assert!(!timeout_error().is_spawn_failure());
    }

    #[test]
    fn is_timeout_predicate() {
        assert!(!spawn_error().is_timeout());
        assert!(!non_zero_exit("").is_timeout());
        assert!(timeout_error().is_timeout());
    }

    #[test]
    fn display_spawn_failure() {
        let msg = format!("{}", spawn_error());
        assert!(msg.contains("spawn"));
        assert!(msg.contains("git"));
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
        assert!(msg.contains("30"));
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
    }

    #[test]
    fn wraps_into_anyhow() {
        let err = spawn_error();
        let _: anyhow::Error = err.into();
    }

}
