//! Test helper: spawn a long-sleeping grandchild and wait for it.
//!
//! Exists to exercise process-group kill-tree semantics. Parent spawns us;
//! we spawn a grandchild that sleeps; if the parent kills our PID directly
//! (not our process group), the grandchild survives. Tests can observe that
//! by reading the PID we wrote to `--sentinel`.
//!
//! Usage: `pp_child_grandchild <ms> [--sentinel <path>]`
//!
//! When invoked with `--role=inner` (an internal marker used by this binary
//! re-invoking itself), we just sleep and exit — we don't spawn another child.
//!
//! Not part of procpilot's public API. Used by internal tests.

use std::io::Write;
use std::process::Command;
use std::time::Duration;

fn main() {
    let mut args = std::env::args().skip(1);
    let ms: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(60_000);

    let mut sentinel: Option<String> = None;
    let mut is_inner = false;
    for arg in args {
        match arg.as_str() {
            "--role=inner" => is_inner = true,
            s if s.starts_with("--sentinel=") => {
                sentinel = Some(s.trim_start_matches("--sentinel=").to_string());
            }
            _ => {}
        }
    }

    // Inner re-invocation: just sleep. No further spawning, no sentinel file.
    if is_inner {
        std::thread::sleep(Duration::from_millis(ms));
        return;
    }

    // Outer invocation: spawn a grandchild (a copy of us with --role=inner)
    // that will outlive us if the caller kills only our direct PID.
    let me = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(&me)
        .arg(ms.to_string())
        .arg("--role=inner")
        .spawn()
        .expect("spawn grandchild");

    if let Some(path) = sentinel
        && let Ok(mut f) = std::fs::File::create(&path)
    {
        let _ = writeln!(f, "{}", child.id());
    }

    let _ = child.wait();
}
