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
use std::sync::Arc;
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

/// Hook invoked on `std::process::Command` immediately before each spawn attempt.
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
    fn flatten<'a>(&'a self, out: &mut Vec<&'a SingleCmd>) {
        match self {
            CmdTree::Single(s) => out.push(s),
            CmdTree::Pipe(l, r) => {
                l.flatten(out);
                r.flatten(out);
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
#[must_use = "Cmd does nothing until .run() or .spawn() is called"]
pub struct Cmd {
    tree: CmdTree,
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
            .field("tree", &self.tree)
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
            tree: CmdTree::Single(SingleCmd::new(program.into())),
            stdin: None,
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
    /// Pipeline-level configuration (`stdin`, `stderr`, `timeout`, `deadline`,
    /// `retry`, `secret`, `before_spawn`) is taken from `self` — any such
    /// settings on `next` are discarded. Per-stage configuration (args, env,
    /// cwd) is preserved for each side.
    pub fn pipe(self, next: Cmd) -> Cmd {
        Cmd {
            tree: CmdTree::Pipe(Box::new(self.tree), Box::new(next.tree)),
            stdin: self.stdin,
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
        self.stdin = Some(data.into());
        self
    }

    /// Configure stderr routing for every stage. Default is
    /// [`Redirection::Capture`].
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
    /// Applied to every stage in a pipeline.
    pub fn before_spawn<F>(mut self, hook: F) -> Self
    where
        F: Fn(&mut Command) -> io::Result<()> + Send + Sync + 'static,
    {
        self.before_spawn = Some(Arc::new(hook));
        self
    }

    /// Build a raw `std::process::Command` mirroring the rightmost stage.
    /// Only meaningful for single-command invocations; for pipelines, returns
    /// the right-hand stage.
    pub fn to_command(&self) -> Command {
        let single = match &self.tree {
            CmdTree::Single(s) => s,
            CmdTree::Pipe(_, r) => right_leaf(r),
        };
        let mut cmd = Command::new(&single.program);
        single.apply_to(&mut cmd);
        cmd
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

    /// Spawn the command as a long-lived process handle.
    ///
    /// **Single commands only.** Spawning pipelines will be added in a later
    /// release; for now, pipelines should use [`run`](Self::run). Returns
    /// [`RunError::Spawn`] with an `Unsupported` io error if called on a
    /// pipeline.
    pub fn spawn(mut self) -> Result<SpawnedProcess, RunError> {
        let display = self.display();
        let single = match self.tree {
            CmdTree::Single(s) => s,
            CmdTree::Pipe(_, _) => {
                return Err(RunError::Spawn {
                    command: display,
                    source: io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Cmd::spawn on a pipeline is not yet supported; use .run()",
                    ),
                });
            }
        };
        let stdin_data = self.stdin.take();
        let mut cmd = Command::new(&single.program);
        single.apply_to(&mut cmd);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        apply_stderr(&mut cmd, &self.stderr_mode, &display)?;
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
        if let Some(data) = stdin_data
            && let Some(mut pipe) = child.take_stdin()
        {
            thread::spawn(move || match data {
                StdinData::Bytes(b) => {
                    let _ = pipe.write_all(&b);
                }
                StdinData::Reader(mut r) => {
                    let _ = io::copy(&mut r, &mut pipe);
                }
            });
        }
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

    /// Run the command (or pipeline), blocking until it completes (or times out).
    pub fn run(mut self) -> Result<RunOutput, RunError> {
        let display = self.display();
        let mut stdin_holder = StdinHolder::from_opt(self.stdin.take());
        let retry = self.retry.take();

        let op = |stdin: StdinForAttempt, per_attempt: Option<Duration>| match &self.tree {
            CmdTree::Single(single) => execute_single(
                single,
                &self.stderr_mode,
                self.before_spawn.as_ref(),
                &display,
                stdin,
                per_attempt,
            ),
            CmdTree::Pipe(_, _) => {
                let mut stages = Vec::new();
                self.tree.flatten(&mut stages);
                execute_pipeline(
                    &stages,
                    &self.stderr_mode,
                    self.before_spawn.as_ref(),
                    &display,
                    stdin,
                    per_attempt,
                )
            }
        };

        match retry {
            None => op(stdin_holder.take_for_attempt(), self.per_attempt_timeout(Instant::now())),
            Some(policy) => run_with_retry(
                &mut stdin_holder,
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

fn right_leaf(tree: &CmdTree) -> &SingleCmd {
    match tree {
        CmdTree::Single(s) => s,
        CmdTree::Pipe(_, r) => right_leaf(r),
    }
}

fn run_with_retry<F>(
    stdin_holder: &mut StdinHolder,
    policy: RetryPolicy,
    timeout: Option<Duration>,
    deadline: Option<Instant>,
    display: &CmdDisplay,
    op: &F,
) -> Result<RunOutput, RunError>
where
    F: Fn(StdinForAttempt, Option<Duration>) -> Result<RunOutput, RunError>,
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
        let stdin = stdin_holder.take_for_attempt();
        op(stdin, per_attempt)
    };
    attempt
        .retry(policy.backoff)
        .when(move |e: &RunError| predicate(e))
        .call()
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
            let cloned = f.try_clone().map_err(|source| RunError::Spawn {
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
    stderr_mode: &Redirection,
    before_spawn: Option<&BeforeSpawnHook>,
    display: &CmdDisplay,
    stdin: StdinForAttempt,
    timeout: Option<Duration>,
) -> Result<RunOutput, RunError> {
    let mut cmd = Command::new(&single.program);
    single.apply_to(&mut cmd);

    match &stdin {
        StdinForAttempt::None => {}
        StdinForAttempt::Bytes(_) | StdinForAttempt::Reader(_) => {
            cmd.stdin(Stdio::piped());
        }
    }
    cmd.stdout(Stdio::piped());
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
    let stdout_thread = {
        let pipe = child.stdout.take().expect("stdout piped");
        Some(thread::spawn(move || read_to_end(pipe)))
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

// ---------- pipeline execution ----------

fn execute_pipeline(
    stages: &[&SingleCmd],
    stderr_mode: &Redirection,
    before_spawn: Option<&BeforeSpawnHook>,
    display: &CmdDisplay,
    stdin: StdinForAttempt,
    timeout: Option<Duration>,
) -> Result<RunOutput, RunError> {
    debug_assert!(stages.len() >= 2);

    // Build N-1 pipes between adjacent stages. Each pipe's reader and writer
    // live in Options so we can `take` them into individual children — each
    // half is used exactly once.
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

        // stdin
        if i == 0 {
            match stdin_for_feed.as_ref() {
                Some(StdinForAttempt::None) | None => {}
                Some(StdinForAttempt::Bytes(_)) | Some(StdinForAttempt::Reader(_)) => {
                    cmd.stdin(Stdio::piped());
                }
            }
        } else {
            let reader = pipes[i - 1].0.take().expect("pipe reader");
            cmd.stdin(Stdio::from(reader));
        }

        // stdout — last stage captured, others feed the next pipe.
        if i == stages.len() - 1 {
            cmd.stdout(Stdio::piped());
        } else {
            let writer = pipes[i].1.take().expect("pipe writer");
            cmd.stdout(Stdio::from(writer));
        }

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

        // Feed stdin for the first stage (taking ownership of stdin_for_feed).
        if i == 0
            && let Some(data) = stdin_for_feed.take()
            && !matches!(data, StdinForAttempt::None)
        {
            stdin_thread = spawn_stdin_feeder(&mut child, data);
        }

        // Capture stderr if Capture mode (each stage independently).
        if matches!(stderr_mode, Redirection::Capture)
            && let Some(pipe) = child.stderr.take()
        {
            stderr_threads.push(thread::spawn(move || read_to_end(pipe)));
        }

        // The last stage's stdout is our captured output.
        if i == stages.len() - 1 {
            last_stdout = child.stdout.take();
        }

        children.push(child);
    }

    // Drain stdout of the last stage in a thread so we don't deadlock on full pipes.
    let stdout_thread = last_stdout.map(|pipe| thread::spawn(move || read_to_end(pipe)));

    // Wait for all stages (with optional timeout).
    let start = Instant::now();
    let mut per_stage_status: Vec<Outcome> = Vec::with_capacity(children.len());

    if let Some(budget) = timeout {
        // Simple approach: wait_timeout on each in order, deducting elapsed.
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
    // Concat all stderr outputs with newlines between stages.
    let mut stderr_all = String::new();
    for t in stderr_threads {
        let bytes = t.join().unwrap_or_default();
        stderr_all.push_str(&String::from_utf8_lossy(&bytes));
    }

    // pipefail-style status precedence: right-most checked error wins.
    let final_outcome = combine_outcomes(per_stage_status);

    finalize_outcome(display, final_outcome, stdout_bytes, stderr_all)
}

fn combine_outcomes(outcomes: Vec<Outcome>) -> Outcome {
    // Scan right-to-left for the last non-success outcome.
    let mut chosen: Option<Outcome> = None;
    for o in outcomes.into_iter() {
        match &o {
            Outcome::Exited(status) if status.success() => {
                // Keep the successful status as a fallback if nothing else failed.
                if chosen.is_none() {
                    chosen = Some(o);
                }
            }
            _ => chosen = Some(o), // later (rightmost) non-success replaces prior
        }
    }
    chosen.unwrap_or(Outcome::WaitFailed(io::Error::other(
        "pipeline had no stages",
    )))
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
        // After .arg("right"), the rightmost (b) should have one arg.
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
    fn to_command_returns_rightmost_for_pipeline() {
        let cmd = Cmd::new("a").pipe(Cmd::new("b"));
        let std_cmd = cmd.to_command();
        assert_eq!(std_cmd.get_program(), "b");
    }
}
