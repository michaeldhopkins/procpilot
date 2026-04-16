//! The [`Cmd`] builder — procpilot's sole entry point for running commands.
//!
//! ```no_run
//! use std::time::Duration;
//! use procpilot::Cmd;
//!
//! let output = Cmd::new("git")
//!     .args(["fetch", "origin"])
//!     .in_dir("/repo")
//!     .env("GIT_TERMINAL_PROMPT", "0")
//!     .timeout(Duration::from_secs(30))
//!     .run()?;
//! # Ok::<(), procpilot::RunError>(())
//! ```
//!
//! # Pipelines
//!
//! ```no_run
//! use procpilot::Cmd;
//!
//! // Build: git log --oneline | grep feat | head -5
//! let output = Cmd::new("git").args(["log", "--oneline"])
//!     .pipe(Cmd::new("grep").arg("feat"))
//!     .pipe(Cmd::new("head").arg("-5"))
//!     .run()?;
//!
//! // Equivalent with the `|` operator:
//! let output = (Cmd::new("git").args(["log", "--oneline"])
//!     | Cmd::new("grep").arg("feat")
//!     | Cmd::new("head").arg("-5"))
//!     .run()?;
//! # Ok::<(), procpilot::RunError>(())
//! ```

use std::borrow::Cow;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Write};
use std::ops::BitOr;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use backon::BlockingRetryable;
use os_pipe::PipeReader;
use shared_child::SharedChild;
use wait_timeout::ChildExt;

use crate::cmd_display::CmdDisplay;
use crate::error::{RunError, truncate_suffix, truncate_suffix_string};
use crate::redirection::Redirection;
use crate::retry::RetryPolicy;
use crate::spawned::SpawnedProcess;
use crate::stdin::StdinData;

#[cfg(feature = "tokio")]
mod async_cmd;

/// Internal type alias for the `before_spawn` callback. Not part of the
/// public API — callers pass closures directly to [`Cmd::before_spawn`].
pub(crate) type BeforeSpawnHook = Arc<dyn Fn(&mut Command) -> io::Result<()> + Send + Sync>;

/// Captured output from a successful command.
///
/// Stdout is stored as raw bytes to support binary content. Use
/// [`stdout_lossy()`](RunOutput::stdout_lossy) for text.
///
/// Marked `#[non_exhaustive]` so future fields (e.g., exit status or
/// elapsed time) can be added without breaking callers. Construct via
/// procpilot's runners — downstream code should read fields, not
/// construct this struct.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RunOutput {
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl RunOutput {
    /// Decode stdout as UTF-8, replacing invalid sequences with `�`.
    pub fn stdout_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }
}

/// Per-stage command configuration (program + args + cwd + env).
#[derive(Debug, Clone)]
struct SingleCmd {
    program: OsString,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    env_clear: bool,
    env_remove: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
}

impl SingleCmd {
    fn new(program: OsString) -> Self {
        Self {
            program,
            args: Vec::new(),
            cwd: None,
            env_clear: false,
            env_remove: Vec::new(),
            envs: Vec::new(),
        }
    }

    fn apply_to(&self, cmd: &mut Command) {
        cmd.args(&self.args);
        if let Some(d) = &self.cwd {
            cmd.current_dir(d);
        }
        if self.env_clear {
            cmd.env_clear();
        }
        for k in &self.env_remove {
            cmd.env_remove(k);
        }
        for (k, v) in &self.envs {
            cmd.env(k, v);
        }
    }
}

/// Recursive pipeline tree. Leaves are single commands; internal nodes are
/// pipes (left's stdout → right's stdin).
#[derive(Debug, Clone)]
enum CmdTree {
    Single(SingleCmd),
    Pipe(Box<CmdTree>, Box<CmdTree>),
}

impl CmdTree {
    /// Walk to the rightmost leaf and yield a mutable reference.
    fn rightmost_mut(&mut self) -> &mut SingleCmd {
        match self {
            CmdTree::Single(s) => s,
            CmdTree::Pipe(_, r) => r.rightmost_mut(),
        }
    }

    /// Flatten the tree into a left-to-right sequence of stage references.
    ///
    /// Iterative (explicit-stack) traversal so pathological nesting depth
    /// can't blow the Rust call stack.
    fn flatten<'a>(&'a self, out: &mut Vec<&'a SingleCmd>) {
        let mut stack: Vec<&'a CmdTree> = vec![self];
        while let Some(node) = stack.pop() {
            match node {
                CmdTree::Single(s) => out.push(s),
                CmdTree::Pipe(l, r) => {
                    // Push right first so left is processed first on next pop.
                    stack.push(r);
                    stack.push(l);
                }
            }
        }
    }
}

/// Builder for a subprocess invocation or pipeline.
///
/// Construct via [`Cmd::new`], configure with builder methods, chain with
/// [`Cmd::pipe`] (or `|`), terminate with [`Cmd::run`] or [`Cmd::spawn`].
///
/// Per-stage builders — [`arg`](Self::arg), [`args`](Self::args),
/// [`in_dir`](Self::in_dir), [`env`](Self::env), [`envs`](Self::envs),
/// [`env_clear`](Self::env_clear), [`env_remove`](Self::env_remove) — target
/// the rightmost stage. Pipeline-level builders — [`stdin`](Self::stdin),
/// [`stderr`](Self::stderr), [`timeout`](Self::timeout),
/// [`deadline`](Self::deadline), [`retry`](Self::retry),
/// [`retry_when`](Self::retry_when), [`secret`](Self::secret),
/// [`before_spawn`](Self::before_spawn) — apply to the whole pipeline.
///
/// # Cloning
///
/// `Cmd: Clone` so you can build a base configuration and branch off
/// variants. Most state is cheap to clone (owned data or `Arc`). One
/// caveat: if [`stdin`](Self::stdin) was set with [`StdinData::from_reader`],
/// the reader is shared across clones — whichever attempt runs first takes
/// it, and later attempts (or other clones) see no stdin. For bytes-based
/// stdin, every clone and every retry re-feeds the same buffer.
#[must_use = "Cmd does nothing until .run() or .spawn() is called"]
#[derive(Clone)]
pub struct Cmd {
    tree: CmdTree,
    stdin: Option<SharedStdin>,
    stdout_mode: Redirection,
    stderr_mode: Redirection,
    timeout: Option<Duration>,
    deadline: Option<Instant>,
    retry: Option<RetryPolicy>,
    before_spawn: Option<BeforeSpawnHook>,
    secret: bool,
}

/// Cloneable internal wrapper around [`StdinData`].
///
/// For `Bytes`, clones share the same buffer via `Arc<Vec<u8>>` — cheap and
/// lets every retry or clone re-feed the same data. For `Reader`, clones
/// share a `Mutex<Option<…>>` — whichever attempt runs first takes the
/// reader; subsequent attempts (or concurrent clones) see `None`.
#[derive(Clone)]
enum SharedStdin {
    Bytes(Arc<Vec<u8>>),
    Reader(Arc<Mutex<Option<Box<dyn Read + Send>>>>),
    #[cfg(feature = "tokio")]
    AsyncReader(Arc<Mutex<Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>>>),
}

