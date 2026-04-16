//! Test doubles for [`Runner`]. Gated on the `testing` feature so
//! production builds don't compile mock infrastructure.
//!
//! [`Runner`]: crate::Runner
//!
//! # Quick example
//!
//! ```
//! use procpilot::{Cmd, Runner};
//! use procpilot::testing::{MockRunner, ok_str};
//!
//! let mock = MockRunner::new()
//!     .expect("git branch --show-current", ok_str("main\n"));
//!
//! let out = mock.run(Cmd::new("git").args(["branch", "--show-current"])).unwrap();
//! assert_eq!(out.stdout_lossy().trim(), "main");
//!
//! mock.verify().unwrap();
//! ```
//!
//! # Matchers
//!
//! - [`MockRunner::expect`] matches by the command's shell-style display
//!   string (program + args, joined as if for `Cmd::new(...).args(...)`).
//!   Cwd, env, and stderr/stdout routing are not part of the match.
//! - [`MockRunner::expect_when`] takes a `Fn(&Cmd) -> bool` predicate for
//!   matching on properties the display string doesn't carry (cwd, env).
//!   The predicate can reach the underlying `std::process::Command` via
//!   [`Cmd::to_rightmost_command`](crate::Cmd::to_rightmost_command) for
//!   `get_current_dir` / `get_envs` / etc.
//!
//! # Match-count control
//!
//! - `expect` / `expect_when` match **exactly once**.
//! - `expect_repeated` / `expect_when_repeated` match **up to N times**,
//!   calling a factory each time.
//! - `expect_always` / `expect_when_always` match **unlimited times**.
//!
//! [`MockRunner::verify`] reports expectations that weren't consumed at
//! least once.
//!
//! # Result-builder helpers
//!
//! Helpers return [`MockResult`] — a command-agnostic outcome that the
//! [`MockRunner`] resolves into a `Result<RunOutput, RunError>` at match
//! time, inserting the actual invoked command's display into the error's
//! `command` field. This keeps `err.command()` truthful in test
//! assertions.
//!
//! - [`ok`] / [`ok_str`] — success with stdout bytes / string.
//! - [`nonzero`] — `NonZeroExit` with an exit code and stderr.
//! - [`spawn_error`] — `Spawn` with a message.
//! - [`timeout`] — `Timeout` with elapsed and stderr.
//!
//! If you're writing a custom `Runner` impl (not using `MockRunner`), use
//! [`MockResult::resolve`] to produce the final `Result` with your own
//! `CmdDisplay`.
//!
//! # Limitations
//!
//! - **Only `Runner::run` is mockable.** Code paths that call
//!   [`Cmd::spawn`](crate::Cmd::spawn),
//!   [`Cmd::spawn_and_collect_lines`](crate::Cmd::spawn_and_collect_lines),
//!   or (with the `tokio` feature) [`Cmd::run_async`](crate::Cmd::run_async) /
//!   [`Cmd::spawn_async`](crate::Cmd::spawn_async) still hit real
//!   subprocesses. Mocking those requires simulating `SpawnedProcess` /
//!   `AsyncSpawnedProcess` state machines and is a tracked follow-up.
//! - **Concurrent `run` calls serialize** through an internal `Mutex`.
//!   Fine for the typical single-threaded test pattern; not meant for
//!   high-throughput concurrent mocking.

use std::sync::Mutex;

use crate::cmd::{Cmd, RunOutput};
use crate::cmd_display::CmdDisplay;
use crate::error::RunError;
use crate::runner_trait::Runner;

/// In-memory test runner that returns canned results without actually
/// spawning processes. See the module docs.
pub struct MockRunner {
    expectations: Mutex<Vec<Expectation>>,
    /// `true` → panic on no-match (default, easiest for tests). `false`
    /// → return `RunError::Spawn` so production code paths that catch
    /// subprocess errors can be exercised.
    panic_on_no_match: bool,
}

struct Expectation {
    matcher: Matcher,
    responder: Responder,
    calls: usize,
    matcher_desc: String,
}

enum Matcher {
    Display(String),
    Predicate(Box<dyn Fn(&Cmd) -> bool + Send + Sync>),
}

impl Matcher {
    fn matches(&self, cmd: &Cmd) -> bool {
        match self {
            Self::Display(expected) => &cmd.display().to_string() == expected,
            Self::Predicate(f) => f(cmd),
        }
    }
}

enum Responder {
    /// Match exactly once. After a match, `taken` becomes true.
    Once { result: Option<MockResult> },
    /// Match up to `remaining` more times. `factory` produces a fresh
    /// `MockResult` on each call.
    Bounded {
        factory: Box<dyn FnMut() -> MockResult + Send>,
        remaining: usize,
    },
    /// Match unlimited times.
    Unlimited {
        factory: Box<dyn FnMut() -> MockResult + Send>,
    },
}

