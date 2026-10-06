//! Whether a child a test spawned is still running, asked of `ps` so that nothing here reaps it.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a test waits for a process it expects to die.
const GONE_DEADLINE: Duration = Duration::from_secs(10);

/// Whether `pid` names a live process. A zombie counts as gone: it has exited and only waits
/// for its parent to reap it.
pub fn alive(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .output()
        .expect("ps runs");
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

/// Polls until `pid` is gone, for at most [`GONE_DEADLINE`]. Returns whether it went, and on
/// a timeout SIGKILLs it so the failing test leaves nothing behind.
pub fn gone_within_deadline(pid: u32) -> bool {
    let deadline = Instant::now() + GONE_DEADLINE;
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    // Only a mock is ours to kill: the pid could have been reused by an unrelated process.
    if is_mock(pid) {
        let _ = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdin(Stdio::null())
            .status();
    }
    false
}

fn is_mock(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&out.stdout).contains("pp_")
}
