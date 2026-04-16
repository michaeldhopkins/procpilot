//! Long-lived process handles for streaming and bidirectional protocols.
//!
//! [`Cmd::spawn`](crate::Cmd::spawn) returns a [`SpawnedProcess`] instead of
//! running the command synchronously. Use it for:
//!
//! - **Bidirectional protocols** (`git cat-file --batch`, `jj log --stream-json`):
//!   [`take_stdin`](SpawnedProcess::take_stdin) and
//!   [`take_stdout`](SpawnedProcess::take_stdout) give you the owned pipe
//!   handles for interactive I/O.
//! - **Live streaming of lines** (`cargo check --message-format=json`,
//!   `kubectl logs -f`): use [`Cmd::spawn_and_collect_lines`](crate::Cmd::spawn_and_collect_lines)
//!   or the `Read` impl on [`SpawnedProcess`].
//! - **Pipelines** — spawning `a | b | c` yields one `SpawnedProcess` whose
//!   [`take_stdin`](SpawnedProcess::take_stdin) routes to the leftmost stage
//!   and [`take_stdout`](SpawnedProcess::take_stdout) reads from the
//!   rightmost. Lifecycle methods operate on all stages.
//!
//! Stderr (when [`Redirection::Capture`](crate::Redirection::Capture), the
//! default) is drained into a background thread and attached to the
//! [`RunOutput`] / [`RunError`] on [`wait`](SpawnedProcess::wait).

use std::io::{self, Read};
use std::process::{ChildStdin, ChildStdout, ExitStatus};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use shared_child::SharedChild;

use crate::cmd::RunOutput;
use crate::cmd_display::CmdDisplay;
use crate::error::{RunError, truncate_suffix, truncate_suffix_string};

/// Handle to one or more spawned subprocesses (a single command or a pipeline).
///
/// Lifecycle methods ([`wait`](Self::wait), [`kill`](Self::kill),
/// [`try_wait`](Self::try_wait), [`wait_timeout`](Self::wait_timeout)) take
/// `&self` so the handle can be shared across threads. Stdio accessors
/// ([`take_stdin`](Self::take_stdin), [`take_stdout`](Self::take_stdout))
/// are one-shot — the second call returns `None`.
///
/// For pipelines, `take_stdin` targets the leftmost stage, `take_stdout` the
/// rightmost, and wait/kill operate on every stage. Exit status follows
/// pipefail semantics: rightmost non-success wins.
///
/// # Wait idempotency
///
/// [`wait`](Self::wait), [`try_wait`](Self::try_wait), and
/// [`wait_timeout`](Self::wait_timeout) are all idempotent. The first
/// finalize captures stdout, stderr, and per-stage exit statuses into an
/// internal `Arc`; subsequent calls reconstruct the same
/// `Result<RunOutput, RunError>` from that cache. This matters for:
///
/// - **`tokio::select!`-style cancellation** where a pending `wait` future
///   is dropped and a second `wait` is issued after `kill`.
/// - **Retry loops** over a spawned handle.
/// - **Concurrent `wait` calls** from multiple threads — internal
///   serialization ensures both see the same outcome, not split-brain
///   partial state.
///
/// Cost of the second call: one `Vec<u8>` clone and one `String` clone per
/// invocation (the cached raw bytes are copied into a fresh `RunOutput`).
/// For multi-gigabyte outputs this is not free, but the common cases
/// (accidental double-call, cancellation pattern) are cheap.
///
/// # Dropping without waiting
///
/// Dropping a `SpawnedProcess` without calling [`wait`](Self::wait) leaves
/// the child(ren) to be reaped by the OS; a valid pattern for
/// fire-and-forget jobs but may leave short-lived zombies until parent
/// exit on Unix.
pub struct SpawnedProcess {
    children: Vec<Arc<SharedChild>>,
    stdout: Mutex<StdoutState>,
    stderr_threads: Mutex<Vec<thread::JoinHandle<Vec<u8>>>>,
    command: CmdDisplay,
    // None until the first successful finalize; then populated with the
    // raw ingredients so subsequent wait/try_wait/wait_timeout return the
    // same outcome.
    finalized: Mutex<Option<Arc<FinalizedState>>>,
}

/// Captured ingredients of a finished invocation. Shared across repeat
/// calls to wait/try_wait/wait_timeout via `Arc` so the stdout/stderr are
/// stored exactly once.
struct FinalizedState {
    statuses: Vec<ExitStatus>,
    stdout: Vec<u8>,
    stderr: String,
}

enum StdoutState {
    /// Still held inside the rightmost `SharedChild`; not yet taken.
    NotTaken,
    /// Taken by us (lazily, on first `Read`) and cached here.
    Cached(ChildStdout),
    /// Handed to the caller via [`take_stdout`]; reads return EOF,
    /// finalize won't try to drain.
    GivenAway,
}

