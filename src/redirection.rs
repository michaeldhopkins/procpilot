//! Stdio redirection for [`Cmd`](crate::Cmd). Used by both
//! [`Cmd::stderr`](crate::Cmd::stderr) and
//! [`Cmd::stdout`](crate::Cmd::stdout).

use std::fs::File;
use std::sync::Arc;

/// Where a child process's stdout or stderr goes.
///
/// The stderr default is [`Capture`](Self::Capture) (every error variant
/// carries captured stderr, so that's almost always what you want). The
/// stdout default is also [`Capture`](Self::Capture) for [`Cmd::run`](crate::Cmd::run) —
/// the bytes end up in [`RunOutput::stdout`](crate::RunOutput::stdout).
///
/// For [`Cmd::spawn`](crate::Cmd::spawn), stdout is always piped internally
/// so callers can [`take_stdout`](crate::SpawnedProcess::take_stdout) or
/// read through the handle; a non-`Capture` stdout redirection here is not
/// currently supported on the spawn path.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub enum Redirection {
    /// Capture into memory (default). Available in
    /// [`RunOutput`](crate::RunOutput) on success and in error variants on
    /// failure.
    #[default]
    Capture,
    /// Inherit the parent's file descriptor. Useful when the child should
    /// prompt the user (e.g., `ssh` password prompts) or when the user
    /// should see live progress.
    Inherit,
    /// Discard (`/dev/null`). The corresponding captured field will be
    /// empty.
    Null,
    /// Redirect to a file. The `Arc` lets [`Cmd`](crate::Cmd) stay `Clone`
    /// — the underlying file is `try_clone()`d per spawn so every stage /
    /// retry gets its own file descriptor. Construct via
    /// [`Redirection::file`](Self::file) to avoid wrapping the `Arc` by
    /// hand.
    File(Arc<File>),
}

impl Redirection {
    /// Build a [`File`](Self::File) variant from a [`std::fs::File`], wrapping
    /// it in `Arc` internally. Prefer this over constructing the variant
    /// directly.
    pub fn file(f: File) -> Self {
        Self::File(Arc::new(f))
    }
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
