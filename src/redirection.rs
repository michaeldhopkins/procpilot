//! Stderr (and potentially stdout) redirection mode for [`Cmd`](crate::Cmd).
//!
//! Modeled after the subprocess crate's `Redirection` enum — one type, five
//! variants covering every practical I/O configuration. Replaces a sprawl of
//! `inherit_stderr()` / `null_stderr()` / `merge_to_stdout()` builder methods.

use std::fs::File;

/// Where a child process's stderr (or stdout, when supported) goes.
///
/// The default for [`Cmd::stderr()`](crate::Cmd) is [`Capture`](Self::Capture)
/// — every error variant carries captured stderr, so that's almost always
/// what production code wants. Use the other variants only when you have a
/// reason.
#[derive(Debug, Default)]
#[non_exhaustive]
pub enum Redirection {
    /// Capture into memory (default). Available in `RunOutput.stderr` on
    /// success and in error variants on failure.
    #[default]
    Capture,
    /// Inherit the parent's file descriptor. Output streams to the parent's
    /// stderr (i.e., the user's terminal) instead of being captured.
    /// Useful when the child should prompt the user (e.g., `ssh` password
    /// prompts) or when the user should see live progress.
    Inherit,
    /// Discard (`/dev/null`). Captured stderr will be empty.
    Null,
    /// Redirect to a file. Captured stderr will be empty.
    File(File),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_capture() {
        match Redirection::default() {
            Redirection::Capture => {}
            _ => panic!("default should be Capture"),
        }
    }

    #[test]
    fn variants_are_constructible() {
        let _ = Redirection::Capture;
        let _ = Redirection::Inherit;
        let _ = Redirection::Null;
        // File variant is tested in integration tests where we have an actual file.
    }
}
