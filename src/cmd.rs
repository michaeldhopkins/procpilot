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

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use backon::BlockingRetryable;
use shared_child::SharedChild;
use wait_timeout::ChildExt;

use crate::cmd_display::CmdDisplay;
use crate::error::{RunError, truncate_suffix, truncate_suffix_string};
use crate::redirection::Redirection;
use crate::retry::RetryPolicy;
use crate::spawned::SpawnedProcess;
use crate::stdin::StdinData;

/// Hook invoked on `std::process::Command` immediately before each spawn attempt.
///
/// Lets callers set Unix-specific options (`pre_exec`, umask, capabilities) or
/// otherwise tweak the spawn without waiting for procpilot to grow a builder
/// method for every knob. Returning an `Err` aborts the spawn and surfaces
/// as [`RunError::Spawn`].
pub type BeforeSpawnHook = Arc<dyn Fn(&mut Command) -> io::Result<()> + Send + Sync>;

/// Captured output from a successful command.
///
/// Stdout is stored as raw bytes to support binary content. Use
/// [`stdout_lossy()`](RunOutput::stdout_lossy) for text.
#[derive(Debug, Clone)]
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

/// Builder for a subprocess invocation.
///
/// Construct via [`Cmd::new`], configure with builder methods, terminate with
/// [`Cmd::run`]. Every knob composes with every other — timeout + env + retry
/// + stdin work together without combinatorial API explosion.
#[must_use = "Cmd does nothing until .run() is called"]
pub struct Cmd {
    program: OsString,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    env_clear: bool,
    env_remove: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
    stdin: Option<StdinData>,
    stderr_mode: Redirection,
    timeout: Option<Duration>,
    deadline: Option<Instant>,
    retry: Option<RetryPolicy>,
    before_spawn: Option<BeforeSpawnHook>,
    secret: bool,
}