impl SpawnedProcess {
    pub(crate) fn new_single(
        child: Arc<SharedChild>,
        stderr_thread: Option<thread::JoinHandle<Vec<u8>>>,
        command: CmdDisplay,
    ) -> Self {
        Self {
            children: vec![child],
            stdout: Mutex::new(StdoutState::NotTaken),
            stderr_threads: Mutex::new(stderr_thread.into_iter().collect()),
            command,
            finalized: Mutex::new(None),
        }
    }

    pub(crate) fn new_pipeline(
        children: Vec<Arc<SharedChild>>,
        stderr_threads: Vec<thread::JoinHandle<Vec<u8>>>,
        command: CmdDisplay,
    ) -> Self {
        debug_assert!(!children.is_empty());
        Self {
            children,
            stdout: Mutex::new(StdoutState::NotTaken),
            stderr_threads: Mutex::new(stderr_threads),
            command,
            finalized: Mutex::new(None),
        }
    }

    /// Snapshot of the command used to spawn (shell-quoted, secret-redacted).
    pub fn command(&self) -> &CmdDisplay {
        &self.command
    }

    /// Whether this handle represents a multi-stage pipeline.
    pub fn is_pipeline(&self) -> bool {
        self.children.len() > 1
    }

    /// Take ownership of the leftmost child's stdin. Returns `None` after the
    /// first call or if stdin wasn't piped. Drop the returned `ChildStdin` to
    /// send EOF.
    pub fn take_stdin(&self) -> Option<ChildStdin> {
        self.children[0].take_stdin()
    }

    /// Take ownership of the rightmost child's stdout. Returns `None` after
    /// the first call or once the [`Read`] impl has consumed stdout.
    pub fn take_stdout(&self) -> Option<ChildStdout> {
        let mut guard = self.stdout.lock().ok()?;
        if matches!(*guard, StdoutState::NotTaken) {
            *guard = StdoutState::GivenAway;
            self.children.last()?.take_stdout()
        } else {
            None
        }
    }

    /// All pids, leftmost-first. For a single command, length 1.
    pub fn pids(&self) -> Vec<u32> {
        self.children.iter().map(|c| c.id()).collect()
    }

