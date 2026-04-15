//! Test helper: exit with the given status code. Optionally writes to stdout/stderr
//! and/or sleeps before exiting.
//!
//! Usage: `pp_status <exit-code> [--out <text>] [--err <text>] [--sleep-ms <ms>]`
//!
//! Not part of procpilot's public API. Used by internal tests.

use std::io::Write;
use std::time::Duration;

fn main() {
    let mut args = std::env::args().skip(1);
    let code: i32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(0);

    let mut sleep_ms: u64 = 0;

    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_default();
        match flag.as_str() {
            "--out" => {
                let _ = std::io::stdout().write_all(value.as_bytes());
                let _ = std::io::stdout().write_all(b"\n");
                let _ = std::io::stdout().flush();
            }
            "--err" => {
                let _ = std::io::stderr().write_all(value.as_bytes());
                let _ = std::io::stderr().write_all(b"\n");
                let _ = std::io::stderr().flush();
            }
            "--sleep-ms" => {
                sleep_ms = value.parse().unwrap_or(0);
            }
            _ => {}
        }
    }

    if sleep_ms > 0 {
        std::thread::sleep(Duration::from_millis(sleep_ms));
    }

    std::process::exit(code);
}