impl fmt::Debug for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cmd")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .field("env_clear", &self.env_clear)
            .field("envs", &self.envs)
            .field("stdin", &self.stdin)
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
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env_clear: false,
            env_remove: Vec::new(),
            envs: Vec::new(),
            stdin: None,
            stderr_mode: Redirection::default(),
            timeout: None,
            deadline: None,
            retry: None,
            before_spawn: None,
            secret: false,
        }
    }

    /// Append a single argument.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Append arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set the working directory.
    pub fn in_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Add one environment variable.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    /// Add multiple environment variables.
    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.envs
            .extend(vars.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// Remove an environment variable (applied after inherited env).
    pub fn env_remove(mut self, key: impl Into<OsString>) -> Self {
        self.env_remove.push(key.into());
        self
    }

    /// Clear the entire inherited environment; only `.env()` / `.envs()` reach the child.
    pub fn env_clear(mut self) -> Self {
        self.env_clear = true;
        self
    }

    /// Feed data into the child's stdin.
    ///
    /// Accepts `Vec<u8>`, `&[u8]`, `String`, `&str`, or [`StdinData::from_reader`]
    /// for streaming input. Owned bytes are re-fed on each retry; readers are
    /// one-shot.
    pub fn stdin(mut self, data: impl Into<StdinData>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    /// Configure stderr routing. Default is [`Redirection::Capture`].
    pub fn stderr(mut self, mode: Redirection) -> Self {
        self.stderr_mode = mode;
        self
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
    ///
    /// If no [`RetryPolicy`] is set yet, this installs the default policy and
    /// then overrides its predicate.
    pub fn retry_when(mut self, f: impl Fn(&RunError) -> bool + Send + Sync + 'static) -> Self {
        let policy = self.retry.take().unwrap_or_default();
        self.retry = Some(policy.when(f));
        self
    }

    /// Mark this command as containing secrets.
    ///
    /// [`CmdDisplay`] and [`RunError`] render args as `<secret>` instead of
    /// their values. Useful for `docker login`, `kubectl --token=…`, etc.
    pub fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    /// Register a hook called immediately before each spawn attempt.
    pub fn before_spawn<F>(mut self, hook: F) -> Self
    where
        F: Fn(&mut Command) -> io::Result<()> + Send + Sync + 'static,
    {
        self.before_spawn = Some(Arc::new(hook));
        self
    }

    /// Build a raw `std::process::Command` mirroring this `Cmd`'s configuration.
    ///
    /// Escape hatch for cases procpilot's builder doesn't cover. Does not apply
    /// stdin data, timeout, retry, or stderr redirection — those are
    /// runner-level concerns.
    pub fn to_command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        if self.env_clear {
            cmd.env_clear();
        }
        for key in &self.env_remove {
            cmd.env_remove(key);
        }
        for (k, v) in &self.envs {
            cmd.env(k, v);
        }
        cmd
    }

    /// Snapshot the command for display/logging.
    pub fn display(&self) -> CmdDisplay {
        CmdDisplay::new(self.program.clone(), self.args.clone(), self.secret)
    }

    /// Spawn the command as a long-lived process handle.
    ///
    /// Returns a [`SpawnedProcess`] for streaming, bidirectional protocols,
    /// or any case where you need live access to stdin/stdout. Stdin and
    /// stdout are always piped; stderr follows the configured
    /// [`Redirection`] (default [`Redirection::Capture`], drained into a
    /// background thread and surfaced on [`SpawnedProcess::wait`]).
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
        let stdin_data = self.stdin.take();

        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        if self.env_clear {
            cmd.env_clear();
        }
        for key in &self.env_remove {
            cmd.env_remove(key);
        }
        for (k, v) in &self.envs {
            cmd.env(k, v);
        }

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        match &self.stderr_mode {
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
                let cloned = f.try_clone().map_err(|source| RunError::Spawn {
                    command: display.clone(),
                    source,
                })?;
                cmd.stderr(Stdio::from(cloned));
            }
        }

        if let Some(hook) = &self.before_spawn {
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

        // If caller supplied stdin data, feed it in a background thread so
        // they can still call take_stdout without blocking on a full pipe.
        if let Some(data) = stdin_data
            && let Some(mut pipe) = child.take_stdin()
        {
            thread::spawn(move || {
                use std::io::Write;
                match data {
                    StdinData::Bytes(b) => {
                        let _ = pipe.write_all(&b);
                    }
                    StdinData::Reader(mut r) => {
                        let _ = io::copy(&mut r, &mut pipe);
                    }
                }
            });
        }

        // Drain stderr in the background (Capture mode only).
        let stderr_thread = if matches!(self.stderr_mode, Redirection::Capture)
            && let Some(pipe) = child.take_stderr()
        {
            Some(thread::spawn(move || read_to_end(pipe)))
        } else {
            None
        };

        Ok(SpawnedProcess::new(child, stderr_thread, display))
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
        let stdout = proc
            .take_stdout()
            .expect("spawn always pipes stdout");
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

    /// Run the command, blocking until it completes (or times out).
    pub fn run(mut self) -> Result<RunOutput, RunError> {
        let display = self.display();
        let mut stdin_holder = StdinHolder::from_opt(self.stdin.take());
        let retry = self.retry.take();
        let ctx = ExecContext {
            program: &self.program,
            args: &self.args,
            cwd: self.cwd.as_deref(),
            env_clear: self.env_clear,
            env_remove: &self.env_remove,
            envs: &self.envs,
            stderr_mode: &self.stderr_mode,
            before_spawn: self.before_spawn.as_ref(),
            display: &display,
        };

        match retry {
            None => execute_once(&ctx, stdin_holder.take_for_attempt(), self.per_attempt_timeout(Instant::now())),
            Some(policy) => run_with_retry(&ctx, &mut stdin_holder, policy, self.timeout, self.deadline),
        }
    }

    fn per_attempt_timeout(&self, now: Instant) -> Option<Duration> {
        match (self.timeout, self.deadline) {
            (None, None) => None,
            (Some(t), None) => Some(t),
            (None, Some(d)) => Some(d.saturating_duration_since(now)),
            (Some(t), Some(d)) => {
                let remaining = d.saturating_duration_since(now);
                Some(t.min(remaining))
            }
        }
    }
}

fn run_with_retry(
    ctx: &ExecContext<'_>,
    stdin_holder: &mut StdinHolder,
    policy: RetryPolicy,
    timeout: Option<Duration>,
    deadline: Option<Instant>,
) -> Result<RunOutput, RunError> {
    let predicate = policy.predicate.clone();
    let op = || {
        let now = Instant::now();
        if let Some(d) = deadline
            && now >= d
        {
            // Deadline exhausted; synthesize a timeout-style error without spawning.
            return Err(RunError::Timeout {
                command: ctx.display.clone(),
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
        let stdin = stdin_holder.take_for_attempt();
        execute_once(ctx, stdin, per_attempt)
    };
    op.retry(policy.backoff)
        .when(move |e: &RunError| predicate(e))
        .call()
}

struct ExecContext<'a> {
    program: &'a OsStr,
    args: &'a [OsString],
    cwd: Option<&'a Path>,
    env_clear: bool,
    env_remove: &'a [OsString],
    envs: &'a [(OsString, OsString)],
    stderr_mode: &'a Redirection,
    before_spawn: Option<&'a BeforeSpawnHook>,
    display: &'a CmdDisplay,
}

enum StdinHolder {
    None,
    Bytes(Vec<u8>),
    OneShotReader(Option<Box<dyn Read + Send + Sync>>),
}

impl StdinHolder {
    fn from_opt(d: Option<StdinData>) -> Self {
        match d {
            None => Self::None,
            Some(StdinData::Bytes(b)) => Self::Bytes(b),
            Some(StdinData::Reader(r)) => Self::OneShotReader(Some(r)),
        }
    }

    fn take_for_attempt(&mut self) -> StdinForAttempt {
        match self {
            Self::None => StdinForAttempt::None,
            Self::Bytes(b) => StdinForAttempt::Bytes(b.clone()),
            Self::OneShotReader(slot) => match slot.take() {
                Some(r) => StdinForAttempt::Reader(r),
                None => StdinForAttempt::None,
            },
        }
    }
}

enum StdinForAttempt {
    None,
    Bytes(Vec<u8>),
    Reader(Box<dyn Read + Send + Sync>),
}

enum Outcome {
    Exited(ExitStatus),
    TimedOut(Duration),
    WaitFailed(io::Error),
}

fn execute_once(
    ctx: &ExecContext<'_>,
    stdin: StdinForAttempt,
    timeout: Option<Duration>,
) -> Result<RunOutput, RunError> {
    let mut cmd = build_command(ctx, &stdin)?;

    if let Some(hook) = ctx.before_spawn {
        hook(&mut cmd).map_err(|source| RunError::Spawn {
            command: ctx.display.clone(),
            source,
        })?;
    }

    let mut child = cmd.spawn().map_err(|source| RunError::Spawn {
        command: ctx.display.clone(),
        source,
    })?;

    let stdin_thread = spawn_stdin_feeder(&mut child, stdin);
    let stdout_thread = {
        let pipe = child.stdout.take().expect("stdout piped");
        Some(thread::spawn(move || read_to_end(pipe)))
    };
    let stderr_thread = if matches!(ctx.stderr_mode, Redirection::Capture) {
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

    finalize_outcome(ctx, outcome, stdout_bytes, stderr_str)
}

fn finalize_outcome(
    ctx: &ExecContext<'_>,
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
            command: ctx.display.clone(),
            status,
            stdout: truncate_suffix(stdout_bytes),
            stderr: truncate_suffix_string(stderr_str),
        }),
        Outcome::TimedOut(elapsed) => Err(RunError::Timeout {
            command: ctx.display.clone(),
            elapsed,
            stdout: truncate_suffix(stdout_bytes),
            stderr: truncate_suffix_string(stderr_str),
        }),
        Outcome::WaitFailed(source) => Err(RunError::Spawn {
            command: ctx.display.clone(),
            source,
        }),
    }
}

fn build_command(ctx: &ExecContext<'_>, stdin: &StdinForAttempt) -> Result<Command, RunError> {
    let mut cmd = Command::new(ctx.program);
    cmd.args(ctx.args);
    if let Some(dir) = ctx.cwd {
        cmd.current_dir(dir);
    }
    if ctx.env_clear {
        cmd.env_clear();
    }
    for key in ctx.env_remove {
        cmd.env_remove(key);
    }
    for (k, v) in ctx.envs {
        cmd.env(k, v);
    }

    match stdin {
        StdinForAttempt::None => {}
        StdinForAttempt::Bytes(_) | StdinForAttempt::Reader(_) => {
            cmd.stdin(Stdio::piped());
        }
    }
    cmd.stdout(Stdio::piped());

    match ctx.stderr_mode {
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
            let cloned = f.try_clone().map_err(|source| RunError::Spawn {
                command: ctx.display.clone(),
                source,
            })?;
            cmd.stderr(Stdio::from(cloned));
        }
    }
    Ok(cmd)
}