impl SharedStdin {
    fn from_data(data: StdinData) -> Self {
        match data {
            StdinData::Bytes(b) => Self::Bytes(Arc::new(b)),
            StdinData::Reader(r) => Self::Reader(Arc::new(Mutex::new(Some(r)))),
            #[cfg(feature = "tokio")]
            StdinData::AsyncReader(r) => Self::AsyncReader(Arc::new(Mutex::new(Some(r)))),
        }
    }

    /// Whether this stdin can be driven by the sync runner. `AsyncReader`
    /// returns `false`; everything else returns `true`.
    fn is_sync_compatible(&self) -> bool {
        #[cfg(feature = "tokio")]
        {
            !matches!(self, Self::AsyncReader(_))
        }
        #[cfg(not(feature = "tokio"))]
        {
            true
        }
    }

    /// Take the per-attempt value for the sync runner.
    ///
    /// **Precondition:** [`is_sync_compatible`](Self::is_sync_compatible)
    /// returned `true`. Callers must gate on that before calling; if they
    /// don't, this function panics on the `AsyncReader` variant.
    fn take_for_sync(&self) -> SyncStdinForAttempt {
        match self {
            Self::Bytes(b) => SyncStdinForAttempt::Bytes(Arc::clone(b)),
            Self::Reader(r) => match r.lock() {
                Ok(mut guard) => match guard.take() {
                    Some(reader) => SyncStdinForAttempt::Reader(reader),
                    None => SyncStdinForAttempt::None,
                },
                Err(_) => SyncStdinForAttempt::None,
            },
            #[cfg(feature = "tokio")]
            Self::AsyncReader(_) => {
                // Guaranteed not to happen: the sync entry points
                // (`Cmd::run`, `Cmd::spawn`) return InvalidInput before
                // reaching this path when stdin is an `AsyncReader`.
                // Reaching this branch would be a procpilot bug.
                panic!(
                    "SharedStdin::take_for_sync called with AsyncReader — \
                     sync entry points should have rejected it via \
                     is_sync_compatible()"
                );
            }
        }
    }

    #[cfg(feature = "tokio")]
    fn take_for_async(&self) -> AsyncStdinForAttempt {
        match self {
            Self::Bytes(b) => AsyncStdinForAttempt::Bytes(Arc::clone(b)),
            Self::Reader(r) => match r.lock() {
                Ok(mut guard) => match guard.take() {
                    Some(reader) => AsyncStdinForAttempt::Reader(reader),
                    None => AsyncStdinForAttempt::None,
                },
                Err(_) => AsyncStdinForAttempt::None,
            },
            Self::AsyncReader(r) => match r.lock() {
                Ok(mut guard) => match guard.take() {
                    Some(reader) => AsyncStdinForAttempt::AsyncReader(reader),
                    None => AsyncStdinForAttempt::None,
                },
                Err(_) => AsyncStdinForAttempt::None,
            },
        }
    }
}

impl fmt::Debug for SharedStdin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bytes(b) => f
                .debug_struct("Bytes")
                .field("len", &b.len())
                .finish(),
            Self::Reader(_) => f.debug_struct("Reader").finish_non_exhaustive(),
            #[cfg(feature = "tokio")]
            Self::AsyncReader(_) => f.debug_struct("AsyncReader").finish_non_exhaustive(),
        }
    }
}

impl fmt::Debug for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cmd")
            .field("tree", &self.tree)
            .field("stdin", &self.stdin)
            .field("stdout_mode", &self.stdout_mode)
            .field("stderr_mode", &self.stderr_mode)
            .field("timeout", &self.timeout)
            .field("deadline", &self.deadline)
            .field("retry", &self.retry)
            .field("secret", &self.secret)
            .finish()
    }
}