impl Responder {
    fn take(&mut self) -> Option<MockResult> {
        match self {
            Self::Once { result } => result.take(),
            Self::Bounded { factory, remaining } => {
                if *remaining == 0 {
                    None
                } else {
                    *remaining -= 1;
                    Some(factory())
                }
            }
            Self::Unlimited { factory } => Some(factory()),
        }
    }

    fn exhausted(&self) -> bool {
        match self {
            Self::Once { result } => result.is_none(),
            Self::Bounded { remaining, .. } => *remaining == 0,
            Self::Unlimited { .. } => false,
        }
    }
}

impl MockRunner {
    pub fn new() -> Self {
        Self {
            expectations: Mutex::new(Vec::new()),
            panic_on_no_match: true,
        }
    }

    /// Switch to `RunError::Spawn`-on-no-match (rather than panic).
    /// Useful when the production code under test catches subprocess
    /// failures and you want the no-match case to flow through that
    /// error path.
    pub fn error_on_no_match(mut self) -> Self {
        self.panic_on_no_match = false;
        self
    }

    /// Register an expectation matching by the command's shell-style
    /// display (program + args). Matches exactly once.
    pub fn expect(self, display: impl Into<String>, result: MockResult) -> Self {
        let display = display.into();
        let desc = format!("display = {display:?}");
        self.push(Matcher::Display(display), Responder::Once { result: Some(result) }, desc)
    }

    /// Register an expectation matching by predicate. Matches exactly once.
    pub fn expect_when<F>(self, matcher: F, result: MockResult) -> Self
    where
        F: Fn(&Cmd) -> bool + Send + Sync + 'static,
    {
        self.push(
            Matcher::Predicate(Box::new(matcher)),
            Responder::Once { result: Some(result) },
            "<predicate>".to_string(),
        )
    }

    /// Register a display-matching expectation that matches up to `times`
    /// times. `factory` is called on each match to produce a fresh
    /// [`MockResult`].
    pub fn expect_repeated<F>(self, display: impl Into<String>, times: usize, factory: F) -> Self
    where
        F: FnMut() -> MockResult + Send + 'static,
    {
        let display = display.into();
        let desc = format!("display = {display:?} (×{times})");
        self.push(
            Matcher::Display(display),
            Responder::Bounded {
                factory: Box::new(factory),
                remaining: times,
            },
            desc,
        )
    }

    /// Register a predicate-matching expectation that matches up to
    /// `times` times.
    pub fn expect_when_repeated<M, F>(self, matcher: M, times: usize, factory: F) -> Self
    where
        M: Fn(&Cmd) -> bool + Send + Sync + 'static,
        F: FnMut() -> MockResult + Send + 'static,
    {
        self.push(
            Matcher::Predicate(Box::new(matcher)),
            Responder::Bounded {
                factory: Box::new(factory),
                remaining: times,
            },
            format!("<predicate> (×{times})"),
        )
    }

    /// Register a display-matching expectation that matches **unlimited**
    /// times. Useful for "every invocation of `X` returns `Y`" setups.
    pub fn expect_always<F>(self, display: impl Into<String>, factory: F) -> Self
    where
        F: FnMut() -> MockResult + Send + 'static,
    {
        let display = display.into();
        let desc = format!("display = {display:?} (∞)");
        self.push(
            Matcher::Display(display),
            Responder::Unlimited { factory: Box::new(factory) },
            desc,
        )
    }

    /// Register a predicate-matching expectation that matches unlimited
    /// times.
    pub fn expect_when_always<M, F>(self, matcher: M, factory: F) -> Self
    where
        M: Fn(&Cmd) -> bool + Send + Sync + 'static,
        F: FnMut() -> MockResult + Send + 'static,
    {
        self.push(
            Matcher::Predicate(Box::new(matcher)),
            Responder::Unlimited { factory: Box::new(factory) },
            "<predicate> (∞)".to_string(),
        )
    }

    fn push(self, matcher: Matcher, responder: Responder, matcher_desc: String) -> Self {
        self.expectations
            .lock()
            .expect("MockRunner mutex poisoned")
            .push(Expectation {
                matcher,
                responder,
                calls: 0,
                matcher_desc,
            });
        self
    }

    /// Verify every expectation was matched at least once. Returns `Err`
    /// listing unmatched expectations. Call at the end of each test (or
    /// wrap in a `Drop` guard if you prefer that style).
    ///
    /// `expect_always` / `expect_when_always` expectations pass verify
    /// if they were hit at least once; unbounded responders don't imply
    /// "must be hit many times".
    pub fn verify(&self) -> Result<(), String> {
        let exps = self
            .expectations
            .lock()
            .expect("MockRunner mutex poisoned");
        let unmet: Vec<_> = exps.iter().filter(|e| e.calls == 0).collect();
        if unmet.is_empty() {
            Ok(())
        } else {
            let descriptions: Vec<_> = unmet.iter().map(|e| e.matcher_desc.clone()).collect();
            Err(format!(
                "{} unmet MockRunner expectation(s):\n  - {}",
                unmet.len(),
                descriptions.join("\n  - "),
            ))
        }
    }
}

