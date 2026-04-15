//! Test helper: write `N` bytes of `A` characters (0x41) to stdout.
//!
//! Usage: `pp_spam <byte-count>`
//!
//! Used to exercise pipe-buffer handling in timeout tests.
//! Not part of procpilot's public API.

use std::io::Write;

fn main() {
    let count: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(0);

    let chunk = vec![b'A'; 4096];
    let mut remaining = count;
    let mut stdout = std::io::stdout().lock();

    while remaining > 0 {
        let n = remaining.min(chunk.len());
        if stdout.write_all(&chunk[..n]).is_err() {
            return;
        }
        remaining -= n;
    }
    let _ = stdout.flush();
}