impl Cmd {
    /// Start a new command with the given program.
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            tree: CmdTree::Single(SingleCmd::new(program.into())),
            stdin: None,
            stdout_mode: Redirection::default(),
            stderr_mode: Redirection::default(),
            timeout: None,
            deadline: None,
            retry: None,
            before_spawn: None,
            secret: false,
        }
    }

    /// Pipe this command's stdout into `next`'s stdin.
    ///
    /// Per-stage configuration (args, env, cwd) is preserved for each side.
    /// Pipeline-level configuration (`stdin`, `stdout`, `stderr`,
    /// `timeout`, `deadline`, `retry`, `secret`, `before_spawn`) is taken
    /// from `self` — **any such settings on `next` are silently dropped**.
    ///
    /// # Gotcha
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # use procpilot::Cmd;
    /// // WRONG: the inner `.timeout(...)` is discarded.
    /// let _ = Cmd::new("a").pipe(Cmd::new("b").timeout(Duration::from_secs(5)));
    ///
    /// // RIGHT: pipeline-level settings go on the outer `Cmd`.
    /// let _ = Cmd::new("a").pipe(Cmd::new("b")).timeout(Duration::from_secs(5));
    /// ```
    pub fn pipe(self, next: Cmd) -> Cmd {
        Cmd {
            tree: CmdTree::Pipe(Box::new(self.tree), Box::new(next.tree)),
            stdin: self.stdin,
            stdout_mode: self.stdout_mode,
            stderr_mode: self.stderr_mode,
            timeout: self.timeout,
            deadline: self.deadline,
            retry: self.retry,
            before_spawn: self.before_spawn,
            // Propagate secret if either side set it — leaking is worse than over-redaction.
            secret: self.secret || next.secret,
        }
    }

    /// Append a single argument to the rightmost stage.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.tree.rightmost_mut().args.push(arg.into());
        self
    }

    /// Append arguments to the rightmost stage.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.tree
            .rightmost_mut()
            .args
            .extend(args.into_iter().map(Into::into));
        self
    }

    /// Set the working directory of the rightmost stage.
    pub fn in_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.tree.rightmost_mut().cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Add one environment variable to the rightmost stage.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.tree
            .rightmost_mut()
            .envs
            .push((key.into(), value.into()));
        self
    }

    /// Add multiple environment variables to the rightmost stage.
    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.tree
            .rightmost_mut()
            .envs
            .extend(vars.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// Remove an environment variable from the rightmost stage.
    pub fn env_remove(mut self, key: impl Into<OsString>) -> Self {
        self.tree.rightmost_mut().env_remove.push(key.into());
        self
    }

    /// Clear the inherited environment of the rightmost stage.
    pub fn env_clear(mut self) -> Self {
        self.tree.rightmost_mut().env_clear = true;
        self
    }

    /// Feed data into the leftmost stage's stdin.
    pub fn stdin(mut self, data: impl Into<StdinData>) -> Self {
        self.stdin = Some(SharedStdin::from_data(data.into()));
        self
    }

    /// Configure stderr routing for every stage. Default is
    /// [`Redirection::Capture`].
    pub fn stderr(mut self, mode: Redirection) -> Self {
        self.stderr_mode = mode;
        self
    }

    /// Configure stdout routing for the **last** stage (for single
    /// commands, the only stage).
    ///
    /// Default is [`Redirection::Capture`], which populates
    /// [`RunOutput::stdout`] on [`run`](Self::run). Pick
    /// [`Redirection::Inherit`] to stream to the parent's stdout,
    /// [`Redirection::Null`] to discard, or [`Redirection::File`] /
    /// [`Redirection::file`](Redirection::file) to redirect to a file.
    ///
    /// Setting non-`Capture` stdout **is only supported on [`run`](Self::run)**.
    /// [`spawn`](Self::spawn) / [`spawn_async`](Self::spawn_async) always
    /// pipe stdout so the handle can expose it via
    /// [`take_stdout`](crate::SpawnedProcess::take_stdout) / the `Read`
    /// impl; a non-`Capture` stdout on the spawn path surfaces a
    /// [`RunError::Spawn`](crate::RunError::Spawn) with
    /// `ErrorKind::InvalidInput`.
    pub fn stdout(mut self, mode: Redirection) -> Self {
        self.stdout_mode = mode;
        self
    }

    /// Convenience: redirect stderr to a file without wrapping the
    /// `File` in `Arc` yourself. Shorthand for
    /// `self.stderr(Redirection::file(f))`.
    pub fn stderr_file(self, f: std::fs::File) -> Self {
        self.stderr(Redirection::file(f))
    }

    /// Convenience: redirect stdout (of the last stage) to a file.
    /// Shorthand for `self.stdout(Redirection::file(f))`.
    pub fn stdout_file(self, f: std::fs::File) -> Self {
        self.stdout(Redirection::file(f))
    }

    /// Kill this attempt after the given duration.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Kill if not done by this instant (composes across retries).
    pub fn deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Attach a [`RetryPolicy`]. Defaults retry up to 3× on transient errors.
    pub fn retry(mut self, policy: RetryPolicy) -> Self {
        self.retry = Some(policy);
        self
    }

    /// Replace the retry predicate without changing the backoff schedule.
    pub fn retry_when(mut self, f: impl Fn(&RunError) -> bool + Send + Sync + 'static) -> Self {
        let policy = self.retry.take().unwrap_or_default();
        self.retry = Some(policy.when(f));
        self
    }

    /// Mark the pipeline as containing secrets; [`CmdDisplay`] will render
    /// args as `<secret>`.
    pub fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    /// Register a hook called immediately before each spawn attempt.
    ///
    /// The hook receives a `&mut std::process::Command` and may mutate it
    /// arbitrarily — set Unix-specific options via `CommandExt` (umask,
    /// pre_exec, process_group), attach IPC sockets, toggle environment
    /// late, etc. Returning an error aborts that spawn attempt and
    /// surfaces as [`RunError::Spawn`].
    ///
    /// # Scope
    ///
    /// - **Sync path** ([`run`](Self::run), [`spawn`](Self::spawn)): hook
    ///   fires per stage per retry attempt.
    /// - **Async path** ([`run_async`](Self::run_async),
    ///   [`spawn_async`](Self::spawn_async)): hook fires per stage per
    ///   retry attempt. Tokio's `Command` exposes its underlying
    ///   [`std::process::Command`] via `as_std_mut`, so the same hook
    ///   works on both paths — no parallel async hook type.
    ///
    /// # Tokio-specific settings
    ///
    /// Note that modifications made through `as_std_mut` do not include
    /// tokio's own knobs (e.g., `kill_on_drop`). Those are set by
    /// procpilot internally and are not exposed to `before_spawn`.
    pub fn before_spawn<F>(mut self, hook: F) -> Self
    where
        F: Fn(&mut Command) -> io::Result<()> + Send + Sync + 'static,
    {
        self.before_spawn = Some(Arc::new(hook));
        self
    }

    /// Build a raw `std::process::Command` mirroring the rightmost stage.
    ///
    /// For a single command this is the one you configured. For a
    /// pipeline, the upstream stages are discarded — the returned
    /// `Command` is only the rightmost one, without stdio wiring. Use
    /// [`to_commands`](Self::to_commands) to recover all stages.
    ///
    /// The explicit name is a signpost: on a pipeline, this method
    /// silently drops information, so use it deliberately.
    pub fn to_rightmost_command(&self) -> Command {
        let single = match &self.tree {
            CmdTree::Single(s) => s,
            CmdTree::Pipe(_, r) => right_leaf(r),
        };
        let mut cmd = Command::new(&single.program);
        single.apply_to(&mut cmd);
        cmd
    }

    /// Build one raw `std::process::Command` per stage, leftmost first.
    ///
    /// Stdio wiring between stages is **not** set up — callers are
    /// responsible for piping the returned `Command`s together if they need
    /// the full shell-style behavior. For the typical case where you just
    /// want to execute the pipeline, use [`run`](Self::run) or
    /// [`spawn`](Self::spawn).
    pub fn to_commands(&self) -> Vec<Command> {
        let mut leaves = Vec::new();
        self.tree.flatten(&mut leaves);
        leaves
            .into_iter()
            .map(|s| {
                let mut cmd = Command::new(&s.program);
                s.apply_to(&mut cmd);
                cmd
            })
            .collect()
    }

    /// Snapshot the command (or pipeline) for display/logging.
    pub fn display(&self) -> CmdDisplay {
        let mut leaves = Vec::new();
        self.tree.flatten(&mut leaves);
        let first = &leaves[0];
        let mut d = CmdDisplay::new(first.program.clone(), first.args.clone(), self.secret);
        for leaf in leaves.into_iter().skip(1) {
            d.push_stage(leaf.program.clone(), leaf.args.clone());
        }
        d
    }

    fn per_attempt_timeout(&self, now: Instant) -> Option<Duration> {
        match (self.timeout, self.deadline) {
            (None, None) => None,
            (Some(t), None) => Some(t),
            (None, Some(d)) => Some(d.saturating_duration_since(now)),
            (Some(t), Some(d)) => Some(t.min(d.saturating_duration_since(now))),
        }
    }

    /// Spawn the command (or pipeline) as a long-lived process handle.
    ///
    /// Returns a [`SpawnedProcess`] for streaming, bidirectional protocols,
    /// or any case where you need live access to stdin/stdout. Stdin and
    /// stdout are always piped; stderr follows the configured
    /// [`Redirection`] (default [`Redirection::Capture`], drained into a
    /// background thread and surfaced on [`SpawnedProcess::wait`]).
    ///
    /// For pipelines, [`SpawnedProcess::take_stdin`] targets the leftmost
    /// stage, [`SpawnedProcess::take_stdout`] the rightmost, and lifecycle
    /// methods operate on every stage.
    ///
    /// If stdin bytes were set via [`stdin`](Self::stdin), they're fed
    /// automatically in a background thread; otherwise the caller can pipe
    /// data via [`SpawnedProcess::take_stdin`].
    ///
    /// `timeout`, `deadline`, and `retry` are **ignored** on this path —
    /// they only apply to the one-shot [`run`](Self::run) method. Use
    /// [`SpawnedProcess::wait_timeout`] or [`SpawnedProcess::kill`] for
    /// per-call bounds.
    pub fn spawn(mut self) -> Result<SpawnedProcess, RunError> {
        let display = self.display();
        let stdin_shared = self.stdin.take();
        reject_async_stdin_on_sync(stdin_shared.as_ref(), &display)?;
        reject_non_capture_stdout_on_spawn(&self.stdout_mode, &display)?;
        let stdin_attempt = attempt_stdin_sync(&stdin_shared);
        let mut stages = Vec::new();
        flatten_owned(self.tree, &mut stages);
        match stages.len() {
            1 => spawn_single_stage(
                stages.into_iter().next().expect("len == 1"),
                &self.stderr_mode,
                self.before_spawn.as_ref(),
                stdin_attempt,
                display,
            ),
            _ => spawn_pipeline_stages(
                stages,
                &self.stderr_mode,
                self.before_spawn.as_ref(),
                stdin_attempt,
                display,
            ),
        }
    }

    /// Spawn and invoke `f` for each line of stdout as it arrives.
    ///
    /// Returns the final [`RunOutput`] when the child exits, or a
    /// [`RunError::NonZeroExit`] if it exited non-zero. If `f` returns an
    /// error, the child is killed and the error is surfaced as
    /// [`RunError::Spawn`].
    ///
    /// ```no_run
    /// # use procpilot::Cmd;
    /// Cmd::new("cargo")
    ///     .args(["check", "--message-format=json"])
    ///     .spawn_and_collect_lines(|line| {
    ///         println!("{line}");
    ///         Ok(())
    ///     })?;
    /// # Ok::<(), procpilot::RunError>(())
    /// ```
    pub fn spawn_and_collect_lines<F>(self, mut f: F) -> Result<RunOutput, RunError>
    where
        F: FnMut(&str) -> io::Result<()>,
    {
        let proc = self.spawn()?;
        let stdout = proc.take_stdout().expect("spawn always pipes stdout");
        let reader = std::io::BufReader::new(stdout);
        use std::io::BufRead;
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(source) => {
                    let _ = proc.kill();
                    let _ = proc.wait();
                    return Err(RunError::Spawn {
                        command: proc.command().clone(),
                        source,
                    });
                }
            };
            if let Err(source) = f(&line) {
                let _ = proc.kill();
                let _ = proc.wait();
                return Err(RunError::Spawn {
                    command: proc.command().clone(),
                    source,
                });
            }
        }
        proc.wait()
    }

    /// Run the command (or pipeline) synchronously, blocking until it
    /// completes (or its timeout / deadline fires).
    ///
    /// Returns [`RunOutput`] on exit status 0. Non-zero exits become
    /// [`RunError::NonZeroExit`] carrying the last 128 KiB of
    /// stdout/stderr. Spawn failures become [`RunError::Spawn`]; timeouts
    /// become [`RunError::Timeout`].
    ///
    /// For an async variant usable from inside a tokio runtime, enable
    /// the `tokio` feature and use [`run_async`](Self::run_async). For
    /// long-lived or streaming processes, see [`spawn`](Self::spawn).
    ///
    /// # Examples
    ///
    /// Basic capture:
    ///
    /// ```no_run
    /// # use procpilot::Cmd;
    /// let out = Cmd::new("git").args(["rev-parse", "HEAD"]).in_dir("/repo").run()?;
    /// println!("{}", out.stdout_lossy().trim());
    /// # Ok::<(), procpilot::RunError>(())
    /// ```
    ///
    /// Typed error branching:
    ///
    /// ```no_run
    /// # use procpilot::{Cmd, RunError};
    /// let maybe = match Cmd::new("git").args(["show", "possibly-missing"]).run() {
    ///     Ok(out) => Some(out.stdout),
    ///     Err(RunError::NonZeroExit { .. }) => None, // ref not found — expected
    ///     Err(e) => return Err(e.into()),             // real failure
    /// };
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    ///
    /// Pipeline with timeout and retry:
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # use procpilot::{Cmd, RetryPolicy};
    /// (Cmd::new("git").args(["log", "--oneline"]) | Cmd::new("head").arg("-5"))
    ///     .timeout(Duration::from_secs(10))
    ///     .retry(RetryPolicy::default())
    ///     .run()?;
    /// # Ok::<(), procpilot::RunError>(())
    /// ```
    pub fn run(mut self) -> Result<RunOutput, RunError> {
        let display = self.display();
        let stdin = self.stdin.take();
        // Fail-fast before any per-attempt stdin take, so an `AsyncReader`
        // isn't consumed by a doomed first attempt and stays available for
        // a later `run_async` on a clone.
        reject_async_stdin_on_sync(stdin.as_ref(), &display)?;
        let retry = self.retry.take();

        let op = |stdin_attempt: SyncStdinForAttempt, per_attempt: Option<Duration>| match &self
            .tree
        {
            CmdTree::Single(single) => execute_single(
                single,
                &self.stdout_mode,
                &self.stderr_mode,
                self.before_spawn.as_ref(),
                &display,
                stdin_attempt,
                per_attempt,
            ),
            CmdTree::Pipe(_, _) => {
                let mut stages = Vec::new();
                self.tree.flatten(&mut stages);
                execute_pipeline(
                    &stages,
                    &self.stdout_mode,
                    &self.stderr_mode,
                    self.before_spawn.as_ref(),
                    &display,
                    stdin_attempt,
                    per_attempt,
                )
            }
        };

        match retry {
            None => op(
                attempt_stdin_sync(&stdin),
                self.per_attempt_timeout(Instant::now()),
            ),
            Some(policy) => run_with_retry(
                &stdin,
                policy,
                self.timeout,
                self.deadline,
                &display,
                &op,
            ),
        }
    }
}

