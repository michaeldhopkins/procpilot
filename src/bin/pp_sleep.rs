//! Test helper: sleep for the given number of milliseconds.
//!
//! Usage: `pp_sleep <ms> [--ignore-sigterm]`
//!
//! `--ignore-sigterm` (Unix) makes the process survive `SIGTERM`, so only the
//! `SIGKILL` that follows cancellation's grace period stops it.
//!
//! Not part of procpilot's public API. Used by internal tests.

use std::time::Duration;

fn main() {
    let mut args = std::env::args().skip(1);
    let ms: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(0);
    if args.any(|a| a == "--ignore-sigterm") {
        ignore_sigterm();
    }
    std::thread::sleep(Duration::from_millis(ms));
}

#[cfg(unix)]
fn ignore_sigterm() {
    // Declared here rather than through the libc crate, as src/cmd.rs does for kill(2).
    unsafe extern "C" {
        fn signal(sig: i32, handler: usize) -> usize;
    }
    const SIGTERM: i32 = 15;
    const SIG_IGN: usize = 1;
    // SAFETY: installing SIG_IGN for SIGTERM touches no memory; the call cannot fail for a valid
    // signal number.
    unsafe {
        signal(SIGTERM, SIG_IGN);
    }
}

#[cfg(not(unix))]
fn ignore_sigterm() {}