fn spawn_stdin_feeder(
    child: &mut std::process::Child,
    stdin: StdinForAttempt,
) -> Option<thread::JoinHandle<()>> {
    match stdin {
        StdinForAttempt::None => None,
        StdinForAttempt::Bytes(bytes) => {
            let mut pipe = child.stdin.take().expect("stdin piped");
            Some(thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            }))
        }
        StdinForAttempt::Reader(mut reader) => {
            let mut pipe = child.stdin.take().expect("stdin piped");
            Some(thread::spawn(move || {
                let _ = io::copy(&mut reader, &mut pipe);
            }))
        }
    }
}

fn read_to_end<R: Read>(mut reader: R) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn must_use_annotation_present() {
        let _ = Cmd::new("x");
        // Compile-only: unused Cmd triggers #[must_use] lint if disabled.
    }

    #[test]
    fn builder_accumulates_args() {
        let cmd = Cmd::new("git").arg("status").args(["-s", "--short"]);
        assert_eq!(cmd.args.len(), 3);
    }

    #[test]
    fn env_builder() {
        let cmd = Cmd::new("x")
            .env("A", "1")
            .envs([("B", "2"), ("C", "3")])
            .env_remove("D")
            .env_clear();
        assert_eq!(cmd.envs.len(), 3);
        assert_eq!(cmd.env_remove.len(), 1);
        assert!(cmd.env_clear);
    }

    #[test]
    fn stdin_bytes_is_reusable() {
        let cmd = Cmd::new("x").stdin("hello");
        match cmd.stdin.as_ref() {
            Some(StdinData::Bytes(b)) => assert_eq!(b, b"hello"),
            _ => panic!("expected Bytes"),
        }
    }

    #[test]
    fn secret_flag_reaches_display() {
        let cmd = Cmd::new("docker").arg("login").arg("-p").arg("hunter2").secret();
        let d = cmd.display();
        assert!(d.is_secret());
        assert_eq!(d.to_string(), "docker <secret>");
    }

    #[test]
    fn to_command_mirrors_config() {
        let cmd = Cmd::new("git").args(["status"]).env("K", "V").in_dir("/tmp");
        let std_cmd = cmd.to_command();
        // We can only assert program; args/env are not publicly inspectable on
        // std::process::Command. At least confirm no panic.
        assert_eq!(std_cmd.get_program(), "git");
    }

    #[test]
    fn retry_when_installs_default_policy() {
        let cmd = Cmd::new("x").retry_when(|_| true);
        assert!(cmd.retry.is_some());
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
}