impl BitOr for Cmd {
    type Output = Cmd;
    /// Pipeline composition via `|`. Equivalent to [`Cmd::pipe`].
    fn bitor(self, rhs: Cmd) -> Cmd {
        self.pipe(rhs)
    }
}

impl fmt::Display for Cmd {
    /// Renders the command (or pipeline) shell-style via [`CmdDisplay`],
    /// respecting `.secret()`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.display().fmt(f)
    }
}

fn right_leaf(tree: &CmdTree) -> &SingleCmd {
    match tree {
        CmdTree::Single(s) => s,
        CmdTree::Pipe(_, r) => right_leaf(r),
    }
}

fn run_with_retry<F>(
    stdin: &Option<SharedStdin>,
    policy: RetryPolicy,
    timeout: Option<Duration>,
    deadline: Option<Instant>,
    display: &CmdDisplay,
    op: &F,
) -> Result<RunOutput, RunError>
where
    F: Fn(SyncStdinForAttempt, Option<Duration>) -> Result<RunOutput, RunError>,
{
    let predicate = policy.predicate.clone();
    let attempt = || {
        let now = Instant::now();
        if let Some(d) = deadline
            && now >= d
        {
            return Err(RunError::Timeout {
                command: display.clone(),
                elapsed: Duration::ZERO,
                stdout: Vec::new(),
                stderr: String::new(),
            });
        }
        let per_attempt = match (timeout, deadline) {
            (None, None) => None,
            (Some(t), None) => Some(t),
            (None, Some(d)) => Some(d.saturating_duration_since(now)),
            (Some(t), Some(d)) => Some(t.min(d.saturating_duration_since(now))),
        };
        op(attempt_stdin_sync(stdin), per_attempt)
    };
    attempt
        .retry(policy.backoff)
        .when(move |e: &RunError| predicate(e))
        .call()
}

