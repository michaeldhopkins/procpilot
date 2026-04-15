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
//!
//! Stderr (when [`Redirection::Capture`](crate::Redirection::Capture), the
//! default) is drained into a background thread and attached to the
//! [`RunOutput`] / [`RunError`] on [`wait`](SpawnedProcess::wait).

use std::io::{self, Read};
use std::process::{ChildStdin, ChildStdout, ExitStatus};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use shared_child::SharedChild;

use crate::cmd::RunOutput;
use crate::cmd_display::CmdDisplay;
use crate::error::{RunError, truncate_suffix, truncate_suffix_string};

/// Handle to a spawned subprocess.
///
/// Lifecycle methods ([`wait`](Self::wait), [`kill`](Self::kill),
/// [`try_wait`](Self::try_wait), [`wait_timeout`](Self::wait_timeout)) take
/// `&self` so the handle can be shared across threads. Stdio accessors
/// ([`take_stdin`](Self::take_stdin), [`take_stdout`](Self::take_stdout))
/// are one-shot — the second call returns `None`.
///
/// Dropping a `SpawnedProcess` without calling [`wait`](Self::wait) leaves
/// the child to be reaped by the OS; this is a valid pattern for
/// fire-and-forget jobs but may leave a short-lived zombie until parent exit
/// on Unix.
pub struct SpawnedProcess {
    child: Arc<SharedChild>,
    stdout: Mutex<StdoutState>,
    stderr_thread: Mutex<Option<thread::JoinHandle<Vec<u8>>>>,
    command: CmdDisplay,
}

enum StdoutState {
    /// Still held inside `SharedChild`; not yet taken.
    NotTaken,
    /// Taken by us (lazily, on first `Read`) and cached here.
    Cached(ChildStdout),
    /// Handed to the caller via [`take_stdout`]; reads return EOF,
    /// finalize won't try to drain.
    GivenAway,
}

impl SpawnedProcess {
    pub(crate) fn new(
        child: Arc<SharedChild>,
        stderr_thread: Option<thread::JoinHandle<Vec<u8>>>,
        command: CmdDisplay,
    ) -> Self {
        Self {
            child,
            stdout: Mutex::new(StdoutState::NotTaken),
            stderr_thread: Mutex::new(stderr_thread),
            command,
        }
    }

    /// Snapshot of the command used to spawn (shell-quoted, secret-redacted).
    pub fn command(&self) -> &CmdDisplay {
        &self.command
    }

    /// Take ownership of the child's stdin. Returns `None` after the first
    /// call or if stdin wasn't piped. Drop the returned `ChildStdin` to send
    /// EOF.
    pub fn take_stdin(&self) -> Option<ChildStdin> {
        self.child.take_stdin()
    }

    /// Take ownership of the child's stdout. Returns `None` after the first
    /// call or once the [`Read`] impl has consumed stdout.
    pub fn take_stdout(&self) -> Option<ChildStdout> {
        let mut guard = self.stdout.lock().ok()?;
        if matches!(*guard, StdoutState::NotTaken) {
            *guard = StdoutState::GivenAway;
            self.child.take_stdout()
        } else {
            None
        }
    }

    /// Pids of the process (one for now; `Vec` to keep the shape stable
    /// when pipelines land in Phase 3).
    pub fn pids(&self) -> Vec<u32> {
        vec![self.child.id()]
    }

    /// Kill the child. Callable from any thread, even while another thread
    /// is blocked in [`wait`](Self::wait) — `shared_child` provides the
    /// lock-free kill path.
    pub fn kill(&self) -> io::Result<()> {
        self.child.kill()
    }

    /// Non-blocking status check. `Ok(None)` means still running.
    pub fn try_wait(&self) -> Result<Option<RunOutput>, RunError> {
        match self.child.try_wait() {
            Ok(Some(status)) => self.finalize(status).map(Some),
            Ok(None) => Ok(None),
            Err(source) => Err(RunError::Spawn {
                command: self.command.clone(),
                source,
            }),
        }
    }

    /// Block until the child exits, then assemble a [`RunOutput`] or
    /// [`RunError::NonZeroExit`] (attaching drained stderr and any stdout
    /// still held internally).
    pub fn wait(&self) -> Result<RunOutput, RunError> {
        let status = self.child.wait().map_err(|source| RunError::Spawn {
            command: self.command.clone(),
            source,
        })?;
        self.finalize(status)
    }

    /// Wait up to `timeout`. `Ok(None)` means the child is still running —
    /// caller decides whether to [`kill`](Self::kill) or wait again.
    pub fn wait_timeout(&self, timeout: Duration) -> Result<Option<RunOutput>, RunError> {
        match self.child.wait_timeout(timeout) {
            Ok(Some(status)) => self.finalize(status).map(Some),
            Ok(None) => Ok(None),
            Err(source) => Err(RunError::Spawn {
                command: self.command.clone(),
                source,
            }),
        }
    }

    fn finalize(&self, status: ExitStatus) -> Result<RunOutput, RunError> {
        let stderr_bytes = self
            .stderr_thread
            .lock()
            .ok()
            .and_then(|mut s| s.take())
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        let stderr_str = String::from_utf8_lossy(&stderr_bytes).into_owned();

        let stdout_bytes = self.drain_remaining_stdout();

        if status.success() {
            Ok(RunOutput {
                stdout: stdout_bytes,
                stderr: stderr_str,
            })
        } else {
            Err(RunError::NonZeroExit {
                command: self.command.clone(),
                status,
                stdout: truncate_suffix(stdout_bytes),
                stderr: truncate_suffix_string(stderr_str),
            })
        }
    }

    fn drain_remaining_stdout(&self) -> Vec<u8> {
        let Ok(mut guard) = self.stdout.lock() else {
            return Vec::new();
        };
        let mut pipe = match std::mem::replace(&mut *guard, StdoutState::GivenAway) {
            StdoutState::NotTaken => match self.child.take_stdout() {
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

impl std::fmt::Debug for SpawnedProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnedProcess")
            .field("command", &self.command)
            .field("pid", &self.child.id())
            .finish()
    }
}

/// Read directly from the child's stdout.
///
/// On first read, takes ownership of stdout internally (so subsequent
/// [`take_stdout`](SpawnedProcess::take_stdout) calls return `None`).
/// Reads return `Ok(0)` when the child closes stdout (EOF). Call
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
        match p.child.take_stdout() {
            Some(pipe) => *guard = StdoutState::Cached(pipe),
            None => *guard = StdoutState::GivenAway,
        }
    }
    match &mut *guard {
        StdoutState::Cached(pipe) => pipe.read(buf),
        StdoutState::NotTaken | StdoutState::GivenAway => Ok(0),
    }
}
