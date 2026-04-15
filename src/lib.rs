//! Production-grade subprocess runner with typed errors, retry, and timeout.
//!
//! `procpilot` provides one entry point — the [`Cmd`] builder — covering every
//! practical subprocess configuration. Errors are typed: [`RunError`]
//! distinguishes spawn failure, non-zero exit, and timeout, each carrying
//! the last 128 KiB of stdout/stderr for diagnosis.
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
//! # Features
//!
//! - **Typed errors**: [`RunError`] variants for spawn / non-zero / timeout,
//!   with shell-quoted command display (secret-redacted via [`Cmd::secret`]).
//! - **Stdin**: [`Cmd::stdin`] accepts owned bytes (reusable across retries)
//!   or a boxed `Read` (one-shot).
//! - **Stderr routing**: [`Redirection`] covers capture, inherit, null, and
//!   file redirection.
//! - **Timeout + deadline**: [`Cmd::timeout`] for per-attempt, [`Cmd::deadline`]
//!   for overall wall-clock budget across retries.
//! - **Retry with exponential backoff**: [`Cmd::retry`] / [`Cmd::retry_when`].
//! - **Escape hatches**: [`Cmd::before_spawn`] for pre-spawn hooks,
//!   [`Cmd::to_command`] to drop to raw `std::process::Command`.
//!
//! Release history: [CHANGELOG.md](https://github.com/michaeldhopkins/procpilot/blob/main/CHANGELOG.md).

mod cmd;
mod cmd_display;
mod error;
mod redirection;
mod retry;
mod runner;
mod spawned;
mod stdin;

pub use cmd::{BeforeSpawnHook, Cmd, RunOutput};
pub use cmd_display::CmdDisplay;
pub use error::{RunError, STREAM_SUFFIX_SIZE};
pub use redirection::Redirection;
pub use retry::{RetryPolicy, default_transient};
pub use runner::{binary_available, binary_version};
pub use spawned::SpawnedProcess;
pub use stdin::StdinData;