pub(crate) enum SyncStdinForAttempt {
    None,
    Bytes(Arc<Vec<u8>>),
    Reader(Box<dyn Read + Send>),
}

#[cfg(feature = "tokio")]
pub(crate) enum AsyncStdinForAttempt {
    None,
    Bytes(Arc<Vec<u8>>),
    Reader(Box<dyn Read + Send>),
    AsyncReader(Box<dyn tokio::io::AsyncRead + Send + Unpin>),
}

/// Reject `StdinData::AsyncReader` when used with the sync runner. Called
/// once at the entry of [`Cmd::run`] and [`Cmd::spawn`], before any
/// per-attempt stdin take — so the underlying reader is never consumed by
/// a doomed attempt and remains available if the user later passes the
/// same config (or a clone) to the async runner.
fn reject_async_stdin_on_sync(
    stdin: Option<&SharedStdin>,
    display: &CmdDisplay,
) -> Result<(), RunError> {
    if let Some(s) = stdin
        && !s.is_sync_compatible()
    {
        return Err(RunError::Spawn {
            command: display.clone(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "StdinData::AsyncReader requires run_async / spawn_async; \
                 the sync runner cannot drive an async reader source",
            ),
        });
    }
    Ok(())
}

fn attempt_stdin_sync(shared: &Option<SharedStdin>) -> SyncStdinForAttempt {
    match shared {
        None => SyncStdinForAttempt::None,
        Some(s) => s.take_for_sync(),
    }
}

#[cfg(feature = "tokio")]
fn attempt_stdin_async(shared: &Option<SharedStdin>) -> AsyncStdinForAttempt {
    match shared {
        None => AsyncStdinForAttempt::None,
        Some(s) => s.take_for_async(),
    }
}

enum Outcome {
    Exited(ExitStatus),
    TimedOut(Duration),
    WaitFailed(io::Error),
}

/// Apply stdout routing. Used by the `run` path; the `spawn` path always
/// pipes stdout so the returned handle can expose it.
fn apply_stdout(
    cmd: &mut Command,
    mode: &Redirection,
    display: &CmdDisplay,
) -> Result<(), RunError> {
    match mode {
        Redirection::Capture => {
            cmd.stdout(Stdio::piped());
        }
        Redirection::Inherit => {
            cmd.stdout(Stdio::inherit());
        }
        Redirection::Null => {
            cmd.stdout(Stdio::null());
        }
        Redirection::File(f) => {
            let cloned = f.as_ref().try_clone().map_err(|source| RunError::Spawn {
                command: display.clone(),
                source,
            })?;
            cmd.stdout(Stdio::from(cloned));
        }
    }
    Ok(())
}

/// Reject non-`Capture` stdout on the spawn path. `spawn` / `spawn_async`
/// need stdout piped so the handle can expose it via `take_stdout` / the
/// `Read` impl; any other routing on that path is a user error.
fn reject_non_capture_stdout_on_spawn(
    mode: &Redirection,
    display: &CmdDisplay,
) -> Result<(), RunError> {
    if !matches!(mode, Redirection::Capture) {
        return Err(RunError::Spawn {
            command: display.clone(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "non-Capture stdout routing is not supported on spawn / \
                 spawn_async — use Cmd::run, or take the child's stdout \
                 via SpawnedProcess::take_stdout and route it yourself",
            ),
        });
    }
    Ok(())
}

fn apply_stderr(
    cmd: &mut Command,
    mode: &Redirection,
    display: &CmdDisplay,
) -> Result<(), RunError> {
    match mode {
        Redirection::Capture => {
            cmd.stderr(Stdio::piped());
        }
        Redirection::Inherit => {
            cmd.stderr(Stdio::inherit());
        }
        Redirection::Null => {
            cmd.stderr(Stdio::null());
        }
        Redirection::File(f) => {
            let cloned = f.as_ref().try_clone().map_err(|source| RunError::Spawn {
                command: display.clone(),
                source,
            })?;
            cmd.stderr(Stdio::from(cloned));
        }
    }
    Ok(())
}

fn execute_single(
    single: &SingleCmd,
    stdout_mode: &Redirection,
    stderr_mode: &Redirection,
    before_spawn: Option<&BeforeSpawnHook>,
    display: &CmdDisplay,
    stdin: SyncStdinForAttempt,
    timeout: Option<Duration>,
) -> Result<RunOutput, RunError> {
    let mut cmd = Command::new(&single.program);
    single.apply_to(&mut cmd);

    match &stdin {
        SyncStdinForAttempt::None => {}
        SyncStdinForAttempt::Bytes(_) | SyncStdinForAttempt::Reader(_) => {
            cmd.stdin(Stdio::piped());
        }
    }
    apply_stdout(&mut cmd, stdout_mode, display)?;
    apply_stderr(&mut cmd, stderr_mode, display)?;

    if let Some(hook) = before_spawn {
        hook(&mut cmd).map_err(|source| RunError::Spawn {
            command: display.clone(),
            source,
        })?;
    }

    let mut child = cmd.spawn().map_err(|source| RunError::Spawn {
        command: display.clone(),
        source,
    })?;

    let stdin_thread = spawn_stdin_feeder(&mut child, stdin);
    let stdout_thread = if matches!(stdout_mode, Redirection::Capture) {
        let pipe = child.stdout.take().expect("stdout piped");
        Some(thread::spawn(move || read_to_end(pipe)))
    } else {
        None
    };
    let stderr_thread = if matches!(stderr_mode, Redirection::Capture) {
        let pipe = child.stderr.take().expect("stderr piped");
        Some(thread::spawn(move || read_to_end(pipe)))
    } else {
        None
    };

    let start = Instant::now();
    let outcome = match timeout {
        Some(t) => match child.wait_timeout(t) {
            Ok(Some(status)) => Outcome::Exited(status),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                Outcome::TimedOut(start.elapsed())
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                Outcome::WaitFailed(e)
            }
        },
        None => match child.wait() {
            Ok(status) => Outcome::Exited(status),
            Err(e) => Outcome::WaitFailed(e),
        },
    };

    if let Some(t) = stdin_thread {
        let _ = t.join();
    }
    let stdout_bytes = stdout_thread
        .map(|t| t.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr_bytes = stderr_thread
        .map(|t| t.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr_str = String::from_utf8_lossy(&stderr_bytes).into_owned();

    finalize_outcome(display, outcome, stdout_bytes, stderr_str)
}

fn finalize_outcome(
    display: &CmdDisplay,
    outcome: Outcome,
    stdout_bytes: Vec<u8>,
    stderr_str: String,
) -> Result<RunOutput, RunError> {
    match outcome {
        Outcome::Exited(status) if status.success() => Ok(RunOutput {
            stdout: stdout_bytes,
            stderr: stderr_str,
        }),
        Outcome::Exited(status) => Err(RunError::NonZeroExit {
            command: display.clone(),
            status,
            stdout: truncate_suffix(stdout_bytes),
            stderr: truncate_suffix_string(stderr_str),
        }),
        Outcome::TimedOut(elapsed) => Err(RunError::Timeout {
            command: display.clone(),
            elapsed,
            stdout: truncate_suffix(stdout_bytes),
            stderr: truncate_suffix_string(stderr_str),
        }),
        Outcome::WaitFailed(source) => Err(RunError::Spawn {
            command: display.clone(),
            source,
        }),
    }
}

fn spawn_stdin_feeder(
    child: &mut std::process::Child,
    stdin: SyncStdinForAttempt,
) -> Option<thread::JoinHandle<()>> {
    match stdin {
        SyncStdinForAttempt::None => None,
        SyncStdinForAttempt::Bytes(bytes) => {
            let mut pipe = child.stdin.take().expect("stdin piped");
            Some(thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            }))
        }
        SyncStdinForAttempt::Reader(mut reader) => {
            let mut pipe = child.stdin.take().expect("stdin piped");
            Some(thread::spawn(move || {
                let _ = io::copy(&mut reader, &mut pipe);
            }))
        }
    }
}