impl Default for MockRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl Runner for MockRunner {
    fn run(&self, cmd: Cmd) -> Result<RunOutput, RunError> {
        let display = cmd.display();
        // Build the no-match report inside a scope so the MutexGuard
        // drops before the potential panic. Otherwise a
        // `panic_on_no_match` panic unwinds with the guard alive and
        // poisons the mutex.
        let no_match_msg = {
            let mut exps = self
                .expectations
                .lock()
                .expect("MockRunner mutex poisoned");
            for exp in exps.iter_mut() {
                if exp.responder.exhausted() {
                    continue;
                }
                if exp.matcher.matches(&cmd)
                    && let Some(mock_result) = exp.responder.take()
                {
                    exp.calls += 1;
                    return mock_result.resolve(&display);
                }
            }
            let registered: Vec<_> = exps
                .iter()
                .map(|e| {
                    format!(
                        "{} (calls={}, exhausted={})",
                        e.matcher_desc,
                        e.calls,
                        e.responder.exhausted()
                    )
                })
                .collect();
            format!(
                "MockRunner: no matching expectation for command:\n  {}\nregistered:\n  - {}",
                display,
                registered.join("\n  - "),
            )
        };
        if self.panic_on_no_match {
            panic!("{no_match_msg}");
        }
        Err(RunError::Spawn {
            command: display,
            source: std::io::Error::other(no_match_msg),
        })
    }
}

// ---------- MockResult: command-agnostic outcome ----------

/// A command-agnostic outcome for a mocked subprocess invocation.
///
/// [`MockRunner`] resolves this into a `Result<RunOutput, RunError>` at
/// match time, inserting the actual invoked command's [`CmdDisplay`] —
/// so `err.command()` in downstream test code reports the real
/// invocation rather than a placeholder.
///
/// Callers writing a custom `Runner` impl can use [`MockResult::resolve`]
/// to perform the same fix-up themselves.
pub enum MockResult {
    Ok {
        stdout: Vec<u8>,
        stderr: String,
    },
    NonZeroExit {
        code: i32,
        stdout: Vec<u8>,
        stderr: String,
    },
    Spawn {
        source: std::io::Error,
    },
    Timeout {
        elapsed: std::time::Duration,
        stdout: Vec<u8>,
        stderr: String,
    },
}

impl MockResult {
    /// Resolve into a full `Result<RunOutput, RunError>` by attaching
    /// the given command display to the error variants.
    ///
    /// **Exhaustive match (no wildcard arm) is intentional.** Adding a
    /// new `MockResult` variant should fail to compile here, prompting
    /// the maintainer to decide how it resolves. Don't "fix" that with
    /// `_ => …`.
    pub fn resolve(self, command: &CmdDisplay) -> Result<RunOutput, RunError> {
        match self {
            Self::Ok { stdout, stderr } => Ok(RunOutput { stdout, stderr }),
            Self::NonZeroExit {
                code,
                stdout,
                stderr,
            } => Err(RunError::NonZeroExit {
                command: command.clone(),
                status: build_exit_status(code),
                stdout,
                stderr,
            }),
            Self::Spawn { source } => Err(RunError::Spawn {
                command: command.clone(),
                source,
            }),
            Self::Timeout {
                elapsed,
                stdout,
                stderr,
            } => Err(RunError::Timeout {
                command: command.clone(),
                elapsed,
                stdout,
                stderr,
            }),
        }
    }
}

// ---------- builder helpers ----------

/// `MockResult::Ok` with the given stdout bytes and empty stderr.
pub fn ok(stdout: impl Into<Vec<u8>>) -> MockResult {
    MockResult::Ok {
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

/// `MockResult::Ok` with the given stdout string and empty stderr.
pub fn ok_str(stdout: impl Into<String>) -> MockResult {
    MockResult::Ok {
        stdout: stdout.into().into_bytes(),
        stderr: String::new(),
    }
}

/// `MockResult::NonZeroExit` with the given code, empty stdout, and
/// given stderr.
pub fn nonzero(code: i32, stderr: impl Into<String>) -> MockResult {
    MockResult::NonZeroExit {
        code,
        stdout: vec![],
        stderr: stderr.into(),
    }
}

/// `MockResult::Spawn` with the given message wrapped in
/// `io::Error::other`.
pub fn spawn_error(message: impl Into<String>) -> MockResult {
    MockResult::Spawn {
        source: std::io::Error::other(message.into()),
    }
}

/// `MockResult::Timeout` with the given elapsed and stderr.
pub fn timeout(elapsed: std::time::Duration, stderr: impl Into<String>) -> MockResult {
    MockResult::Timeout {
        elapsed,
        stdout: vec![],
        stderr: stderr.into(),
    }
}

#[cfg(unix)]
fn build_exit_status(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    // waitpid encodes the exit code in the high byte of the status word.
    std::process::ExitStatus::from_raw(code << 8)
}

#[cfg(windows)]
fn build_exit_status(code: i32) -> std::process::ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(code as u32)
}
