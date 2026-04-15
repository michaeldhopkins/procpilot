//! Stderr (and potentially stdout) redirection mode for [`Cmd`](crate::Cmd).
//!
//! Modeled after the subprocess crate's `Redirection` enum — one type, five
//! variants covering every practical I/O configuration. Replaces a sprawl of
//! `inherit_stderr()` / `null_stderr()` / `merge_to_stdout()` builder methods.

use std::fs::File;
use std::sync::Arc;

/// Where a child process's stderr goes.
///
/// The default for [`Cmd::stderr()`](crate::Cmd) is [`Capture`](Self::Capture)
/// — every error variant carries captured stderr, so that's almost always
/// what production code wants.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub enum Redirection {
    /// Capture into memory (default). Available in `RunOutput.stderr` on
    /// success and in error variants on failure.
    #[default]
    Capture,
    /// Inherit the parent's file descriptor. Useful when the child should
    /// prompt the user (e.g., `ssh` password prompts) or when the user
    /// should see live progress.
    Inherit,
    /// Discard (`/dev/null`). Captured stderr will be empty.
    Null,
    /// Redirect to a file. The `Arc` lets [`Cmd`](crate::Cmd) stay `Clone`
    /// — the underlying file is `try_clone()`d per spawn so every stage /
    /// retry gets its own file descriptor.
    File(Arc<File>),
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
