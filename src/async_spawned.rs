//! Handle returned by [`Cmd::spawn_async`](crate::Cmd::spawn_async), the
//! tokio counterpart to [`SpawnedProcess`](crate::SpawnedProcess).
//!
//! Lifecycle methods take `&mut self` rather than `&self` — concurrent
//! kill-during-wait is handled via [`tokio::select!`], not by sharing the
//! handle by reference.

use std::io;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::task::JoinHandle;

use crate::cmd::RunOutput;
use crate::cmd_display::CmdDisplay;
use crate::error::{RunError, truncate_suffix, truncate_suffix_string};

/// Handle to one or more spawned async subprocesses (a single command or a
/// pipeline).
///
/// For pipelines, `take_stdin` targets the leftmost stage, `take_stdout`
/// the rightmost, and lifecycle methods operate on every stage. Exit
/// status follows pipefail: rightmost non-success wins.
///
/// # Wait idempotency
///
/// [`wait`](Self::wait), [`try_wait`](Self::try_wait), and
/// [`wait_timeout`](Self::wait_timeout) are all idempotent. The first
/// finalize captures stdout, stderr, and per-stage exit statuses into an
/// internal `Arc`; subsequent calls reconstruct the same
/// `Result<RunOutput, RunError>` from that cache. This matters for
/// `tokio::select!` cancellation patterns where a pending `wait` future is
/// dropped and another `wait` is issued after `kill`.
///
/// Cost of the second call: one `Vec<u8>` clone and one `String` clone per
/// invocation (the cached raw bytes are copied into a fresh `RunOutput`).
pub struct AsyncSpawnedProcess {
    children: Vec<Child>,
    stderr_tasks: Vec<JoinHandle<Vec<u8>>>,
    command: CmdDisplay,
    // None until the first successful finalize; then populated with the
    // raw ingredients so subsequent wait calls return the same outcome.
    finalized: Option<Arc<FinalizedState>>,
}

struct FinalizedState {
    statuses: Vec<ExitStatus>,
    stdout: Vec<u8>,
    stderr: String,
}

impl AsyncSpawnedProcess {
    pub(crate) fn new_single(
        child: Child,
        stderr_task: Option<JoinHandle<Vec<u8>>>,
        command: CmdDisplay,
    ) -> Self {
        Self {
            children: vec![child],
            stderr_tasks: stderr_task.into_iter().collect(),
            command,
            finalized: None,
        }
    }

    pub(crate) fn new_pipeline(
        children: Vec<Child>,
        stderr_tasks: Vec<JoinHandle<Vec<u8>>>,
        command: CmdDisplay,
    ) -> Self {
        debug_assert!(!children.is_empty());
        Self {
            children,
            stderr_tasks,
            command,
            finalized: None,
        }
    }

    /// Shell-style, secret-respecting rendering of the command.
    pub fn command(&self) -> &CmdDisplay {
        &self.command
    }

    /// Whether this handle represents a multi-stage pipeline.
    pub fn is_pipeline(&self) -> bool {
        self.children.len() > 1
    }

    /// Pids of every stage, leftmost first. `None` entries are filtered —
    /// tokio returns `Option<u32>` for the pid.
    pub fn pids(&self) -> Vec<u32> {
        self.children.iter().filter_map(|c| c.id()).collect()
    }

