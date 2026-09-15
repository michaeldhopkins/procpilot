//! Test helper: print the name this process was started as (`argv[0]`), followed by a newline.
//!
//! Not part of procpilot's public API. Used by internal tests.

fn main() {
    let arg0 = std::env::args_os().next().unwrap_or_default();
    println!("{}", arg0.to_string_lossy());
}