fn spawn_stdin_feeder_shared(child: &Arc<SharedChild>, stdin: SyncStdinForAttempt) {
    // Only take stdin when we actually have bytes / a reader — otherwise
    // leaving the pipe attached lets the caller grab it via
    // `SpawnedProcess::take_stdin` for interactive writes.
    match stdin {
        SyncStdinForAttempt::None => {}
        SyncStdinForAttempt::Bytes(bytes) => {
            if let Some(mut pipe) = child.take_stdin() {
                thread::spawn(move || {
                    let _ = pipe.write_all(&bytes);
                });
            }
        }
        SyncStdinForAttempt::Reader(mut reader) => {
            if let Some(mut pipe) = child.take_stdin() {
                thread::spawn(move || {
                    let _ = io::copy(&mut reader, &mut pipe);
                });
            }
        }
    }
}

fn read_to_end<R: Read>(mut reader: R) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf);
    buf
}

fn execute_pipeline(
    stages: &[&SingleCmd],
    stdout_mode: &Redirection,
    stderr_mode: &Redirection,
    before_spawn: Option<&BeforeSpawnHook>,
    display: &CmdDisplay,
    stdin: SyncStdinForAttempt,
    timeout: Option<Duration>,
) -> Result<RunOutput, RunError> {
    debug_assert!(stages.len() >= 2);

    let mut pipes: Vec<(Option<PipeReader>, Option<os_pipe::PipeWriter>)> = Vec::new();
    for _ in 0..stages.len() - 1 {
        let (r, w) = os_pipe::pipe().map_err(|source| RunError::Spawn {
            command: display.clone(),
            source,
        })?;
        pipes.push((Some(r), Some(w)));
    }

    let mut children: Vec<std::process::Child> = Vec::with_capacity(stages.len());
    let mut stdin_thread: Option<thread::JoinHandle<()>> = None;
    let mut last_stdout: Option<std::process::ChildStdout> = None;
    let mut stderr_threads: Vec<thread::JoinHandle<Vec<u8>>> = Vec::new();
    let mut stdin_for_feed = Some(stdin);

    for (i, stage) in stages.iter().enumerate() {
        let mut cmd = Command::new(&stage.program);
        stage.apply_to(&mut cmd);

        if i == 0 {
            match stdin_for_feed.as_ref() {
                Some(SyncStdinForAttempt::None) | None => {}
                Some(SyncStdinForAttempt::Bytes(_)) | Some(SyncStdinForAttempt::Reader(_)) => {
                    cmd.stdin(Stdio::piped());
                }
            }
        } else {
            let reader = pipes[i - 1].0.take().expect("pipe reader");
            cmd.stdin(Stdio::from(reader));
        }

        if i == stages.len() - 1 {
            if let Err(e) = apply_stdout(&mut cmd, stdout_mode, display) {
                kill_and_wait_std_children(&mut children);
                return Err(e);
            }
        } else {
            let writer = pipes[i].1.take().expect("pipe writer");
            cmd.stdout(Stdio::from(writer));
        }

        if let Err(e) = apply_stderr(&mut cmd, stderr_mode, display) {
            kill_and_wait_std_children(&mut children);
            return Err(e);
        }

        if let Some(hook) = before_spawn
            && let Err(source) = hook(&mut cmd)
        {
            kill_and_wait_std_children(&mut children);
            return Err(RunError::Spawn {
                command: display.clone(),
                source,
            });
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(source) => {
                kill_and_wait_std_children(&mut children);
                return Err(RunError::Spawn {
                    command: display.clone(),
                    source,
                });
            }
        };

        if i == 0
            && let Some(data) = stdin_for_feed.take()
            && !matches!(data, SyncStdinForAttempt::None)
        {
            stdin_thread = spawn_stdin_feeder(&mut child, data);
        }

        if matches!(stderr_mode, Redirection::Capture)
            && let Some(pipe) = child.stderr.take()
        {
            stderr_threads.push(thread::spawn(move || read_to_end(pipe)));
        }

        if i == stages.len() - 1 && matches!(stdout_mode, Redirection::Capture) {
            last_stdout = child.stdout.take();
        }

        children.push(child);
    }

    // Drain captured stdout in a background thread — a chatty rightmost
    // stage could otherwise block on a full pipe buffer and prevent the
    // child from exiting. Non-Capture modes route stdout elsewhere and
    // there is nothing to drain.
    let stdout_thread = last_stdout.map(|pipe| thread::spawn(move || read_to_end(pipe)));

    let start = Instant::now();
    let mut per_stage_status: Vec<Outcome> = Vec::with_capacity(children.len());

    if let Some(budget) = timeout {
        for child in children.iter_mut() {
            let remaining = budget.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                let _ = child.kill();
                let _ = child.wait();
                per_stage_status.push(Outcome::TimedOut(start.elapsed()));
                continue;
            }
            match child.wait_timeout(remaining) {
                Ok(Some(status)) => per_stage_status.push(Outcome::Exited(status)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    per_stage_status.push(Outcome::TimedOut(start.elapsed()));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    per_stage_status.push(Outcome::WaitFailed(e));
                }
            }
        }
    } else {
        for child in children.iter_mut() {
            match child.wait() {
                Ok(status) => per_stage_status.push(Outcome::Exited(status)),
                Err(e) => per_stage_status.push(Outcome::WaitFailed(e)),
            }
        }
    }

    if let Some(t) = stdin_thread {
        let _ = t.join();
    }
    let stdout_bytes = stdout_thread
        .map(|t| t.join().unwrap_or_default())
        .unwrap_or_default();
    let mut stderr_all = String::new();
    for t in stderr_threads {
        let bytes = t.join().unwrap_or_default();
        stderr_all.push_str(&String::from_utf8_lossy(&bytes));
    }

    let final_outcome = combine_outcomes(per_stage_status);

    finalize_outcome(display, final_outcome, stdout_bytes, stderr_all)
}

