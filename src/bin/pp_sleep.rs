//! Test helper: sleep for the given number of milliseconds.
//!
//! Usage: `pp_sleep <ms>`
//!
//! Not part of procpilot's public API. Used by internal tests.

use std::time::Duration;

fn main() {
    let ms: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(ms));
}
