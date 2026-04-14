//! Production-grade subprocess runner with typed errors, retry, and timeout.
//!
//! `procpilot` provides primitives for running external commands from Rust CLI
//! tools that need to handle failure modes precisely. It distinguishes three
//! kinds of failure — spawn failure, non-zero exit, and timeout — via a typed
//! [`RunError`] enum, so callers can treat each appropriately.
//!
//! # Quick start
//!
//! ```no_run
//! use procpilot::{run_cmd, RunError};
//!
//! match run_cmd("git", &["show", "maybe-missing-ref"]) {
//!     Ok(output) => println!("{}", output.stdout_lossy()),
//!     Err(RunError::NonZeroExit { .. }) => {
//!         // The command ran and reported failure — a legitimate in-band signal.
//!     }
//!     Err(e) => return Err(e.into()),
//! }
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! # Features
//!
//! - **Typed errors**: [`RunError`] distinguishes infrastructure failure (binary
//!   missing, fork failed), command-level failure (non-zero exit), and timeouts.
//! - **Retry with backoff**: [`run_with_retry`] for transient error recovery.
//! - **Timeout with pipe-draining**: [`run_cmd_in_with_timeout`] kills hung
//!   processes without deadlocking on chatty output.
//! - **Binary-safe output**: stdout is `Vec<u8>`; [`RunOutput::stdout_lossy`]
//!   decodes when you want text.
//! - **Env vars**: [`run_cmd_in_with_env`] for when the child needs extra
//!   environment variables (e.g., `GIT_INDEX_FILE`).
//!
//! # Design
//!
//! `procpilot` is aimed at production CLI tools — programs that handle failure
//! carefully, not scripts that can `panic!` on subprocess weirdness. For
//! scripting, [`xshell`](https://crates.io/crates/xshell) is the better fit.

mod error;
mod runner;

pub use error::RunError;
pub use runner::{
    RunOutput, binary_available, binary_version, run_cmd, run_cmd_in, run_cmd_in_with_env,
    run_cmd_in_with_timeout, run_cmd_inherited, run_with_retry,
};
