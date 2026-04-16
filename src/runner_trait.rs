//! Pluggable subprocess runner trait.
//!
//! Production code uses [`DefaultRunner`] (which delegates to [`Cmd::run`]).
//! Test code can substitute [`MockRunner`](crate::testing::MockRunner) — gated
//! behind the `testing` feature — or any custom implementation.
//!
//! # Pattern
//!
//! Take `&dyn Runner` in functions that shell out so callers can swap in a
//! mock for unit tests:
//!
//! ```no_run
//! use procpilot::{Cmd, Runner, RunError};
//! use std::path::Path;
//!
//! fn current_branch(runner: &dyn Runner, repo: &Path) -> Result<String, RunError> {
//!     let cmd = Cmd::new("git").args(["branch", "--show-current"]).in_dir(repo);
//!     let out = runner.run(cmd)?;
//!     Ok(out.stdout_lossy().trim().to_string())
//! }
//! ```
//!
//! In production, pass `&DefaultRunner` (or a long-lived global). In tests,
//! pass a [`MockRunner`](crate::testing::MockRunner) configured with the
//! commands you expect and the canned outputs to return.

use crate::cmd::{Cmd, RunOutput};
use crate::error::RunError;

/// Pluggable backend for executing a [`Cmd`]. See the module-level docs.
///
/// `Send + Sync` so a `&dyn Runner` can cross thread / task boundaries.
///
/// # Scope
///
/// This trait only abstracts the sync [`Cmd::run`] path. Code that calls
/// [`Cmd::spawn`], [`Cmd::spawn_and_collect_lines`], or (with the
/// `tokio` feature) [`Cmd::run_async`] / [`Cmd::spawn_async`] still
/// invokes the real subprocess machinery — those paths aren't mockable
/// through this trait in procpilot 0.7. If you want your code fully
/// testable without spawning processes, route shell-outs through `run`
/// (via a `&dyn Runner`) for now. Spawn-handle mocking is tracked as
/// follow-up work.
pub trait Runner: Send + Sync {
    /// Execute the [`Cmd`] synchronously, blocking until completion.
    /// Mirrors [`Cmd::run`].
    fn run(&self, cmd: Cmd) -> Result<RunOutput, RunError>;
}

/// Default `Runner` that delegates to [`Cmd::run`].
///
/// What `Cmd::run()` invokes internally, exposed as a `Runner` so callers
/// can pass `&DefaultRunner` where a `&dyn Runner` is expected.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultRunner;

impl Runner for DefaultRunner {
    fn run(&self, cmd: Cmd) -> Result<RunOutput, RunError> {
        cmd.run()
    }
}
