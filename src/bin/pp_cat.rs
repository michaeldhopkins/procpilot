//! Test helper: echo stdin to stdout, byte-for-byte.
//!
//! Not part of procpilot's public API. Used by internal tests.

use std::io::{self, Read, Write};

fn main() {
    let mut buf = Vec::new();
    if io::stdin().read_to_end(&mut buf).is_ok() {
        let _ = io::stdout().write_all(&buf);
    }
}
