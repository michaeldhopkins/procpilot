//! Subprocess runner for Rust. Typed errors, retry, timeout, stdin piping,
//! pipelines, secret redaction, optional async.
//!
//! `procpilot` provides one entry point — the [`Cmd`] builder — with four
//! terminators: [`Cmd::run`] / [`Cmd::spawn`] for the sync runner, and
//! (behind the `tokio` feature) [`Cmd::run_async`](crate::Cmd::run_async) /
//! [`Cmd::spawn_async`](crate::Cmd::spawn_async) for the async runner.
//!
//! # Quick start
//!
//! ```no_run
//! use std::time::Duration;
//! use procpilot::{Cmd, RunError};
//!
//! let output = Cmd::new("git")
//!     .args(["show", "maybe-missing-ref"])
//!     .timeout(Duration::from_secs(30))
//!     .run();
//!
//! match output {
//!     Ok(o) => println!("{}", o.stdout_lossy()),
//!     Err(RunError::NonZeroExit { .. }) => { /* ref not found */ }
//!     Err(e) => return Err(e.into()),
//! }
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! # What's on offer
//!
//! - **Typed errors**: [`RunError`] variants for spawn / non-zero / timeout,
//!   with shell-quoted command display (secret-redacted via [`Cmd::secret`]).
//! - **Stdin**: [`Cmd::stdin`] accepts owned bytes (reusable across retries),
//!   a boxed `Read` (one-shot), or — with the `tokio` feature —
//!   [`StdinData::from_async_reader`] for true async streaming.
//! - **Stdout/stderr routing**: [`Redirection`] covers capture, inherit,
//!   null, and file redirection via [`Cmd::stdout`] / [`Cmd::stderr`] (or
//!   the [`Cmd::stdout_file`] / [`Cmd::stderr_file`] shortcuts).
//! - **Timeout + deadline**: [`Cmd::timeout`] for per-attempt,
//!   [`Cmd::deadline`] for overall wall-clock budget across retries.
//! - **Retry with exponential backoff**: [`Cmd::retry`] / [`Cmd::retry_when`].
//! - **Pipelines**: [`Cmd::pipe`] or the `|` operator chains commands
//!   (`a | b | c`) with pipefail semantics — non-success on any stage
//!   fails the pipeline, rightmost non-success wins. Works on both
//!   [`run`](Cmd::run) and [`spawn`](Cmd::spawn).
//! - **Streaming / bidirectional**: [`Cmd::spawn`] returns a
//!   [`SpawnedProcess`] with `take_stdin` / `take_stdout`, `Read` impls,
//!   `kill`, and idempotent `wait` / `wait_timeout`.
//! - **Async (tokio, opt-in)**: enable the `tokio` feature for
//!   [`Cmd::run_async`](Cmd::run_async) and
//!   [`Cmd::spawn_async`](Cmd::spawn_async). Returns an
//!   [`AsyncSpawnedProcess`] for the spawn path. All builder knobs work
//!   identically.
//! - **Cloneable `Cmd`**: configure a base once, `.clone()` to branch off
//!   variants.
//! - **Escape hatches**: [`Cmd::before_spawn`] for arbitrary
//!   pre-spawn mutation; [`Cmd::to_rightmost_command`] /
//!   [`Cmd::to_commands`] to drop to raw `std::process::Command`.
//!
//! # Platform support
//!
//! Tested on macOS and Linux. Windows is best-effort — core functionality
//! should work but edge cases (e.g. [`Redirection::File`]) are not CI-covered.
//!
//! Release history: [CHANGELOG.md](https://github.com/michaeldhopkins/procpilot/blob/main/CHANGELOG.md).

#[cfg(feature = "tokio")]
mod async_spawned;
mod cmd;
mod cmd_display;
mod error;
mod redirection;
mod retry;
mod runner;
mod spawned;
mod stdin;

#[cfg(feature = "tokio")]
#[cfg_attr(docsrs, doc(cfg(feature = "tokio")))]
pub use async_spawned::AsyncSpawnedProcess;
pub use cmd::{Cmd, RunOutput};
pub use cmd_display::CmdDisplay;
pub use error::{RunError, STREAM_SUFFIX_SIZE};
pub use redirection::Redirection;
pub use retry::{RetryPolicy, default_transient};
pub use runner::{binary_available, binary_version};
pub use spawned::SpawnedProcess;
pub use stdin::StdinData;

/// Common types for everyday use.
///
/// `use procpilot::prelude::*;` brings in the handful of items most callers
/// will reach for: the [`Cmd`] builder, [`RunError`] / [`RunOutput`] for
/// dispatching results, [`Redirection`] for stdio routing, [`StdinData`]
/// for the `.stdin(...)` argument, [`RetryPolicy`] for `.retry(...)`, and
/// the spawn handles ([`SpawnedProcess`] always; [`AsyncSpawnedProcess`]
/// behind the `tokio` feature).
///
/// The prelude intentionally omits [`CmdDisplay`] and [`STREAM_SUFFIX_SIZE`]
/// — useful but rarely named in client code. Reach for those by full path
/// when needed.
pub mod prelude {
    #[cfg(feature = "tokio")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tokio")))]
    pub use crate::AsyncSpawnedProcess;
    pub use crate::{
        Cmd, Redirection, RetryPolicy, RunError, RunOutput, SpawnedProcess, StdinData,
    };
}
