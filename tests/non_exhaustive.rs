//! Catches accidental removal of `#[non_exhaustive]` on `RunError`.
//!
//! Integration tests compile as a separate crate, so they see `RunError` from a
//! downstream-consumer perspective. If `#[non_exhaustive]` is removed, the
//! wildcard arm below becomes unreachable and `#[deny(unreachable_patterns)]`
//! fails the build.

use std::io;

use procpilot::RunError;

#[test]
fn run_error_stays_non_exhaustive() {
    let err = RunError::Spawn {
        program: "x".into(),
        source: io::Error::other("x"),
    };

    #[deny(unreachable_patterns)]
    match err {
        RunError::Spawn { .. } => {}
        RunError::NonZeroExit { .. } => {}
        RunError::Timeout { .. } => {}
        _ => {}
    }
}