/// Duct-style pipefail: any non-success trumps success; the rightmost
/// non-success wins. All-success returns the first exit status.
fn combine_outcomes(outcomes: Vec<Outcome>) -> Outcome {
    let mut chosen: Option<Outcome> = None;
    for o in outcomes.into_iter() {
        match &o {
            Outcome::Exited(status) if status.success() => {
                if chosen.is_none() {
                    chosen = Some(o);
                }
            }
            _ => chosen = Some(o),
        }
    }
    chosen.unwrap_or(Outcome::WaitFailed(io::Error::other(
        "pipeline had no stages",
    )))
}

/// Best-effort kill + wait for each child in the list. Used to clean up
/// partially-spawned pipelines when a later stage fails — otherwise those
/// already-spawned children outlive the Err return as orphans.
fn kill_and_wait_std_children(children: &mut [std::process::Child]) {
    for c in children.iter_mut() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Best-effort kill + wait across a shared-child pipeline.
fn kill_and_wait_shared_children(children: &[Arc<SharedChild>]) {
    for c in children {
        let _ = c.kill();
        let _ = c.wait();
    }
}

fn flatten_owned(tree: CmdTree, out: &mut Vec<SingleCmd>) {
    let mut stack: Vec<CmdTree> = vec![tree];
    while let Some(node) = stack.pop() {
        match node {
            CmdTree::Single(s) => out.push(s),
            CmdTree::Pipe(l, r) => {
                stack.push(*r);
                stack.push(*l);
            }
        }
    }
}

fn spawn_single_stage(
    single: SingleCmd,
    stderr_mode: &Redirection,
    before_spawn: Option<&BeforeSpawnHook>,
    stdin_attempt: SyncStdinForAttempt,
    display: CmdDisplay,
) -> Result<SpawnedProcess, RunError> {
    let mut cmd = Command::new(&single.program);
    single.apply_to(&mut cmd);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    apply_stderr(&mut cmd, stderr_mode, &display)?;
    if let Some(hook) = before_spawn {
        hook(&mut cmd).map_err(|source| RunError::Spawn {
            command: display.clone(),
            source,
        })?;
    }
    let child = SharedChild::spawn(&mut cmd).map_err(|source| RunError::Spawn {
        command: display.clone(),
        source,
    })?;
    let child = Arc::new(child);
    spawn_stdin_feeder_shared(&child, stdin_attempt);
    let stderr_thread = capture_stderr_bg(&child, stderr_mode);
    Ok(SpawnedProcess::new_single(child, stderr_thread, display))
}

fn spawn_pipeline_stages(
    stages: Vec<SingleCmd>,
    stderr_mode: &Redirection,
    before_spawn: Option<&BeforeSpawnHook>,
    mut stdin_attempt: SyncStdinForAttempt,
    display: CmdDisplay,
) -> Result<SpawnedProcess, RunError> {
    let mut pipes: Vec<(Option<PipeReader>, Option<os_pipe::PipeWriter>)> = Vec::new();
    for _ in 0..stages.len() - 1 {
        let (r, w) = os_pipe::pipe().map_err(|source| RunError::Spawn {
            command: display.clone(),
            source,
        })?;
        pipes.push((Some(r), Some(w)));
    }

    let mut children: Vec<Arc<SharedChild>> = Vec::with_capacity(stages.len());
    let mut stderr_threads: Vec<thread::JoinHandle<Vec<u8>>> = Vec::new();

    for (i, stage) in stages.iter().enumerate() {
        let mut cmd = Command::new(&stage.program);
        stage.apply_to(&mut cmd);

        if i == 0 {
            cmd.stdin(Stdio::piped());
        } else {
            let reader = pipes[i - 1].0.take().expect("pipe reader");
            cmd.stdin(Stdio::from(reader));
        }

        if i == stages.len() - 1 {
            cmd.stdout(Stdio::piped());
        } else {
            let writer = pipes[i].1.take().expect("pipe writer");
            cmd.stdout(Stdio::from(writer));
        }

        if let Err(e) = apply_stderr(&mut cmd, stderr_mode, &display) {
            kill_and_wait_shared_children(&children);
            return Err(e);
        }

        if let Some(hook) = before_spawn
            && let Err(source) = hook(&mut cmd)
        {
            kill_and_wait_shared_children(&children);
            return Err(RunError::Spawn {
                command: display.clone(),
                source,
            });
        }

        let child = match SharedChild::spawn(&mut cmd) {
            Ok(c) => Arc::new(c),
            Err(source) => {
                kill_and_wait_shared_children(&children);
                return Err(RunError::Spawn {
                    command: display.clone(),
                    source,
                });
            }
        };

        if i == 0 {
            let attempt = std::mem::replace(&mut stdin_attempt, SyncStdinForAttempt::None);
            spawn_stdin_feeder_shared(&child, attempt);
        }
        if let Some(handle) = capture_stderr_bg(&child, stderr_mode) {
            stderr_threads.push(handle);
        }

        children.push(child);
    }

    Ok(SpawnedProcess::new_pipeline(
        children,
        stderr_threads,
        display,
    ))
}

fn capture_stderr_bg(
    child: &Arc<SharedChild>,
    stderr_mode: &Redirection,
) -> Option<thread::JoinHandle<Vec<u8>>> {
    if !matches!(stderr_mode, Redirection::Capture) {
        return None;
    }
    let pipe = child.take_stderr()?;
    Some(thread::spawn(move || read_to_end(pipe)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn must_use_annotation_present() {
        let _ = Cmd::new("x");
    }

    #[test]
    fn builder_accumulates_args_on_single() {
        let cmd = Cmd::new("git").arg("status").args(["-s", "--short"]);
        match &cmd.tree {
            CmdTree::Single(s) => assert_eq!(s.args.len(), 3),
            _ => panic!("expected Single"),
        }
    }

    #[test]
    fn pipe_builds_tree_and_args_target_rightmost() {
        let cmd = Cmd::new("a").arg("1").pipe(Cmd::new("b")).arg("right");
        let mut stages = Vec::new();
        cmd.tree.flatten(&mut stages);
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].args, vec![OsString::from("1")]);
        assert_eq!(stages[1].args, vec![OsString::from("right")]);
    }

    #[test]
    fn bitor_builds_pipeline() {
        let cmd = Cmd::new("a") | Cmd::new("b") | Cmd::new("c");
        let mut stages = Vec::new();
        cmd.tree.flatten(&mut stages);
        assert_eq!(stages.len(), 3);
        assert_eq!(stages[0].program, OsString::from("a"));
        assert_eq!(stages[2].program, OsString::from("c"));
    }

    #[test]
    fn flatten_preserves_left_to_right_order_on_deep_pipeline() {
        // 128-stage pipeline to exercise the iterative flatten without
        // risking stack overflow. Left-to-right order of programs must
        // match construction order.
        let mut cmd = Cmd::new("stage0");
        for i in 1..128 {
            cmd = cmd.pipe(Cmd::new(format!("stage{i}")));
        }
        let mut stages = Vec::new();
        cmd.tree.flatten(&mut stages);
        assert_eq!(stages.len(), 128);
        for (i, s) in stages.iter().enumerate() {
            assert_eq!(s.program, OsString::from(format!("stage{i}")));
        }
    }

    #[test]
    fn flatten_owned_preserves_order() {
        let cmd = Cmd::new("a") | Cmd::new("b") | Cmd::new("c") | Cmd::new("d");
        let mut stages = Vec::new();
        flatten_owned(cmd.tree, &mut stages);
        let progs: Vec<_> = stages.iter().map(|s| s.program.clone()).collect();
        assert_eq!(
            progs,
            vec![
                OsString::from("a"),
                OsString::from("b"),
                OsString::from("c"),
                OsString::from("d")
            ]
        );
    }

    #[test]
    fn secret_flag_propagates_through_pipe() {
        let cmd = Cmd::new("docker").arg("login").secret().pipe(Cmd::new("jq"));
        let d = cmd.display();
        assert!(d.is_secret());
        assert_eq!(d.to_string(), "docker <secret> | jq <secret>");
    }

    #[test]
    fn env_builder_targets_rightmost() {
        let cmd = Cmd::new("a").env("X", "1").pipe(Cmd::new("b")).env("Y", "2");
        let mut stages = Vec::new();
        cmd.tree.flatten(&mut stages);
        assert_eq!(stages[0].envs, vec![(OsString::from("X"), OsString::from("1"))]);
        assert_eq!(stages[1].envs, vec![(OsString::from("Y"), OsString::from("2"))]);
    }

    #[test]
    fn display_renders_pipeline() {
        let cmd = Cmd::new("git").args(["log", "--oneline"])
            .pipe(Cmd::new("grep").arg("feat"))
            .pipe(Cmd::new("head").arg("-5"));
        let d = cmd.display();
        assert!(d.is_pipeline());
        assert_eq!(d.to_string(), "git log --oneline | grep feat | head -5");
    }

    #[test]
    fn per_attempt_timeout_respects_both_bounds() {
        let cmd = Cmd::new("x")
            .timeout(Duration::from_secs(60))
            .deadline(Instant::now() + Duration::from_secs(5));
        let t = cmd.per_attempt_timeout(Instant::now()).unwrap();
        assert!(t <= Duration::from_secs(60));
        assert!(t <= Duration::from_secs(6));
    }

    #[test]
    fn combine_outcomes_prefers_rightmost_failure() {
        use std::process::ExitStatus;
        #[cfg(unix)]
        let fail_status = {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(256)
        };
        #[cfg(windows)]
        let fail_status = {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(1)
        };
        #[cfg(unix)]
        let ok_status = {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(0)
        };
        #[cfg(windows)]
        let ok_status = {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(0)
        };
        let outcomes = vec![
            Outcome::Exited(fail_status),
            Outcome::Exited(ok_status),
            Outcome::Exited(fail_status),
        ];
        let combined = combine_outcomes(outcomes);
        match combined {
            Outcome::Exited(s) => assert!(!s.success()),
            _ => panic!("expected Exited"),
        }
    }

    #[test]
    fn to_rightmost_command_returns_rightmost_for_pipeline() {
        let cmd = Cmd::new("a").pipe(Cmd::new("b"));
        let std_cmd = cmd.to_rightmost_command();
        assert_eq!(std_cmd.get_program(), "b");
    }

    #[test]
    fn display_on_cmd_matches_cmd_display() {
        let cmd = Cmd::new("git").args(["log", "-1"]).pipe(Cmd::new("head"));
        assert_eq!(format!("{cmd}"), "git log -1 | head");
    }

    #[test]
    fn display_respects_secret_via_cmd_display() {
        let cmd = Cmd::new("docker").arg("login").arg("-p").arg("tok").secret();
        assert_eq!(format!("{cmd}"), "docker <secret>");
    }

    #[test]
    fn to_commands_returns_all_stages_left_to_right() {
        let cmd = Cmd::new("a").pipe(Cmd::new("b")).pipe(Cmd::new("c"));
        let cmds = cmd.to_commands();
        let progs: Vec<_> = cmds.iter().map(|c| c.get_program().to_os_string()).collect();
        assert_eq!(progs, vec![OsString::from("a"), OsString::from("b"), OsString::from("c")]);
    }

    #[test]
    fn cmd_is_clone_and_divergent_after_clone() {
        // Template pattern: configure a base Cmd, clone to make variants.
        let base = Cmd::new("git").in_dir("/repo").env("K", "V");
        let c1 = base.clone().args(["status"]);
        let c2 = base.clone().args(["log", "-1"]);

        let mut s1 = Vec::new();
        c1.tree.flatten(&mut s1);
        let mut s2 = Vec::new();
        c2.tree.flatten(&mut s2);
        assert_eq!(s1[0].args, vec![OsString::from("status")]);
        assert_eq!(s2[0].args, vec![OsString::from("log"), OsString::from("-1")]);
    }

    #[test]
    fn reader_stdin_shared_across_clones_is_one_shot() {
        // The reader must only produce data once — whichever attempt takes
        // it first wins; subsequent clones/retries see no stdin.
        use std::io::Cursor;
        let original = Cmd::new("x").stdin(StdinData::from_reader(Cursor::new(b"payload".to_vec())));
        let clone_a = original.clone();
        let clone_b = original.clone();

        let a = match clone_a.stdin.as_ref().unwrap() {
            SharedStdin::Reader(r) => r.clone(),
            _ => panic!("expected Reader"),
        };
        let b = match clone_b.stdin.as_ref().unwrap() {
            SharedStdin::Reader(r) => r.clone(),
            _ => panic!("expected Reader"),
        };
        // Both clones point at the same Mutex<Option<Reader>>.
        assert!(Arc::ptr_eq(&a, &b));

        // First take consumes it; second sees None.
        let first = a.lock().unwrap().take();
        assert!(first.is_some());
        let second = b.lock().unwrap().take();
        assert!(second.is_none());
    }

    #[test]
    fn clone_shares_bytes_stdin_cheaply() {
        let original = Cmd::new("x").stdin(b"big input".to_vec());
        let clone = original.clone();
        // Arc shares the underlying Vec; both clones observe the same len.
        let a = match original.stdin.as_ref().unwrap() {
            SharedStdin::Bytes(b) => Arc::strong_count(b),
            _ => unreachable!(),
        };
        let b = match clone.stdin.as_ref().unwrap() {
            SharedStdin::Bytes(b) => Arc::strong_count(b),
            _ => unreachable!(),
        };
        assert_eq!(a, b);
        assert!(a >= 2);
    }
}