    /// Take ownership of the leftmost stage's stdin. `None` after the first
    /// call or if stdin wasn't piped.
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.children[0].stdin.take()
    }

    /// Take ownership of the rightmost stage's stdout. `None` after the
    /// first call.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.children.last_mut()?.stdout.take()
    }

    /// Send `SIGKILL` (Unix) / `TerminateProcess` (Windows) to every stage.
    /// Returns the first error encountered; still attempts all stages.
    pub async fn kill(&mut self) -> io::Result<()> {
        let mut first_err: Option<io::Error> = None;
        for c in &mut self.children {
            if let Err(e) = c.kill().await
                && first_err.is_none()
            {
                first_err = Some(e);
            }
        }
        match first_err {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    /// Non-blocking status check. Returns `Ok(None)` if any stage is still
    /// running. Idempotent once all stages have exited.
    pub async fn try_wait(&mut self) -> Result<Option<RunOutput>, RunError> {
        if let Some(state) = self.finalized.clone() {
            return self.reconstruct(&state).map(Some);
        }
        let mut statuses = Vec::with_capacity(self.children.len());
        for c in &mut self.children {
            match c.try_wait() {
                Ok(Some(status)) => statuses.push(status),
                Ok(None) => return Ok(None),
                Err(source) => {
                    return Err(RunError::Spawn {
                        command: self.command.clone(),
                        source,
                    });
                }
            }
        }
        self.finalize(statuses).await.map(Some)
    }

    /// Await every stage; assemble a [`RunOutput`] or [`RunError::NonZeroExit`]
    /// using pipefail precedence. Idempotent — subsequent calls return the
    /// same cached outcome.
    pub async fn wait(&mut self) -> Result<RunOutput, RunError> {
        if let Some(state) = self.finalized.clone() {
            return self.reconstruct(&state);
        }
        let mut statuses = Vec::with_capacity(self.children.len());
        for c in &mut self.children {
            let status = c.wait().await.map_err(|source| RunError::Spawn {
                command: self.command.clone(),
                source,
            })?;
            statuses.push(status);
        }
        self.finalize(statuses).await
    }

    /// Wait up to `timeout`. `Ok(None)` means at least one stage is still
    /// running; caller decides whether to [`kill`](Self::kill) or wait again.
    /// Idempotent once all stages have exited.
    pub async fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<RunOutput>, RunError> {
        match tokio::time::timeout(timeout, self.wait()).await {
            Ok(res) => res.map(Some),
            Err(_) => Ok(None),
        }
    }

    fn reconstruct(&self, state: &FinalizedState) -> Result<RunOutput, RunError> {
        let chosen = pipefail_status(&state.statuses);
        if chosen.success() {
            Ok(RunOutput {
                stdout: state.stdout.clone(),
                stderr: state.stderr.clone(),
            })
        } else {
            Err(RunError::NonZeroExit {
                command: self.command.clone(),
                status: chosen,
                stdout: truncate_suffix(state.stdout.clone()),
                stderr: truncate_suffix_string(state.stderr.clone()),
            })
        }
    }

    async fn finalize(&mut self, statuses: Vec<ExitStatus>) -> Result<RunOutput, RunError> {
        // Guard against double-finalize in case a concurrent path got here
        // first (e.g., via try_wait). &mut self makes this unlikely in
        // practice, but the check is cheap.
        if let Some(state) = self.finalized.clone() {
            return self.reconstruct(&state);
        }
        let stderr_bytes = self.drain_stderr_tasks().await;
        let stderr_str = String::from_utf8_lossy(&stderr_bytes).into_owned();
        let stdout_bytes = self.drain_stdout().await;
        let state = Arc::new(FinalizedState {
            statuses,
            stdout: stdout_bytes,
            stderr: stderr_str,
        });
        self.finalized = Some(Arc::clone(&state));
        self.reconstruct(&state)
    }

    async fn drain_stderr_tasks(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for t in self.stderr_tasks.drain(..) {
            if let Ok(bytes) = t.await {
                out.extend(bytes);
            }
        }
        out
    }

    async fn drain_stdout(&mut self) -> Vec<u8> {
        let Some(last) = self.children.last_mut() else {
            return Vec::new();
        };
        let Some(mut pipe) = last.stdout.take() else {
            return Vec::new();
        };
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf).await;
        buf
    }
}

impl std::fmt::Debug for AsyncSpawnedProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncSpawnedProcess")
            .field("command", &self.command)
            .field("pids", &self.pids())
            .finish()
    }
}

/// Duct-style pipefail: rightmost non-success wins; if all succeed, returns
/// the rightmost success (any will do — they're equivalent).
fn pipefail_status(statuses: &[ExitStatus]) -> ExitStatus {
    let mut chosen = statuses[0];
    for &s in statuses.iter().skip(1) {
        if !s.success() || chosen.success() {
            chosen = s;
        }
    }
    chosen
}