    /// Send `SIGKILL` (Unix) or `TerminateProcess` (Windows) to every stage.
    /// Returns the first error encountered, if any; still attempts all.
    pub fn kill(&self) -> io::Result<()> {
        let mut first_err: Option<io::Error> = None;
        for c in &self.children {
            if let Err(e) = c.kill()
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

    /// Non-blocking status check. `Ok(None)` means at least one stage is
    /// still running; only returns `Ok(Some(_))` when every stage has
    /// exited. Idempotent — after the first `Ok(Some(_))`, subsequent
    /// calls return the same cached outcome.
    pub fn try_wait(&self) -> Result<Option<RunOutput>, RunError> {
        if let Some(state) = self.cached_state() {
            return self.reconstruct(&state).map(Some);
        }
        let mut statuses = Vec::with_capacity(self.children.len());
        for c in &self.children {
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
        self.finalize_or_cached(statuses).map(Some)
    }

    /// Block until every stage exits, then assemble a [`RunOutput`] or
    /// [`RunError::NonZeroExit`] using pipefail status precedence.
    /// Idempotent — subsequent calls return the same cached outcome.
    pub fn wait(&self) -> Result<RunOutput, RunError> {
        if let Some(state) = self.cached_state() {
            return self.reconstruct(&state);
        }
        let mut statuses = Vec::with_capacity(self.children.len());
        for c in &self.children {
            let status = c.wait().map_err(|source| RunError::Spawn {
                command: self.command.clone(),
                source,
            })?;
            statuses.push(status);
        }
        self.finalize_or_cached(statuses)
    }

    /// Wait up to `timeout`. `Ok(None)` means at least one stage is still
    /// running after the timeout — caller decides whether to
    /// [`kill`](Self::kill) or wait again. Idempotent once all stages
    /// have exited.
    pub fn wait_timeout(&self, timeout: Duration) -> Result<Option<RunOutput>, RunError> {
        if let Some(state) = self.cached_state() {
            return self.reconstruct(&state).map(Some);
        }
        let start = Instant::now();
        let mut statuses = Vec::with_capacity(self.children.len());
        for c in &self.children {
            let remaining = timeout.saturating_sub(start.elapsed());
            match c.wait_timeout(remaining) {
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
        self.finalize_or_cached(statuses).map(Some)
    }

    fn cached_state(&self) -> Option<Arc<FinalizedState>> {
        self.finalized
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(Arc::clone))
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

    /// Holds the `finalized` lock for the entire finalize sequence so
    /// concurrent callers can't race on draining stderr/stdout; whoever
    /// gets the lock first fills the cache, others see it on entry.
    fn finalize_or_cached(
        &self,
        statuses: Vec<ExitStatus>,
    ) -> Result<RunOutput, RunError> {
        let mut guard = self
            .finalized
            .lock()
            .expect("finalized mutex poisoned");
        if let Some(state) = guard.as_ref() {
            let state = Arc::clone(state);
            drop(guard);
            return self.reconstruct(&state);
        }
        let stderr_bytes = self.join_stderr_threads();
        let stderr_str = String::from_utf8_lossy(&stderr_bytes).into_owned();
        let stdout_bytes = self.drain_remaining_stdout();
        let state = Arc::new(FinalizedState {
            statuses,
            stdout: stdout_bytes,
            stderr: stderr_str,
        });
        *guard = Some(Arc::clone(&state));
        drop(guard);
        self.reconstruct(&state)
    }

    fn join_stderr_threads(&self) -> Vec<u8> {
        let Ok(mut guard) = self.stderr_threads.lock() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for t in guard.drain(..) {
            if let Ok(bytes) = t.join() {
                out.extend(bytes);
            }
        }
        out
    }

    fn drain_remaining_stdout(&self) -> Vec<u8> {
        let Ok(mut guard) = self.stdout.lock() else {
            return Vec::new();
        };
        let mut pipe = match std::mem::replace(&mut *guard, StdoutState::GivenAway) {
            StdoutState::NotTaken => match self.children.last().and_then(|c| c.take_stdout()) {
                Some(p) => p,
                None => return Vec::new(),
            },
            StdoutState::Cached(p) => p,
            StdoutState::GivenAway => return Vec::new(),
        };
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    }
}

/// Duct-style pipefail: rightmost non-success wins; if all succeed, the
/// rightmost success (any, they're equivalent) wins.
fn pipefail_status(statuses: &[ExitStatus]) -> ExitStatus {
    // A later non-success always replaces the prior choice. A later success
    // only replaces if the prior is also a success — so an earlier failure
    // "sticks" past subsequent successes (matching pipefail).
    let mut chosen = statuses[0];
    for &s in statuses.iter().skip(1) {
        if !s.success() || chosen.success() {
            chosen = s;
        }
    }
    chosen
}

impl std::fmt::Debug for SpawnedProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnedProcess")
            .field("command", &self.command)
            .field("pids", &self.pids())
            .finish()
    }
}

/// Read directly from the rightmost stage's stdout.
///
/// On first read, takes ownership of stdout internally (so subsequent
/// [`take_stdout`](SpawnedProcess::take_stdout) calls return `None`).
/// Reads return `Ok(0)` when stdout closes (EOF). Call
/// [`wait`](SpawnedProcess::wait) after EOF to surface the exit status.
impl Read for SpawnedProcess {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_via_handle(self, buf)
    }
}

/// Dual impl enabling `(&proc).read(…)`. Lets one thread read while another
/// holds the handle by reference and calls [`kill`](SpawnedProcess::kill)
/// or [`wait`](SpawnedProcess::wait).
impl Read for &SpawnedProcess {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_via_handle(self, buf)
    }
}

fn read_via_handle(p: &SpawnedProcess, buf: &mut [u8]) -> io::Result<usize> {
    let mut guard = p
        .stdout
        .lock()
        .map_err(|_| io::Error::other("stdout mutex poisoned"))?;
    if matches!(*guard, StdoutState::NotTaken) {
        match p.children.last().and_then(|c| c.take_stdout()) {
            Some(pipe) => *guard = StdoutState::Cached(pipe),
            None => *guard = StdoutState::GivenAway,
        }
    }
    match &mut *guard {
        StdoutState::Cached(pipe) => pipe.read(buf),
        StdoutState::NotTaken | StdoutState::GivenAway => Ok(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn make_status(code: i32) -> ExitStatus {
        // waitpid encodes the exit code in the high byte of the status word;
        // ExitStatus::from_raw expects that encoding, not the raw code.
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn make_status(code: u32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code)
    }

    #[test]
    fn pipefail_rightmost_failure_wins() {
        let s = pipefail_status(&[make_status(1), make_status(0), make_status(2)]);
        assert_eq!(s.code(), Some(2));
    }

    #[test]
    fn pipefail_only_failure_wins_over_later_success() {
        let s = pipefail_status(&[make_status(7), make_status(0), make_status(0)]);
        assert_eq!(s.code(), Some(7));
    }

    #[test]
    fn pipefail_all_success() {
        let s = pipefail_status(&[make_status(0), make_status(0)]);
        assert!(s.success());
    }
}
